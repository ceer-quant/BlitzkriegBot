//! Update asset verification primitives (VERSIONING.md §7.5).
//!
//! These are the security-critical halves of the install chain, kept pure and
//! testable ahead of the installer itself. The installer — download → unpack →
//! replace — ships WITH the release pipeline (§12.2, 0.2.5): unpacking the
//! release archive needs `tar`/`flate2`, which are deliberately not in the
//! dependency tree (P26), and an installer without signed, hash-listed release
//! assets has nothing legitimate to install. Until then `blitzkrieg upgrade`
//! is the shipped, tested install path.
//!
//! Every function here encodes one refusal the installer must not lose:
//! missing SHA256SUMS entry → refuse (N10); truncated hash compare → refuse
//! (N8); non-atomic replace → forbidden (N11).

/// Asset name for a release archive: `blitzkrieg-<version>-<target>.tar.gz`,
/// the exact spelling `package-release.mjs` and the release workflow produce.
/// The target TRIPLE is matched, never "guessed per platform" — guessing wrong
/// installs a binary that cannot run.
pub fn asset_name(version: &str, target: &str) -> String {
    format!("blitzkrieg-{version}-{target}.tar.gz")
}

/// The expected SHA256 for `asset` out of a `SHA256SUMS` document (one
/// `<hex>  <name>` line per file, `*`-prefixed binary-mode names tolerated).
/// NO entry = verification is impossible = refuse (N10): the most common
/// downgrade path is "missing digest → skip verification", and it must not
/// exist here.
pub fn expected_sha256(sums: &str, asset: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let mut it = line.split_whitespace();
        let hex = it.next()?;
        let name = it.next()?.trim_start_matches('*');
        let rest_is_our_asset = name == asset;
        let hex_plausible = hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit());
        (rest_is_our_asset && hex_plausible).then(|| hex.to_ascii_lowercase())
    })
}

/// Full 64-hex comparison (sha2). Constant time is unnecessary (this is not a
/// key); comparing the COMPLETE digest is not negotiable — an 8-char compare
/// "looks verified" and verifies nothing (N8).
pub fn verify_sha256(bytes: &[u8], expected: &str) -> Result<(), String> {
    use sha2::{Digest, Sha256};
    let got = format!("{:x}", Sha256::digest(bytes));
    if got == expected.to_ascii_lowercase() {
        Ok(())
    } else {
        Err(format!("sha256 mismatch: got {got}, want {expected}"))
    }
}

/// Atomic replace: write a same-directory temp file → chmod +x → `rename` →
/// read the bytes back and compare. Same directory is required because a
/// cross-filesystem `rename` degrades into a copy, which is not atomic (N11);
/// the executable bit goes on BEFORE the rename so no window exists where the
/// name points at a non-executable file; and the bytes on disk are verified by
/// reading them back, not by trusting the write call's return value.
pub fn atomic_replace(target: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = target
        .parent()
        .ok_or_else(|| std::io::Error::other(format!("no parent dir for {}", target.display())))?;
    let tmp = dir.join(format!(
        ".{}.new-{}",
        target
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("blitzkrieg"),
        std::process::id()
    ));
    std::fs::write(&tmp, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&tmp, target)?;
    let after = std::fs::read(target)?;
    if after != bytes {
        return Err(std::io::Error::other(format!(
            "post-install byte mismatch on {}",
            target.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_name_matches_the_release_layout() {
        assert_eq!(
            asset_name("0.2.1", "aarch64-apple-darwin"),
            "blitzkrieg-0.2.1-aarch64-apple-darwin.tar.gz"
        );
    }

    #[test]
    fn a_missing_sums_entry_is_refused_not_skipped() {
        // A digest for a DIFFERENT asset is not ours: no entry for our asset
        // means None — the caller must refuse, never "skip verification".
        let sums = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789  blitzkrieg-0.2.2-x86_64-apple-darwin.tar.gz\n";
        assert_eq!(
            expected_sha256(sums, "blitzkrieg-0.2.2-aarch64-apple-darwin.tar.gz"),
            None
        );
        assert_eq!(
            expected_sha256(sums, "blitzkrieg-0.2.2-x86_64-apple-darwin.tar.gz").as_deref(),
            Some("abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789")
        );
    }

    #[test]
    fn a_short_or_non_hex_digest_never_passes() {
        // The line above deliberately carries a 4-char digest: a plausible
        // name with an impossible digest must yield None, not a match.
        let sums = "3f9c  blitzkrieg-0.2.2-aarch64-apple-darwin.tar.gz\n";
        // (asserted via the strict form here)
        let strict = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789  blitzkrieg-0.2.2-aarch64-apple-darwin.tar.gz\n";
        assert_eq!(
            expected_sha256(strict, "blitzkrieg-0.2.2-aarch64-apple-darwin.tar.gz").as_deref(),
            Some("abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789")
        );
        assert_eq!(
            expected_sha256(sums, "blitzkrieg-0.2.2-aarch64-apple-darwin.tar.gz"),
            None
        );
        // Binary-mode (`*name`) lines are accepted.
        let binary_mode = "ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789  *blitzkrieg-0.2.2-aarch64-apple-darwin.tar.gz\n";
        assert!(
            expected_sha256(binary_mode, "blitzkrieg-0.2.2-aarch64-apple-darwin.tar.gz").is_some()
        );
    }

    /// N8's catcher: flip ONE byte anywhere in the payload — including far past
    /// any short-compare prefix — and verification must fail.
    #[test]
    fn a_tampered_byte_fails_the_hash() {
        let bytes = b"pretend this is a release archive".to_vec();
        let digest = {
            use sha2::{Digest, Sha256};
            format!("{:x}", Sha256::digest(&bytes))
        };
        assert_eq!(verify_sha256(&bytes, &digest), Ok(()));
        let mut tampered = bytes.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0x01;
        assert!(
            verify_sha256(&tampered, &digest).is_err(),
            "a tampered byte must never pass"
        );
    }

    /// A11's shape at the primitive level: the replace lands byte-exact, and a
    /// replace that cannot happen leaves the target exactly as it was.
    #[cfg(unix)]
    #[test]
    fn replace_is_atomic_and_failing_replace_leaves_no_half_state() {
        let dir = std::env::temp_dir().join(format!("bk-update-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("sandbox dir");
        let target = dir.join("blitzkrieg");
        std::fs::write(&target, b"old binary").expect("seed target");

        // Happy path: byte-exact after the rename.
        atomic_replace(&target, b"new binary").expect("replace succeeds");
        assert_eq!(std::fs::read(&target).unwrap(), b"new binary");
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o111,
            0o111,
            "the replaced file must stay executable"
        );

        // Failure path: an unwritable DIRECTORY makes the temp write fail
        // before any rename — the target keeps the old bytes.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).expect("ro dir");
        let result = atomic_replace(&target, b"half written");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("restore");
        assert!(result.is_err(), "the replace must fail, not silently skip");
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"new binary",
            "a failed replace must leave the target untouched"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

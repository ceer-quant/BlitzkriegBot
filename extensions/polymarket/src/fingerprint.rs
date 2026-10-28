//! Order-source fingerprint (#694) — the kernel's one configuration surface
//! for "who placed this order", read from env here and consumed by `venue.rs`.
//!
//! Two independent mechanisms, both opt-in, both leaving the EIP-712 order
//! structure untouched:
//!
//! **A — dedicated API key (owner is the identity).** `POST /auth/api-key`
//! derives a key deterministically from (address, nonce); the web session and
//! any other key holder all share nonce 0, which is why fills could not be
//! attributed. `POLYMARKET_API_KEY_NONCE` moves the KERNEL's derivation to its
//! own nonce — the fill's `owner` field then names the kernel's key id. The
//! default (unset/empty/0) keeps the historical nonce-0 key: a deployment that
//! never sets the variable does not silently change identity mid-life.
//!
//! **B — deployment watermark in the order salt.** Every order's `salt` (an
//! EIP-712 field, readable back via authenticated `GET /data/order/{id}`) is
//! normally venue-random. A custom `salt_generator` composes it instead:
//! high 20 bits = deployment fingerprint, low 33 bits = per-order random. The
//! SDK masks the value to `Number.MAX_SAFE_INTEGER` (2^53−1), and the layout
//! below fills exactly those 53 bits, so composition survives the mask
//! byte-for-byte. The fingerprint comes from `BLITZKRIEG_DEPLOY_FINGERPRINT`
//! when set; otherwise it is derived once from machine identity + install
//! moment and persisted at `data/runtime/deploy-fingerprint` so restarts keep
//! the same value (the "robot instance id" semantics: same machine, fresh
//! install → new fingerprint).
//!
//! Neither value is secret and neither is echoed anywhere sensitive: the one
//! startup line prints the 20-bit fingerprint and the nonce number only.

use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::OnceLock;

// ── Salt layout ──────────────────────────────────────────────────────────────

/// High-bit width reserved for the deployment fingerprint.
pub const FINGERPRINT_BITS: u32 = 20;
/// Low-bit width left to per-order randomness.
pub const RANDOM_BITS: u32 = 33;
/// `0xFFFFF` — the 20-bit fingerprint field.
pub const FINGERPRINT_MASK: u32 = (1 << FINGERPRINT_BITS) - 1;
/// The 33-bit random field.
pub const RANDOM_MASK: u64 = (1 << RANDOM_BITS) - 1;
/// Composed salts never exceed this — the SDK's `Number.MAX_SAFE_INTEGER`
/// mask (`to_ieee_754_int`), so a composed salt round-trips the venue's
/// JSON-number serialization bit-for-bit.
pub const SALT_MAX: u64 = (1 << (FINGERPRINT_BITS + RANDOM_BITS)) - 1;

/// Compose one order salt: fingerprint in the high 20 bits, per-order random
/// in the low 33. Inputs are masked first, so the result is always ≤
/// [`SALT_MAX`] no matter what the caller passes.
pub fn compose_salt(fingerprint: u32, random: u64) -> u64 {
    (u64::from(fingerprint & FINGERPRINT_MASK) << RANDOM_BITS) | (random & RANDOM_MASK)
}

/// Read the deployment fingerprint back out of a salt (the forensic read:
/// authenticated `GET /data/order/{id}` → salt → this).
pub fn salt_fingerprint(salt: u64) -> u32 {
    ((salt >> RANDOM_BITS) as u32) & FINGERPRINT_MASK
}

/// Read the per-order random part back out of a salt. Two orders from the same
/// deployment must never collide here — that is what keeps the watermark from
/// perturbing order uniqueness.
pub fn salt_random(salt: u64) -> u64 {
    salt & RANDOM_MASK
}

// ── Env parsing (Plan A + B's fingerprint override) ─────────────────────────
//
// Both parsers take the RAW value as a parameter instead of reading the
// process env themselves: the env read stays one line at the call site and the
// parsing is testable without mutating process-global state.

/// Parse `POLYMARKET_API_KEY_NONCE` — the kernel API-key derivation nonce.
/// Unset/empty → `None` (the SDK's own default: nonce 0, the historical
/// shared-with-web key). `0` is accepted as an explicit no-op. Malformed
/// values are a hard error — a typo must not quietly fall back to nonce 0 and
/// misattribute every order. The error names the variable, never the value.
pub fn parse_api_key_nonce(raw: Option<&str>) -> anyhow::Result<Option<u32>> {
    const WHAT: &str = "POLYMARKET_API_KEY_NONCE must be a decimal or 0x-hex u32";
    let Some(raw) = raw.map(str::trim).filter(|r| !r.is_empty()) else {
        return Ok(None);
    };
    let value = if let Some(hex) = raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X")) {
        u32::from_str_radix(hex, 16).map_err(|_| anyhow::anyhow!("{WHAT}"))?
    } else {
        raw.parse::<u32>().map_err(|_| anyhow::anyhow!("{WHAT}"))?
    };
    Ok(Some(value))
}

/// Strict numeric token: decimal or `0x`-hex, masked to the 20-bit field.
/// This is the ONLY form the fingerprint file accepts — the file's content is
/// machine-written, so anything non-numeric there is corruption, not intent,
/// and must read back as None (regenerate) rather than hash to a stable lie.
/// The hash-a-name path exists for the ENV alone, where an operator typed it.
fn parse_fingerprint_token(raw: &str) -> Option<u32> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if let Some(hex) = raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X")) {
        return u32::from_str_radix(hex, 16)
            .ok()
            .map(|v| v & FINGERPRINT_MASK);
    }
    raw.parse::<u32>().ok().map(|v| v & FINGERPRINT_MASK)
}

/// Parse `BLITZKRIEG_DEPLOY_FINGERPRINT` into the 20-bit watermark. Accepts a
/// decimal or `0x`-hex number (masked to 20 bits) or — for operators who
/// prefer names over numbers — any other string, hashed. Unset/empty → `None`
/// (fall through to the persisted default).
pub fn parse_fingerprint_env(raw: Option<&str>) -> Option<u32> {
    let raw = raw.map(str::trim).filter(|r| !r.is_empty())?;
    if let Some(v) = parse_fingerprint_token(raw) {
        return Some(v);
    }
    Some(fingerprint_of_bytes(raw.as_bytes()))
}

// ── Default fingerprint: machine identity + install moment, persisted ───────

/// Where the default fingerprint persists, cwd-relative like every other
/// `data/` path the core writes (`data/runtime/` is already the runtime-cell
/// directory). One tiny file, one hex token.
pub const FINGERPRINT_FILE: &str = "data/runtime/deploy-fingerprint";

/// Where the fingerprint came from — the startup line reports this so an
/// operator can tell an env-pinned watermark from a generated one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FingerprintSource {
    /// `BLITZKRIEG_DEPLOY_FINGERPRINT` pinned it; the file is not consulted
    /// and not written.
    Env,
    /// Read back from [`FINGERPRINT_FILE`] (a previous run persisted it).
    File,
    /// First boot on this install: generated from machine identity + install
    /// moment and persisted.
    Generated,
}

/// First 20 bits of SHA-256 over `input`.
fn fingerprint_of_bytes(input: &[u8]) -> u32 {
    let digest = Sha256::digest(input);
    u32::from_be_bytes(digest[..4].try_into().expect("sha256 digest is 32 bytes"))
        & FINGERPRINT_MASK
}

/// Best-effort stable machine identity: freedesktop machine-id (Linux),
/// IOPlatformUUID (macOS), hostname as the last resort. Failure degrades to an
/// empty string on purpose — the install-moment salt still separates two
/// machines that both refuse to identify themselves, and the venue must not
/// fail to start over a fingerprint nicety.
fn machine_identity() -> String {
    for path in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
        if let Ok(text) = std::fs::read_to_string(path) {
            let id = text.trim();
            if !id.is_empty() {
                return id.to_string();
            }
        }
    }
    if let Ok(output) = std::process::Command::new("ioreg")
        .args(["-rd1", "-c", "IOPlatformExpertDevice"])
        .output()
        && output.status.success()
    {
        let text = String::from_utf8_lossy(&output.stdout);
        for line in text.lines() {
            if let Some(eq) = line
                .find("\"IOPlatformUUID\"")
                .and_then(|i| line[i..].find("= ").map(|o| i + o))
            {
                let value = line[eq + 2..].trim().trim_matches('"');
                if !value.is_empty() {
                    return value.to_string();
                }
            }
        }
    }
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("HOST"))
        .unwrap_or_default()
}

fn read_fingerprint_file(path: &Path) -> Option<u32> {
    parse_fingerprint_token(&std::fs::read_to_string(path).ok()?)
}

/// Atomic write (temp + rename), matching the crate's other durable-cell
/// writes: a crash mid-write never leaves a half token that a later boot would
/// have to regenerate (regeneration is the one thing this file exists to
/// prevent).
fn write_fingerprint_file(path: &Path, fp: u32) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    // Decimal on purpose: the read side (`parse_fingerprint_env`) takes a
    // bare number as a number, but a bare HEX string would fall through to
    // the hash-a-name path and break restart stability.
    std::fs::write(&tmp, format!("{fp}\n"))?;
    std::fs::rename(&tmp, path)
}

/// File-or-generate half of [`init_deploy_fingerprint`], with the inputs
/// injected so tests can drive it against a scratch directory.
fn resolve_fingerprint_file(
    path: &Path,
    machine_id: &str,
    install_nanos: u128,
) -> std::io::Result<(u32, FingerprintSource)> {
    if let Some(fp) = read_fingerprint_file(path) {
        return Ok((fp, FingerprintSource::File));
    }
    // A corrupt token regenerates loudly: the alternative (fail the venue
    // spawn) trades a forensic nicety for an outage, but the loss of stability
    // must be visible in the run log.
    let fp = fingerprint_of_bytes(format!("{machine_id}|{install_nanos}").as_bytes());
    if let Err(e) = write_fingerprint_file(path, fp) {
        eprintln!(
            "polymarket-extension: deploy fingerprint file write failed ({e}); fingerprint will not survive restarts"
        );
    }
    Ok((fp, FingerprintSource::Generated))
}

// ── The salt generator the SDK hooks in ─────────────────────────────────────
//
// The SDK's hook is a plain `fn() -> u64` (no closures), so the fingerprint
// lives in process-global state set once at venue spawn. Before any spawn
// (backtests, dry runs) it stays 0: composed salts are then pure per-order
// random — unique, just unwatermarked, which is exactly the historical
// behaviour.

static DEPLOY_FINGERPRINT: OnceLock<u32> = OnceLock::new();

/// Pin the process-wide deployment fingerprint. First writer wins; later calls
/// (a second venue spawn) keep the pinned value so one process cannot straddle
/// two identities. Returns `false` when something was already pinned.
pub fn pin_deploy_fingerprint(fp: u32) -> bool {
    DEPLOY_FINGERPRINT.set(fp).is_ok()
}

/// The pinned fingerprint, or 0 when never pinned.
pub fn current_deploy_fingerprint() -> u32 {
    *DEPLOY_FINGERPRINT.get().unwrap_or(&0)
}

/// Per-order random half: SHA-256 over (nanos, process counter, pid). The salt
/// is not security-sensitive (the SDK's own generator is a time×random float),
/// so a hashed counter is enough — what matters is that two orders never share
/// the low 33 bits.
fn next_random() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut hasher = Sha256::new();
    hasher.update(nanos.to_le_bytes());
    hasher.update(n.to_le_bytes());
    hasher.update(std::process::id().to_le_bytes());
    let digest = hasher.finalize();
    u64::from_le_bytes(digest[..8].try_into().expect("sha256 digest is 32 bytes")) & RANDOM_MASK
}

/// The hook handed to the SDK's `salt_generator`: fingerprint in the high 20
/// bits, fresh randomness in the low 33, always within [`SALT_MAX`].
pub fn salt_generator() -> u64 {
    compose_salt(current_deploy_fingerprint(), next_random())
}

// ── Startup wiring ───────────────────────────────────────────────────────────

/// Resolve the deployment fingerprint (env > file > generate-and-persist),
/// pin it for [`salt_generator`], and report it once. Never fails: every
/// fallback path still yields a fingerprint, because an unattributable order
/// is worse than a generated one.
pub fn init_deploy_fingerprint() -> (u32, FingerprintSource) {
    // 1. Operator pin. The file is deliberately untouched: an env pin means
    //    the operator, not this install, owns the identity.
    if let Some(fp) = parse_fingerprint_env(
        std::env::var("BLITZKRIEG_DEPLOY_FINGERPRINT")
            .ok()
            .as_deref(),
    ) {
        pin_deploy_fingerprint(fp);
        return (fp, FingerprintSource::Env);
    }
    // 2./3. Persisted default, generated on first boot.
    let install_nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let (fp, source) = match resolve_fingerprint_file(
        Path::new(FINGERPRINT_FILE),
        &machine_identity(),
        install_nanos,
    ) {
        Ok(found) => found,
        Err(e) => {
            // Only a read error (as opposed to a missing file) lands here;
            // fall back to a transient fingerprint rather than refusing to
            // trade, and say so.
            eprintln!(
                "polymarket-extension: deploy fingerprint file unreadable ({e}); using a transient one"
            );
            (
                fingerprint_of_bytes(
                    format!("{}|{}", machine_identity(), install_nanos).as_bytes(),
                ),
                FingerprintSource::Generated,
            )
        }
    };
    pin_deploy_fingerprint(fp);
    (fp, source)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::path::PathBuf;

    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bk-694-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[test]
    fn salt_compose_decompose_roundtrips_across_the_field() {
        // Boundary values plus typicals: every composed salt must survive the
        // roundtrip AND stay within the JS-safe mask the venue applies.
        for fp in [0u32, 1, 0xFFFFF, 0x1_0000, u32::MAX] {
            for rand in [0u64, 1, RANDOM_MASK, RANDOM_MASK - 1, u64::MAX] {
                let salt = compose_salt(fp, rand);
                assert!(salt <= SALT_MAX, "salt exceeds JS_SAFE_INTEGER: {salt}");
                assert_eq!(salt_fingerprint(salt), fp & FINGERPRINT_MASK);
                assert_eq!(salt_random(salt), rand & RANDOM_MASK);
            }
        }
    }

    #[test]
    fn salt_max_is_exactly_the_53_bit_boundary() {
        assert_eq!(SALT_MAX, (1u64 << 53) - 1);
        assert_eq!(
            compose_salt(FINGERPRINT_MASK, RANDOM_MASK),
            SALT_MAX,
            "full fields must compose to the exact JS-safe bound, not one less"
        );
    }

    #[test]
    fn low_bits_vary_per_order_and_never_collide() {
        // One writer, first-wins: pin a distinctive fingerprint and assert
        // every generated salt carries it in the high bits while the low 33
        // bits stay collision-free across a realistic burst.
        pin_deploy_fingerprint(0xA5A5A);
        let fp = current_deploy_fingerprint();
        let count = 10_000;
        let mut seen = HashSet::with_capacity(count);
        for _ in 0..count {
            let salt = salt_generator();
            assert_eq!(
                salt_fingerprint(salt),
                fp,
                "high bits must be the pinned fingerprint"
            );
            assert!(
                seen.insert(salt_random(salt)),
                "random collision in {count} salts"
            );
        }
    }

    #[test]
    fn api_key_nonce_env_parses_strictly() {
        assert_eq!(parse_api_key_nonce(None).unwrap(), None);
        assert_eq!(parse_api_key_nonce(Some("")).unwrap(), None);
        assert_eq!(parse_api_key_nonce(Some("  ")).unwrap(), None);
        // Explicit 0 = the historical shared key, by choice.
        assert_eq!(parse_api_key_nonce(Some("0")).unwrap(), Some(0));
        assert_eq!(parse_api_key_nonce(Some("1")).unwrap(), Some(1));
        assert_eq!(parse_api_key_nonce(Some(" 2 ")).unwrap(), Some(2));
        assert_eq!(parse_api_key_nonce(Some("0x10")).unwrap(), Some(16));
        assert_eq!(
            parse_api_key_nonce(Some(u32::MAX.to_string().as_str())).unwrap(),
            Some(u32::MAX)
        );
        // Malformed is a hard error, never a silent nonce-0 fallback.
        assert!(parse_api_key_nonce(Some("abc")).is_err());
        assert!(parse_api_key_nonce(Some("-1")).is_err());
        assert!(parse_api_key_nonce(Some("4294967296")).is_err());
    }

    #[test]
    fn fingerprint_env_accepts_numbers_hex_and_names() {
        assert_eq!(parse_fingerprint_env(None), None);
        assert_eq!(parse_fingerprint_env(Some("")), None);
        assert_eq!(parse_fingerprint_env(Some("0")), Some(0));
        assert_eq!(parse_fingerprint_env(Some("42")), Some(42));
        assert_eq!(parse_fingerprint_env(Some("0xFFFFF")), Some(0xFFFFF));
        // Out-of-range numbers mask to the 20-bit field.
        assert_eq!(parse_fingerprint_env(Some("0x100000")), Some(0));
        assert_eq!(parse_fingerprint_env(Some("1048577")), Some(1));
        // Non-numeric strings hash deterministically into the field.
        let a = parse_fingerprint_env(Some("edge-pdx-1")).expect("named fingerprint");
        let b = parse_fingerprint_env(Some("edge-pdx-1")).expect("named fingerprint");
        let c = parse_fingerprint_env(Some("edge-pdx-2")).expect("named fingerprint");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn fingerprint_file_is_stable_across_restart_and_regenerates_when_corrupt() {
        let dir = scratch_dir("stable");
        let path = dir.join("runtime").join("deploy-fingerprint");

        // First boot: generated from (machine, install moment), persisted.
        let (first, source) =
            resolve_fingerprint_file(&path, "machine-A", 1_000).expect("first resolve");
        assert_eq!(source, FingerprintSource::Generated);
        assert!(first <= FINGERPRINT_MASK);
        assert!(path.is_file(), "fingerprint must be persisted for restarts");

        // Restart with a DIFFERENT machine clock: the file wins, the identity
        // is stable — that is the entire point of persisting it.
        let (second, source) =
            resolve_fingerprint_file(&path, "machine-A", 9_999_999).expect("second resolve");
        assert_eq!(source, FingerprintSource::File);
        assert_eq!(first, second);

        // Different install inputs must not silently map to the same token:
        // spot-check that the generator actually consumes both fields.
        let (other, _) =
            resolve_fingerprint_file(&dir.join("other"), "machine-B", 1_000).expect("other");
        let (later, _) =
            resolve_fingerprint_file(&dir.join("later"), "machine-A", 1_001).expect("later");
        assert_ne!(other, first);
        assert_ne!(later, first);

        // A corrupt token regenerates (loudly) instead of wedging the venue.
        std::fs::write(&path, "zzz-not-hex").unwrap();
        let (regen, source) =
            resolve_fingerprint_file(&path, "machine-A", 1_000).expect("regen resolve");
        assert_eq!(source, FingerprintSource::Generated);
        assert!(regen <= FINGERPRINT_MASK);

        std::fs::remove_dir_all(&dir).ok();
    }
}

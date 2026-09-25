//! Build provenance, at runtime (VERSIONING.md §3.3).
//!
//! `build.rs` stamps the compiled tree; this crate is the ONE runtime spelling
//! of it: `--version`, `blitzkrieg version`, `system.version` and the startup
//! banner all read from here, so they cannot disagree about which code is
//! running — the exact failure class #172/#179 came from.
//!
//! No literal version may appear in this file (guard: N4 in VERSIONING.md §10.2).

/// The complete build info. `const`, so reading it costs nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildInfo {
    /// `<major>.<minor>.<patch>[-rc.N]`, from the root `[workspace.package].version`.
    pub version: &'static str,
    /// 12-hex short git revision, or `nogit` (no git and no explicit stamp).
    pub git_hash: &'static str,
    /// `"1"` when tracked files were modified at build time, `"0"` otherwise.
    pub git_dirty: &'static str,
    /// Build moment (UTC), e.g. `20260925T073811Z`.
    pub build_date: &'static str,
    /// Compilation target triple, e.g. `aarch64-apple-darwin`.
    pub target: &'static str,
}

pub const BUILD_INFO: BuildInfo = BuildInfo {
    version: env!("BLITZKRIEG_VERSION"),
    git_hash: env!("BLITZKRIEG_GIT_HASH"),
    git_dirty: env!("BLITZKRIEG_GIT_DIRTY"),
    build_date: env!("BLITZKRIEG_BUILD_DATE"),
    target: env!("BLITZKRIEG_TARGET"),
};

impl BuildInfo {
    pub fn is_dirty(&self) -> bool {
        self.git_dirty == "1"
    }

    /// The build could not name its own revision (tarball, git-less environment).
    pub fn revision_unknown(&self) -> bool {
        self.git_hash == "nogit"
    }

    /// `g<sha>` / `nogit` — only the build-metadata half of the version string.
    pub fn revision_token(&self) -> String {
        if self.revision_unknown() {
            "nogit".to_string()
        } else {
            format!("g{}", self.git_hash)
        }
    }

    /// **The format the gates parse**: `<semver>+g<sha>` / `<semver>+nogit`,
    /// single line, no spaces. `scripts/lib/core-provenance.mjs` matches its
    /// `VERSION_RE` against a binary's self-description; changing this format
    /// means changing that regex in the same commit (VERSIONING.md V8-5).
    pub fn version_string(&self) -> String {
        format!("{}+{}", self.version, self.revision_token())
    }

    /// One human-readable line naming the code this process is: revision, dirty
    /// flag and version — what an incident review pins a log to a commit with.
    pub fn provenance_line(&self) -> String {
        format!(
            "version {} (git {}, {} build)",
            self.version_string(),
            self.git_hash,
            if self.is_dirty() { "dirty" } else { "clean" }
        )
    }

    /// The JSON shape shared by `blitzkrieg version --json` and `system.version`.
    /// serde is deliberately not involved: this crate stays zero-dependency
    /// (P6), so any process can link it safely. Keys are **camelCase**, matching
    /// the IPC wire contract (`schema.rs`, repo-wide).
    pub fn to_json(&self) -> String {
        let s = |v: &str| json_string(v);
        format!(
            "{{\"version\":{},\"gitHash\":{},\"gitDirty\":{},\"buildDate\":{},\"target\":{}}}",
            s(self.version),
            s(self.git_hash),
            self.is_dirty(),
            s(self.build_date),
            s(self.target),
        )
    }
}

/// Minimal JSON string escaping. These values all come from the build
/// environment (version, sha, target triple, timestamp) and should never
/// contain control characters; they are escaped anyway because one weird
/// character from the environment must not be able to break a JSON parser.
fn json_string(v: &str) -> String {
    let mut out = String::with_capacity(v.len() + 2);
    out.push('"');
    for c in v.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_string_is_the_format_the_gate_parses() {
        let v = BUILD_INFO.version_string();
        let (semver, meta) = v.split_once('+').expect("must carry +<metadata>");
        assert_eq!(semver, BUILD_INFO.version);
        assert!(
            meta == "nogit" || (meta.starts_with('g') && (4..=40).contains(&(meta.len() - 1))),
            "unexpected revision token: {meta}"
        );
        // Byte-equivalent with the gate's VERSION_RE: bare hex after the `g`.
        let ok = meta == "nogit" || meta[1..].chars().all(|c| c.is_ascii_hexdigit());
        assert!(ok, "revision token is not hex: {meta}");
    }

    #[test]
    fn the_stamp_is_never_a_placeholder() {
        // "Not stamped" must be distinguishable: 0.0.0 / empty values / an empty
        // target are all failures of the stamper, not facts about the build.
        assert_ne!(BUILD_INFO.version, "0.0.0", "build.rs did not run");
        assert!(!BUILD_INFO.version.is_empty(), "empty version stamp");
        assert!(!BUILD_INFO.target.is_empty(), "empty target stamp");
        assert_eq!(BUILD_INFO.build_date.len(), 16, "{}", BUILD_INFO.build_date);
        assert!(
            BUILD_INFO.build_date.ends_with('Z'),
            "{}",
            BUILD_INFO.build_date
        );
    }

    #[test]
    fn the_json_shape_matches_the_ipc_contract() {
        let j = BUILD_INFO.to_json();
        for key in [
            "\"version\"",
            "\"gitHash\"",
            "\"gitDirty\"",
            "\"buildDate\"",
            "\"target\"",
        ] {
            assert!(j.contains(key), "{j} is missing {key}");
        }
        assert!(j.starts_with('{') && j.ends_with('}'), "{j}");
        // gitDirty must be a JSON boolean, not a string — the WebUI binds it.
        assert!(
            j.contains("\"gitDirty\":true") || j.contains("\"gitDirty\":false"),
            "{j}"
        );
    }

    #[test]
    fn a_missing_revision_is_named_not_faked() {
        let t = if BUILD_INFO.revision_unknown() {
            "nogit".to_string()
        } else {
            format!("g{}", BUILD_INFO.git_hash)
        };
        assert!(!t.is_empty());
        assert_ne!(t, "g");
    }
}

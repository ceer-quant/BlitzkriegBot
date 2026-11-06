//! Configuration provenance vocabulary.
//!
//! The kernel's settings come from CLI flags, environment variables, and
//! built-in defaults — full stop. There is NO TOML config file layer
//! (issue #967, operator ruling 2026-11-06: NO TOML at all as live source
//! or fallback layer).
//!
//! **CLI > env > built-in default**
//!
//! Resolution is a pure function ([`pick`]) so the precedence is testable
//! without a process. Every effective value carries its [`Source`], and the
//! binary logs the non-default ones at startup — "where did this number come
//! from" is answerable from the log alone, which is the point of a
//! declarative config layer. `source=toml` is a regression failure.

use rust_decimal::Decimal;

/// Lock 2 ceiling: a per-step gradient above this is rejected outright
/// (see `shadow_evolution::guard`).
pub const MAX_GRADIENT_CEILING: Decimal = rust_decimal_macros::dec!(0.05);

/// Where an effective setting came from, highest precedence first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    /// Command-line flag.
    Cli,
    /// `BK_*` environment variable.
    Env,
    /// The value compiled into the binary.
    Default,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Cli => "cli",
            Source::Env => "env",
            Source::Default => "default",
        }
    }
}

/// A value plus where it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct Sourced<T> {
    pub value: T,
    pub source: Source,
}

impl<T> Sourced<T> {
    pub fn new(value: T, source: Source) -> Self {
        Self { value, source }
    }

    /// True when anything above the compiled default supplied the value.
    pub fn is_explicit(&self) -> bool {
        self.source != Source::Default
    }
}

/// Resolve one setting through the precedence chain. The single place the
/// order is expressed; every caller goes through it.
pub fn pick<T>(cli: Option<T>, env: Option<T>, default: T) -> Sourced<T> {
    if let Some(v) = cli {
        Sourced::new(v, Source::Cli)
    } else if let Some(v) = env {
        Sourced::new(v, Source::Env)
    } else {
        Sourced::new(default, Source::Default)
    }
}

// ── Extension config (`extensions/<name>/config.toml`) ──────────────────────

/// What an extension's config file declares about itself.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExtensionMeta {
    pub name: Option<String>,
    pub version: Option<String>,
    pub extension_type: Option<String>,
    /// The file's own claim about whether the extension should start enabled.
    pub enabled: Option<bool>,
    pub description: Option<String>,
}

/// A declared setting the kernel does not act on, with why.
#[derive(Debug, Clone, PartialEq)]
pub struct DeclaredOnly {
    pub key: String,
    pub reason: &'static str,
}

/// The result of reading an extension's config file.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExtensionConfig {
    pub path: Option<std::path::PathBuf>,
    pub meta: ExtensionMeta,
    /// Keys the file declares that the kernel reads but does not yet act on.
    /// Reported separately from [`unknown_keys`](Self::unknown_keys) so
    /// "documented, not wired" and "typo" are not the same message.
    pub declared_only: Vec<DeclaredOnly>,
    pub unknown_keys: Vec<String>,
    pub warnings: Vec<String>,
}

/// Sections an extension config may declare. The kernel validates these but does
/// not act on them: acting on `[market]`/`[risk]` needs a market adapter wired
/// into the order pipeline, and inventing one from a config file is a trading
/// behaviour change, not a config change.
const EXTENSION_DECLARED_SECTIONS: [(&str, &str); 3] = [
    (
        "market",
        "market wiring is by Cargo feature + `--market-plugin`; there is no adapter that reads these",
    ),
    (
        "risk",
        "risk limits come from RiskConfig; a second source here could silently contradict it",
    ),
    (
        "dependencies",
        "dependencies resolve at build time via Cargo, not at load time",
    ),
];

impl ExtensionConfig {
    /// Read `extensions/<name>/config.toml`. A missing file is fine (an extension
    /// need not have one) and a malformed one only warns.
    pub fn load(path: &std::path::Path) -> Self {
        let mut out = Self {
            path: Some(path.to_path_buf()),
            ..Self::default()
        };
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound {
                    out.warnings.push(format!("{}: {e}", path.display()));
                }
                out.path = None;
                return out;
            }
        };
        let value: toml::Value = match toml::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                out.warnings
                    .push(format!("{}: malformed TOML: {e}", path.display()));
                out.path = None;
                return out;
            }
        };
        let Some(root) = value.as_table() else {
            out.warnings
                .push(format!("{}: not a TOML table", path.display()));
            return out;
        };
        for (section, table) in root {
            let Some(table) = table.as_table() else {
                out.unknown_keys.push(section.clone());
                continue;
            };
            match section.as_str() {
                "meta" => {
                    for (key, v) in table {
                        let full = format!("meta.{key}");
                        let w = &mut out.warnings;
                        match key.as_str() {
                            "name" => got(&mut out.meta.name, string(v), &full, w),
                            "version" => got(&mut out.meta.version, string(v), &full, w),
                            "extension_type" => {
                                got(&mut out.meta.extension_type, string(v), &full, w)
                            }
                            "enabled" => got(&mut out.meta.enabled, bool_(v), &full, w),
                            "description" => got(&mut out.meta.description, string(v), &full, w),
                            _ => out.unknown_keys.push(full),
                        }
                    }
                }
                // Recognised, validated, and honestly reported as not acted on.
                s if EXTENSION_DECLARED_SECTIONS.iter().any(|(n, _)| *n == s) => {
                    let reason = EXTENSION_DECLARED_SECTIONS
                        .iter()
                        .find(|(n, _)| *n == s)
                        .map(|(_, r)| *r)
                        .unwrap_or("not acted on");
                    for key in table.keys() {
                        out.declared_only.push(DeclaredOnly {
                            key: format!("{s}.{key}"),
                            reason,
                        });
                    }
                }
                other => out.unknown_keys.push(other.to_string()),
            }
        }
        out
    }

    /// Check the file's self-description against the extension actually
    /// registered under that name. A config that has drifted from the code is
    /// one of the ways a "config file" stops being trustworthy, so it is
    /// surfaced at startup rather than left to be discovered.
    pub fn check_against(&self, name: &str, version: &str, kind: &str) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(n) = &self.meta.name
            && n != name
        {
            out.push(format!(
                "config declares name '{n}' but the registered extension is '{name}'"
            ));
        }
        if let Some(v) = &self.meta.version
            && v != version
        {
            out.push(format!(
                "config declares version '{v}' but the linked extension is '{version}'"
            ));
        }
        if let Some(t) = &self.meta.extension_type
            && t != kind
        {
            out.push(format!(
                "config declares type '{t}' but the linked extension is '{kind}'"
            ));
        }
        out
    }
}

/// Record a parsed key, or a precise warning when the value had the wrong type.
/// An unusable value leaves the key unset rather than guessing at it.
fn got<T>(slot: &mut Option<T>, parsed: Result<T, String>, key: &str, warnings: &mut Vec<String>) {
    match parsed {
        Ok(v) => *slot = Some(v),
        Err(what) => warnings.push(format!("{key}: expected {what}; ignoring the key")),
    }
}

fn string(v: &toml::Value) -> Result<String, String> {
    v.as_str()
        .map(str::to_string)
        .ok_or_else(|| "a string".to_string())
}

fn bool_(v: &toml::Value) -> Result<bool, String> {
    v.as_bool().ok_or_else(|| "a boolean".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn precedence_is_cli_env_default() {
        // Every layer present: CLI wins.
        let got = pick(Some(1), Some(2), 0);
        assert_eq!((got.value, got.source), (1, Source::Cli));
        // No CLI: env wins over default.
        let got = pick(None, Some(2), 0);
        assert_eq!((got.value, got.source), (2, Source::Env));
        // Nothing given: the compiled default, flagged as such.
        let got = pick(None::<i32>, None, 0);
        assert_eq!((got.value, got.source), (0, Source::Default));
        assert!(!got.is_explicit());
        assert!(pick(Some(1), None, 0).is_explicit());
    }
}

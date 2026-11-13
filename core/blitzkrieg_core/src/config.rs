//! Effective-setting provenance for the kernel's startup chain.
//!
//! The TOML config layer is RETIRED (issue #967, phase 5): no file is an
//! effective source any more. The kernel's settings come from, in order,
//!
//! **CLI > env (`.env` folded into the process environment) > built-in default**
//!
//! Resolution is a pure function ([`pick`]) so the precedence is testable
//! without a process. Every effective value carries its [`Source`], and the
//! binary logs the non-default ones at startup — "where did this number come
//! from" is answerable from the log alone.
//!
//! A `source=toml` line in a boot log is a regression, not a configuration.

use std::path::{Path, PathBuf};

use rust_decimal::Decimal;

/// Where an effective setting came from, highest precedence first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    /// Command-line flag.
    Cli,
    /// `BK_*` / `SHADOW_*` environment variable (from the deployment env or
    /// the `.env` file the launcher folds into it).
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

/// Lock 2's built-in ceiling (±5% per evolution step). An operator may lower
/// `max_gradient` but a value above this is rejected with a warning: the four
/// safety locks are not configurable surfaces, and a setting that could widen
/// one is not a tuning knob.
pub const MAX_GRADIENT_CEILING: Decimal = rust_decimal_macros::dec!(0.05);

// ── Extension config (`extensions/<name>/config.toml`) ──────────────────────
//
// NOT an operator settings layer: an extension's own manifest, read by name at
// load time and never competing with the kernel's `.env` chain.

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
    pub path: Option<PathBuf>,
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
    pub fn load(path: &Path) -> Self {
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

/// The env suffix for an account id: uppercased, every non-alphanumeric
/// mapped to `_`. The same grammar as the execution-policy per-account keys
/// (`POLICY_BUDGET_RATIO_BINANCE_MAIN`), reused so risk/exit per-account keys
/// read the same way an operator already expects.
pub fn account_env_suffix(account_id: &str) -> String {
    account_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// The account-suffixed spelling of a cockpit env key. `BK_RISK_MAX_POSITION_SIZE`
/// + `paper` → `BK_RISK_MAX_POSITION_SIZE_PAPER`.
///
/// The infix spelling (`BK_RISK_PAPER_MAX_POSITION_SIZE`) is also accepted on
/// read; the suffix form is what the write path emits.
pub fn account_env_key(base: &str, account_id: &str) -> String {
    format!("{base}_{}", account_env_suffix(account_id))
}

/// Per-account exit-discipline overrides, derived from the SAME effective
/// global [`crate::exit_policy::ExitConfig`] the process resolved at boot.
///
/// Only accounts that actually carry a `BK_EXIT_*_<ACCOUNT>` key get a row;
/// every other account falls back to the global config, so a later global
/// change is still visible to it (the same fallback rule as the risk matrix).
pub fn account_exit_overrides<I, S>(
    base: &crate::exit_policy::ExitConfig,
    accounts: I,
    lookup: impl Fn(&str) -> Option<String>,
) -> std::collections::HashMap<String, crate::exit_policy::ExitConfig>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut out = std::collections::HashMap::new();
    for account in accounts {
        let account = account.as_ref();
        let suffix = account_env_suffix(account);
        let mut cfg = base.clone();
        let mut touched = false;
        macro_rules! dec_key {
            ($field:ident, $env:expr) => {{
                let key = format!("BK_EXIT_{}_{}", $env, suffix);
                if let Some(raw) = lookup(&key) {
                    if let Ok(v) = raw.trim().parse::<rust_decimal::Decimal>() {
                        cfg.$field = v;
                        touched = true;
                    } else {
                        tracing::warn!(key = %key, raw = %raw, "per-account exit key unparseable — global value stays");
                    }
                }
            }};
        }
        dec_key!(stop_loss_pct, "STOP_LOSS_PCT");
        dec_key!(take_profit_pct, "TAKE_PROFIT_PCT");
        dec_key!(min_trail_pct, "MIN_TRAIL_PCT");
        dec_key!(trailing_min_high_pct, "TRAILING_MIN_HIGH_PCT");
        let force_key = format!("BK_EXIT_FORCE_EXIT_SEC_{suffix}");
        if let Some(raw) = lookup(&force_key) {
            match raw.trim().parse::<i64>() {
                Ok(v) => {
                    cfg.force_exit_sec = v;
                    touched = true;
                }
                Err(_) => tracing::warn!(
                    key = %force_key,
                    raw = %raw,
                    "per-account exit key unparseable — global value stays"
                ),
            }
        }
        let trailing_key = format!("BK_EXIT_TRAILING_ENABLED_{suffix}");
        if let Some(raw) = lookup(&trailing_key)
            && let Some(v) = parse_env_bool(&raw)
        {
            cfg.trailing_enabled = v;
            touched = true;
        }
        let maker_key = format!("BK_EXIT_STRATEGY_EXIT_MAKER_FIRST_{suffix}");
        if let Some(raw) = lookup(&maker_key)
            && let Some(v) = parse_env_bool(&raw)
        {
            cfg.strategy_exit_maker_first = v;
            touched = true;
        }
        if touched {
            out.insert(account.to_string(), cfg);
        }
    }
    out
}

/// Parse the boolean spellings the env face accepts (the launcher's parser
/// accepts `1/true/yes/on` and `0/false/no/off`; keep the same set here so a
/// hand-edited key means the same thing in both places).
pub(crate) fn parse_env_bool(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
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

    #[test]
    fn precedence_is_cli_env_default() {
        assert_eq!(pick(Some(1), Some(2), 3).source, Source::Cli);
        assert_eq!(pick(None, Some(2), 3), Sourced::new(2, Source::Env));
        assert_eq!(pick(None, None, 3), Sourced::new(3, Source::Default));
    }

    /// issue #1189: two accounts can run different exit disciplines; an
    /// account without a row keeps the global config.
    #[test]
    fn per_account_exit_overrides_are_independent() {
        let base = crate::exit_policy::ExitConfig::default();
        let vars = std::collections::HashMap::from([
            ("BK_EXIT_STOP_LOSS_PCT_PAPER".to_string(), "8".to_string()),
            (
                "BK_EXIT_TAKE_PROFIT_PCT_PAPER".to_string(),
                "90".to_string(),
            ),
            ("BK_EXIT_STOP_LOSS_PCT_LIVE".to_string(), "15".to_string()),
            (
                "BK_EXIT_TRAILING_ENABLED_LIVE".to_string(),
                "true".to_string(),
            ),
        ]);
        let out =
            account_exit_overrides(&base, ["paper", "live", "other"], |k| vars.get(k).cloned());
        assert_eq!(out["paper"].stop_loss_pct, rust_decimal_macros::dec!(8));
        assert_eq!(out["paper"].take_profit_pct, rust_decimal_macros::dec!(90));
        assert_eq!(out["live"].stop_loss_pct, rust_decimal_macros::dec!(15));
        assert!(out["live"].trailing_enabled);
        assert!(!out.contains_key("other"));
    }

    #[test]
    fn the_toml_layer_is_gone() {
        // Issue #967: the retired layer must not come back through the
        // provenance vocabulary — `source=toml` is a regression on the wire.
        assert_eq!(Source::Cli.as_str(), "cli");
        assert_eq!(Source::Env.as_str(), "env");
        assert_eq!(Source::Default.as_str(), "default");
    }

    /// Write `text` to a scratch file and load it the way the kernel does.
    fn extension_from(text: &str) -> ExtensionConfig {
        let dir = std::env::temp_dir().join(format!(
            "bk-extcfg-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, text).unwrap();
        let cfg = ExtensionConfig::load(&path);
        std::fs::remove_dir_all(&dir).ok();
        cfg
    }

    #[test]
    fn the_shipped_extension_config_is_read_and_matches_its_extension() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../extensions/binance_spot/config.toml");
        let cfg = ExtensionConfig::load(&path);
        assert!(cfg.path.is_some(), "the shipped config must exist");
        assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);
        assert!(
            cfg.unknown_keys.is_empty(),
            "no typo'd section or key: {:?}",
            cfg.unknown_keys
        );
        assert_eq!(cfg.meta.name.as_deref(), Some("binance_spot"));
        assert_eq!(cfg.meta.extension_type.as_deref(), Some("market"));
        assert_eq!(cfg.meta.enabled, Some(false));
        assert!(
            cfg.check_against("binance_spot", "0.1.0", "market")
                .is_empty()
        );
        assert!(
            cfg.declared_only.iter().any(|d| d.key == "market.symbols"),
            "{:?}",
            cfg.declared_only
        );
        assert!(
            cfg.declared_only
                .iter()
                .any(|d| d.key == "risk.max_daily_loss"),
            "{:?}",
            cfg.declared_only
        );
        assert!(
            cfg.declared_only
                .iter()
                .any(|d| d.key == "dependencies.blitzkrieg_core")
        );
        assert!(cfg.declared_only.iter().all(|d| !d.reason.is_empty()));
    }

    #[test]
    fn a_drifted_extension_config_is_caught_against_the_code() {
        let cfg = extension_from(
            r#"
            [meta]
            name = "binance_futures"
            version = "9.9.9"
            extension_type = "strategy"
            enabled = true
            "#,
        );
        let problems = cfg.check_against("binance_spot", "0.1.0", "market");
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("binance_futures")));
        assert!(problems.iter().any(|p| p.contains("9.9.9")));
        assert!(problems.iter().any(|p| p.contains("strategy")));
        assert_eq!(cfg.meta.enabled, Some(true));
    }

    #[test]
    fn an_extension_config_without_the_meta_section_does_not_invent_one() {
        let cfg = extension_from(
            r#"
            [market]
            symbols = ["BTCUSDT"]
            "#,
        );
        assert_eq!(cfg.meta, ExtensionMeta::default());
        assert!(cfg.check_against("anything", "0", "market").is_empty());
        assert_eq!(cfg.declared_only.len(), 1);
    }

    #[test]
    fn a_missing_extension_config_is_not_an_error() {
        let cfg = ExtensionConfig::load(Path::new("/nonexistent/ext/config.toml"));
        assert!(cfg.path.is_none());
        assert!(cfg.warnings.is_empty());
        assert_eq!(cfg.meta, ExtensionMeta::default());
    }
}

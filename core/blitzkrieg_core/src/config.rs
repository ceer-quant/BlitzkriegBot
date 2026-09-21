//! File-based configuration (KI-11 / D-1).
//!
//! The kernel's settings used to come from CLI flags and code constants alone,
//! which left `user_layer/configs/*.toml` as dead files: editing one changed
//! nothing. This module makes those files a real source, with one rule:
//!
//! **CLI > env > TOML > built-in default**
//!
//! Resolution is a pure function ([`pick`]) so the precedence is testable
//! without a process. Every effective value carries its [`Source`], and the
//! binary logs the non-default ones at startup — "where did this number come
//! from" is answerable from the log alone, which is the point of a declarative
//! config layer.
//!
//! Failure policy: a missing file is not an error (defaults apply), and a
//! malformed file is a WARNING, never fatal. A typo in a config file must not be
//! able to stop a trading kernel from starting; it must be loud instead. Unknown
//! keys are warned about one by one for the same reason — a silently ignored
//! key is exactly the failure this module exists to end.

use rust_decimal::Decimal;
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// Where an effective setting came from, highest precedence first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    /// Command-line flag.
    Cli,
    /// `BK_*` environment variable.
    Env,
    /// A TOML config file.
    Toml,
    /// The value compiled into the binary.
    Default,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Cli => "cli",
            Source::Env => "env",
            Source::Toml => "toml",
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
pub fn pick<T>(cli: Option<T>, env: Option<T>, toml: Option<T>, default: T) -> Sourced<T> {
    if let Some(v) = cli {
        Sourced::new(v, Source::Cli)
    } else if let Some(v) = env {
        Sourced::new(v, Source::Env)
    } else if let Some(v) = toml {
        Sourced::new(v, Source::Toml)
    } else {
        Sourced::new(default, Source::Default)
    }
}

/// The default config file, relative to the core's working directory (the repo
/// root for the production shell).
pub const DEFAULT_CONFIG_PATH: &str = "user_layer/configs/default.toml";

/// Sibling file holding the Shadow Evolution section. Looked up next to the main
/// config so a self-contained config directory stays self-contained.
pub const SHADOW_SECTION_FILE: &str = "shadow_evolution.toml";

/// Shadow Evolution settings as written in the file. All optional: an absent key
/// keeps its built-in default.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ShadowFile {
    pub enabled: Option<bool>,
    /// Rolling metrics window, in MINUTES (the file's unit).
    pub evaluation_window_minutes: Option<i64>,
    pub min_sample_count: Option<u32>,
    pub min_win_rate_improvement: Option<Decimal>,
    pub min_profit_factor_improvement: Option<Decimal>,
    pub min_observation_minutes: Option<i64>,
    pub cooldown_minutes: Option<i64>,
    /// Per-step relative move ceiling (Lock 1). A file may TIGHTEN this, never
    /// widen it — see [`crate::config::MAX_GRADIENT_CEILING`].
    pub max_gradient: Option<Decimal>,
    pub variant_count: Option<usize>,
    pub audit_dir: Option<String>,
    /// E13: start in the unattended mode (a qualifying variant applies itself,
    /// still under every guard). Defaults to false — the operator decides.
    pub auto_evolve: Option<bool>,
    /// Minutes between DEEP evolution rounds (the file's unit; 72h = 4320).
    pub evolution_cycle_minutes: Option<i64>,
    /// Minutes an undecided proposal stays decidable (7 days = 10080).
    pub proposal_ttl_minutes: Option<i64>,
    /// Knobs one DEEP-cycle variant moves simultaneously (>= 1).
    pub deep_dims: Option<usize>,
}

/// Lock 1's built-in ceiling (±5% per evolution step). A config file may lower
/// `max_gradient` but a value above this is rejected with a warning: the three
/// safety locks are not configurable surfaces, and a setting that could widen
/// one is not a tuning knob.
pub const MAX_GRADIENT_CEILING: Decimal = rust_decimal_macros::dec!(0.05);

/// Everything read from a config file. `warnings` is what the caller logs; it is
/// never an error, so the file can be partly wrong and the kernel still starts.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FileConfig {
    /// The file that was read, if one existed.
    pub path: Option<PathBuf>,
    // ── [engine] ────────────────────────────────────────────────────────────
    pub assets: Option<Vec<String>>,
    pub round_sec: Option<i64>,
    pub min_round_age_sec: Option<i64>,
    pub min_time_left_sec: Option<i64>,
    // ── [shadow_evolution] ──────────────────────────────────────────────────
    pub shadow: ShadowFile,
    pub warnings: Vec<String>,
    /// Keys present in the file that nothing consumes. Reported, so the
    /// "dead config" problem (KI-11) cannot come back unnoticed.
    pub unknown_keys: Vec<String>,
}

impl FileConfig {
    /// Load the main file plus its Shadow Evolution sibling.
    ///
    /// `path` = None means "no file requested" (pure defaults). A missing file,
    /// an unreadable file, or a malformed one all resolve to defaults + a
    /// warning.
    pub fn load(path: Option<&Path>) -> Self {
        let Some(path) = path else {
            return Self::default();
        };
        let mut out = Self::load_one(path);
        // The shadow file lives beside the main one; both are optional.
        if let Some(dir) = path.parent() {
            let shadow_path = dir.join(SHADOW_SECTION_FILE);
            if shadow_path.exists() {
                let shadow = Self::load_one(&shadow_path);
                out.warnings.extend(shadow.warnings);
                out.unknown_keys.extend(shadow.unknown_keys);
                out.shadow = shadow.shadow;
            }
        }
        out
    }

    fn load_one(path: &Path) -> Self {
        let mut out = Self {
            path: Some(path.to_path_buf()),
            ..Self::default()
        };
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => {
                // A missing default file is the normal case for a checkout that
                // never configured anything; do not cry wolf about it.
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
        out.read(&value, path);
        out
    }

    fn read(&mut self, value: &toml::Value, path: &Path) {
        let Some(root) = value.as_table() else {
            self.warnings
                .push(format!("{}: not a TOML table", path.display()));
            return;
        };
        let mut known_section = false;
        for (section, table) in root {
            let Some(table) = table.as_table() else {
                self.unknown_keys.push(section.clone());
                continue;
            };
            // A renamed or invented section is the classic silent typo, so it is
            // reported once (its keys are not enumerated individually).
            if !matches!(section.as_str(), "engine" | "shadow_evolution") {
                self.unknown_keys.push(section.clone());
                continue;
            }
            known_section = true;
            for (key, v) in table {
                let full = format!("{section}.{key}");
                let w = &mut self.warnings;
                match (section.as_str(), key.as_str()) {
                    ("engine", "assets") => got(&mut self.assets, str_array(v), &full, w),
                    ("engine", "round_sec") => got(&mut self.round_sec, int(v), &full, w),
                    ("engine", "min_round_age_sec") => {
                        got(&mut self.min_round_age_sec, int(v), &full, w)
                    }
                    ("engine", "min_time_left_sec") => {
                        got(&mut self.min_time_left_sec, int(v), &full, w)
                    }
                    ("shadow_evolution", "enabled") => {
                        got(&mut self.shadow.enabled, bool_(v), &full, w)
                    }
                    ("shadow_evolution", "evaluation_window_minutes") => {
                        got(&mut self.shadow.evaluation_window_minutes, int(v), &full, w)
                    }
                    ("shadow_evolution", "min_sample_count") => {
                        got(&mut self.shadow.min_sample_count, uint32(v), &full, w)
                    }
                    ("shadow_evolution", "min_win_rate_improvement") => got(
                        &mut self.shadow.min_win_rate_improvement,
                        decimal(v),
                        &full,
                        w,
                    ),
                    ("shadow_evolution", "min_profit_factor_improvement") => got(
                        &mut self.shadow.min_profit_factor_improvement,
                        decimal(v),
                        &full,
                        w,
                    ),
                    ("shadow_evolution", "min_observation_minutes") => {
                        got(&mut self.shadow.min_observation_minutes, int(v), &full, w)
                    }
                    ("shadow_evolution", "cooldown_minutes") => {
                        got(&mut self.shadow.cooldown_minutes, int(v), &full, w)
                    }
                    ("shadow_evolution", "max_gradient") => {
                        got(&mut self.shadow.max_gradient, decimal(v), &full, w)
                    }
                    ("shadow_evolution", "variant_count") => {
                        got(&mut self.shadow.variant_count, uint(v), &full, w)
                    }
                    ("shadow_evolution", "audit_dir") => {
                        got(&mut self.shadow.audit_dir, string(v), &full, w)
                    }
                    ("shadow_evolution", "auto_evolve") => {
                        got(&mut self.shadow.auto_evolve, bool_(v), &full, w)
                    }
                    ("shadow_evolution", "evolution_cycle_minutes") => {
                        got(&mut self.shadow.evolution_cycle_minutes, int(v), &full, w)
                    }
                    ("shadow_evolution", "proposal_ttl_minutes") => {
                        got(&mut self.shadow.proposal_ttl_minutes, int(v), &full, w)
                    }
                    ("shadow_evolution", "deep_dims") => {
                        got(&mut self.shadow.deep_dims, uint(v), &full, w)
                    }
                    _ => self.unknown_keys.push(full),
                }
            }
        }
        if !known_section {
            self.warnings
                .push(format!("{}: no known sections", path.display()));
        }
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

fn int(v: &toml::Value) -> Result<i64, String> {
    v.as_integer().ok_or_else(|| "an integer".to_string())
}

fn uint(v: &toml::Value) -> Result<usize, String> {
    let n = int(v)?;
    usize::try_from(n).map_err(|_| "a non-negative integer".to_string())
}

fn uint32(v: &toml::Value) -> Result<u32, String> {
    let n = int(v)?;
    u32::try_from(n).map_err(|_| "a non-negative integer".to_string())
}

fn decimal(v: &toml::Value) -> Result<Decimal, String> {
    // Accept both a TOML float (0.05) and a quoted string ("0.05"): a float
    // cannot express every decimal exactly, so the quoted form is the precise
    // one and must keep working.
    match v {
        toml::Value::Float(f) => Decimal::try_from(*f).map_err(|_| "a decimal".to_string()),
        toml::Value::Integer(i) => Ok(Decimal::from(*i)),
        toml::Value::String(s) => Decimal::from_str(s.trim()).map_err(|_| "a decimal".to_string()),
        _ => Err("a decimal (number or string)".to_string()),
    }
}

fn str_array(v: &toml::Value) -> Result<Vec<String>, String> {
    let arr = v
        .as_array()
        .ok_or_else(|| "an array of strings".to_string())?;
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let s = item
            .as_str()
            .ok_or_else(|| "an array of strings".to_string())?;
        let s = s.trim();
        if !s.is_empty() {
            out.push(s.to_string());
        }
    }
    Ok(out)
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

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn parsed(text: &str) -> FileConfig {
        let v: toml::Value = toml::from_str(text).expect("test TOML");
        let mut cfg = FileConfig::default();
        cfg.read(&v, Path::new("test.toml"));
        cfg
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
        // meta matches the code, so nothing to report.
        assert!(
            cfg.check_against("binance_spot", "0.1.0", "market")
                .is_empty()
        );
        // [market]/[risk]/[dependencies] are declared and named as not acted on,
        // rather than silently dropped.
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
        // Every declared-only key names a reason.
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
        // Its `enabled = true` is a claim the kernel does not silently act on.
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

    #[test]
    fn precedence_is_cli_env_toml_default() {
        // Every layer present: CLI wins.
        let got = pick(Some(1), Some(2), Some(3), 0);
        assert_eq!((got.value, got.source), (1, Source::Cli));
        // No CLI: env wins over TOML.
        let got = pick(None, Some(2), Some(3), 0);
        assert_eq!((got.value, got.source), (2, Source::Env));
        // No CLI/env: TOML wins over the default.
        let got = pick(None::<i32>, None, Some(3), 0);
        assert_eq!((got.value, got.source), (3, Source::Toml));
        // Nothing given: the compiled default, flagged as such.
        let got = pick(None::<i32>, None, None, 0);
        assert_eq!((got.value, got.source), (0, Source::Default));
        assert!(!got.is_explicit());
        assert!(pick(Some(1), None, None, 0).is_explicit());
    }

    #[test]
    fn reads_every_documented_key() {
        let cfg = parsed(
            r#"
            [engine]
            assets = ["BTC", "ETH"]
            round_sec = 300
            min_round_age_sec = 12
            min_time_left_sec = 34

            [shadow_evolution]
            enabled = true
            evaluation_window_minutes = 7
            min_sample_count = 11
            min_win_rate_improvement = 0.07
            min_profit_factor_improvement = "0.21"
            min_observation_minutes = 3
            cooldown_minutes = 4
            max_gradient = 0.01
            variant_count = 5
            audit_dir = "data/evolution"
            auto_evolve = false
            evolution_cycle_minutes = 4320
            proposal_ttl_minutes = 10080
            deep_dims = 2
            "#,
        );
        assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);
        assert!(cfg.unknown_keys.is_empty(), "{:?}", cfg.unknown_keys);
        assert_eq!(
            cfg.assets.as_deref(),
            Some(&["BTC".to_string(), "ETH".to_string()][..])
        );
        assert_eq!(cfg.round_sec, Some(300));
        assert_eq!(cfg.min_round_age_sec, Some(12));
        assert_eq!(cfg.min_time_left_sec, Some(34));
        let s = &cfg.shadow;
        assert_eq!(s.enabled, Some(true));
        assert_eq!(s.evaluation_window_minutes, Some(7));
        assert_eq!(s.min_sample_count, Some(11));
        assert_eq!(s.min_win_rate_improvement, Some(dec!(0.07)));
        // The quoted form parses exactly, unlike a binary float.
        assert_eq!(s.min_profit_factor_improvement, Some(dec!(0.21)));
        assert_eq!(s.min_observation_minutes, Some(3));
        assert_eq!(s.cooldown_minutes, Some(4));
        assert_eq!(s.max_gradient, Some(dec!(0.01)));
        assert_eq!(s.variant_count, Some(5));
        assert_eq!(s.audit_dir.as_deref(), Some("data/evolution"));
        assert_eq!(s.auto_evolve, Some(false));
        assert_eq!(s.evolution_cycle_minutes, Some(4320));
        assert_eq!(s.proposal_ttl_minutes, Some(10080));
        assert_eq!(s.deep_dims, Some(2));
    }

    #[test]
    fn the_shipped_default_file_is_fully_understood() {
        // The repo's own config must not contain a single unknown key: this test
        // is what keeps the file and the parser from drifting apart.
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../user_layer/configs");
        let cfg = FileConfig::load(Some(&root.join("default.toml")));
        assert!(cfg.path.is_some(), "default.toml must exist");
        assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);
        assert!(cfg.unknown_keys.is_empty(), "{:?}", cfg.unknown_keys);
        assert_eq!(
            cfg.assets.as_deref(),
            Some(
                &[
                    "BTC".to_string(),
                    "ETH".to_string(),
                    "SOL".to_string(),
                    "XRP".to_string()
                ][..]
            )
        );
        // 900 (15m), not 300 (5m): the file only became live with KI-11, and a
        // file-driven 5m is a measured regression (MIGRATION_LOG §12) that
        // `scripts/soak-health.sh` alarms on. See DECISIONS_PENDING D-26.
        assert_eq!(cfg.round_sec, Some(900));
        assert_eq!(cfg.min_round_age_sec, Some(30));
        assert_eq!(cfg.min_time_left_sec, Some(180));
        // Loaded from the sibling file, not invented.
        let s = &cfg.shadow;
        // #249: the shipped file turns the evaluator ON. The switch is runtime
        // state that a panel flip persists over this file, so a shipped `false`
        // was a default nobody could see being reset on every restart — the
        // engine kept coming back off under a panel that said it was on.
        assert_eq!(s.enabled, Some(true));
        // And the unattended mode is the shipped one: with the evaluator running
        // and nobody watching, a proposal held for a human is a proposal nobody
        // will ever answer.
        assert_eq!(s.auto_evolve, Some(true));
        assert_eq!(s.evaluation_window_minutes, Some(30));
        assert_eq!(s.min_sample_count, Some(30));
        assert_eq!(s.min_win_rate_improvement, Some(dec!(0.05)));
        assert_eq!(s.min_profit_factor_improvement, Some(dec!(0.10)));
        assert_eq!(s.min_observation_minutes, Some(5));
        assert_eq!(s.cooldown_minutes, Some(10));
        assert_eq!(s.max_gradient, Some(dec!(0.05)));
        assert_eq!(s.variant_count, Some(3));
        assert_eq!(s.audit_dir.as_deref(), Some("data/evolution"));
    }

    #[test]
    fn a_wrong_type_warns_precisely_and_leaves_the_key_unset() {
        let cfg = parsed(
            r#"
            [engine]
            round_sec = "five minutes"
            assets = "BTC"
            min_time_left_sec = 60
            "#,
        );
        assert_eq!(cfg.round_sec, None, "a bad value must not be guessed");
        assert_eq!(cfg.assets, None);
        // The good key in the same file still lands.
        assert_eq!(cfg.min_time_left_sec, Some(60));
        assert_eq!(cfg.warnings.len(), 2, "{:?}", cfg.warnings);
        assert!(
            cfg.warnings
                .iter()
                .any(|w| w.starts_with("engine.round_sec"))
        );
        assert!(cfg.warnings.iter().any(|w| w.starts_with("engine.assets")));
    }

    #[test]
    fn unknown_keys_are_reported_not_silently_ignored() {
        let cfg = parsed(
            r#"
            [engine]
            round_sec = 300
            round_seconds = 300

            [engine2]
            x = 1
            "#,
        );
        assert_eq!(cfg.round_sec, Some(300));
        assert_eq!(
            cfg.unknown_keys,
            vec!["engine.round_seconds".to_string(), "engine2".to_string()]
        );
    }

    #[test]
    fn a_missing_file_is_defaults_plus_an_explicit_none() {
        let cfg = FileConfig::load(Some(Path::new("/nonexistent/bk-config.toml")));
        assert!(cfg.path.is_none());
        assert!(cfg.assets.is_none());
        assert_eq!(cfg.shadow, ShadowFile::default());
        // A missing file is not a warning: an unconfigured checkout is normal.
        assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);
        // And no path at all means no file, not an error.
        assert!(FileConfig::load(None).path.is_none());
    }

    #[test]
    fn a_malformed_file_warns_and_yields_defaults() {
        let dir = std::env::temp_dir().join(format!("bk-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("broken.toml");
        std::fs::write(&path, "[engine\nround_sec = ").unwrap();
        let cfg = FileConfig::load(Some(&path));
        assert!(cfg.path.is_none(), "a broken file is not a loaded config");
        assert_eq!(cfg.round_sec, None);
        assert_eq!(cfg.warnings.len(), 1);
        assert!(cfg.warnings[0].contains("malformed TOML"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn max_gradient_ceiling_is_the_builtin_lock_1_value() {
        // The constant exists so callers can compare against it rather than
        // re-declaring 0.05 in two places.
        assert_eq!(MAX_GRADIENT_CEILING, dec!(0.05));
    }
}

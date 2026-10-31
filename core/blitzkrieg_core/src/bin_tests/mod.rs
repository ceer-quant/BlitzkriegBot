// #699 — the bin's `#[cfg(test)] mod tests`, moved here verbatim so
// `main.rs` stays inside its review budget. Declared from the bin root as a
// directory module, so the tests stay beside `main.rs`. The explicit imports below stand in
// for the old `use super::*` pull of the root's private imports (a glob does
// not see those across the split), so every test keeps resolving unchanged.
use super::bin_support::boot::{
    Args, ConfigChoice, DEFAULT_ASSETS, DEFAULT_EVENT_ARCHIVE, DEFAULT_MIN_ROUND_AGE_SEC,
    DEFAULT_MIN_TIME_LEFT_SEC, DEFAULT_ROUND_SEC, EnvVars, config_choice, daily_loss_path_for,
    parse_args, parse_exit_pct, parse_strategy_limits, parse_virtual_limits, prescan_config,
    resolve_event_archive, round_duration_source_from, stale_budget_rejection,
};
use blitzkrieg_core::model::Mode;
use rust_decimal::Decimal;

mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    mod bin_config_args_tests;
    mod bin_snapshot_tests;
    mod bin_strategy_args_tests;

    // ── E2-a: the --strategy-limit grammar ──────────────────────────────────

    fn parse_one(flag: &str) -> Option<blitzkrieg_core::service::StrategyLimit> {
        parse_strategy_limits(&[flag.to_string()])
            .into_iter()
            .next()
            .map(|(_, l)| l)
    }

    // ── KI-11: the file-config source and its precedence ────────────────────

    /// Parse an argv with no config file and no env overrides in play.
    fn args_from(flags: &[&str]) -> Args {
        let argv: Vec<String> = flags.iter().map(|s| s.to_string()).collect();
        parse_args(
            &blitzkrieg_core::config::FileConfig::default(),
            &argv,
            &EnvVars::default(),
        )
    }

    /// An env layer built from literal pairs, so the env tier of the precedence
    /// chain is testable without touching the process environment.
    fn env_of(pairs: &[(&str, &str)]) -> EnvVars {
        EnvVars {
            vars: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    /// Write `text` to a scratch `default.toml` and load it the way the kernel
    /// does (the loader consults the file system, so a temp file is the honest
    /// way to exercise it).
    fn file_with(text: &str) -> blitzkrieg_core::config::FileConfig {
        let dir = std::env::temp_dir().join(format!(
            "bk-main-cfg-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("default.toml");
        std::fs::write(&path, text).unwrap();
        let cfg = blitzkrieg_core::config::FileConfig::load(Some(&path));
        std::fs::remove_dir_all(&dir).ok();
        cfg
    }
}

// #699 — the bin's `#[cfg(test)] mod tests`, moved here verbatim so
// `main.rs` stays inside its review budget. Declared from the bin root as a
// directory module, so the tests stay beside `main.rs`. The explicit imports below stand in
// for the old `use super::*` pull of the root's private imports (a glob does
// not see those across the split), so every test keeps resolving unchanged.
use super::bin_support::boot::{
    Args, DEFAULT_ASSETS, DEFAULT_EVENT_ARCHIVE, DEFAULT_MIN_ROUND_AGE_SEC,
    DEFAULT_MIN_TIME_LEFT_SEC, DEFAULT_ROUND_SEC, EnvVars, daily_loss_path_for,
    default_position_log_for, default_strategy_state_for, parse_args, parse_exit_pct,
    parse_strategy_limits, parse_virtual_limits, resolve_event_archive,
    risk_state_path_for, round_duration_source_from, stale_budget_rejection,
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

    // ── KI-11: CLI > env > default, with no file layer (issue #967) ─────────

    /// Parse an argv with no env overrides in play.
    fn args_from(flags: &[&str]) -> Args {
        let argv: Vec<String> = flags.iter().map(|s| s.to_string()).collect();
        parse_args(&argv, &EnvVars::default())
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
}

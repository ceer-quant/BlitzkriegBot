//! The process-wide log filter and its self-report (#184).
//!
//! `EnvFilter::from_default_env()` with no `RUST_LOG` in the environment keeps
//! only ERROR. A production run is started by `scripts/upgrade.sh` as
//! `nohup … run` with stderr redirected to a run log, so the log existed but the
//! lines that would explain an incident — warn-level near misses, info-level
//! decisions, the error-level breadcrumbs this crate emits — were filtered out
//! before they were ever written, while the run looked perfectly healthy.
//!
//! So: the shipped default is INFO, an explicit `RUST_LOG` still wins outright
//! (the operator's escape hatch to DEBUG, unchanged), and the effective level is
//! echoed at startup — a run log must answer "what was this process configured
//! to record?" without anyone having to guess.
//!
//! One spelling of the rule, used by the subscriber in `main` and by the boot
//! banner in [`crate::ipc::server`], so the two cannot disagree about the level
//! the process is actually running.

use tracing::level_filters::LevelFilter;
use tracing_subscriber::EnvFilter;

/// The level a run records when `RUST_LOG` says nothing.
pub const DEFAULT_LEVEL: LevelFilter = LevelFilter::INFO;

/// The filter this process logs through: `RUST_LOG` when set, else
/// [`DEFAULT_LEVEL`].
///
/// `from_env_lossy` ignores a malformed directive instead of failing: a typo in
/// an environment variable must not stop a trading process from booting.
pub fn log_filter() -> EnvFilter {
    EnvFilter::builder()
        .with_default_directive(DEFAULT_LEVEL.into())
        .from_env_lossy()
}

/// Where the effective level came from, for the startup echo.
pub fn level_source() -> &'static str {
    match std::env::var("RUST_LOG") {
        Ok(v) if !v.trim().is_empty() => "RUST_LOG",
        _ => "default",
    }
}

/// The effective level as a lower-case word (`info`, `debug`, …): the highest
/// level any directive in the filter enables, or `off` when it enables nothing.
pub fn effective_level() -> String {
    effective_level_of(&log_filter())
}

fn effective_level_of(filter: &EnvFilter) -> String {
    // `Filter` is generic over the subscriber type; `Registry` is the subscriber
    // this process uses (`fmt().with_env_filter(...)`), and `EnvFilter` implements
    // it for every subscriber, so the choice only pins inference.
    let hint =
        tracing_subscriber::layer::Filter::<tracing_subscriber::Registry>::max_level_hint(filter);
    match hint {
        Some(level) if level != LevelFilter::OFF => level.to_string().to_ascii_lowercase(),
        _ => "off".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_default_is_info() {
        // The regression this guards (#184): a default of ERROR is what made a
        // production run's log look empty. Asserted on the CONSTANT, not on the
        // environment, so the check means the same thing under any RUST_LOG.
        assert_eq!(DEFAULT_LEVEL, LevelFilter::INFO);
        let filter = EnvFilter::builder()
            .with_default_directive(DEFAULT_LEVEL.into())
            .from_env_lossy();
        assert_eq!(effective_level_of(&filter), "info");
        // A directive that only raises the ceiling is reported as the ceiling.
        let verbose = EnvFilter::builder()
            .with_default_directive(DEFAULT_LEVEL.into())
            .parse_lossy("blitzkrieg_core=debug");
        assert_eq!(effective_level_of(&verbose), "debug");
        // And "off" is reported as off rather than as an empty string.
        let silent = EnvFilter::builder().parse_lossy("off");
        assert_eq!(effective_level_of(&silent), "off");
    }
}

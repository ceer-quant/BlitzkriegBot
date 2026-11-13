//! The cockpit families' single source of truth (operator ruling
//! 2026-11-08): the `.env` FILE is the authoritative state for the exit
//! discipline (`BK_EXIT_*`) and the per-account systemic limits (`BK_RISK_*`)
//! — the same families the panel's 风控与策略 card reads and writes. The
//! kernel's in-memory config is a READ-ONLY CACHE of the file:
//!
//!   * every write (panel or RPC) lands in the file FIRST, then the cache is
//!     re-derived FROM the file — the cache can never hold a value the file
//!     does not have (the 2026-11-08 divergence class: panel ON / file OFF,
//!     because the launcher's process-env snapshot had gone stale while the
//!     kernel held the boot-time value — three layers, three answers);
//!   * external hand edits are picked up by the env-file watcher (mtime
//!     poll, `ipc/server`) and re-derived the same way;
//!   * `risk.limits` sources its cockpit sections from the file at query
//!     time, so the panel always reads the file (through the kernel), never
//!     a divergent copy;
//!   * boot derives the working copy from the file directly — the launcher's
//!     env snapshot is no longer consulted for these keys.
//!
//! One table here names every cockpit key once; the write path's key mapping
//! (`risk.rs` exit_env_key / systemic_env_key) and this table are pinned
//! together by test, so the two faces cannot drift.

/// Which working copy a cockpit key arms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CockpitFamily {
    /// `ExitConfig` — the exit discipline (Gate 4 binding + the exit sweep).
    Exit,
    /// `SystemicRiskLimits::account` — the per-account risk matrix.
    Systemic,
}

/// One cockpit key: the `.env` spelling and the wire field name (the
/// `risk.setExit` / `risk.setSystemic` audit vocabulary).
#[derive(Debug, Clone, Copy)]
pub struct CockpitKey {
    pub env_key: &'static str,
    pub field: &'static str,
    pub family: CockpitFamily,
}

/// The ten cockpit keys. Order is display order of the 风控与策略 card's two
/// sections; nothing here may exist twice.
pub const COCKPIT_KEYS: &[CockpitKey] = &[
    CockpitKey {
        env_key: "BK_EXIT_STOP_LOSS_PCT",
        field: "stopLossPct",
        family: CockpitFamily::Exit,
    },
    CockpitKey {
        env_key: "BK_EXIT_TAKE_PROFIT_PCT",
        field: "takeProfitPct",
        family: CockpitFamily::Exit,
    },
    CockpitKey {
        env_key: "BK_EXIT_FORCE_EXIT_SEC",
        field: "forceExitSec",
        family: CockpitFamily::Exit,
    },
    CockpitKey {
        env_key: "BK_EXIT_TRAILING_ENABLED",
        field: "trailingEnabled",
        family: CockpitFamily::Exit,
    },
    CockpitKey {
        env_key: "BK_EXIT_STRATEGY_EXIT_MAKER_FIRST",
        field: "strategyExitMakerFirst",
        family: CockpitFamily::Exit,
    },
    CockpitKey {
        env_key: "BK_RISK_MAX_SINGLE_LOSS_USD",
        field: "maxSingleLossUsd",
        family: CockpitFamily::Systemic,
    },
    CockpitKey {
        env_key: "BK_RISK_MAX_DAILY_DRAWDOWN_USD",
        field: "maxDailyDrawdownUsd",
        family: CockpitFamily::Systemic,
    },
    CockpitKey {
        env_key: "BK_RISK_MAX_POSITION_SIZE",
        field: "maxPositionSize",
        family: CockpitFamily::Systemic,
    },
    CockpitKey {
        env_key: "BK_RISK_MAX_CONSECUTIVE_LOSSES",
        field: "maxConsecutiveLosses",
        family: CockpitFamily::Systemic,
    },
    CockpitKey {
        env_key: "BK_RISK_COOLDOWN_MINUTES",
        field: "cooldownMinutes",
        family: CockpitFamily::Systemic,
    },
    CockpitKey {
        env_key: "BK_RISK_MAX_ORDER_NOTIONAL_USD",
        field: "maxOrderNotionalUsd",
        family: CockpitFamily::Systemic,
    },
];

/// The `.env` key a wire field name maps to — THE one mapping table (the
/// setExit / setSystemic persistence faces delegate here, so a key rename can
/// never desync the write face from the derivation table).
pub fn env_key_for_field(field: &str) -> Option<&'static str> {
    COCKPIT_KEYS
        .iter()
        .find(|k| k.field == field)
        .map(|k| k.env_key)
}

/// A parsed cockpit value — what a raw `.env` string becomes. Exit
/// percentages are Decimals, deadlines are seconds, switches are bools.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CockpitValue {
    Decimal(rust_decimal::Decimal),
    Int(i64),
    Bool(bool),
}

/// Parse one cockpit key's raw file value. `None` = unparseable for this
/// key's declared shape (the caller warns naming the key and skips — one
/// typo must never freeze the other nine).
pub fn parse_cockpit_value(key: &CockpitKey, raw: &str) -> Option<CockpitValue> {
    let raw = raw.trim();
    match key.field {
        // The two percentages and the six systemic limits: decimals >= 0.
        "stopLossPct" | "takeProfitPct" => rust_decimal::Decimal::from_str_exact(raw)
            .ok()
            .map(CockpitValue::Decimal),
        "forceExitSec" => raw.parse::<i64>().ok().map(CockpitValue::Int),
        "trailingEnabled" | "strategyExitMakerFirst" => match raw {
            "true" => Some(CockpitValue::Bool(true)),
            "false" => Some(CockpitValue::Bool(false)),
            _ => None,
        },
        "maxSingleLossUsd"
        | "maxDailyDrawdownUsd"
        | "maxPositionSize"
        | "maxConsecutiveLosses"
        | "cooldownMinutes"
        | "maxOrderNotionalUsd" => rust_decimal::Decimal::from_str_exact(raw)
            .ok()
            .map(CockpitValue::Decimal),
        other => {
            debug_assert!(false, "unmapped cockpit field {other}");
            None
        }
    }
}

/// Overlay the FILE's cockpit values onto a `risk.limits` readout — the
/// panel reads the file (through the kernel), never a divergent cache. A key
/// absent from the file leaves the working copy's value standing (the file
/// no longer speaks for it); an unparseable value is skipped (same fail
/// direction as the derivation).
pub fn overlay_risk_limits(
    vars: &std::collections::BTreeMap<String, String>,
    exit: &mut crate::schema::RiskExitView,
    systemic: &mut crate::schema::RiskAccountLimitsView,
) {
    use crate::risk::limits::{Bound, LimitSource};
    for key in COCKPIT_KEYS {
        let Some(raw) = vars.get(key.env_key) else {
            continue;
        };
        let Some(value) = parse_cockpit_value(key, raw) else {
            continue;
        };
        match (key.family, value) {
            (CockpitFamily::Exit, CockpitValue::Decimal(v)) if key.field == "stopLossPct" => {
                exit.stop_loss_pct = v
            }
            (CockpitFamily::Exit, CockpitValue::Decimal(v)) if key.field == "takeProfitPct" => {
                exit.take_profit_pct = v
            }
            (CockpitFamily::Exit, CockpitValue::Int(v)) if key.field == "forceExitSec" => {
                exit.force_exit_sec = v
            }
            (CockpitFamily::Exit, CockpitValue::Bool(v)) if key.field == "trailingEnabled" => {
                exit.trailing_enabled = v
            }
            (CockpitFamily::Exit, CockpitValue::Bool(v))
                if key.field == "strategyExitMakerFirst" =>
            {
                exit.strategy_exit_maker_first = v
            }
            (CockpitFamily::Systemic, CockpitValue::Decimal(v)) => {
                let bound = match key.field {
                    "maxSingleLossUsd" => &mut systemic.limits.max_single_loss_usd,
                    "maxDailyDrawdownUsd" => &mut systemic.limits.max_daily_drawdown_usd,
                    "maxPositionSize" => &mut systemic.limits.max_position_size,
                    "maxConsecutiveLosses" => &mut systemic.limits.max_consecutive_losses,
                    "cooldownMinutes" => &mut systemic.limits.cooldown_minutes,
                    "maxOrderNotionalUsd" => &mut systemic.limits.max_order_notional_usd,
                    other => {
                        debug_assert!(false, "unmapped systemic overlay field {other}");
                        continue;
                    }
                };
                *bound = Bound::new(v, LimitSource::Env);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_complete_and_unique() {
        assert_eq!(COCKPIT_KEYS.len(), 11);
        let mut keys: Vec<_> = COCKPIT_KEYS.iter().map(|k| k.env_key).collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), 11, "duplicate env key in the cockpit table");
        let mut fields: Vec<_> = COCKPIT_KEYS.iter().map(|k| k.field).collect();
        fields.sort();
        fields.dedup();
        assert_eq!(
            fields.len(),
            11,
            "duplicate wire field in the cockpit table"
        );
    }

    #[test]
    fn parse_shapes_are_strict() {
        let pct = COCKPIT_KEYS
            .iter()
            .find(|k| k.field == "stopLossPct")
            .unwrap();
        assert!(matches!(
            parse_cockpit_value(pct, "12"),
            Some(CockpitValue::Decimal(_))
        ));
        assert!(parse_cockpit_value(pct, "on").is_none());
        let flag = COCKPIT_KEYS
            .iter()
            .find(|k| k.field == "strategyExitMakerFirst")
            .unwrap();
        assert_eq!(
            parse_cockpit_value(flag, "true"),
            Some(CockpitValue::Bool(true))
        );
        assert_eq!(
            parse_cockpit_value(flag, "false"),
            Some(CockpitValue::Bool(false))
        );
        assert!(
            parse_cockpit_value(flag, "1").is_none(),
            "bools are words, not numbers"
        );
    }
}

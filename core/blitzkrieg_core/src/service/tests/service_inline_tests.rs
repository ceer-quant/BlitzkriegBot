#![cfg(test)]
//! Verbatim move of the original inline `mod tests` (pure code motion,
//! no logic edits) — the shared dry_core/order/reset fixtures the sibling
//! theme files reach through `use super::*`.

use super::*;
use crate::risk::RiskConfig;
use rust_decimal_macros::dec;

pub(crate) fn dry_core(balance: Decimal) -> Core {
    let mut c = Core::new(CoreConfig {
        risk: RiskConfig {
            max_order_notional: dec!(3),
            ..Default::default()
        },
        dry_seed_balance: Decimal::from(10_000),
        // #457: pinned so a default change cannot silently arm the expiry
        // sweep in fixtures that jump the clock hours ahead.
        round_duration_sec: 900,
        ..Default::default()
    });
    c.set_balance(balance);
    c
}

pub(crate) fn order(mode: FillPolicy, price: Decimal, size: Decimal, key: &str) -> OrderRequest {
    OrderRequest {
        token_id: "tok".into(),
        condition_id: "cond".into(),
        side: Side::Buy,
        mode,
        price,
        size,
        internal_key: key.into(),
        strategy: "spread_arb".into(),
        asset: "BTC".into(),
        direction: "up".into(),
        round_slot: 1,
        account_id: crate::model::default_account_id(),
    }
}

// ── #458: system.reset_trading —— 三道门 + 重置闭环（直接单测） ─────────

pub(crate) fn reset_params(confirm: &str) -> crate::schema::ResetTradingParams {
    crate::schema::ResetTradingParams {
        confirm: confirm.to_string(),
        reason: Some("unit".to_string()),
        account_id: None,
    }
}

pub(crate) fn reset_audit_dir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("bk-reset-unit-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

pub(crate) fn live_core(balance: Decimal) -> Core {
    let mut c = Core::new(CoreConfig {
        mode: Mode::Live,
        risk: RiskConfig {
            max_order_notional: dec!(3),
            ..Default::default()
        },
        dry_seed_balance: Decimal::from(10_000),
        ..Default::default()
    });
    c.set_balance(balance);
    c
}

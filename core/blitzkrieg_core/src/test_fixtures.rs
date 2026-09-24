//! **Test-only** fixtures shared by more than one unit-test module.
//!
//! Each of these was carried verbatim by two or three test modules. The copies
//! were provably identical (byte-for-byte), so the fixture moves here instead
//! of the tests moving together: a test that wants a different market gets it
//! by editing its own call site.
//!
//! The two markets are deliberately not one function. `round_market` is the
//! *moving* 15-minute BTC round the dispatch/feed/backtest tests drive (slot
//! derived from `now`, expiry at the next boundary), while `evo_market` is a
//! flat mid-0.5 stub with a fixed far-away expiry, paired with `evo_book` by
//! the shadow-evolution tests. `shadow_evolution::variants` keeps its own
//! market copy on purpose: its `down_token_id` reads `t-down`, and unifying
//! the value would be a test-data change, not a de-duplication.

use crate::model::{CryptoMarket, OrderbookSnapshot};
use rust_decimal::Decimal;
use rust_decimal::prelude::FromPrimitive;
use rust_decimal_macros::dec;

/// The 15-minute round market the dispatch, feed and backtest tests drive:
/// one BTC up/down round whose slot is derived from `now`, expiring at the
/// next 900 s boundary.
pub fn round_market(now: i64) -> CryptoMarket {
    let slot = now / 1000 / 900;
    CryptoMarket {
        asset: "BTC".into(),
        condition_id: "cond".into(),
        question_id: "q".into(),
        up_token_id: "up".into(),
        down_token_id: "down".into(),
        up_price: dec!(0.6),
        down_price: dec!(0.4),
        expires_at_ms: (slot + 1) * 900 * 1000,
        round_slot: slot,
        neg_risk: true,
        question: "BTC up or down".into(),
    }
}

/// The flat mid-0.5 market the shadow-evolution tests roll their own books
/// against: up token `t`, down token `t-d`, no neg-risk, fixed far-away expiry.
pub fn evo_market() -> CryptoMarket {
    CryptoMarket {
        asset: "BTC".into(),
        condition_id: "c".into(),
        question_id: "q".into(),
        up_token_id: "t".into(),
        down_token_id: "t-d".into(),
        up_price: dec!(0.5),
        down_price: dec!(0.5),
        expires_at_ms: 900_000,
        round_slot: 1,
        neg_risk: false,
        question: "?".into(),
    }
}

/// One 100-share level each side of token `t` — the book the shadow-evolution
/// tests tick a `buy` through.
pub fn evo_book(bid: f64, ask: f64) -> OrderbookSnapshot {
    OrderbookSnapshot::from_levels(
        "t",
        vec![(Decimal::from_f64(bid).unwrap(), dec!(100))],
        vec![(Decimal::from_f64(ask).unwrap(), dec!(100))],
        0,
    )
}

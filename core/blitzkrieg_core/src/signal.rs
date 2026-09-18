//! Signal engine — RE-EXPORT SHELL over the shared strategy reference
//! implementation (`user_layer/strategy_logic`, PR-B).
//!
//! The algorithm code (price buffers, trend/momentum/fade trackers, entry
//! evaluators, knob tables) moved to `strategy-logic` so the example cdylibs and
//! any third-party strategy compile against ONE copy of the logic and cannot
//! drift (E7 / #38). The kernel ships no strategy itself; this module exists so
//! kernel-side code (config plumbing, `on_config` marshalling, tests) keeps
//! stable import paths for the shared config/domain types.
//!
//! The kernel's own `OrderbookSnapshot`/`SignalDirection` domain types are
//! likewise re-exports of the shared `strategy_logic::model` types (see
//! `crate::model`).

pub use strategy_logic::mean_reversion::MeanReversionConfig;
pub use strategy_logic::model::OrderbookSnapshot;
pub use strategy_logic::model::SignalDirection;
pub use strategy_logic::signal::{
    PriceBuffer, SpreadArbConfig, TradeSignal, TrendConfig, TrendPhase, TrendTracker,
    evaluate_spread_arb, round2,
};
pub use strategy_logic::trend_follow::TrendFollowConfig;

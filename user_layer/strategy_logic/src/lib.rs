//! Shared strategy logic — the deterministic strategy primitives as ONE crate
//! with no kernel dependency (E7 / #38, PR-B).
//!
//! This is the reference implementation of the three builtin strategies'
//! decision logic: the rolling price buffer, the trend/momentum/fade trackers,
//! the three entry evaluators and each strategy's knob declaration. The kernel's
//! `EngineStrategy` wrappers and every external strategy cdylib compile against
//! THIS code, so "builtin" and "external" can never drift apart — there is only
//! one copy of the algorithm.
//!
//! Content rules:
//!  - **No kernel types.** The crate must stay compilable by a third party from
//!    `crates.io`-style sources alone; anything that needs `StrategyCtx`,
//!    `ParamRegistry` or the engine stays in `blitzkrieg_core`.
//!  - **Exact decimals everywhere.** Prices/sizes are `rust_decimal::Decimal`;
//!    wire forms (serde, config) carry decimal STRINGS, never floats.
//!  - **Deterministic given the inputs.** No clocks, no randomness, no I/O —
//!    the same tick sequence produces the same signals bit-for-bit, which is
//!    what the kernel↔dylib parity gate asserts.

pub mod decimal;
pub mod knobs;
pub mod mean_reversion;
pub mod model;
pub mod params;
pub mod signal;
pub mod spread_arb;
pub mod trend_follow;

pub use mean_reversion::{
    FadeTracker, MeanReversionConfig, evaluate_mean_reversion, mean_reversion_knobs,
};
pub use model::{OrderbookSnapshot, SignalDirection, TokenId};
pub use params::{KnobDeclaration, MutableParams, StrategyParams};
pub use signal::{
    PriceBuffer, SpreadArbConfig, TradeSignal, TrendConfig, TrendPhase, TrendTracker,
    evaluate_spread_arb, round2,
};
pub use spread_arb::{SPREAD_ARB_KNOBS, apply_knobs as spread_arb_apply_knobs, spread_arb_knobs};
pub use trend_follow::{
    MomentumTracker, TREND_FOLLOW_KNOBS, TrendFollowConfig, evaluate_trend_follow,
    trend_follow_knobs,
};

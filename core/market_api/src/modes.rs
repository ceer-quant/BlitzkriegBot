//! Market declaration types — DEV_V0_3 §7.3. **Types only.**
//!
//! Wave 0 freezes the shapes; `parse_modes` / `parse_strategy_modes` /
//! `capability_by_name()` land with E27 (§7.4), in this same file. Keeping the
//! definition here (not in `blitzkrieg_core`) is what keeps the dependency DAG
//! of §11.1 acyclic: the strategy API needs these types for
//! `StrategyMode`/`declare_modes`, and the strategy API may not depend on the
//! core. `market_api` has no internal dependencies, so both sides can link it —
//! this is the ONE new crate edge of 0.3 (`strategy_api → market_api`).
//!
//! Directionality rule, written down once because it is the easiest thing in
//! this file to get backwards (§7.5): **a strategy may be loose, a plugin must
//! be specific.** A plugin that declares `structure: None` says "I adapt to
//! every structure under this market type"; that is allowed, but it is then
//! INCOMPATIBLE with a strategy requiring a concrete structure — a plugin that
//! cannot say whether it is a CLOB cannot promise CLOB semantics.

use crate::types::MarketType;
use serde::{Deserialize, Serialize};

/// How a market matches orders. `None` (in a mode) = every structure under
/// that `market_type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarketStructure {
    /// Continuous two-sided book. The dominant shape for spot/futures.
    CentralLimitOrderBook,
    /// No book; fills in arrival order (some DEX routers, OTC matching).
    ContinuousAuction,
    /// Periodic batch auction (opens/closes, round switches).
    CallAuction,
    /// Constant-function market maker (AMM/CFMM).
    AutomatedMarketMaker,
    /// Binary outcome wheel: one settlement per round, two complementary
    /// tokens (Polymarket prediction markets).
    BinaryOutcomeWheel,
    /// Quote-driven: trade on request (options, block trades).
    RequestForQuote,
}

/// What a market offers. A hand-written bitmap: `bitflags` is only a
/// transitive dependency in this tree, and adding a direct dependency for ten
/// constants is not worth it — `u64` bit maths is stdlib.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MarketCapabilities(pub u64);

impl MarketCapabilities {
    pub const NONE: Self = Self(0);
    pub const WEBSOCKET_FEED: Self = Self(1 << 0);
    pub const LEVEL2_SNAPSHOT: Self = Self(1 << 1);
    pub const KLINE_STREAM: Self = Self(1 << 2);
    pub const TRADE_STREAM: Self = Self(1 << 3);
    pub const LEVERAGE: Self = Self(1 << 4);
    pub const SHORT_SELLING: Self = Self(1 << 5);
    pub const BATCH_ORDERS: Self = Self(1 << 6);
    pub const POST_ONLY: Self = Self(1 << 7);
    pub const CANCEL_ON_DISCONNECT: Self = Self(1 << 8);
    pub const MAKER_REBATE: Self = Self(1 << 9);

    /// Superset test: does `self` satisfy `required`? One implementation, one
    /// direction — the plugin must be a superset of what the strategy requires.
    pub fn satisfies(self, required: Self) -> bool {
        self.0 & required.0 == required.0
    }
}

/// One market mode a PLUGIN declares. Field meanings are identical to the
/// strategy-side `StrategyMode`; the two types exist to say WHO is declaring,
/// not to carry two different shapes. Validation is one shared implementation
/// (§7.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketMode {
    pub market_type: MarketType,
    pub structure: Option<MarketStructure>,
    pub capabilities: MarketCapabilities,
}

/// Why a modes payload was rejected. "Declared but empty" is an ERROR, not
/// "undeclared" (§7.4): turning `{"modes":[]}` into "declares nothing" would
/// silently convert a typo into an opt-out, and an opt-out is invisible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModeError {
    NotJson(String),
    /// The modes array is empty — declared-but-nothing-declared is a
    /// configuration error, not an absence of declaration.
    Empty,
    /// `market_type` missing or invalid. It is the only required field.
    MissingMarketType {
        index: usize,
    },
    UnknownMarketType {
        index: usize,
        got: String,
    },
    UnknownStructure {
        index: usize,
        got: String,
    },
    UnknownCapability {
        index: usize,
        got: String,
    },
}

impl std::fmt::Display for ModeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotJson(e) => write!(f, "modes payload is not JSON: {e}"),
            Self::Empty => write!(
                f,
                "modes payload declares an empty array (Empty: declared-but-empty \
                 is an error, not 'undeclared')"
            ),
            Self::MissingMarketType { index } => write!(
                f,
                "mode[index={index}] is missing market_type (the only required field)"
            ),
            Self::UnknownMarketType { index, got } => {
                write!(f, "mode[index={index}] has unknown market_type got={got:?}")
            }
            Self::UnknownStructure { index, got } => {
                write!(f, "mode[index={index}] has unknown structure got={got:?}")
            }
            Self::UnknownCapability { index, got } => {
                write!(f, "mode[index={index}] has unknown capability got={got:?}")
            }
        }
    }
}

impl std::error::Error for ModeError {}

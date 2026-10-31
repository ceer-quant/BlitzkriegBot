//! The read-only `bk.*` surface (DEV_V0_3 §6.4 / E30 #336 task 2) plus the
//! suggested entry-point contract.
//!
//! Everything under `bk` is a PROJECTION of host state, never a capability:
//! no credentials, no order submission, no venue client. A strategy reads the
//! world and returns intents — the kernel adjudicates, sizes, gates, signs and
//! submits (§6.4: "建议出口，返回值交内核裁决 —— 不是下单 API").
//!
//! Surfaces (all read-only):
//!
//! | function          | returns                                                   |
//! |-------------------|-----------------------------------------------------------|
//! | `bk.now_ms()`     | host clock, integer ms                                     |
//! | `bk.round()`      | `{slot=, time_left_sec=, now_ms=}` or `nil` between rounds |
//! | `bk.markets()`    | array of round markets (`asset/condition_id/up_token/venue/…`) |
//! | `bk.book(sym)`    | one round token's book, decimal STRINGS, or `nil`          |
//! | `bk.params()`     | this strategy's hot parameters (`name → string value`)     |
//! | `bk.kline(sym,iv)`| the last CLOSED bar, or `nil` (E29's aggregator owns bars) |
//! | `bk.account()`    | public display fields only (§11.1: no credentials)         |
//! | `bk.holdings()`   | THIS strategy's open positions (array, decimal STRINGS)    |
//! | `bk.fees()`       | the active taker-fee schedule (`name/rate/exponent`) or nil |
//! | `bk.unified_events()` | confirmed cross-venue mappings (#427) or empty array   |
//! | `bk.arb.pairs()`  | RFC 568: THIS strategy's pair-book views (owner-filtered, eventually consistent) or empty array |
//!
//! Prices and sizes cross as exact decimal STRINGS (the `safe.rs` boundary
//! rule): a float would let a resting price drift by a tick. `bk.kline`
//! returns `nil` until the E29 aggregator lands behind it — disclosed in the
//! PR; the strategy treats it as "no data yet", not as an error.
//!
//! The `*_to_lua` helpers build the SAME table shapes the closures read, so
//! the adapter can push callback arguments through one spelling and the
//! read-only surface answers through another without the two drifting.

mod install;
mod state;

pub use install::*;
pub use state::*;

#[cfg(test)]
mod tests;

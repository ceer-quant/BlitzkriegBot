//! blitzkrieg-data-model — the replay-stream data contract as a leaf crate.
//!
//! One taxonomy of market-data events ([`DataEvent`]) and one shape of a
//! round market ([`CryptoMarket`]), shared by:
//!
//! * the core (`blitzkrieg_core`) — the engine consumes `DataEvent`, the
//!   archive layer (de)serialises it, and both are re-exported from the
//!   kernel's `engine`/`model` modules so every existing import path keeps
//!   compiling (the definitions MOVED here, the spellings did not change);
//! * every market plugin — the feed layer already produced the same tuples;
//! * the offline tools (`tools/onchain_pull`) — the #352 puller converts
//!   raw venue JSON into this stream, and its tests replay the output
//!   through the same [`DataSource`] readers the kernel's backtester uses.
//!
//! Leaf discipline: this crate depends on NOTHING internal except
//! `blitzkrieg-market-api` (the same zero-internal-deps contract crate the
//! plugins and the core already share), so any crate in the graph may take
//! it without forming a cycle.

pub mod events;
pub mod market;
pub mod replay;

/// `Side` is a `blitzkrieg-market-api` type (one definition for core and
/// plugins); re-exported so consumers of the stream contract — the kernel and
/// the offline puller — name it from the same leaf they get `DataEvent` from.
pub use blitzkrieg_market_api::Side;
pub use events::{BatchBook, CashflowKind, DataEvent, event_at_ms, event_from_json, event_to_json};
pub use market::CryptoMarket;
pub use replay::{
    DataSource, ReplaySource, SegmentSource, SourceStats, TimedEvent, open_replay, open_replay_all,
};

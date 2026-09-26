//! `StrategyMode` — what a strategy declares it can work in (DEV_V0_3 §2.5).
//!
//! One type only, by decision A.1.6: the shared declaration types
//! (`MarketType` / `MarketStructure` / `MarketCapabilities`) live in
//! `blitzkrieg_market_api`, and this module adds just the strategy-side
//! wrapper. Field meanings are IDENTICAL to the plugin-side `MarketMode` —
//! the two types say WHO is declaring, not two different shapes, and the
//! validator is one shared implementation (§7.4).

use blitzkrieg_market_api::{MarketCapabilities, MarketStructure, MarketType};

/// One market mode a strategy declares it can work in.
///
/// An empty `Vec` (the `SafeStrategy::declare_modes` default, and what a 0.2
/// library that exports no such symbol yields) means "undeclared" — not
/// "declares nothing": an undeclared strategy does not participate in the
/// compatibility handshake at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrategyMode {
    /// Required. Missing makes the whole mode invalid — it is NOT "any market".
    pub market_type: MarketType,
    /// Optional. `None` = adapts to every structure under this market_type.
    ///
    /// Directionality (§7.5): the strategy may be loose, the PLUGIN must be
    /// specific. A plugin declaring `structure: None` is not a match for a
    /// strategy that asks for a concrete structure.
    pub structure: Option<MarketStructure>,
    /// Optional. Empty default = requires no capability bits.
    pub required_capabilities: MarketCapabilities,
}

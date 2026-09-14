//! Strategy engine — loads and runs user-layer strategy logic.
//!
//! ARCHITECTURAL BOUNDARY (v1.1): the *strategy engine* lives in the kernel; the
//! *strategy logic* lives in the user layer. A user strategy receives market
//! ticks and returns a `Signal`; it can ONLY express intent. It cannot sign,
//! place orders, touch credentials, or access the network — the kernel validates
//! the signal, applies risk, reserves funds, signs and submits.
//!
//! Two loading paths are supported:
//!  1. **Built-in strategies** — compiled into the kernel and registered into
//!     the self-driving engine's dispatch at startup (P-1.1 moved these to
//!     `crate::strategies`, e.g. the proven `spread_arb` builtin).
//!  2. **Dynamic libraries** (`strategy/*.dylib`/`.so`) — loaded via
//!     `libloading` through [`loader`], each exposing a C ABI factory. A loaded
//!     strategy is registered (disabled) into the engine dispatch when the
//!     engine is attached, else into this standalone registry.

pub mod loader;

use crate::model::{CoreResult, OrderbookSnapshot, Side};
use rust_decimal::Decimal;

/// A single market observation handed to a strategy.
#[derive(Debug, Clone)]
pub struct MarketTick {
    pub symbol: String,
    pub asset: String,
    /// Best bid / ask / mid for the token.
    pub best_bid: Decimal,
    pub best_ask: Decimal,
    pub mid: Decimal,
    pub timestamp_ms: i64,
}

impl MarketTick {
    pub fn from_book(symbol: &str, asset: &str, book: &OrderbookSnapshot) -> Self {
        Self {
            symbol: symbol.to_string(),
            asset: asset.to_string(),
            best_bid: book.best_bid,
            best_ask: book.best_ask,
            mid: book.mid_price,
            timestamp_ms: book.timestamp,
        }
    }
}

/// A strategy's intent. It expresses a wish; the kernel decides whether/how to
/// execute it. Never carries a signature, key or order id.
#[derive(Debug, Clone, PartialEq)]
pub enum Signal {
    /// Open a long on `symbol` at (or better than) `price`.
    Buy { symbol: String, price: Decimal, size: Decimal },
    /// Close the position on `symbol`.
    Sell { symbol: String, price: Decimal },
    /// No action.
    Hold,
}

/// The contract a user-layer strategy implements. Pure decision logic only.
pub trait Strategy: Send + Sync {
    fn name(&self) -> &str;
    /// Called on each market tick; returns a signal (or Hold).
    fn on_tick(&mut self, tick: &MarketTick) -> Option<Signal>;
    /// Reset per-round state when the round changes.
    fn on_round(&mut self, _slot: i64) {}
}

/// A registered strategy plus its enablement state.
pub struct RegisteredStrategy {
    pub strategy: Box<dyn Strategy>,
    pub enabled: bool,
    /// Where it came from: "builtin" or a dynamic-library path.
    pub source: String,
}

/// The kernel-side strategy engine. Owns the registered strategies and the
/// signal-validation gate (rules the user layer cannot bypass).
#[derive(Default)]
pub struct StrategyEngine {
    strategies: Vec<RegisteredStrategy>,
}

impl StrategyEngine {
    pub fn new() -> Self {
        Self { strategies: Vec::new() }
    }

    pub fn register(&mut self, strategy: Box<dyn Strategy>, source: impl Into<String>) {
        self.strategies.push(RegisteredStrategy { strategy, enabled: true, source: source.into() });
    }

    pub fn names(&self) -> Vec<String> {
        self.strategies.iter().map(|s| s.strategy.name().to_string()).collect()
    }

    pub fn set_enabled(&mut self, name: &str, enabled: bool) -> bool {
        for s in &mut self.strategies {
            if s.strategy.name() == name {
                s.enabled = enabled;
                return true;
            }
        }
        false
    }

    pub fn len(&self) -> usize {
        self.strategies.len()
    }
    pub fn is_empty(&self) -> bool {
        self.strategies.is_empty()
    }

    /// Feed a tick to every enabled strategy and collect validated signals.
    pub fn on_tick(&mut self, tick: &MarketTick) -> Vec<Signal> {
        let mut out = Vec::new();
        for s in self.strategies.iter_mut() {
            if !s.enabled {
                continue;
            }
            if let Some(sig) = s.strategy.on_tick(tick) {
                if validate_signal(&sig, tick).is_ok() {
                    out.push(sig);
                }
            }
        }
        out
    }

    pub fn on_round(&mut self, slot: i64) {
        for s in self.strategies.iter_mut() {
            s.strategy.on_round(slot);
        }
    }
}

/// The kernel's signal-validation gate. Runs BEFORE any risk/ledger/order work
/// so a buggy or malicious strategy cannot emit an out-of-bounds request.
pub fn validate_signal(sig: &Signal, tick: &MarketTick) -> CoreResult<()> {
    use crate::model::{CoreError, CoreErrorCode};
    let check = |symbol: &str, price: Decimal| -> CoreResult<()> {
        if symbol != tick.symbol {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                format!("signal symbol {symbol} does not match tick symbol {}", tick.symbol),
            ));
        }
        if price <= Decimal::ZERO || price > Decimal::ONE {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                format!("signal price {price} out of prediction-market range (0,1]"),
            ));
        }
        Ok(())
    };
    match sig {
        Signal::Buy { symbol, price, size } => {
            check(symbol, *price)?;
            if *size <= Decimal::ZERO {
                return Err(CoreError::new(CoreErrorCode::InvalidSize, "signal size must be positive"));
            }
            Ok(())
        }
        Signal::Sell { symbol, price } => check(symbol, *price),
        Signal::Hold => Ok(()),
    }
}

/// A `Signal` mapped onto the kernel's order side (exposed for tests/adapters).
impl Signal {
    pub fn side(&self) -> Option<Side> {
        match self {
            Signal::Buy { .. } => Some(Side::Buy),
            Signal::Sell { .. } => Some(Side::Sell),
            Signal::Hold => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    struct DipBuyer {
        buy_below: Decimal,
    }
    impl Strategy for DipBuyer {
        fn name(&self) -> &str {
            "dip_buyer"
        }
        fn on_tick(&mut self, tick: &MarketTick) -> Option<Signal> {
            if tick.mid <= self.buy_below {
                Some(Signal::Buy { symbol: tick.symbol.clone(), price: tick.mid, size: dec!(10) })
            } else {
                None
            }
        }
    }

    fn tick(mid: Decimal) -> MarketTick {
        MarketTick {
            symbol: "tok".into(),
            asset: "BTC".into(),
            best_bid: mid - dec!(0.01),
            best_ask: mid + dec!(0.01),
            mid,
            timestamp_ms: 0,
        }
    }

    #[test]
    fn engine_runs_enabled_strategy_and_validates() {
        let mut e = StrategyEngine::new();
        e.register(Box::new(DipBuyer { buy_below: dec!(0.45) }), "builtin");
        assert_eq!(e.names(), vec!["dip_buyer".to_string()]);
        // Above threshold → hold.
        assert!(e.on_tick(&tick(dec!(0.50))).is_empty());
        // Dip → buy signal.
        let sigs = e.on_tick(&tick(dec!(0.43)));
        assert_eq!(sigs.len(), 1);
        assert!(matches!(sigs[0], Signal::Buy { .. }));
        // Disable → no signals.
        assert!(e.set_enabled("dip_buyer", false));
        assert!(e.on_tick(&tick(dec!(0.40))).is_empty());
    }

    #[test]
    fn gate_rejects_out_of_range_price() {
        let t = tick(dec!(0.4));
        let bad = Signal::Buy { symbol: "tok".into(), price: dec!(1.5), size: dec!(10) };
        assert!(validate_signal(&bad, &t).is_err());
        let ok = Signal::Buy { symbol: "tok".into(), price: dec!(0.4), size: dec!(10) };
        assert!(validate_signal(&ok, &t).is_ok());
    }

    #[test]
    fn gate_rejects_symbol_mismatch() {
        let t = tick(dec!(0.4));
        let bad = Signal::Sell { symbol: "other".into(), price: dec!(0.4) };
        assert!(validate_signal(&bad, &t).is_err());
    }
}

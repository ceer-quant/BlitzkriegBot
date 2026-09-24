//! Market data — local L2 orderbook reconstruction.
//!
//! Rust port of the book-building responsibilities in
//! `src/strategies/crypto-hft/orderbook.ts` (buildOrderbookSnapshot; both the TS
//! file and its `local-orderbook.ts`, which was never wired, are gone with the
//! Node source layer, `62b16c88`) plus a delta-capable local book.
//!
//! The Polymarket market channel delivers either a full `book` snapshot or a
//! `price_change` / `best_bid_ask` top-of-book update; this book accepts both and
//! always exposes a consistent `OrderbookSnapshot` for the strategy and exits.

use crate::model::OrderbookSnapshot;
use rust_decimal::Decimal;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default)]
pub struct LocalBook {
    /// price → size; BTreeMap keeps levels sorted for deterministic snapshots.
    bids: BTreeMap<Decimal, Decimal>,
    asks: BTreeMap<Decimal, Decimal>,
    timestamp: i64,
}

impl LocalBook {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the book with a full snapshot from the market channel.
    ///
    /// Levels are diffed into the existing maps instead of clear()+rebuild, so
    /// the B-tree nodes are reused and repeated full snapshots don't churn
    /// node allocations.
    pub fn apply_snapshot(
        &mut self,
        bids: &[(Decimal, Decimal)],
        asks: &[(Decimal, Decimal)],
        now_ms: i64,
    ) {
        replace_levels(&mut self.bids, bids);
        replace_levels(&mut self.asks, asks);
        self.timestamp = now_ms;
    }

    /// Merge incremental level updates (size 0 removes the level).
    pub fn apply_delta(
        &mut self,
        bids: &[(Decimal, Decimal)],
        asks: &[(Decimal, Decimal)],
        now_ms: i64,
    ) {
        apply_levels(&mut self.bids, bids);
        apply_levels(&mut self.asks, asks);
        self.timestamp = now_ms;
    }

    /// Update only the top of book (used by price_change / best_bid_ask which do
    /// not carry full depth). Sizes are unknown, so we set a nominal size.
    pub fn update_top(
        &mut self,
        best_bid: Option<Decimal>,
        best_ask: Option<Decimal>,
        now_ms: i64,
    ) {
        let nominal = Decimal::new(1, 0);
        if let Some(b) = best_bid
            && b > Decimal::ZERO
        {
            // Drop crossed/over levels so best_bid really is the best.
            self.asks.retain(|p, _| *p > b);
            self.bids.insert(b, nominal);
            // Remove any bids above the new best (shouldn't happen, but be safe).
            self.bids.retain(|p, _| *p <= b);
        }
        if let Some(a) = best_ask
            && a > Decimal::ZERO
        {
            self.bids.retain(|p, _| *p < a);
            self.asks.insert(a, nominal);
            self.asks.retain(|p, _| *p >= a);
        }
        self.timestamp = now_ms;
    }

    pub fn best_bid(&self) -> Decimal {
        self.bids
            .keys()
            .next_back()
            .copied()
            .unwrap_or(Decimal::ZERO)
    }
    pub fn best_ask(&self) -> Decimal {
        self.asks.keys().next().copied().unwrap_or(Decimal::ZERO)
    }

    /// Build the immutable snapshot the strategy/exit layers consume.
    ///
    /// BTreeMap iteration is already best-first on both sides, so the snapshot
    /// skips `from_levels`' defensive sort (`from_sorted_levels`).
    pub fn snapshot(&self, token_id: &str) -> OrderbookSnapshot {
        let bids: Vec<(Decimal, Decimal)> = self.bids.iter().rev().map(|(p, s)| (*p, *s)).collect();
        let asks: Vec<(Decimal, Decimal)> = self.asks.iter().map(|(p, s)| (*p, *s)).collect();
        OrderbookSnapshot::from_sorted_levels(token_id.to_string(), bids, asks, self.timestamp)
    }

    pub fn is_empty(&self) -> bool {
        self.bids.is_empty() && self.asks.is_empty()
    }

    pub fn timestamp(&self) -> i64 {
        self.timestamp
    }
}

fn apply_levels(side: &mut BTreeMap<Decimal, Decimal>, levels: &[(Decimal, Decimal)]) {
    for (p, s) in levels {
        if *s <= Decimal::ZERO {
            side.remove(p);
        } else {
            side.insert(*p, *s);
        }
    }
}

/// Diff a full snapshot into an existing side: upsert incoming levels, then
/// drop levels absent from the snapshot (zero-size levels are simply never
/// upserted, so they fall out here). The O(existing × incoming) retain scan
/// only runs when some level actually went stale; a snapshot whose price set
/// is already fully known (the common tick) is a pure in-place upsert with no
/// scan and no node churn.
fn replace_levels(side: &mut BTreeMap<Decimal, Decimal>, levels: &[(Decimal, Decimal)]) {
    let mut valid = 0usize;
    for (p, s) in levels {
        if *s > Decimal::ZERO {
            side.insert(*p, *s);
            valid += 1;
        }
    }
    if side.len() != valid {
        side.retain(|p, _| levels.iter().any(|(np, ns)| np == p && *ns > Decimal::ZERO));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn snapshot_computes_depth_obi_best() {
        let mut b = LocalBook::new();
        b.apply_snapshot(
            &[(dec!(0.40), dec!(100)), (dec!(0.39), dec!(50))],
            &[(dec!(0.42), dec!(200))],
            5,
        );
        let s = b.snapshot("tok");
        assert_eq!(s.best_bid, dec!(0.40));
        assert_eq!(s.best_ask, dec!(0.42));
        assert_eq!(s.mid_price, dec!(0.41));
        assert_eq!(s.bid_depth, dec!(150));
        assert_eq!(s.ask_depth, dec!(200));
        assert_eq!(s.obi, (dec!(150) - dec!(200)) / dec!(350));
    }

    #[test]
    fn delta_removes_zero_size_and_reorders() {
        let mut b = LocalBook::new();
        b.apply_snapshot(&[(dec!(0.40), dec!(100))], &[(dec!(0.42), dec!(100))], 0);
        // Add a better bid, remove the ask, add a new ask lower.
        b.apply_delta(
            &[(dec!(0.41), dec!(10))],
            &[(dec!(0.42), dec!(0)), (dec!(0.43), dec!(5))],
            1,
        );
        assert_eq!(b.best_bid(), dec!(0.41));
        assert_eq!(b.best_ask(), dec!(0.43));
        let s = b.snapshot("t");
        assert_eq!(s.bids.len(), 2);
    }

    #[test]
    fn top_update_uncrosses_book() {
        let mut b = LocalBook::new();
        b.apply_snapshot(&[(dec!(0.40), dec!(100))], &[(dec!(0.42), dec!(100))], 0);
        // best_bid_ask pushes bid up past the old ask → crossed levels dropped.
        b.update_top(Some(dec!(0.45)), Some(dec!(0.46)), 1);
        assert_eq!(b.best_bid(), dec!(0.45));
        assert_eq!(b.best_ask(), dec!(0.46));
    }

    #[test]
    fn snapshot_drops_zero_size_and_stale_levels() {
        let mut b = LocalBook::new();
        b.apply_snapshot(&[(dec!(0.40), dec!(100))], &[(dec!(0.42), dec!(100))], 0);
        // Second snapshot: the old bid goes to size 0, a new ask appears.
        b.apply_snapshot(
            &[(dec!(0.40), dec!(0)), (dec!(0.41), dec!(5))],
            &[(dec!(0.43), dec!(3))],
            1,
        );
        let s = b.snapshot("t");
        // 0.40 must not survive just because its zero-size entry names it.
        assert!(s.bids.iter().all(|(p, _)| *p == dec!(0.41)));
        assert!(s.asks.iter().all(|(p, _)| *p == dec!(0.43)));
    }
}

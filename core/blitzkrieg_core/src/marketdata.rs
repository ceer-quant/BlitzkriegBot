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
    ///
    /// Locked-top semantics (issue #406 family, root cause of OPT-v2 §5.1):
    /// a locked book (`best_bid == best_ask`) is a legal venue state — two
    /// market makers quoting the same price on both sides — and the shared
    /// price must SURVIVE on both sides, so the mid is that price and the
    /// snapshot stays two-sided. The original bounds (`> b` on asks, `< a` on
    /// bids) deleted the shared level from BOTH sides on every locked top,
    /// collapsing the book to one side and driving `mid_price` to 0 — the
    /// structural zero-signal that starved every mid-dependent strategy on
    /// top-only feeds. The bounds are therefore INCLUSIVE (`>=` / `<=`): a
    /// level only leaves the book when it is strictly worse than the new
    /// opposite best.
    ///
    /// Crossed input (`best_bid > best_ask` — a feed glitch, not a state the
    /// venue can be in for a binary book) never yields a crossed book. The
    /// inclusive bounds already sweep the impossible quote: the new bid above
    /// the ask is removed by the bid-side retain, so a crossed TOP on a fresh
    /// book degrades to the ASK side only (the tighter, buyer-conservative
    /// quote survives; old bids at or below the ask are untouched, so a stale
    /// wide book stays a wide book). The residual case — pre-existing levels
    /// that cross each other once both bounds applied — is clamped by the
    /// final block: the bid side is pulled down to the best ask, locking the
    /// book at that price. Either way the snapshot invariant `bb <= ba` holds.
    /// Behavior is pinned by the `update_top_*` tests below.
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
            // Drop only levels strictly WORSE than the new bid so best_bid
            // really is the best; a locked ask AT b stays (inclusive bound).
            self.asks.retain(|p, _| *p >= b);
            self.bids.insert(b, nominal);
            // Remove any bids above the new best (shouldn't happen, but be safe).
            self.bids.retain(|p, _| *p <= b);
        }
        if let Some(a) = best_ask
            && a > Decimal::ZERO
        {
            // Inclusive bound: a locked bid AT a stays on the bid side.
            self.bids.retain(|p, _| *p <= a);
            self.asks.insert(a, nominal);
            self.asks.retain(|p, _| *p >= a);
        }
        // Crossed-input clamp (best_bid > best_ask): keep the tighter (higher)
        // ask and retract the bid to it. Both quotes survive; the book becomes
        // locked at the ask instead of presenting an impossible spread.
        let (bb, ba) = (self.best_bid(), self.best_ask());
        if bb > Decimal::ZERO && ba > Decimal::ZERO && bb > ba {
            self.bids.insert(ba, nominal);
            self.bids.retain(|p, _| *p <= ba);
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

    /// A locked top (best_bid == best_ask) is a LEGAL venue state (makers
    /// quoting both sides at one price). The shared level must survive on
    /// BOTH sides — the old strict bounds deleted it bid- and ask-wise in one
    /// call, collapsing the book and driving the mid to 0 (OPT-v2 §5.1, the
    /// structural zero-signal on top-only feeds). Pinned: two-sided book,
    /// mid == the locked price, both quotes present.
    #[test]
    fn top_update_locked_book_keeps_both_sides_at_the_shared_price() {
        let mut b = LocalBook::new();
        b.apply_snapshot(&[(dec!(0.40), dec!(100))], &[(dec!(0.42), dec!(100))], 0);
        // The venue locks: makers lift their ask onto the bid at 0.41.
        b.update_top(Some(dec!(0.41)), Some(dec!(0.41)), 1);
        assert_eq!(b.best_bid(), dec!(0.41));
        assert_eq!(b.best_ask(), dec!(0.41));
        let s = b.snapshot("tok");
        assert_eq!(s.best_bid, dec!(0.41));
        assert_eq!(s.best_ask, dec!(0.41));
        assert_eq!(s.mid_price, dec!(0.41), "locked mid IS the shared price");
        assert_eq!(s.spread, Decimal::ZERO);
        // Depth: top updates carry no depth, so still-consistent OLD levels
        // remain on both sides (pre-existing semantics — only strictly-worse
        // levels than the new tops are swept): the 0.40 bid (size 100) stays
        // as a deeper level, the 0.42 ask likewise; each side adds the locked
        // price at nominal size.
        assert_eq!(s.bid_depth, dec!(101));
        assert_eq!(s.ask_depth, dec!(101));
    }

    /// Locked top on a book that had NO prior levels (fresh token, first
    /// event is already locked): both sides materialize at the shared price.
    #[test]
    fn top_update_locked_first_event_builds_a_two_sided_book() {
        let mut b = LocalBook::new();
        b.update_top(Some(dec!(0.50)), Some(dec!(0.50)), 0);
        assert_eq!(b.best_bid(), dec!(0.50));
        assert_eq!(b.best_ask(), dec!(0.50));
        let s = b.snapshot("t");
        assert_eq!(s.mid_price, dec!(0.50));
        assert!(s.bids.len() == 1 && s.asks.len() == 1);
    }

    /// The old code's exact failure sequence: locked tops in succession must
    /// keep the book two-sided through every update, and a later unlocked
    /// top must still work from the locked state.
    #[test]
    fn top_update_repeated_locked_tops_stay_two_sided() {
        let mut b = LocalBook::new();
        for t in 0..5 {
            b.update_top(Some(dec!(0.45)), Some(dec!(0.45)), t);
            assert_eq!(b.best_bid(), dec!(0.45));
            assert_eq!(b.best_ask(), dec!(0.45));
        }
        // The lock breaks: bid retreats to 0.44, ask stays 0.45.
        b.update_top(Some(dec!(0.44)), Some(dec!(0.45)), 5);
        assert_eq!(b.best_bid(), dec!(0.44));
        assert_eq!(b.best_ask(), dec!(0.45));
        let s = b.snapshot("t");
        assert_eq!(s.mid_price, dec!(0.445));
    }

    /// Crossed top (best_bid > best_ask, a feed glitch): the snapshot must
    /// never present a crossed spread. The inclusive bid-side bound sweeps
    /// the impossible bid, degrading to the ASK side only (the tighter,
    /// buyer-conservative quote) when the book was otherwise empty; a pre-
    /// existing wide book keeps its remaining levels below the ask.
    #[test]
    fn top_update_crossed_input_never_presents_a_crossed_book() {
        // Fresh book, crossed top: bid-side retain sweeps the impossible bid;
        // the ask side quotes. best_bid reads 0 (no bids) — one-sided at the
        // ask, mid 0 by the F6 rule, and definitely NOT a crossed snapshot.
        let mut b = LocalBook::new();
        b.update_top(Some(dec!(0.60)), Some(dec!(0.40)), 0);
        assert!(
            b.best_bid() <= b.best_ask(),
            "bb {} > ba {}",
            b.best_bid(),
            b.best_ask()
        );
        assert_eq!(b.best_ask(), dec!(0.40));
        let s = b.snapshot("t");
        assert!(s.best_bid <= s.best_ask);

        // A book with a stale wide spread keeps its bid when the crossed top
        // arrives: the impossible NEW bid is swept, the old legal one stays.
        let mut w = LocalBook::new();
        w.apply_snapshot(&[(dec!(0.30), dec!(100))], &[(dec!(0.50), dec!(100))], 0);
        w.update_top(Some(dec!(0.60)), Some(dec!(0.40)), 1);
        assert!(
            w.best_bid() <= w.best_ask(),
            "bb {} > ba {}",
            w.best_bid(),
            w.best_ask()
        );
        assert_eq!(w.best_ask(), dec!(0.40));
        assert_eq!(
            w.best_bid(),
            dec!(0.30),
            "stale legal bid survives the glitch"
        );
    }

    /// One-sided tops (the other quote absent) keep their historical meaning:
    /// a bid-only top updates the bid side and must not manufacture an ask.
    #[test]
    fn top_update_one_sided_inputs_keep_historical_semantics() {
        let mut b = LocalBook::new();
        b.update_top(Some(dec!(0.55)), None, 0);
        assert_eq!(b.best_bid(), dec!(0.55));
        assert_eq!(b.best_ask(), Decimal::ZERO, "no ask was ever quoted");

        let mut a = LocalBook::new();
        a.update_top(None, Some(dec!(0.60)), 0);
        assert_eq!(a.best_ask(), dec!(0.60));
        assert_eq!(a.best_bid(), Decimal::ZERO, "no bid was ever quoted");
    }

    /// Zero/negative quotes are ignored (no book poison), as before.
    #[test]
    fn top_update_ignores_non_positive_quotes() {
        let mut b = LocalBook::new();
        b.apply_snapshot(&[(dec!(0.40), dec!(100))], &[(dec!(0.42), dec!(100))], 0);
        b.update_top(Some(Decimal::ZERO), Some(dec!(-1)), 1);
        assert_eq!(b.best_bid(), dec!(0.40));
        assert_eq!(b.best_ask(), dec!(0.42));
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

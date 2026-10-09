//! Settlement alignment for cross-venue hedges (issue #426, item ⑦).
//!
//! A hedged pair settles on TWO venues at (nominally) the same time. The
//! danger the issue names: one venue pays and the other does not (or pays
//! differently), and the books silently absorb the gap as if it were P&L.
//! The rule fixed here: the two sides are reconciled as a PAIR, and any
//! mismatch is an ANOMALY — audited, alerted, never silently booked as
//! trading profit.
//!
//! This module is pure: it takes the two venues' settlement facts (per
//! unified event) and produces the reconciliation verdict + the audit rows.
//! The wiring feeds it from `on_market_resolution` on one side and the
//! venue plugin's redemption reports on the other.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Tolerance for payout agreement, in USD. Two venues paying for the same
/// shares can differ by fees withheld at redemption; anything beyond this
/// is an anomaly. Conservative default; configurable at wiring time.
pub const PAYOUT_TOLERANCE_USD: Decimal = rust_decimal_macros::dec!(0.01);

/// One venue's settlement fact for one unified event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VenueSettlement {
    /// Which venue paid (data, not logic — issue #427).
    pub venue: String,
    /// Total payout the venue credited, in USD.
    pub payout_usd: Decimal,
    /// Total shares the venue redeemed.
    pub shares: Decimal,
    /// Per-share payout the venue applied (0 / 1 / 0.5 for invalid …).
    pub payout_per_share: Decimal,
}

/// The reconciliation verdict for one event's two-sided settlement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettlementReconcile {
    /// Both venues paid the same per-share for the same shares.
    Aligned,
    /// The venues disagree beyond tolerance. NOTHING may book this as P&L:
    /// the discrepancy is an anomaly (audited + alerted) until resolved
    /// manually.
    Mismatched {
        detail: String,
        venue_a: String,
        venue_b: String,
        delta_usd: Decimal,
    },
    /// Only one venue has settled so far (the other is pending): hold the
    /// pair open — do not book, do not alert yet.
    Partial { settled: String, pending: String },
}

impl SettlementReconcile {
    pub fn is_aligned(&self) -> bool {
        matches!(self, SettlementReconcile::Aligned)
    }
}

/// The per-pair reconciler. Keyed by unified event id; stateless beyond the
/// last-seen facts per venue (a venue's report can arrive twice — the last
/// one wins).
#[derive(Debug, Default)]
pub struct SettlementAligner {
    /// event_id → venue → settlement fact.
    facts: BTreeMap<String, BTreeMap<String, VenueSettlement>>,
}

impl SettlementAligner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one venue's settlement fact for an event; returns the pair's
    /// current reconciliation verdict.
    pub fn record(&mut self, event_id: &str, fact: VenueSettlement) -> SettlementReconcile {
        let per_venue = self.facts.entry(event_id.to_string()).or_default();
        per_venue.insert(fact.venue.clone(), fact);
        self.reconcile(event_id)
    }

    /// The current verdict for an event (without recording anything).
    pub fn reconcile(&self, event_id: &str) -> SettlementReconcile {
        let Some(per_venue) = self.facts.get(event_id) else {
            // No facts at all: nothing to reconcile.
            return SettlementReconcile::Aligned;
        };
        if per_venue.len() < 2 {
            // Zero or one venue: partial (or nothing). With exactly one
            // venue's fact, the pair is still waiting for the other side.
            let settled = per_venue
                .keys()
                .next()
                .cloned()
                .unwrap_or_else(|| "none".to_string());
            let pending = per_venue
                .keys()
                .next()
                .map(|k| {
                    // The OTHER venue: any listed venue that is not the
                    // settled one. The caller names the pair; we can only
                    // say "the second venue" generically here.
                    if k == "polymarket" {
                        "second venue".to_string()
                    } else {
                        k.clone()
                    }
                })
                .unwrap_or_else(|| "second venue".to_string());
            return SettlementReconcile::Partial { settled, pending };
        }
        let mut it = per_venue.values();
        let a = it.next().expect("len >= 2");
        let b = it.next().expect("len >= 2");
        let delta = a.payout_usd - b.payout_usd;
        if a.payout_per_share != b.payout_per_share
            || delta.abs() > PAYOUT_TOLERANCE_USD
            || a.shares != b.shares
        {
            let detail = format!(
                "venues disagree on settlement: {} paid {}/share × {} shares = ${}, {} paid {}/share × {} shares = ${}",
                a.venue,
                a.payout_per_share,
                a.shares,
                a.payout_usd,
                b.venue,
                b.payout_per_share,
                b.shares,
                b.payout_usd,
            );
            return SettlementReconcile::Mismatched {
                detail,
                venue_a: a.venue.clone(),
                venue_b: b.venue.clone(),
                delta_usd: delta,
            };
        }
        SettlementReconcile::Aligned
    }

    /// Events with facts recorded (for the panel readout).
    pub fn event_ids(&self) -> Vec<String> {
        self.facts.keys().cloned().collect()
    }

    /// Drop an event's facts once the pair has been reconciled AND resolved
    /// (the wiring decides when — typically after manual confirmation of a
    /// mismatch or automatic cleanup of an aligned pair).
    pub fn clear(&mut self, event_id: &str) {
        self.facts.remove(event_id);
    }
}

/// Why a mismatch blocks trading (for the audit/alert row).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct SettlementAnomaly {
    pub event_id: String,
    pub at_ms: i64,
    pub detail: String,
    /// `true` when the pair may not open NEW hedges until resolved.
    pub blocks_new_hedges: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn fact(venue: &str, per_share: Decimal, shares: Decimal) -> VenueSettlement {
        VenueSettlement {
            venue: venue.to_string(),
            payout_usd: per_share * shares,
            shares,
            payout_per_share: per_share,
        }
    }

    #[test]
    fn aligned_pair_pays_the_same() {
        let mut a = SettlementAligner::new();
        let v = a.record("ev", fact("polymarket", dec!(1), dec!(10)));
        assert!(matches!(v, SettlementReconcile::Partial { .. }));
        let v = a.record("ev", fact("kalshi", dec!(1), dec!(10)));
        assert!(v.is_aligned());
    }

    #[test]
    fn mismatched_per_share_is_an_anomaly() {
        // The classic invalid-settlement divergence: one venue pays 0.5,
        // the other 1.0 for the same invalid event.
        let mut a = SettlementAligner::new();
        a.record("ev", fact("polymarket", dec!(0.5), dec!(10)));
        let v = a.record("ev", fact("kalshi", dec!(1.0), dec!(10)));
        match v {
            SettlementReconcile::Mismatched { delta_usd, .. } => {
                // Which venue is "a" follows the map order (venue names),
                // not semantics — assert the magnitude.
                assert_eq!(delta_usd.abs(), dec!(5));
            }
            other => panic!("expected Mismatched, got {other:?}"),
        }
    }

    #[test]
    fn missing_payout_on_one_side_is_an_anomaly() {
        // One venue paid $10, the other $0 for the same winning shares.
        let mut a = SettlementAligner::new();
        a.record("ev", fact("polymarket", dec!(1), dec!(10)));
        let v = a.record("ev", fact("kalshi", dec!(0), dec!(10)));
        assert!(matches!(v, SettlementReconcile::Mismatched { .. }));
    }

    #[test]
    fn single_venue_settlement_holds_open() {
        let mut a = SettlementAligner::new();
        let v = a.record("ev", fact("polymarket", dec!(1), dec!(10)));
        assert!(matches!(v, SettlementReconcile::Partial { .. }));
        // And it never becomes Aligned on one fact.
        assert!(!v.is_aligned());
    }

    #[test]
    fn within_tolerance_counts_as_aligned() {
        let mut a = SettlementAligner::new();
        let mut f2 = fact("kalshi", dec!(1), dec!(10));
        f2.payout_usd = dec!(10) - dec!(0.005); // fee withheld, within 1¢
        a.record("ev", fact("polymarket", dec!(1), dec!(10)));
        let v = a.record("ev", f2);
        assert!(
            v.is_aligned(),
            "sub-tolerance fee withholding is not an anomaly"
        );
    }

    #[test]
    fn share_count_mismatch_is_an_anomaly() {
        let mut a = SettlementAligner::new();
        a.record("ev", fact("polymarket", dec!(1), dec!(10)));
        let v = a.record("ev", fact("kalshi", dec!(1), dec!(9)));
        assert!(matches!(v, SettlementReconcile::Mismatched { .. }));
    }

    #[test]
    fn last_fact_wins_per_venue() {
        let mut a = SettlementAligner::new();
        a.record("ev", fact("polymarket", dec!(1), dec!(10)));
        a.record("ev", fact("kalshi", dec!(0), dec!(10)));
        let v = a.record("ev", fact("kalshi", dec!(1), dec!(10)));
        assert!(v.is_aligned(), "a corrected report replaces the anomaly");
    }

    #[test]
    fn clear_drops_facts() {
        let mut a = SettlementAligner::new();
        a.record("ev", fact("polymarket", dec!(1), dec!(10)));
        a.clear("ev");
        assert!(a.event_ids().is_empty());
    }
}

//! The unified event/market model (issue #425).
//!
//! One "real-world bet" can be listed on several venues. A [`UnifiedEvent`]
//! is the operator-confirmed mapping that says which venue listings ARE the
//! same bet; a [`SettlementDiscrepancy`] on the event is the machine-readable
//! warning that the venues may not pay the same way — the stage-4 executor
//! treats such an event as a pseudo-hedge and refuses it.
//!
//! Venue names exist ONLY as a closed identifier enum here; no logic anywhere
//! may branch on a specific venue (issue #427 reverse acceptance).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::types::TokenId;

/// Closed set of supported venues. Extensible by code, not by data: a new
/// venue arrives as a new enum arm plus its plugin, never as a free-form
/// string that silently means nothing downstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Venue {
    Polymarket,
    Kalshi,
    PredictFun,
}

impl Venue {
    pub fn as_str(&self) -> &'static str {
        match self {
            Venue::Polymarket => "polymarket",
            Venue::Kalshi => "kalshi",
            Venue::PredictFun => "predictfun",
        }
    }
}

/// Declared settlement source of a market's verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SettlementSource {
    Chainlink,
    CfBrti,
    Uma,
    OfficialStatement,
    /// The venue did not declare one. NOT interchangeable with any real
    /// source: two `Unknown`s can never be called "the same source".
    Unknown,
}

/// Why a listing is not currently tradeable in full.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ListingStatus {
    Active,
    Paused,
    Halted,
    Expired,
    Delisted,
}

/// One venue's listing of (one side of) a real-world event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlatformListing {
    pub venue: Venue,
    /// Venue-native market id: polymarket `condition_id`, kalshi `ticker`,
    /// predict.fun `market id`. Opaque here — the plugin owns its format.
    pub condition_id: String,
    pub up_token_id: TokenId,
    pub down_token_id: TokenId,
    /// The venue-plugin's normalized underlying asset ("bitcoin", "ethereum",
    /// …) when discovery could identify one. This is the matcher's strongest
    /// same-event signal: two markets naming different assets are DIFFERENT
    /// events no matter how alike their titles read.
    #[serde(default)]
    pub asset: Option<String>,
    pub title: String,
    /// Settlement rules verbatim, as the venue states them.
    pub settlement_rules: String,
    pub settlement_source: SettlementSource,
    /// Epoch millis when this venue's market closes/settles.
    pub expires_at_ms: i64,
    pub status: ListingStatus,
}

/// Ways two venues can disagree about one event. Each kind independently
/// blocks automatic hedging in stage 4 (they all render the same way in the
/// panel: `SettlementDiscrepancy`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DiscrepancyKind {
    /// Different declared settlement sources (e.g. UMA vs Chainlink).
    SettlementSource,
    /// Settlement rules texts are materially different.
    SettlementRules,
    /// Closes differ by more than [`super::matching::EXPIRY_HARD_GAP_MS`].
    ExpiryGap,
}

impl DiscrepancyKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            DiscrepancyKind::SettlementSource => "settlement_source",
            DiscrepancyKind::SettlementRules => "settlement_rules",
            DiscrepancyKind::ExpiryGap => "expiry_gap",
        }
    }
}

/// A confirmed cross-venue mapping. Only the ledger constructs one reachable
/// by trading; a [`super::matching::MatchCandidate`] becomes this ONLY through
/// [`super::ledger::MappingLedger::confirm`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnifiedEvent {
    pub id: String,
    /// Operator-facing title, free to differ from any venue's wording.
    pub title: String,
    /// Canonical settlement rules (usually one side's verbatim text, chosen at
    /// confirmation time).
    pub settlement_rules: String,
    pub settlement_source: SettlementSource,
    pub listings: BTreeMap<Venue, PlatformListing>,
    /// Machine-readable `SettlementDiscrepancy` marks (issue #425). Derived
    /// from the listings by [`Self::revalidate`], never trusted from callers.
    pub discrepancies: Vec<DiscrepancyKind>,
    /// When any leg's status leaves `Active`, the mapping degrades to
    /// single-leg; crossing legs only while this is `Paired` (stage 4 checks).
    pub status: EventStatus,
}

impl UnifiedEvent {
    /// Recompute derived fields (discrepancies, status) from the listings.
    ///
    /// The discrepancy verdict is CONSERVATIVE in the dangerous direction
    /// only: unknown sources never match each other, missing rules text is
    /// treated as material disagreement. It is LENIENT where leniency is
    /// safe: identical declared sources with no rules text on either side
    /// (a venue that simply does not publish rules) do not fabricate a
    /// settlement-rules discrepancy.
    pub fn revalidate(&mut self) {
        let mut disc: Vec<DiscrepancyKind> = Vec::new();
        let first = self.listings.values().next();
        if let Some(first) = first {
            for other in self.listings.values().skip(1) {
                // Source: two `Unknown`s are NOT the same source.
                let source_equal = match (first.settlement_source, other.settlement_source) {
                    (SettlementSource::Unknown, _) | (_, SettlementSource::Unknown) => false,
                    (a, b) => a == b,
                };
                if !source_equal && !disc.contains(&DiscrepancyKind::SettlementSource) {
                    disc.push(DiscrepancyKind::SettlementSource);
                }
                // Rules: both empty = both venues silent, not conflicting.
                // One empty, one not = missing data, treated as disagreement.
                let rules_disagree =
                    match (first.settlement_rules.trim(), other.settlement_rules.trim()) {
                        ("", "") => false,
                        ("", _) | (_, "") => true,
                        (a, b) => super::matching::rules_conflict(a, b),
                    };
                if rules_disagree && !disc.contains(&DiscrepancyKind::SettlementRules) {
                    disc.push(DiscrepancyKind::SettlementRules);
                }
                // Expiry: beyond the hard gap these cannot be one event.
                let gap = (first.expires_at_ms - other.expires_at_ms).abs();
                if gap > super::matching::EXPIRY_HARD_GAP_MS
                    && !disc.contains(&DiscrepancyKind::ExpiryGap)
                {
                    disc.push(DiscrepancyKind::ExpiryGap);
                }
            }
        }
        self.discrepancies = disc;

        self.status = if self
            .listings
            .values()
            .all(|l| l.status == ListingStatus::Active)
        {
            EventStatus::Paired
        } else {
            EventStatus::SingleLegAvailable
        };
    }
}

/// Cross-venue availability of one confirmed mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EventStatus {
    /// Every listing active: full hedge possible.
    Paired,
    /// At least one leg paused/halted/expired while others continue. Explicit
    /// semantics (issue #425): the executor must not keep hedging as if the
    /// pair were whole, and the panel must show it, not hide it.
    SingleLegAvailable,
}

/// A recorded confirmation or revocation — the "who/when/what" trail.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MappingAuditEntry {
    pub at_ms: i64,
    pub actor: String,
    pub action: AuditAction,
    pub event_id: String,
    /// Snapshot of the listings involved, so the audit answers "what exactly
    /// did this confirmation pair together" even after the event changes.
    pub listing_count: usize,
    pub discrepancies: Vec<DiscrepancyKind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AuditAction {
    Confirmed,
    Revoked,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listing(
        venue: Venue,
        title: &str,
        rules: &str,
        src: SettlementSource,
        exp: i64,
    ) -> PlatformListing {
        PlatformListing {
            venue,
            condition_id: format!("{venue:?}-1"),
            up_token_id: format!("{venue:?}-up"),
            down_token_id: format!("{venue:?}-down"),
            asset: Some("bitcoin".into()),
            title: title.into(),
            settlement_rules: rules.into(),
            settlement_source: src,
            expires_at_ms: exp,
            status: ListingStatus::Active,
        }
    }

    fn event(a: PlatformListing, b: PlatformListing) -> UnifiedEvent {
        let mut e = UnifiedEvent {
            id: "evt-test".into(),
            title: "Test event".into(),
            settlement_rules: a.settlement_rules.clone(),
            settlement_source: a.settlement_source,
            listings: BTreeMap::new(),
            discrepancies: Vec::new(),
            status: EventStatus::Paired,
        };
        e.listings.insert(a.venue, a);
        e.listings.insert(b.venue, b);
        e
    }

    #[test]
    fn three_venues_can_hang_off_one_event() {
        let mut e = event(
            listing(
                Venue::Polymarket,
                "Bitcoin Up or Down: October 8, 2PM ET",
                "Resolves to the Binance 1 minute candle for the BTCUSDT price at 2pm ET",
                SettlementSource::Chainlink,
                1_000_000,
            ),
            listing(
                Venue::Kalshi,
                "Bitcoin up or down at 2pm ET on October 8",
                "Resolves per the Binance 1 minute candle price at 2pm ET",
                SettlementSource::Chainlink,
                1_000_000,
            ),
        );
        e.listings.insert(
            Venue::PredictFun,
            listing(
                Venue::PredictFun,
                "Will BTC be up at 2pm ET October 8?",
                "Binance BTCUSDT 1 minute candle at 2pm ET decides",
                SettlementSource::Chainlink,
                1_000_000,
            ),
        );
        e.revalidate();
        assert_eq!(e.listings.len(), 3);
        assert!(e.discrepancies.is_empty(), "{:?}", e.discrepancies);
        assert_eq!(e.status, EventStatus::Paired);
    }

    #[test]
    fn unknown_source_never_matches_known_or_unknown() {
        // Unknown vs known → discrepancy (conservative).
        let mut e = event(
            listing(
                Venue::Polymarket,
                "t",
                "rules one two",
                SettlementSource::Unknown,
                0,
            ),
            listing(
                Venue::Kalshi,
                "t",
                "rules one two",
                SettlementSource::Uma,
                0,
            ),
        );
        e.revalidate();
        assert!(e.discrepancies.contains(&DiscrepancyKind::SettlementSource));
        // Unknown vs Unknown → STILL a discrepancy: "we don't know" is not
        // "the same".
        let mut e = event(
            listing(
                Venue::Polymarket,
                "t",
                "rules one two",
                SettlementSource::Unknown,
                0,
            ),
            listing(
                Venue::Kalshi,
                "t",
                "rules one two",
                SettlementSource::Unknown,
                0,
            ),
        );
        e.revalidate();
        assert!(e.discrepancies.contains(&DiscrepancyKind::SettlementSource));
    }

    #[test]
    fn venue_silent_about_rules_does_not_fabricate_conflict() {
        let mut e = event(
            listing(Venue::Polymarket, "t", "", SettlementSource::Uma, 0),
            listing(Venue::Kalshi, "t", "", SettlementSource::Uma, 0),
        );
        e.revalidate();
        assert!(e.discrepancies.is_empty(), "{:?}", e.discrepancies);
        // But one silent + one verbose IS a discrepancy (missing data).
        let mut e = event(
            listing(Venue::Polymarket, "t", "", SettlementSource::Uma, 0),
            listing(
                Venue::Kalshi,
                "t",
                "uma resolves yes if btc up",
                SettlementSource::Uma,
                0,
            ),
        );
        e.revalidate();
        assert!(e.discrepancies.contains(&DiscrepancyKind::SettlementRules));
    }

    #[test]
    fn one_leg_paused_degrades_to_single_leg_available() {
        let mut e = event(
            listing(Venue::Polymarket, "t", "r", SettlementSource::Uma, 0),
            listing(Venue::Kalshi, "t", "r", SettlementSource::Uma, 0),
        );
        e.revalidate();
        assert_eq!(e.status, EventStatus::Paired);
        let leg = e.listings.get_mut(&Venue::Kalshi).unwrap();
        leg.status = ListingStatus::Paused;
        e.revalidate();
        assert_eq!(e.status, EventStatus::SingleLegAvailable);
        let leg = e.listings.get_mut(&Venue::Kalshi).unwrap();
        leg.status = ListingStatus::Active;
        e.revalidate();
        assert_eq!(e.status, EventStatus::Paired);
    }

    #[test]
    fn expiry_beyond_hard_gap_marks_discrepancy() {
        let mut e = event(
            listing(Venue::Polymarket, "t", "r", SettlementSource::Uma, 0),
            listing(
                Venue::Kalshi,
                "t",
                "r",
                SettlementSource::Uma,
                super::super::matching::EXPIRY_HARD_GAP_MS + 1,
            ),
        );
        e.revalidate();
        assert!(e.discrepancies.contains(&DiscrepancyKind::ExpiryGap));
    }
}

//! `blitzkrieg-market-api::unified` — cross-venue event mapping (issue #425).
//!
//! One real-world event can be listed on several venues. This module models
//! that pairing market-agnostically (no logic may branch on a specific venue
//! — issue #427 reverse acceptance):
//!
//! - [`model`]: `UnifiedEvent`, `PlatformListing`, `SettlementSource`,
//!   `SettlementDiscrepancy` kinds, single-leg lifecycle semantics, and the
//!   [`HedgeVerdict`] choke point stage 4 consults.
//! - [`matching`]: the automatic proposer — pure functions that can only
//!   ever produce CANDIDATES.
//! - [`ledger`]: the operator's confirmation gate, persistence and audit
//!   trail — the ONLY path from candidate to tradeable mapping.

pub mod ledger;
pub mod matching;
pub mod model;

pub use ledger::{ConfirmError, MIN_CONFIRM_TITLE_SIMILARITY, MappingLedger, RevokeError};
pub use matching::{
    MIN_CANDIDATE_SCORE, MIN_RULES_SIMILARITY, MIN_TITLE_SIMILARITY, MatchCandidate, candidates,
    expiry_factor, jaccard, rules_conflict, tokens,
};
pub use model::{
    AuditAction, DiscrepancyKind, EventStatus, ListingStatus, MappingAuditEntry, PlatformListing,
    SettlementSource, UnifiedEvent, Venue,
};

/// The stage-4 facing verdict on one confirmed event: may its two legs be
/// hedged against each other automatically?
///
/// This is the single choke point the executor asks; "pseudo-hedge" is not a
/// stringly-typed warning somewhere in a panel, it is a typed `No` with the
/// reason attached.
#[derive(Debug, Clone, PartialEq)]
pub enum HedgeVerdict {
    /// Confirmed, all legs active, no settlement discrepancies.
    Allowed(UnifiedEvent),
    /// Confirmed but a settlement discrepancy exists (source, rules or hard
    /// expiry gap): the two venues may not pay the same way. Stage 4 must
    /// refuse new automated hedges; existing positions are not touched.
    PseudoHedge(UnifiedEvent),
    /// Not confirmed (no such mapping) or degraded to single-leg: nothing to
    /// hedge against.
    NotHedgeable,
}

impl HedgeVerdict {
    pub fn for_event(event: &UnifiedEvent) -> Self {
        if !event.discrepancies.is_empty() {
            HedgeVerdict::PseudoHedge(event.clone())
        } else if event.status == EventStatus::Paired {
            HedgeVerdict::Allowed(event.clone())
        } else {
            HedgeVerdict::NotHedgeable
        }
    }
}

//! `blitzkrieg-market-api::unified` — cross-venue event mapping (issue #425).
//!
//! One real-world event can be listed on several venues. This module models
//! that pairing market-agnostically (no logic may branch on a specific venue
//! — issue #427 reverse acceptance):
//!
//! - [`model`]: `UnifiedEvent`, `PlatformListing`, `SettlementSource`,
//!   `SettlementDiscrepancy` kinds, single-leg lifecycle semantics, and the
//!   [`HedgeVerdict`] choke point stage 4 consults — canonical in
//!   `blitzkrieg-bk-contract` since #840 Wave 1, re-exported here.
//! - [`matching`]: the automatic proposer — pure functions that can only
//!   ever produce CANDIDATES — canonical in `blitzkrieg-bk-contract`,
//!   re-exported here.
//! - [`ledger`]: the operator's confirmation gate, persistence and audit
//!   trail — the ONLY path from candidate to tradeable mapping. **Stays in
//!   `blitzkrieg-market-api`** (std::fs persistence is implementation, not
//!   contract).

pub use blitzkrieg_bk_contract::unified::HedgeVerdict;
pub use blitzkrieg_bk_contract::unified::matching;
pub use blitzkrieg_bk_contract::unified::model;

pub mod ledger;

pub use blitzkrieg_bk_contract::unified::matching::{
    DEFAULT_EXPIRY_WINDOW_MS, EXPIRY_HARD_GAP_MS, MIN_CANDIDATE_SCORE, MIN_RULES_SIMILARITY,
    MIN_TITLE_SIMILARITY, MatchCandidate, candidates, expiry_factor, jaccard, rules_conflict,
    tokens,
};
pub use blitzkrieg_bk_contract::unified::model::{
    AuditAction, DiscrepancyKind, EventStatus, ListingStatus, MappingAuditEntry, PlatformListing,
    SettlementSource, UnifiedEvent, Venue,
};
pub use ledger::{ConfirmError, MIN_CONFIRM_TITLE_SIMILARITY, MappingLedger, RevokeError};

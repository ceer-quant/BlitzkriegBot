//! The confirmed-mapping ledger (issue #425).
//!
//! The ONLY path from a [`super::matching::MatchCandidate`] to a tradeable
//! [`UnifiedEvent`] runs through [`MappingLedger::confirm`]: automatic
//! matching proposes, the operator disposes. The ledger owns persistence
//! (atomic write, mirroring `strategy_state`'s shape) and the audit trail
//! ("who confirmed which mapping when").
//!
//! File shape: `{"version":1,"events":[…],"audit":[…]}`. Missing or malformed
//! file means "no mappings", never an error. Events and audit entries are
//! kept sorted by id/time so the file diffs stably.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::model::{AuditAction, MappingAuditEntry, PlatformListing, UnifiedEvent, Venue};

/// Lowest confirmed title similarity an operator confirmation will accept.
///
/// The confirmation gate is the LAST line of defence against "two different
/// events mapped as one": even a human click cannot pair two listings whose
/// titles share fewer tokens than this, because such a pair is almost always
/// a misclick. A genuinely different-wording pair has no business being
/// hedged either — different words describe different resolution criteria.
pub const MIN_CONFIRM_TITLE_SIMILARITY: f64 = 0.70;

#[derive(Debug, Default, Serialize, Deserialize)]
struct LedgerDoc {
    version: u32,
    #[serde(default)]
    events: Vec<UnifiedEvent>,
    #[serde(default)]
    audit: Vec<MappingAuditEntry>,
}
/// Confirmed cross-venue event mappings, persisted and audited.
#[derive(Debug, Default)]
pub struct MappingLedger {
    path: Option<std::path::PathBuf>,
    events: BTreeMap<String, UnifiedEvent>,
    audit: Vec<MappingAuditEntry>,
}

impl MappingLedger {
    /// An in-memory ledger. Persistence is disabled until [`Self::at`] —
    /// tests and dry runs use this.
    pub fn new() -> Self {
        Self::default()
    }

    /// Builder: enable persistence at `path`.
    pub fn at(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.path = Some(path.into());
        self
    }

    /// Load persisted mappings. Missing/malformed file → empty (warn, not
    /// error), exactly like `strategy_state::load`.
    pub fn load(path: &Path) -> Self {
        let mut ledger = Self {
            path: Some(path.to_path_buf()),
            ..Default::default()
        };
        ledger.load_from_disk();
        // Re-derive discrepancies/status from the persisted listings so a
        // file hand-edited while the core was down cannot smuggle a
        // discrepancy-free event past revalidation.
        for e in ledger.events.values_mut() {
            e.revalidate();
        }
        ledger
    }

    fn load_from_disk(&mut self) {
        let Some(path) = &self.path else {
            return;
        };
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        let Ok(doc) = serde_json::from_str::<LedgerDoc>(&text) else {
            eprintln!(
                "WARN unified mapping ledger is not valid JSON; starting with no confirmed mappings (path={})",
                path.display()
            );
            return;
        };
        for e in doc.events {
            self.events.insert(e.id.clone(), e);
        }
        self.audit = doc.audit;
    }

    fn persist(&self) {
        let Some(path) = &self.path else {
            return;
        };
        let doc = LedgerDoc {
            version: 1,
            events: self.events.values().cloned().collect(),
            audit: self.audit.clone(),
        };
        let Ok(text) = serde_json::to_string(&doc) else {
            return;
        };
        if let Some(parent) = path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            eprintln!(
                "WARN cannot create mapping ledger dir (path={}, error={e})",
                path.display()
            );
            return;
        }
        let tmp = path.with_extension("json.tmp");
        if let Err(e) = std::fs::write(&tmp, text) {
            eprintln!(
                "WARN cannot write mapping ledger (path={}, error={e})",
                path.display()
            );
            return;
        }
        if let Err(e) = std::fs::rename(&tmp, path) {
            eprintln!(
                "WARN cannot finalize mapping ledger (path={}, error={e})",
                path.display()
            );
        }
    }

    /// The automatic matcher's proposal for `listings`, for the operator UI:
    /// candidates WITHOUT an event id and WITHOUT effectiveness.
    pub fn candidates(
        &self,
        side_a: &[PlatformListing],
        side_b: &[PlatformListing],
        expiry_window_ms: i64,
    ) -> Vec<super::matching::MatchCandidate> {
        super::matching::candidates(side_a, side_b, expiry_window_ms)
    }

    /// Confirm a mapping: the ONLY way a [`UnifiedEvent`] becomes tradeable.
    ///
    /// Refuses (returns `Err`, changes nothing) when:
    /// - fewer than two distinct venues, or a venue contributes two listings
    ///   (one event, one listing per venue);
    /// - any pair's title similarity falls below
    ///   [`MIN_CONFIRM_TITLE_SIMILARITY`] — the human-gate mirror of the
    ///   matcher's hard gate;
    /// - a confirmed mapping for any of these venue listings already exists
    ///   in a DIFFERENT event (one listing, one event).
    ///
    /// On success the event is stored, revalidated (discrepancies/status
    /// derived from the listings, never trusted from the caller), audited and
    /// persisted.
    pub fn confirm(
        &mut self,
        title: &str,
        actor: &str,
        listings: &[PlatformListing],
        at_ms: i64,
    ) -> Result<UnifiedEvent, ConfirmError> {
        if listings.len() < 2 {
            return Err(ConfirmError::NeedsTwoVenues);
        }
        let mut by_venue: BTreeMap<Venue, &PlatformListing> = BTreeMap::new();
        for l in listings {
            if by_venue.insert(l.venue, l).is_some() {
                return Err(ConfirmError::DuplicateVenue(l.venue));
            }
        }
        // Human-gate: every cross-listing pair must clear the same title bar
        // the matcher uses. n² is fine — n is venues (≤ a handful).
        let ids: Vec<&PlatformListing> = by_venue.values().copied().collect();
        for i in 0..ids.len() {
            for j in (i + 1)..ids.len() {
                // Two KNOWN different assets are two different events — even
                // a confirmed click cannot pair them (the title check alone
                // is dilutable by boilerplate).
                let asset_conflict = match (&ids[i].asset, &ids[j].asset) {
                    (Some(x), Some(y)) => !x.eq_ignore_ascii_case(y),
                    _ => false,
                };
                if asset_conflict {
                    return Err(ConfirmError::AssetMismatch {
                        a: ids[i].condition_id.clone(),
                        b: ids[j].condition_id.clone(),
                        a_asset: ids[i].asset.clone().unwrap_or_default(),
                        b_asset: ids[j].asset.clone().unwrap_or_default(),
                    });
                }
                let sim = super::matching::jaccard(
                    &super::matching::tokens(&ids[i].title),
                    &super::matching::tokens(&ids[j].title),
                );
                if sim < MIN_CONFIRM_TITLE_SIMILARITY {
                    return Err(ConfirmError::TitleTooDissimilar {
                        a: ids[i].condition_id.clone(),
                        b: ids[j].condition_id.clone(),
                        similarity: sim,
                    });
                }
            }
        }
        // One listing may belong to only one event.
        for l in listings {
            if let Some(other) = self.find_by_listing(&l.venue, &l.condition_id)
                && other != format_unified_id(listings).as_str()
            {
                return Err(ConfirmError::AlreadyMapped {
                    venue: l.venue,
                    condition_id: l.condition_id.clone(),
                    event_id: other.clone(),
                });
            }
        }

        let id = format_unified_id(listings);
        let mut event = UnifiedEvent {
            id: id.clone(),
            title: title.to_string(),
            settlement_rules: listings
                .iter()
                .map(|l| l.settlement_rules.as_str())
                .find(|r| !r.trim().is_empty())
                .unwrap_or_default()
                .to_string(),
            settlement_source: listings
                .iter()
                .map(|l| l.settlement_source)
                .find(|s| *s != super::model::SettlementSource::Unknown)
                .unwrap_or(super::model::SettlementSource::Unknown),
            listings: by_venue.iter().map(|(v, l)| (*v, (*l).clone())).collect(),
            discrepancies: Vec::new(),
            status: super::model::EventStatus::Paired,
        };
        event.revalidate();

        self.events.insert(id.clone(), event.clone());
        self.audit.push(MappingAuditEntry {
            at_ms,
            actor: actor.to_string(),
            action: AuditAction::Confirmed,
            event_id: id.clone(),
            listing_count: listings.len(),
            discrepancies: event.discrepancies.clone(),
        });
        self.persist();
        Ok(event)
    }

    /// Revoke a mapping (operator correction channel). Audited, persisted.
    pub fn revoke(&mut self, event_id: &str, actor: &str, at_ms: i64) -> Result<(), RevokeError> {
        let Some(event) = self.events.remove(event_id) else {
            return Err(RevokeError::UnknownEvent(event_id.to_string()));
        };
        self.audit.push(MappingAuditEntry {
            at_ms,
            actor: actor.to_string(),
            action: AuditAction::Revoked,
            event_id: event_id.to_string(),
            listing_count: event.listings.len(),
            discrepancies: event.discrepancies.clone(),
        });
        self.persist();
        Ok(())
    }

    /// The confirmed event holding `condition_id` on `venue`, if any.
    pub fn find_by_listing(&self, venue: &Venue, condition_id: &str) -> Option<String> {
        self.events
            .values()
            .find(|e| {
                e.listings
                    .get(venue)
                    .is_some_and(|l| l.condition_id == condition_id)
            })
            .map(|e| e.id.clone())
    }

    pub fn event(&self, event_id: &str) -> Option<&UnifiedEvent> {
        self.events.get(event_id)
    }

    pub fn events(&self) -> impl Iterator<Item = &UnifiedEvent> {
        self.events.values()
    }

    pub fn audit_trail(&self) -> &[MappingAuditEntry] {
        &self.audit
    }
}

/// Deterministic event id: sorted `venue:condition_id` pairs joined.
fn format_unified_id(listings: &[PlatformListing]) -> String {
    let mut parts: Vec<String> = listings
        .iter()
        .map(|l| format!("{}:{}", l.venue.as_str(), l.condition_id))
        .collect();
    parts.sort();
    parts.join("|")
}

/// Why a confirmation was refused. The refusal IS the safety feature.
#[derive(Debug, Clone, PartialEq)]
pub enum ConfirmError {
    NeedsTwoVenues,
    DuplicateVenue(Venue),
    TitleTooDissimilar {
        a: String,
        b: String,
        similarity: f64,
    },
    /// Two listings declare DIFFERENT underlying assets — refused even on
    /// explicit operator confirmation, because the title check is dilutable
    /// by boilerplate and this is the "two different events as one" trap.
    AssetMismatch {
        a: String,
        b: String,
        a_asset: String,
        b_asset: String,
    },
    AlreadyMapped {
        venue: Venue,
        condition_id: String,
        event_id: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum RevokeError {
    UnknownEvent(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unified::model::{ListingStatus, SettlementSource};

    fn listing(venue: Venue, title: &str, exp: i64) -> PlatformListing {
        PlatformListing {
            venue,
            condition_id: format!("{venue:?}-x"),
            up_token_id: format!("{venue:?}-up"),
            down_token_id: format!("{venue:?}-down"),
            asset: Some("bitcoin".into()),
            title: title.into(),
            settlement_rules: "Resolves to the Binance 1 minute candle price".into(),
            settlement_source: SettlementSource::Chainlink,
            expires_at_ms: exp,
            status: ListingStatus::Active,
        }
    }

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        // Same pattern as strategy_state's tests: the platform temp dir plus
        // pid/nanos, so parallel runs never share a path.
        let d = std::env::temp_dir().join(format!(
            "bk-unified-ledger-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    const BTC_PM: &str = "Bitcoin Up or Down: October 8, 2PM ET";
    const BTC_KX: &str = "Bitcoin up or down at 2pm ET on October 8";

    #[test]
    fn confirm_persists_and_reloads_with_revalidation() {
        let dir = tmpdir("roundtrip");
        let path = dir.join("mappings.json");
        let pm = listing(Venue::Polymarket, BTC_PM, 1_000_000);
        let kx = listing(Venue::Kalshi, BTC_KX, 1_000_000);
        {
            let mut ledger = MappingLedger::load(&path);
            let e = ledger
                .confirm("BTC 2pm", "operator-a", &[pm.clone(), kx.clone()], 42)
                .unwrap();
            assert_eq!(e.listings.len(), 2);
            assert!(e.discrepancies.is_empty());
        }
        // Reload from disk: same event, discrepancies re-derived.
        let ledger = MappingLedger::load(&path);
        assert_eq!(ledger.events().count(), 1);
        let e = ledger.events().next().unwrap();
        assert_eq!(e.listings.len(), 2);
        assert_eq!(ledger.audit_trail().len(), 1);
        assert_eq!(ledger.audit_trail()[0].actor, "operator-a");
        assert_eq!(ledger.audit_trail()[0].action, AuditAction::Confirmed);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn confirmation_refuses_dissimilar_titles_even_from_a_human() {
        let mut ledger = MappingLedger::new();
        let err = ledger
            .confirm(
                "misclick",
                "operator-a",
                &[
                    listing(Venue::Polymarket, "Bitcoin up or down 2pm ET", 0),
                    listing(Venue::Kalshi, "Ethereum weekend range high low", 0),
                ],
                0,
            )
            .unwrap_err();
        assert!(matches!(err, ConfirmError::TitleTooDissimilar { .. }));
        assert!(ledger.events().next().is_none(), "nothing stored");
    }

    #[test]
    fn one_listing_cannot_join_two_events() {
        let mut ledger = MappingLedger::new();
        let pm = listing(Venue::Polymarket, BTC_PM, 0);
        ledger
            .confirm(
                "BTC 2pm",
                "op",
                &[pm.clone(), listing(Venue::Kalshi, BTC_KX, 0)],
                0,
            )
            .unwrap();
        // Kalshi side tries to pair the SAME polymarket listing with a third venue.
        let err = ledger
            .confirm(
                "BTC 2pm again",
                "op",
                &[
                    pm,
                    listing(Venue::PredictFun, "Bitcoin up or down 2pm ET October 8", 0),
                ],
                0,
            )
            .unwrap_err();
        assert!(matches!(
            err,
            ConfirmError::AlreadyMapped {
                venue: Venue::Polymarket,
                ..
            }
        ));
    }

    #[test]
    fn revoke_removes_and_frees_the_listings_with_audit() {
        let mut ledger = MappingLedger::new();
        ledger
            .confirm(
                "BTC 2pm",
                "op",
                &[
                    listing(Venue::Polymarket, BTC_PM, 0),
                    listing(Venue::Kalshi, BTC_KX, 0),
                ],
                1,
            )
            .unwrap();
        let id = ledger.events().next().unwrap().id.clone();
        ledger.revoke(&id, "op", 2).unwrap();
        assert!(ledger.events().next().is_none());
        assert_eq!(ledger.audit_trail().len(), 2);
        assert_eq!(ledger.audit_trail()[1].action, AuditAction::Revoked);
        // The freed polymarket listing can now pair elsewhere.
        ledger
            .confirm(
                "BTC 3pm",
                "op",
                &[
                    listing(Venue::Polymarket, BTC_PM, 0),
                    listing(Venue::PredictFun, BTC_KX, 0),
                ],
                3,
            )
            .unwrap();
        assert_eq!(ledger.events().count(), 1);
    }

    #[test]
    fn malformed_file_degrades_to_empty_not_error() {
        let dir = tmpdir("malformed");
        let path = dir.join("mappings.json");
        std::fs::write(&path, "{ not json").unwrap();
        let ledger = MappingLedger::load(&path);
        assert_eq!(ledger.events().count(), 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn in_memory_ledger_never_touches_disk() {
        let mut ledger = MappingLedger::new();
        ledger
            .confirm(
                "BTC 2pm",
                "op",
                &[
                    listing(Venue::Polymarket, BTC_PM, 0),
                    listing(Venue::Kalshi, BTC_KX, 0),
                ],
                0,
            )
            .unwrap();
        assert_eq!(ledger.events().count(), 1);
        // No path set → persist() is a no-op; nothing to assert beyond it not
        // panicking, but the roundtrip test covers the disk path.
    }
}

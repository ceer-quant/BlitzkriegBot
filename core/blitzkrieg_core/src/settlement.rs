//! Settlement and redemption (issue #175).
//!
//! A position only becomes USDC two ways: somebody buys it, or the market
//! resolves and the winning conditional tokens are redeemed on-chain. Before
//! this module the kernel only knew the first — a position that survived to its
//! market's resolution stayed on the books at a marked price forever, its cash
//! locked until an operator redeemed by hand. For a small live account one
//! stuck position is a fifth of the trading capital.
//!
//! The flow, and why it is shaped this way:
//!
//! ```text
//!   venue answers a resolution          core books the settlement
//!   ─────────────────────────           ────────────────────────
//!   MarketResolution ──► booking_for ──► close position at the payout price
//!                                        (trade record, reason `settlement`)
//!                                     ──► open a RedemptionClaim (RECEIVABLE)
//!   RedemptionResult ◄── redeem ────────────────────────────────┘
//!        └─ confirmed ──► receipt → balance (cash in hand)
//! ```
//!
//! The one design decision worth stating plainly: **the payout is an account
//! receivable, not cash, until the redemption is mined.** Crediting `balance`
//! at settlement would say the wallet holds money it does not hold yet, and the
//! in-kernel accounting audit compares `balance` against the venue's reported
//! free cash every 30s (and blocks new entries on drift) — it would go red
//! immediately, correctly. `CashIdentity` therefore carries `receivable`
//! alongside `balance`, and the identity telescopes over their sum: settling
//! moves no money, redeeming moves the receivable into cash, and the audit
//! stays exactly as true at every instant. See `reconcile::CashIdentity`.
//!
//! Idempotency is durable, not in-memory: every booking is appended to a
//! settlement journal before the position is closed, and the journal is
//! replayed at startup. A second settlement round — after a restart, or with a
//! position book restored from a snapshot written before the close — must not
//! book the same position twice, and must not lose the claim whose cash is
//! still on-chain. `SettlementBook::recover` closes that window explicitly.

use crate::model::ExitReason;
use crate::position::OpenPosition;
use blitzkrieg_market_api::{
    MarketResolution, RedemptionFailure, RedemptionRequest, SettlementQuery,
};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// First retry delay after a failed redemption.
pub const RETRY_BASE_MS: i64 = 5_000;
/// Ceiling on the exponential retry delay: a redemption that keeps failing is
/// retried, but never as a storm.
pub const RETRY_MAX_MS: i64 = 300_000;
/// How long an unanswered settlement query waits before the core asks again.
pub const REQUERY_MS: i64 = 60_000;
/// Unanswered queries past this age mean the venue half is not running at all
/// (no credentials, or the extension died) — reported as a blind settlement
/// path rather than silence.
pub const BLIND_AFTER_MS: i64 = 300_000;
/// How much later than its market's end a position may have been opened and
/// still be treated as that market's position (a venue fill is reported a little
/// after the round it belongs to).
///
/// This is the sanity check that keeps a settlement from being *invented*.
/// `expires_at_ms` is the venue's own round end on the engine path, but the
/// order path derives it from the order's declared `round_slot`
/// (`(slot + 1) * round_duration_sec`, see `Core::apply_delta_effects`), and an
/// order that declares a slot on some other clock — the parity fixtures declare
/// `roundSlot: 1` against an epoch clock, which lands in 1970 — carries a value
/// that is not a market end at all. Settling on it would book a real receivable
/// against a market that is still running, so such a position is never watched
/// for settlement: it stays open in the book where an operator can see it,
/// instead of being closed at a made-up payout.
pub const EXPIRY_TRUST_MS: i64 = 300_000;
/// The exit reason a settled position closes with (`exit:settlement`).
pub const SETTLEMENT_EXIT_REASON: ExitReason = ExitReason::Settlement;

/// One position's settlement, as computed from a venue resolution. Pure data:
/// the caller decides whether it becomes a booking, which keeps the arithmetic
/// testable without a core.
#[derive(Debug, Clone, PartialEq)]
pub struct SettlementBooking {
    /// Durable idempotency key: a position settles at most once, ever.
    pub key: String,
    pub position_id: String,
    pub condition_id: String,
    pub token_id: String,
    pub strategy: String,
    pub asset: String,
    pub shares: Decimal,
    pub payout_per_share: Decimal,
    /// Collateral the winning shares are worth: `shares × payout`.
    pub payout_usd: Decimal,
    pub winning: bool,
    /// Index of the position's token in the resolution's outcome order, when the
    /// resolution names it (drives the NegRisk redeem amounts).
    pub outcome_index: Option<usize>,
    /// Filled by [`SettlementBook::book`]: the claim this settlement landed in,
    /// empty when the position resolved worthless (nothing to redeem).
    pub claim_id: String,
}

/// The idempotency key of one settled position. Stable across a restart: the
/// position id is restored from the position log, and `entered_at_ms` pins it
/// to the round it belongs to (position ids are per-process counters, so the
/// timestamp is what makes the key unique over a long-lived deployment).
pub fn settlement_key(p: &OpenPosition) -> String {
    format!("{}:{}:{}", p.condition_id, p.token_id, p.entered_at_ms)
}

/// Compute what settling this position books, or `None` when the resolution
/// says nothing about the position's token (a resolution for another market, or
/// one that does not cover every outcome).
pub fn booking_for(p: &OpenPosition, resolution: &MarketResolution) -> Option<SettlementBooking> {
    if p.condition_id != resolution.condition_id {
        return None;
    }
    let payout_per_share = resolution.payout_per_share(&p.token_id)?;
    let shares = p.shares.max(Decimal::ZERO);
    let outcome_index = resolution
        .payouts
        .iter()
        .position(|(t, _)| t == &p.token_id);
    Some(SettlementBooking {
        key: settlement_key(p),
        position_id: p.id.clone(),
        condition_id: p.condition_id.clone(),
        token_id: p.token_id.clone(),
        strategy: p.strategy.clone(),
        asset: p.asset.clone(),
        shares,
        payout_per_share,
        payout_usd: shares * payout_per_share,
        winning: payout_per_share > Decimal::ZERO,
        outcome_index,
        claim_id: String::new(),
    })
}

/// What one settlement booked, as persisted in the journal. Everything the
/// trade record and the panel need, plus the claim it belongs to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettlementRecord {
    pub key: String,
    pub position_id: String,
    pub condition_id: String,
    pub token_id: String,
    pub strategy: String,
    pub asset: String,
    #[serde(with = "crate::decimal")]
    pub shares: Decimal,
    #[serde(with = "crate::decimal")]
    pub payout_per_share: Decimal,
    #[serde(with = "crate::decimal")]
    pub payout_usd: Decimal,
    pub winning: bool,
    pub claim_id: String,
    /// The market settles through the NegRisk adapter (drives the redeem call).
    #[serde(default)]
    pub neg_risk: bool,
    /// The outcome token this market pays for.
    #[serde(default)]
    pub winning_token_id: String,
    /// The position's outcome index in the resolution's order, and how many
    /// outcomes that order had — enough to rebuild the NegRisk redeem amounts
    /// after a restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome_index: Option<usize>,
    #[serde(default)]
    pub outcome_count: usize,
    pub source: String,
    pub applied_at_ms: i64,
}

/// A settled position whose collateral has not reached the wallet yet: the
/// receivable. One claim per market, so a market with positions on both
/// outcomes redeems once.
#[derive(Debug, Clone, PartialEq)]
pub struct RedemptionClaim {
    pub id: String,
    pub condition_id: String,
    pub neg_risk: bool,
    pub winning_token_id: String,
    /// Shares held per outcome, aligned to the resolution's outcome order: the
    /// NegRisk adapter redeems exactly these amounts.
    pub outcome_shares: Vec<Decimal>,
    pub payout_usd: Decimal,
    pub position_ids: Vec<String>,
    pub created_at_ms: i64,
    /// Attempts handed to the venue (bumped when a request is dispatched, so a
    /// lost result cannot turn into a spin).
    pub attempts: u32,
    pub next_attempt_ms: i64,
    /// No retry can fix this one (the signer does not hold the positions, the
    /// market is not resolved on-chain yet): reported once, left to the operator.
    pub manual: bool,
    pub last_error: Option<String>,
    pub tx_hash: Option<String>,
    pub block_number: Option<u64>,
}

impl RedemptionClaim {
    /// The boundary DTO the venue executes.
    pub fn request(&self) -> RedemptionRequest {
        RedemptionRequest {
            id: self.id.clone(),
            condition_id: self.condition_id.clone(),
            neg_risk: self.neg_risk,
            outcome_shares: self.outcome_shares.clone(),
            expected_payout_usd: self.payout_usd,
            winning_token_id: self.winning_token_id.clone(),
        }
    }
}

/// How one market's resolution query is going.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryState {
    pub condition_id: String,
    /// Tokens the core holds on this market, as a lookup hint.
    pub token_ids: Vec<String>,
    pub expires_at_ms: i64,
    /// First time the query was handed to the venue (0 = never). The age of
    /// this stamp is what makes silence measurable: `answers == 0` after
    /// [`BLIND_AFTER_MS`] means the venue half is not answering at all.
    pub first_sent_ms: i64,
    /// Last dispatch (0 = ready to dispatch, >0 = waiting for an answer).
    pub sent_at_ms: i64,
    /// Earliest next dispatch (set after an answer).
    pub next_query_ms: i64,
    /// Answers received, resolved or not: a venue that says "not resolved yet"
    /// is alive, which is not the same as one that never says anything.
    pub answers: u32,
}

/// One line of the settlement journal. Append-only; replayed at startup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum JournalLine {
    /// A position was settled (the durable commit point).
    Settlement {
        at_ms: i64,
        // Boxed: the record is far larger than every other variant, and a journal
        // line is written/read one at a time, so the extra indirection is free.
        record: Box<SettlementRecord>,
    },
    /// The settled position's close and its trade record are durable. Recovery
    /// must then NOT close it again — a second close would write a second trade
    /// record and double the realized PnL the audit reads.
    Closed { at_ms: i64, key: String },
    /// A claim's collateral reached the wallet (`tx_hash` is `dry-simulated` in
    /// dry mode, where there is no chain to wait for).
    Redeemed {
        at_ms: i64,
        claim_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tx_hash: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        block_number: Option<u64>,
    },
}

/// The journal file: append-only JSONL beside the other core logs.
struct SettlementJournal {
    path: PathBuf,
}

impl SettlementJournal {
    fn append(&self, line: &JournalLine) {
        crate::jsonl::append(&self.path, line);
    }

    fn load(&self) -> Vec<JournalLine> {
        crate::jsonl::load::<JournalLine>(
            &self.path,
            "settlement journal: skipped unparseable lines",
        )
    }
}

/// The settlement book: what was settled (durable), what is still owed to us
/// (the receivable), and what has been asked of the venue.
pub struct SettlementBook {
    /// Applied settlements by key — the idempotency table.
    applied: BTreeMap<String, SettlementRecord>,
    /// Settlements whose close + trade record were made durable.
    closed: BTreeSet<String>,
    /// Claims not yet redeemed, by id.
    claims: BTreeMap<String, RedemptionClaim>,
    /// Claim ids already redeemed, so an id is never reused.
    redeemed: BTreeSet<String>,
    /// Per-condition redeemed-claim count (drives claim id suffixes).
    redeemed_per_condition: BTreeMap<String, u32>,
    /// Open settlement queries by condition id.
    queries: BTreeMap<String, QueryState>,
    /// Settlements booked this session (diagnostic).
    booked: u64,
    /// Whether the blind state was already alerted (edge detection).
    blind_alerted: bool,
    journal: Option<SettlementJournal>,
}

impl SettlementBook {
    /// Open the book, replaying the durable journal when a path is configured.
    pub fn new(path: Option<&Path>) -> Self {
        let mut book = Self {
            applied: BTreeMap::new(),
            closed: BTreeSet::new(),
            claims: BTreeMap::new(),
            redeemed: BTreeSet::new(),
            redeemed_per_condition: BTreeMap::new(),
            queries: BTreeMap::new(),
            booked: 0,
            blind_alerted: false,
            journal: path.map(|p| SettlementJournal {
                path: p.to_path_buf(),
            }),
        };
        if let Some(j) = book.journal.as_ref() {
            for line in j.load() {
                book.replay(line);
            }
        }
        book
    }

    fn replay(&mut self, line: JournalLine) {
        match line {
            JournalLine::Settlement { record, .. } => {
                // A worthless leg of a market that never got a paying one has no
                // claim (empty id): replaying it must not invent one.
                if !record.claim_id.is_empty() {
                    let shares = settled_shares(&record);
                    let widths = shares.len().max(2);
                    let claim = self
                        .claims
                        .entry(record.claim_id.clone())
                        .or_insert_with(|| RedemptionClaim {
                            id: record.claim_id.clone(),
                            condition_id: record.condition_id.clone(),
                            neg_risk: record.neg_risk,
                            winning_token_id: record.winning_token_id.clone(),
                            outcome_shares: Vec::new(),
                            payout_usd: Decimal::ZERO,
                            position_ids: Vec::new(),
                            created_at_ms: record.applied_at_ms,
                            attempts: 0,
                            next_attempt_ms: 0,
                            manual: false,
                            last_error: None,
                            tx_hash: None,
                            block_number: None,
                        });
                    if claim.outcome_shares.len() < widths {
                        claim.outcome_shares.resize(widths, Decimal::ZERO);
                    }
                    for (i, s) in shares.iter().enumerate() {
                        if let Some(slot) = claim.outcome_shares.get_mut(i) {
                            *slot += *s;
                        }
                    }
                    claim.payout_usd += record.payout_usd;
                    claim.position_ids.push(record.position_id.clone());
                    claim.neg_risk = record.neg_risk;
                    if claim.winning_token_id.is_empty() {
                        claim.winning_token_id = record.winning_token_id.clone();
                    }
                }
                self.applied.insert(record.key.clone(), *record);
            }
            JournalLine::Closed { key, .. } => {
                self.closed.insert(key);
            }
            JournalLine::Redeemed { claim_id, .. } => {
                if let Some(c) = self.claims.remove(&claim_id) {
                    *self
                        .redeemed_per_condition
                        .entry(c.condition_id.clone())
                        .or_insert(0) += 1;
                }
                self.redeemed.insert(claim_id);
            }
        }
    }

    /// Is this position already settled? The durable half of the idempotency
    /// guard: a replayed resolution must not book the same position twice.
    pub fn is_applied(&self, key: &str) -> bool {
        self.applied.contains_key(key)
    }

    pub fn applied_count(&self) -> usize {
        self.applied.len()
    }

    pub fn booked_this_session(&self) -> u64 {
        self.booked
    }

    /// Collateral earned but not yet in the wallet — the receivable the
    /// accounting identity carries.
    pub fn receivable_usd(&self) -> Decimal {
        self.claims
            .values()
            .fold(Decimal::ZERO, |a, c| a + c.payout_usd)
    }

    pub fn claims(&self) -> impl Iterator<Item = &RedemptionClaim> {
        self.claims.values()
    }

    pub fn claim(&self, id: &str) -> Option<&RedemptionClaim> {
        self.claims.get(id)
    }

    pub fn pending_claim_count(&self) -> usize {
        self.claims.len()
    }

    /// How many claims still have an automatic attempt coming.
    pub fn retryable_claim_count(&self) -> usize {
        self.claims.values().filter(|c| !c.manual).count()
    }

    /// Manually-required claims: no retry can land them.
    pub fn manual_claim_count(&self) -> usize {
        self.claims.values().filter(|c| c.manual).count()
    }

    /// Whether the settlement path is blind: a market is watched and the newest
    /// sign of life from the venue is older than [`BLIND_AFTER_MS`], so nothing
    /// will settle until the venue answers. `None` while healthy.
    ///
    /// "Sign of life" is the first dispatch or the moment of the last answer
    /// (recorded as the re-query clock minus its interval), so both failure modes
    /// show up: a query never answered, and a venue that answered once and then
    /// went silent.
    pub fn blind_since_ms(&self, now_ms: i64) -> Option<i64> {
        self.queries
            .values()
            .filter(|q| q.first_sent_ms > 0)
            .map(|q| q.first_sent_ms.max(q.next_query_ms - REQUERY_MS))
            .min()
            .filter(|t| now_ms - *t > BLIND_AFTER_MS)
    }

    /// Edge-detect the blind state so the caller alerts once per episode rather
    /// than every tick. Returns true on the healthy → blind transition.
    pub fn note_blind_state(&mut self, blind: bool) -> bool {
        let edge = blind && !self.blind_alerted;
        self.blind_alerted = blind;
        edge
    }

    // ── Queries ─────────────────────────────────────────────────────────────

    /// Mark the markets the core needs a verdict for. Called on the maintenance
    /// tick with the open positions; returns how many markets became newly
    /// tracked. A market already tracked keeps its query clock.
    ///
    /// A market whose recorded end lies far BEHIND the position's own entry is
    /// not a market end (see [`EXPIRY_TRUST_MS`]): such a position is left open
    /// rather than settled at an invented payout.
    pub fn track_markets(&mut self, positions: &[OpenPosition], now_ms: i64) -> usize {
        let mut added = 0usize;
        for p in positions {
            if p.condition_id.is_empty()
                || p.expires_at_ms > now_ms
                || p.entered_at_ms - p.expires_at_ms > EXPIRY_TRUST_MS
            {
                continue; // not past its expiry: nothing can have resolved yet
            }
            match self.queries.get_mut(&p.condition_id) {
                Some(q) => {
                    if !q.token_ids.contains(&p.token_id) {
                        q.token_ids.push(p.token_id.clone());
                    }
                }
                None => {
                    self.queries.insert(
                        p.condition_id.clone(),
                        QueryState {
                            condition_id: p.condition_id.clone(),
                            token_ids: vec![p.token_id.clone()],
                            expires_at_ms: p.expires_at_ms,
                            first_sent_ms: 0,
                            sent_at_ms: 0,
                            next_query_ms: 0,
                            answers: 0,
                        },
                    );
                    added += 1;
                }
            }
        }
        added
    }

    /// Queries the venue has not been handed yet (and that are not cooling
    /// down). Dispatching them stamps the clock, so an unanswered query is
    /// retried at most once per [`REQUERY_MS`] and its silence is measurable.
    pub fn take_queries(&mut self, now_ms: i64) -> Vec<SettlementQuery> {
        let mut out = Vec::new();
        for q in self.queries.values_mut() {
            // Waiting for a fresh answer, or inside the re-query cooldown.
            let waiting = q.sent_at_ms > 0 && now_ms - q.sent_at_ms <= REQUERY_MS;
            if waiting || (q.sent_at_ms == 0 && now_ms < q.next_query_ms) {
                continue;
            }
            if q.first_sent_ms == 0 {
                q.first_sent_ms = now_ms;
            }
            q.sent_at_ms = now_ms;
            out.push(blitzkrieg_market_api::SettlementQuery {
                condition_id: q.condition_id.clone(),
                token_ids: q.token_ids.clone(),
                expires_at_ms: q.expires_at_ms,
            });
        }
        out
    }

    /// Record that a market was answered (resolved or not), so silence and
    /// "not resolved yet" are distinguishable. A market whose positions are all
    /// settled stops being tracked, by the caller.
    pub fn note_answer(&mut self, condition_id: &str, _resolved: bool, now_ms: i64) {
        if let Some(q) = self.queries.get_mut(condition_id) {
            q.answers += 1;
            q.sent_at_ms = 0;
            q.next_query_ms = now_ms + REQUERY_MS;
        }
    }

    /// Drop the query for a market with nothing left to settle.
    pub fn forget_market(&mut self, condition_id: &str) {
        self.queries.remove(condition_id);
    }

    pub fn tracked_markets(&self) -> usize {
        self.queries.len()
    }

    /// The markets the book is waiting on a verdict for.
    pub fn tracked_condition_ids(&self) -> Vec<String> {
        self.queries.keys().cloned().collect()
    }

    /// Claims already redeemed (durable, across restarts).
    pub fn redeemed_count(&self) -> usize {
        self.redeemed.len()
    }

    /// The newest redemption problem, for the panel: `(claim_id, message,
    /// manual)`. `None` when nothing is wrong.
    pub fn last_error(&self) -> Option<(&str, &str, bool)> {
        self.claims
            .values()
            .filter_map(|c| {
                c.last_error
                    .as_deref()
                    .map(|e| (c.id.as_str(), e, c.manual))
            })
            .next_back()
    }

    // ── Booking ─────────────────────────────────────────────────────────────

    /// Commit one settlement: claim + durable journal line. Returns the booking
    /// that was committed, or `None` when the key was already applied — the
    /// caller must then NOT close the position a second time (see
    /// [`SettlementBook::recover`] for the restart case).
    ///
    /// The CLAIM belongs to the market, not to one position: a NegRisk redemption
    /// takes the whole outcome-share vector in one call, so a leg that resolved
    /// worthless still rides along with its (zero-valued) amount while the market
    /// has a paying leg. A market where every leg is worthless gets no claim at
    /// all — losing conditional tokens are worth nothing to redeem, so a
    /// transaction for them would only cost gas. `claim_id` is empty on such a
    /// booking.
    pub fn book(
        &mut self,
        booking: &SettlementBooking,
        resolution: &MarketResolution,
        now_ms: i64,
    ) -> Option<SettlementBooking> {
        if self.is_applied(&booking.key) {
            return None;
        }
        // The market's open claim, if it has one (redemption removes it).
        let open_claim = self
            .claims
            .values()
            .find(|c| c.condition_id == booking.condition_id)
            .map(|c| c.id.clone());
        let claim_id = if booking.payout_usd > Decimal::ZERO {
            Some(open_claim.unwrap_or_else(|| self.claim_id_for(&booking.condition_id)))
        } else {
            open_claim
        };
        let winning_token_id = resolution
            .winning_token()
            .cloned()
            .unwrap_or_else(|| booking.token_id.clone());
        let outcome_shares = outcome_shares_for(resolution, booking);
        if let Some(id) = claim_id.as_ref() {
            self.accumulate_claim(
                id,
                booking,
                resolution,
                &outcome_shares,
                &winning_token_id,
                now_ms,
            );
        }
        let record = SettlementRecord {
            key: booking.key.clone(),
            position_id: booking.position_id.clone(),
            condition_id: booking.condition_id.clone(),
            token_id: booking.token_id.clone(),
            strategy: booking.strategy.clone(),
            asset: booking.asset.clone(),
            shares: booking.shares,
            payout_per_share: booking.payout_per_share,
            payout_usd: booking.payout_usd,
            winning: booking.winning,
            neg_risk: resolution.neg_risk,
            winning_token_id,
            outcome_index: booking.outcome_index,
            outcome_count: resolution.payouts.len().max(2),
            claim_id: claim_id.unwrap_or_default(),
            source: resolution.source.clone(),
            applied_at_ms: now_ms,
        };
        self.applied.insert(record.key.clone(), record.clone());
        self.booked += 1;
        if let Some(j) = self.journal.as_ref() {
            j.append(&JournalLine::Settlement {
                at_ms: now_ms,
                record: Box::new(record),
            });
        }
        let mut settled = booking.clone();
        settled.claim_id = self
            .applied
            .get(&settled.key)
            .map(|r| r.claim_id.clone())
            .unwrap_or_default();
        Some(settled)
    }

    /// Fold one booking into its market's open claim (one claim per market, so a
    /// market holding positions on both outcomes redeems once).
    fn accumulate_claim(
        &mut self,
        claim_id: &str,
        booking: &SettlementBooking,
        resolution: &MarketResolution,
        outcome_shares: &[Decimal],
        winning_token_id: &str,
        now_ms: i64,
    ) {
        let claim = self
            .claims
            .entry(claim_id.to_string())
            .or_insert_with(|| RedemptionClaim {
                id: claim_id.to_string(),
                condition_id: booking.condition_id.clone(),
                neg_risk: resolution.neg_risk,
                winning_token_id: winning_token_id.to_string(),
                outcome_shares: Vec::new(),
                payout_usd: Decimal::ZERO,
                position_ids: Vec::new(),
                created_at_ms: now_ms,
                attempts: 0,
                next_attempt_ms: 0,
                manual: false,
                last_error: None,
                tx_hash: None,
                block_number: None,
            });
        if claim.outcome_shares.len() < outcome_shares.len() {
            claim
                .outcome_shares
                .resize(outcome_shares.len(), Decimal::ZERO);
        }
        for (i, s) in outcome_shares.iter().enumerate() {
            if let Some(slot) = claim.outcome_shares.get_mut(i) {
                *slot += *s;
            }
        }
        claim.payout_usd += booking.payout_usd;
        claim.position_ids.push(booking.position_id.clone());
        claim.neg_risk = resolution.neg_risk;
        if claim.winning_token_id.is_empty() {
            claim.winning_token_id = winning_token_id.to_string();
        }
    }

    /// The claim id for a new settlement on this market: the plain condition id
    /// for the market's FIRST claim, then `#2`, `#3`, … (the claim's ordinal), so
    /// a redeemed id is never reused.
    fn claim_id_for(&self, condition_id: &str) -> String {
        let used = self
            .redeemed_per_condition
            .get(condition_id)
            .copied()
            .unwrap_or(0);
        if used == 0 && !self.claims.contains_key(condition_id) {
            return condition_id.to_string();
        }
        format!("{condition_id}#{}", used + 1)
    }

    /// Positions whose settlement was committed but whose close never happened
    /// (a crash between the journal append and the close). The caller closes
    /// them exactly once — see `Core::recover_settlements` — and then marks them
    /// closed with [`SettlementBook::note_closed`].
    pub fn recover<'a>(&self, positions: &'a [OpenPosition]) -> Vec<&'a OpenPosition> {
        positions
            .iter()
            .filter(|p| {
                let key = settlement_key(p);
                self.is_applied(&key) && !self.closed.contains(&key)
            })
            .collect()
    }

    /// The committed settlement of a position key, for the recovery close.
    pub fn record(&self, key: &str) -> Option<&SettlementRecord> {
        self.applied.get(key)
    }

    /// Mark a settled position's close (and trade record) as durable. Appended
    /// AFTER the trade record, so recovery that sees this line knows a second
    /// close would double-count the realized PnL.
    pub fn note_closed(&mut self, key: &str, now_ms: i64) {
        if !self.closed.insert(key.to_string()) {
            return;
        }
        if let Some(j) = self.journal.as_ref() {
            j.append(&JournalLine::Closed {
                at_ms: now_ms,
                key: key.to_string(),
            });
        }
    }

    /// Has this settlement's close been made durable?
    pub fn is_closed(&self, key: &str) -> bool {
        self.closed.contains(key)
    }

    // ── Redemption ──────────────────────────────────────────────────────────

    /// Claims due for an attempt. Dispatching one stamps it as attempted with
    /// the current backoff, so a lost result retries later instead of spinning.
    pub fn take_redemptions(&mut self, now_ms: i64) -> Vec<RedemptionRequest> {
        let due: Vec<String> = self
            .claims
            .values()
            .filter(|c| !c.manual && c.next_attempt_ms <= now_ms)
            .map(|c| c.id.clone())
            .collect();
        let mut out = Vec::new();
        for id in due {
            if let Some(c) = self.claims.get_mut(&id) {
                c.attempts += 1;
                c.next_attempt_ms = now_ms + backoff_ms(c.attempts);
                out.push(c.request());
            }
        }
        out
    }

    /// A redemption attempt failed. Alerts on every failure (never silent) and
    /// arms the retry — `manual` failures stop the automatic retries for good.
    pub fn note_failure(&mut self, id: &str, failure: &RedemptionFailure, now_ms: i64) -> bool {
        let Some(c) = self.claims.get_mut(id) else {
            return false;
        };
        c.last_error = Some(failure.message.clone());
        c.manual = failure.manual;
        if failure.manual {
            c.next_attempt_ms = i64::MAX;
        } else {
            c.next_attempt_ms = now_ms + backoff_ms(c.attempts);
        }
        true
    }

    /// A redemption landed: the claim leaves the receivable. Returns the payout
    /// that must be credited to the cash ledger (once).
    pub fn note_confirmed(
        &mut self,
        id: &str,
        tx_hash: Option<String>,
        block_number: Option<u64>,
        now_ms: i64,
    ) -> Option<Decimal> {
        let claim = self.claims.remove(id)?;
        *self
            .redeemed_per_condition
            .entry(claim.condition_id.clone())
            .or_insert(0) += 1;
        self.redeemed.insert(id.to_string());
        if let Some(j) = self.journal.as_ref() {
            j.append(&JournalLine::Redeemed {
                at_ms: now_ms,
                claim_id: id.to_string(),
                tx_hash,
                block_number,
            });
        }
        Some(claim.payout_usd)
    }

    /// One-line summary for the log/panel.
    pub fn summary(&self) -> String {
        format!(
            "settled={} pending_redeem={} receivable={} manual={}",
            self.applied.len(),
            self.claims.len(),
            self.receivable_usd(),
            self.manual_claim_count()
        )
    }
}

fn backoff_ms(attempts: u32) -> i64 {
    let shift = attempts.saturating_sub(1).min(6);
    (RETRY_BASE_MS << shift).min(RETRY_MAX_MS)
}

/// The outcome-share vector a persisted settlement record describes: shares
/// parked at the record's outcome index, in the resolution's outcome order.
fn settled_shares(record: &SettlementRecord) -> Vec<Decimal> {
    let n = record.outcome_count.max(2);
    let mut v = vec![Decimal::ZERO; n];
    if let Some(i) = record.outcome_index
        && let Some(slot) = v.get_mut(i)
    {
        *slot = record.shares;
    }
    v
}

fn outcome_shares_for(resolution: &MarketResolution, booking: &SettlementBooking) -> Vec<Decimal> {
    let n = resolution.payouts.len().max(2);
    let mut v = vec![Decimal::ZERO; n];
    if let Some(i) = booking.outcome_index
        && let Some(slot) = v.get_mut(i)
    {
        *slot = booking.shares;
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SignalDirection;
    use crate::position::CashFlows;
    use rust_decimal_macros::dec;

    fn position(id: &str, token: &str, shares: Decimal, entered_ms: i64) -> OpenPosition {
        OpenPosition {
            id: id.into(),
            strategy: "spread_arb".into(),
            asset: "BTC".into(),
            direction: SignalDirection::Up,
            token_id: token.into(),
            condition_id: "cond-1".into(),
            entry_price: dec!(0.40),
            current_price: dec!(0.40),
            prev_price: dec!(0.40),
            shares,
            cost_usd: dec!(4),
            was_maker_entry: true,
            entry_fee_pct: Decimal::ZERO,
            entry_role: crate::model::OrderRole::Maker,
            exit_role: crate::model::OrderRole::Pending,
            target_exit_price: None,
            entered_at_ms: entered_ms,
            expires_at_ms: entered_ms + 300_000,
            // F6 (main): no book has been seen for this fixture position.
            last_book_ts: 0,
            state: crate::exit_policy::ExitState::new(dec!(0.40), entered_ms),
            flows: CashFlows {
                entry_cost_usd: dec!(4),
                opened_shares: shares,
                ..Default::default()
            },
        }
    }

    fn resolution(payouts: Vec<(&str, Decimal)>) -> MarketResolution {
        MarketResolution {
            condition_id: "cond-1".into(),
            resolved: true,
            payouts: payouts
                .into_iter()
                .map(|(t, p)| (t.to_string(), p))
                .collect(),
            neg_risk: true,
            source: "test".into(),
        }
    }

    fn win() -> MarketResolution {
        resolution(vec![("up", dec!(1)), ("down", dec!(0))])
    }

    fn book_with_journal() -> (SettlementBook, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "bksettle-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("settlements.jsonl");
        (SettlementBook::new(Some(&path)), path)
    }

    #[test]
    fn booking_prices_a_win_and_a_loss() {
        let p = position("hft-1", "up", dec!(10), 1_000);
        let b = booking_for(&p, &win()).unwrap();
        assert_eq!(b.payout_per_share, dec!(1));
        assert_eq!(b.payout_usd, dec!(10));
        assert!(b.winning);
        assert_eq!(b.outcome_index, Some(0));

        let losing = resolution(vec![("up", dec!(0)), ("down", dec!(1))]);
        let b = booking_for(&p, &losing).unwrap();
        assert_eq!(b.payout_usd, Decimal::ZERO);
        assert!(!b.winning);
    }

    #[test]
    fn booking_ignores_a_resolution_for_another_market_or_token() {
        let p = position("hft-1", "up", dec!(10), 1_000);
        let mut other = win();
        other.condition_id = "cond-2".into();
        assert!(booking_for(&p, &other).is_none());
        let partial = resolution(vec![("other-token", dec!(1))]);
        assert!(booking_for(&p, &partial).is_none());
    }

    /// The reverse-acceptance target: settle twice, book once.
    #[test]
    fn settlement_is_idempotent_in_memory() {
        let (mut book, path) = book_with_journal();
        let p = position("hft-1", "up", dec!(10), 1_000);
        let b = booking_for(&p, &win()).unwrap();
        let first = book.book(&b, &win(), 10).expect("first booking");
        assert_eq!(first.claim_id, "cond-1");
        assert!(
            book.book(&b, &win(), 11).is_none(),
            "second booking refused"
        );
        assert_eq!(book.applied_count(), 1);
        assert_eq!(book.receivable_usd(), dec!(10));
        // A diag read of the journal: one line, one settlement.
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 1);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn settlement_survives_a_restart_and_is_not_rebooked() {
        let (mut book, path) = book_with_journal();
        let p = position("hft-1", "up", dec!(10), 1_000);
        let b = booking_for(&p, &win()).unwrap();
        book.book(&b, &win(), 10).unwrap();
        drop(book);

        let mut reopened = SettlementBook::new(Some(&path));
        assert!(reopened.is_applied(&settlement_key(&p)));
        assert_eq!(reopened.receivable_usd(), dec!(10));
        assert_eq!(reopened.pending_claim_count(), 1);
        assert!(reopened.book(&b, &win(), 20).is_none(), "no rebooking");
        // ...and the restored claim is redeemable.
        let due = reopened.take_redemptions(100);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].condition_id, "cond-1");
        assert_eq!(due[0].outcome_shares, vec![dec!(10), Decimal::ZERO]);
        assert!(due[0].neg_risk, "the neg-risk flag survives a restart");
        assert_eq!(due[0].winning_token_id, "up");

        // Confirming it clears the receivable and is durable too.
        let payout = reopened
            .note_confirmed(&due[0].id, Some("0xabc".into()), Some(7), 101)
            .unwrap();
        assert_eq!(payout, dec!(10));
        assert_eq!(reopened.receivable_usd(), Decimal::ZERO);
        drop(reopened);

        let mut after = SettlementBook::new(Some(&path));
        assert_eq!(
            after.receivable_usd(),
            Decimal::ZERO,
            "redeemed stays redeemed"
        );
        assert!(after.take_redemptions(1_000_000).is_empty());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn recover_reports_positions_whose_close_never_ran() {
        let (mut book, path) = book_with_journal();
        let p = position("hft-1", "up", dec!(10), 1_000);
        let b = booking_for(&p, &win()).unwrap();
        book.book(&b, &win(), 10).unwrap();
        // The position book still holds it (the close was interrupted).
        let restored = vec![p.clone()];
        let stale = book.recover(&restored);
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].id, "hft-1");
        // A position that was never settled is not reported.
        let fresh = position("hft-2", "down", dec!(3), 2_000);
        assert!(book.recover(&[fresh]).is_empty());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn two_positions_on_one_market_share_a_claim() {
        let (mut book, path) = book_with_journal();
        let up = position("hft-1", "up", dec!(10), 1_000);
        let down = position("hft-2", "down", dec!(4), 1_500);
        let mut r = win();
        r.neg_risk = true;
        let bu = booking_for(&up, &r).unwrap();
        let bd = booking_for(&down, &r).unwrap();
        let c1 = book.book(&bu, &r, 10).unwrap().claim_id;
        let c2 = book.book(&bd, &r, 11).unwrap().claim_id;
        assert_eq!(c1, c2, "one market, one claim");
        let claim = book.claim(&c1).unwrap();
        assert_eq!(claim.outcome_shares, vec![dec!(10), dec!(4)]);
        assert_eq!(claim.payout_usd, dec!(10));
        assert_eq!(claim.position_ids, vec!["hft-1", "hft-2"]);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_market_settled_again_after_a_redeem_gets_a_fresh_claim_id() {
        let (mut book, path) = book_with_journal();
        let p = position("hft-1", "up", dec!(10), 1_000);
        let b = booking_for(&p, &win()).unwrap();
        let first = book.book(&b, &win(), 10).unwrap().claim_id;
        let due = book.take_redemptions(100);
        book.note_confirmed(&due[0].id, Some("0x1".into()), Some(1), 101);
        assert_eq!(first, "cond-1");
        // A later entry on the same market (a re-open in the same round) settles
        // into a NEW claim: the redeemed id is never reused.
        let p2 = position("hft-9", "up", dec!(2), 5_000);
        let b2 = booking_for(&p2, &win()).unwrap();
        let second = book.book(&b2, &win(), 5_001).unwrap().claim_id;
        assert_eq!(second, "cond-1#2", "the second claim on the market");
        assert_eq!(book.receivable_usd(), dec!(2));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// Redemption failure handling: loud (recorded), backed off, and a manual
    /// verdict stops the loop instead of hammering the chain.
    #[test]
    fn failures_are_recorded_and_backed_off() {
        let (mut book, path) = book_with_journal();
        let p = position("hft-1", "up", dec!(10), 1_000);
        let b = booking_for(&p, &win()).unwrap();
        let id = book.book(&b, &win(), 10).unwrap().claim_id;
        assert_eq!(book.take_redemptions(100).len(), 1);
        assert!(
            book.take_redemptions(100).is_empty(),
            "in flight, not re-sent"
        );
        book.note_failure(
            &id,
            &RedemptionFailure {
                message: "nonce too low".into(),
                manual: false,
            },
            200,
        );
        let (attempts, next_attempt_ms, last_error) = {
            let c = book.claim(&id).unwrap();
            (c.attempts, c.next_attempt_ms, c.last_error.clone())
        };
        assert_eq!(last_error.as_deref(), Some("nonce too low"));
        assert_eq!(attempts, 1);
        assert!(next_attempt_ms > 200, "backoff armed");
        assert!(book.take_redemptions(201).is_empty());
        assert_eq!(book.take_redemptions(next_attempt_ms).len(), 1);

        // A manual verdict is terminal: no further automatic attempt.
        book.note_failure(
            &id,
            &RedemptionFailure {
                message: "positions held by another address".into(),
                manual: true,
            },
            300,
        );
        assert_eq!(book.manual_claim_count(), 1);
        assert!(book.take_redemptions(i64::MAX - 1).is_empty());
        // The money is still owed to us, so the receivable stays on the books.
        assert_eq!(book.receivable_usd(), dec!(10));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn blind_detection_needs_an_unanswered_query_older_than_the_grace() {
        let (mut book, path) = book_with_journal();
        let p = position("hft-1", "up", dec!(10), 1_000);
        book.track_markets(std::slice::from_ref(&p), 400_000);
        assert_eq!(book.tracked_markets(), 1);
        let q = book.take_queries(400_000);
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].token_ids, vec!["up".to_string()]);
        assert!(book.blind_since_ms(400_000 + BLIND_AFTER_MS + 1).is_some());
        // A venue that answers "not resolved yet" is alive: not blind, and no
        // storm of re-queries either.
        book.note_answer("cond-1", false, 400_001);
        assert!(
            book.blind_since_ms(400_001 + BLIND_AFTER_MS).is_none(),
            "a fresh answer is a sign of life"
        );
        assert!(book.take_queries(400_001 + REQUERY_MS - 1).is_empty());
        // The re-query clock re-arms after REQUERY_MS; a lost query (no answer
        // at all) is re-sent on the same cadence.
        assert_eq!(book.take_queries(400_001 + REQUERY_MS + 1).len(), 1);
        assert_eq!(book.take_queries(400_001 + 2 * REQUERY_MS + 2).len(), 1);
        // Answer once and then never again: the newest sign of life ages out.
        assert!(
            book.blind_since_ms(400_001 + REQUERY_MS + 1 + BLIND_AFTER_MS)
                .is_some()
        );
        // The alert fires on the healthy → blind edge, once.
        assert!(book.note_blind_state(true));
        assert!(!book.note_blind_state(true));
        assert!(!book.note_blind_state(false));
        assert!(book.note_blind_state(true), "a new episode alerts again");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn unresolved_markets_are_not_tracked_before_expiry() {
        let (mut book, path) = book_with_journal();
        let p = position("hft-1", "up", dec!(10), 1_000);
        // Before the round expires there is nothing to ask the venue.
        assert_eq!(book.track_markets(std::slice::from_ref(&p), 1_500), 0);
        assert_eq!(book.tracked_markets(), 0);
        assert_eq!(book.track_markets(&[p], 400_000), 1);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// A market whose recorded end lies decades behind the position's entry is
    /// carrying a value that is not a market end (the order path takes
    /// `expires_at_ms` from the order's declared slot, and a slot on another
    /// clock — the parity fixtures declare `roundSlot: 1` on an epoch clock —
    /// lands in 1970). Nothing may ever be settled on it.
    #[test]
    fn a_market_that_ended_before_we_entered_is_never_tracked() {
        let (mut book, path) = book_with_journal();
        let mut p = position("hft-1", "up", dec!(10), 1_756_000_000_000);
        p.expires_at_ms = 2_000; // `(roundSlot 1 + 1) * 1000ms` against an epoch clock
        let now = 1_756_000_000_001;
        assert_eq!(book.track_markets(std::slice::from_ref(&p), now), 0);
        assert_eq!(book.tracked_markets(), 0);
        assert!(book.take_queries(now).is_empty());

        // A market end just behind the entry is still that market's (a venue fill
        // is reported a little after the round it belongs to): it IS watched.
        let mut late = position("hft-2", "up", dec!(10), now);
        late.expires_at_ms = now - EXPIRY_TRUST_MS + 1;
        assert_eq!(
            book.track_markets(std::slice::from_ref(&late), now + 1),
            1,
            "a late-reported fill on a just-ended round is settleable"
        );
        assert_eq!(book.tracked_markets(), 1);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}

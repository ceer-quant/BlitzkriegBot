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
/// #684: how long a watched market may stay UNANSWERED past its first query
/// before the expiry fallback sweep stops waiting. The venue's round list only
/// carries rounds ending in the next ~20h, so an ALREADY-EXPIRED condition can
/// never appear in a settlement answer — the query re-arms every
/// [`REQUERY_MS`] forever and the position is a ghost until an operator
/// notices. That silence is the observable signature: a venue that CAN answer
/// does so within one re-query interval (every answer counts, including a
/// `resolved: false` "not yet"). Unanswered past this bound means the venue
/// will never rule on this condition, structurally.
pub const EXPIRY_FALLBACK_AFTER_MS: i64 = 7_200_000;
/// The exit reason a settled position closes with (`exit:settlement`).
pub const SETTLEMENT_EXIT_REASON: ExitReason = ExitReason::Settlement;
/// #684: the exit reason a fallback-closed position carries
/// (`unverifiable_expiry` on the wire). Deliberately NOT
/// [`ExitReason::Settlement`]: no venue verdict was ever received, so the
/// trade record must never read as one — the close price is a bookkeeping
/// value, not a payout, and the label is what makes that auditable forever.
pub const UNVERIFIED_EXPIRY_EXIT_REASON: ExitReason = ExitReason::UnverifiableExpiry;

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
    /// E28 §9.2: the account the position belongs to — the redemption claim
    /// (and its eventual payout) lands in THIS account's book.
    pub account_id: String,
    /// #647: the position's own execution venue — the redemption claim
    /// (and its request) carries it so the multi-plugin drain routes the
    /// redeem to the venue that holds the collateral. Empty = untagged (the
    /// pre-routing world: the request drains to the host's active plugin).
    pub venue: String,
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
        account_id: p.account_id.as_str().to_string(),
        // #647: the venue this position trades on, stamped at entry from the
        // SAME execution-venue lookup the executor drain routes by — the
        // claim's redeem request must reach the venue holding the collateral.
        venue: p.venue.clone(),
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
    /// The account the payout lands in (§9.2), carried so a restart's claim
    /// replay can restore the attribution. Empty on journal lines written
    /// before the field existed — the credit path routes that to the active
    /// book, the 0.3 behaviour.
    #[serde(default)]
    pub account_id: String,
    /// #647: the venue whose collateral the claim redeems, carried so a
    /// restart's claim replay restores the routing attribution the same way
    /// `account_id` does. Empty on journal lines written before the field
    /// existed — the request then rides the active-plugin fallback.
    #[serde(default)]
    pub venue: String,
    pub applied_at_ms: i64,
}

/// A settled position whose collateral has not reached the wallet yet: the
/// receivable. One claim per (market, account): a market with positions on
/// both outcomes redeems once — but positions of DIFFERENT accounts on the
/// same market never share a claim (the payout lands in the wallet that paid
/// for the shares, §9.2; the account split happens at booking time).
#[derive(Debug, Clone, PartialEq)]
pub struct RedemptionClaim {
    pub id: String,
    pub condition_id: String,
    /// The account whose wallet the redemption pays into — carried from the
    /// position's `account_id` at booking time (E28 §9.2, the same principle
    /// `MergeClaim.account_id` already lives by). A claim redeems into
    /// exactly one book.
    pub account_id: String,
    /// #647: the venue whose on-chain collateral this claim redeems —
    /// carried from the position's execution venue at booking time (the same
    /// stamp the executor drain routes orders by). Empty = untagged: the
    /// request drains to the host's active plugin, the pre-#647 behaviour.
    pub venue: String,
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
            venue: self.venue.clone(),
        }
    }
}

/// A merge whose collateral is still on its way: complete share-pairs were
/// burned on-chain at intent time, but the $1.00/pair of USDC only reaches the
/// wallet when the venue's merge transaction mines. The pairs are an account
/// receivable, not cash, until that receipt — the SAME principle settlement
/// lives by, and for the same reason: crediting `balance` at intent time says
/// the wallet holds money it does not hold, and the accounting audit's venue
/// leg (live) or the local cash identity (dry) goes red, correctly.
#[derive(Debug, Clone, PartialEq)]
pub struct MergeClaim {
    pub id: String,
    pub condition_id: String,
    pub account_id: String,
    /// #647: the venue the burned pairs sit on — carried from the merging
    /// legs' execution venue, so the merge request drains to the executor
    /// that can actually run the CTF merge. Empty = untagged (active-plugin
    /// fallback, the pre-#647 behaviour).
    pub venue: String,
    /// The market settles through the NegRisk adapter (drives the merge call).
    pub neg_risk: bool,
    /// Complete pairs burned = the USDC collateral due (1.00 per pair).
    pub pairs_usd: Decimal,
    pub created_at_ms: i64,
    pub attempts: u32,
    pub next_attempt_ms: i64,
    pub manual: bool,
    pub last_error: Option<String>,
    pub tx_hash: Option<String>,
    pub block_number: Option<u64>,
}

impl MergeClaim {
    /// The boundary DTO the venue executes.
    pub fn request(&self) -> blitzkrieg_market_api::MergeRequest {
        blitzkrieg_market_api::MergeRequest {
            id: self.id.clone(),
            condition_id: self.condition_id.clone(),
            neg_risk: self.neg_risk,
            pairs_usd: self.pairs_usd,
            venue: self.venue.clone(),
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
    /// #461: complete pairs were burned and their collateral is now a
    /// receivable. The durable commit point of a merge — replaying it re-opens
    /// the claim, so the $1/pair is never lost across a restart.
    MergeBurned {
        at_ms: i64,
        claim_id: String,
        condition_id: String,
        account_id: String,
        /// #647: the venue the burned pairs sit on (empty on journals written
        /// before the field existed — the replayed claim drains untagged).
        #[serde(default)]
        venue: String,
        neg_risk: bool,
        #[serde(with = "crate::decimal")]
        pairs_usd: Decimal,
    },
    /// #461: the merge's collateral reached the wallet.
    MergeConfirmed {
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
    /// #461: complete-pair merges burned but not yet confirmed on-chain, by id.
    merges: BTreeMap<String, MergeClaim>,
    /// Merge ids already confirmed, so an id is never confirmed twice.
    merged: BTreeSet<String>,
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
            merges: BTreeMap::new(),
            merged: BTreeSet::new(),
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
                            // Restored from the record (journals written before
                            // the field existed replay under the empty spelling;
                            // the credit path routes that to the active book
                            // exactly as 0.3 did).
                            account_id: record.account_id.clone(),
                            // #647: the venue the claim redeems on — same
                            // restore-or-empty rule as `account_id`: journals
                            // written before the field existed replay untagged
                            // and drain to the active plugin as before.
                            venue: record.venue.clone(),
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
                    // #647: same empty-inherits-first-stamp rule as the live
                    // fold, so a replayed claim routes like the original.
                    if claim.venue.is_empty() {
                        claim.venue = record.venue.clone();
                    }
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
            JournalLine::MergeBurned {
                claim_id,
                condition_id,
                account_id,
                venue,
                neg_risk,
                pairs_usd,
                at_ms,
                ..
            } => {
                // Replay re-opens the claim unless a later MergeConfirmed line
                // already removed it (the match arm below inserts nothing then).
                self.merges
                    .entry(claim_id.clone())
                    .or_insert_with(|| MergeClaim {
                        id: claim_id.clone(),
                        condition_id: condition_id.clone(),
                        account_id,
                        // #647: empty on journals written before the field
                        // existed — the replayed claim drains untagged.
                        venue,
                        neg_risk,
                        pairs_usd,
                        created_at_ms: at_ms,
                        attempts: 0,
                        next_attempt_ms: 0,
                        manual: false,
                        last_error: None,
                        tx_hash: None,
                        block_number: None,
                    });
            }
            JournalLine::MergeConfirmed { claim_id, .. } => {
                self.merges.remove(&claim_id);
                self.merged.insert(claim_id);
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

    /// The receivable of ONE account's claims (each claim carries the
    /// `account_id` it was booked from — E28 §9.2): the accounting audit
    /// compares THIS book's balance against THIS book's receivable; a sibling
    /// account's pending redemption is a different wallet's money.
    pub fn receivable_usd_for(&self, account: &str) -> Decimal {
        self.claims
            .values()
            .filter(|c| c.account_id == account)
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

    /// #684: the watched markets whose settlement query has NEVER been
    /// answered (`answers == 0`) and whose first dispatch is older than
    /// `min_ms`, as `(condition_id, first_sent_ms)` pairs. This is the
    /// structural signature of a market the venue will never rule on: the
    /// round list only carries future rounds, so an already-expired condition
    /// cannot appear in any answer, ever. A venue that CAN answer is heard
    /// within one re-query interval — every answer counts, including a
    /// `resolved: false` "not yet" — so only true silence accumulates here.
    /// Read-only accessor: the query map's shape stays private.
    pub fn unanswered_since(&self, min_ms: i64, now_ms: i64) -> Vec<(String, i64)> {
        self.queries
            .values()
            .filter(|q| q.answers == 0 && q.first_sent_ms > 0 && now_ms - q.first_sent_ms > min_ms)
            .map(|q| (q.condition_id.clone(), q.first_sent_ms))
            .collect()
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

    /// Replay fast path: the earliest instant any tracked query may become
    /// dispatchable again — its re-query cooldown end (`next_query_ms`) when
    /// it has been answered and is waiting, or `now + REQUERY_MS` when a
    /// dispatch is currently in flight (the answer may land any time, but
    /// the next retry cannot be due before the interval ends). `i64::MAX`
    /// when nothing is tracked. The caller may skip the query drain entirely
    /// until this instant without moving any dispatch timestamp: `take_que
    /// ries` stamps only when it actually hands one out, and a skipped tick
    /// would have handed out nothing (the cooldown comparison is `<= now`).
    pub fn next_query_deadline_ms(&self) -> i64 {
        self.queries
            .values()
            .map(|q| {
                if q.sent_at_ms > 0 {
                    q.sent_at_ms + REQUERY_MS
                } else {
                    q.next_query_ms
                }
            })
            .min()
            .unwrap_or(i64::MAX)
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
        // The market+account's open claim, if it has one (redemption removes
        // it). One claim per (market, account): both outcomes of one account's
        // market still redeem once (one NegRisk call), but a second account's
        // positions on the same market get their OWN claim — the payout lands
        // in the wallet that paid for the shares (§9.2). An empty
        // `booking.account_id` (a journal written before the field existed)
        // keeps the historical single-claim shape under the empty-string key.
        let open_claim = self
            .claims
            .values()
            .find(|c| c.condition_id == booking.condition_id && c.account_id == booking.account_id)
            .map(|c| c.id.clone());
        let claim_id =
            if booking.payout_usd > Decimal::ZERO {
                Some(open_claim.unwrap_or_else(|| {
                    self.claim_id_for(&booking.condition_id, &booking.account_id)
                }))
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
            account_id: booking.account_id.clone(),
            venue: booking.venue.clone(),
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

    /// Fold one booking into its market+account's open claim (one claim per
    /// market PER ACCOUNT, so one account's positions on both outcomes redeem
    /// once while a second account's positions get their own claim).
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
                account_id: booking.account_id.clone(),
                venue: booking.venue.clone(),
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
        // #647: a claim folded from an untagged leg stays routable — the venue
        // stamp rides the same "empty inherits the first stamp" rule the
        // winning token uses, so every leg of one claim is one venue's
        // collateral (the drain hands it to exactly one executor).
        if claim.venue.is_empty() {
            claim.venue = booking.venue.clone();
        }
        if claim.winning_token_id.is_empty() {
            claim.winning_token_id = winning_token_id.to_string();
        }
    }

    /// The claim id for a new settlement on this market: the plain condition id
    /// for the market's FIRST claim, then `#2`, `#3`, … (the claim's ordinal), so
    /// a redeemed id is never reused. A SECOND account settling the same market
    /// (its claim id is taken) also falls through to the next ordinal — two
    /// accounts never share a claim id, so their payouts stay attributable.
    fn claim_id_for(&self, condition_id: &str, _account_id: &str) -> String {
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

    /// #647: the per-venue drain — every due claim whose request names
    /// `venue`, plus the UNTAGGED (`""`-venue) claims when `untagged_fallback`
    /// (the requested plugin is the host's active one). Take-once on the
    /// dispatch stamp: a sibling executor never sees a claim this drain
    /// handed out, and a second venue's redeem is never mis-routed to a
    /// foreign bridge. Same dispatch semantics as [`Self::take_redemptions`]
    /// (each handed-out claim is stamped with its backoff immediately).
    pub fn take_redemptions_for_venue(
        &mut self,
        venue: &str,
        untagged_fallback: bool,
        now_ms: i64,
    ) -> Vec<RedemptionRequest> {
        let due: Vec<String> = self
            .claims
            .values()
            .filter(|c| {
                !c.manual
                    && c.next_attempt_ms <= now_ms
                    && (c.venue == venue || (untagged_fallback && c.venue.is_empty()))
            })
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

    /// Replay fast path: the earliest instant any pending redemption claim
    /// can become due (`i64::MAX` when none). `take_redemptions` dispatches
    /// only when `next_attempt_ms <= now`, and a dispatch moves the clock
    /// strictly later, so skipping the drain until this instant dispatches
    /// exactly the same set at exactly the same stamps.
    pub fn next_redemption_ms(&self) -> i64 {
        self.claims
            .values()
            .filter(|c| !c.manual)
            .map(|c| c.next_attempt_ms)
            .min()
            .unwrap_or(i64::MAX)
    }

    /// Replay fast path: the earliest instant any pending merge claim can
    /// become due (`i64::MAX` when none). Same argument as
    /// [`Self::next_redemption_ms`]: `take_merges` dispatches only when
    /// `next_attempt_ms <= now`, and a dispatch moves the clock strictly
    /// later, so skipping the drain until this instant dispatches exactly the
    /// same set at exactly the same stamps.
    pub fn next_merge_ms(&self) -> i64 {
        self.merges
            .values()
            .filter(|m| !m.manual)
            .map(|m| m.next_attempt_ms)
            .min()
            .unwrap_or(i64::MAX)
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
    /// that must be credited to the cash ledger (once) and the account whose
    /// book it lands in (§9.2; empty on journals written before the field —
    /// the caller routes that to the active book, the 0.3 behaviour).
    pub fn note_confirmed(
        &mut self,
        id: &str,
        tx_hash: Option<String>,
        block_number: Option<u64>,
        now_ms: i64,
    ) -> Option<(String, Decimal)> {
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
        Some((claim.account_id, claim.payout_usd))
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

    // ── Merge receivables (#461) ────────────────────────────────────────────

    /// A merge burned `pairs` complete share-pairs at intent time. The pairs
    /// are now on-chain collateral the wallet has NOT received yet: the claim
    /// goes into the receivable, and cash is credited only when the venue's
    /// merge transaction is confirmed. Durable via the journal, so a restart
    /// between the burn and the receipt never loses the $1/pair. `venue` is
    /// #647's routing attribution — the venue the burned pairs sit on, so the
    /// request drains to that venue's executor.
    pub fn note_merge_burned(
        &mut self,
        condition_id: &str,
        account_id: &str,
        venue: &str,
        neg_risk: bool,
        pairs: Decimal,
        now_ms: i64,
    ) -> String {
        let id = format!("{condition_id}#merge");
        let mut n = 1;
        while self.merges.contains_key(&id) || self.merged.contains(&id) {
            n += 1;
            let next = format!("{condition_id}#merge-{n}");
            if self.merges.contains_key(&next) || self.merged.contains(&next) {
                continue;
            }
            return self.note_merge_burned_inner(
                next,
                condition_id,
                account_id,
                venue,
                neg_risk,
                pairs,
                now_ms,
            );
        }
        self.note_merge_burned_inner(id, condition_id, account_id, venue, neg_risk, pairs, now_ms)
    }

    #[allow(clippy::too_many_arguments)]
    fn note_merge_burned_inner(
        &mut self,
        id: String,
        condition_id: &str,
        account_id: &str,
        venue: &str,
        neg_risk: bool,
        pairs: Decimal,
        now_ms: i64,
    ) -> String {
        let pairs_usd = pairs;
        self.merges.insert(
            id.clone(),
            MergeClaim {
                id: id.clone(),
                condition_id: condition_id.to_string(),
                account_id: account_id.to_string(),
                venue: venue.to_string(),
                neg_risk,
                pairs_usd,
                created_at_ms: now_ms,
                attempts: 0,
                next_attempt_ms: 0,
                manual: false,
                last_error: None,
                tx_hash: None,
                block_number: None,
            },
        );
        if let Some(j) = self.journal.as_ref() {
            j.append(&JournalLine::MergeBurned {
                at_ms: now_ms,
                claim_id: id.clone(),
                condition_id: condition_id.to_string(),
                account_id: account_id.to_string(),
                venue: venue.to_string(),
                neg_risk,
                pairs_usd,
            });
        }
        id
    }

    /// Merge collateral due but not yet in the wallet — rides the SAME
    /// receivable the audit's cash identity carries.
    pub fn merge_receivable_usd(&self) -> Decimal {
        self.merges
            .values()
            .fold(Decimal::ZERO, |a, m| a + m.pairs_usd)
    }

    /// ONE account's merge receivable (same principle as
    /// [`Self::receivable_usd_for`]).
    pub fn merge_receivable_usd_for(&self, account: &str) -> Decimal {
        self.merges
            .values()
            .filter(|m| m.account_id == account)
            .fold(Decimal::ZERO, |a, m| a + m.pairs_usd)
    }

    pub fn pending_merge_count(&self) -> usize {
        self.merges.len()
    }

    /// Merges due for an attempt. Dispatching stamps the backoff, so a lost
    /// result retries instead of spinning.
    pub fn take_merges(&mut self, now_ms: i64) -> Vec<blitzkrieg_market_api::MergeRequest> {
        let due: Vec<String> = self
            .merges
            .values()
            .filter(|m| !m.manual && m.next_attempt_ms <= now_ms)
            .map(|m| m.id.clone())
            .collect();
        let mut out = Vec::new();
        for id in due {
            if let Some(m) = self.merges.get_mut(&id) {
                m.attempts += 1;
                m.next_attempt_ms = now_ms + backoff_ms(m.attempts);
                out.push(m.request());
            }
        }
        out
    }

    /// #647: the per-venue drain — every due merge whose request names
    /// `venue`, plus the UNTAGGED (`""`-venue) merges when `untagged_fallback`
    /// (the requested plugin is the host's active one). Same dispatch
    /// semantics as [`Self::take_merges`]; a sibling executor never sees a
    /// merge this drain handed out.
    pub fn take_merges_for_venue(
        &mut self,
        venue: &str,
        untagged_fallback: bool,
        now_ms: i64,
    ) -> Vec<blitzkrieg_market_api::MergeRequest> {
        let due: Vec<String> = self
            .merges
            .values()
            .filter(|m| {
                !m.manual
                    && m.next_attempt_ms <= now_ms
                    && (m.venue == venue || (untagged_fallback && m.venue.is_empty()))
            })
            .map(|m| m.id.clone())
            .collect();
        let mut out = Vec::new();
        for id in due {
            if let Some(m) = self.merges.get_mut(&id) {
                m.attempts += 1;
                m.next_attempt_ms = now_ms + backoff_ms(m.attempts);
                out.push(m.request());
            }
        }
        out
    }

    /// A merge attempt failed. Same retry vocabulary as a redemption.
    pub fn note_merge_failure(
        &mut self,
        id: &str,
        failure: &blitzkrieg_market_api::RedemptionFailure,
        now_ms: i64,
    ) -> bool {
        let Some(m) = self.merges.get_mut(id) else {
            return false;
        };
        m.last_error = Some(failure.message.clone());
        m.manual = failure.manual;
        if failure.manual {
            m.next_attempt_ms = i64::MAX;
        } else {
            m.next_attempt_ms = now_ms + backoff_ms(m.attempts);
        }
        true
    }

    /// A merge landed: the claim leaves the receivable. Returns
    /// `(account_id, usd)` — the collateral that must be credited (once).
    pub fn note_merge_confirmed(
        &mut self,
        id: &str,
        tx_hash: Option<String>,
        block_number: Option<u64>,
        now_ms: i64,
    ) -> Option<(String, Decimal)> {
        let m = self.merges.remove(id)?;
        self.merged.insert(id.to_string());
        if let Some(j) = self.journal.as_ref() {
            j.append(&JournalLine::MergeConfirmed {
                at_ms: now_ms,
                claim_id: id.to_string(),
                tx_hash,
                block_number,
            });
        }
        Some((m.account_id, m.pairs_usd))
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
mod tests;

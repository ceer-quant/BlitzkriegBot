//! Order Management Engine — the authoritative order state machine.
//!
//! Pure, deterministic, no I/O. Owns:
//!  - order lifecycle Pending → Live/Partial → Filled/Cancelled/Rejected/Failed
//!  - an idempotent fill ledger keyed by trade id (or tx hash / synthetic key)
//!  - cumulative-size deltas capped to order size, weighted-average price
//!  - FAILED rollback via negative deltas
//!  - a short buffer for fills that arrive before their order is registered
//!
//! This is the Rust port of the Node `order-manager` + `handleFill/applyFillDelta`
//! semantics, so dry and live share one implementation. Position/PnL accounting
//! stays in Node during P0: the OME emits canonical `FillDelta`s for Node to
//! project; P2 moves positions into the core too.
//!
//! # Size semantics (issue #178)
//!
//! `TrackedOrder::size` is the TOTAL size the order asked for; `filled_size` is
//! the CUMULATIVE size already applied. The remaining size — what still rests on
//! the book, and what a cancel retires — is `size - filled_size` ([`Ome::remaining`]),
//! never either of the two fields on its own. `size` is never mutated by a fill,
//! so "remaining" is derived, never stored.
//!
//! # Idempotency across a restart (issue #178)
//!
//! The applied-fill table is what makes a re-delivered trade id a no-op. It is
//! the OME's own state, not reconstructible from the trade log (`trades.jsonl`
//! holds one aggregated record per CLOSED position — no trade ids), so it is
//! persisted through an append-only mutation journal ([`AppliedFillRecord`])
//! that the caller drains and stores; [`Ome::restore_applied`] folds it back.
//! The OME itself still performs no I/O.
//!
//! Two further invariants live here:
//!  - **terminal absorption**: `Filled`/`Cancelled`/`Rejected`/`Failed` are never
//!    flipped back to `PartiallyFilled`. A late fill still accrues size (the
//!    shares are real) and is reported as a [`LateFill`] instead of resurrecting
//!    the order into a state the venue no longer agrees with.
//!  - **explicit LRU eviction**: entries are ordered by a monotone application
//!    sequence, and only entries belonging to terminal/untracked orders are ever
//!    evicted. A `HashMap`'s own key order is arbitrary, so the old
//!    `keys().next()` eviction could drop a trade id that was still active and
//!    let its fill be booked a second time.

use crate::model::*;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};

/// Entries the applied-fill table keeps before old ones are evicted. Only
/// terminal/untracked entries are evictable (see [`Ome::evict_applied`]), so the
/// table may legitimately exceed this while many orders rest (bounded by the risk
/// limits) — the cap is a memory bound for a long session, not a correctness one.
pub const APPLIED_CAP: usize = 5000;

/// Safety valve for the mutation journal: the caller drains it after every
/// applied fill, so this only trips if a caller stops draining entirely.
const APPLIED_JOURNAL_CAP: usize = 16_384;

#[derive(Debug, Clone, PartialEq)]
pub struct FillDelta {
    pub order_id: OrderId,
    pub token_id: TokenId,
    /// E28 (§9.2): the account this fill's cash moves in, copied from the
    /// tracked order when the delta is built — every downstream ledger move is
    /// then a field read on the fill fact, never a lookup that can fail or
    /// guess between books.
    pub account_id: AccountId,
    pub side: Side,
    /// Signed change in filled shares (negative = rollback of a provisional fill).
    pub delta: Decimal,
    pub price: Decimal,
    pub cumulative: Decimal,
    pub strategy: String,
    pub asset: String,
    pub direction: String,
    pub condition_id: String,
    pub round_slot: i64,
    pub mode: FillPolicy,
    /// Role THIS fill actually played (Maker or Taker) — the single basis for
    /// every fee decision downstream (E17). `mode` is what was requested; this is
    /// what happened.
    pub role: OrderRole,
    /// The order's own limit price. A resting BUY reserved `limit_price * size`;
    /// the filled shares must release the notional they were reserved AT, not the
    /// (better) price they happened to execute at — releasing only the execution
    /// notional strands the difference as a reservation the order can never
    /// spend back (issue #181).
    pub limit_price: Decimal,
    /// OME fill-ledger key of the trade that produced this delta (`trade_id`,
    /// falling back to tx hash / a synthetic key). A FAILED rollback carries the
    /// SAME key as the provisional fill it reverses, so the service can pair a
    /// rollback with the close bookkeeping that fill already drove (F4).
    pub trade_id: String,
}

/// A fill that arrived for an order whose lifecycle had already ended, or one the
/// venue reported beyond the order's own size. Both are legitimate venue
/// behaviour (a delayed user-WS event, a cancel racing an execution, an
/// over-report) and neither may move the order back into a live state — but both
/// must be VISIBLE, because the size they carry is still real money (issue #178).
#[derive(Debug, Clone, PartialEq)]
pub struct LateFill {
    pub order_id: OrderId,
    /// Venue trade id / tx hash / synthetic key the fill was matched on.
    pub trade_id: String,
    /// The terminal status that absorbed the fill, or `None` when the order was
    /// still live and only the size cap was hit.
    pub absorbed_by: Option<OrderStatus>,
    /// Signed size applied by THIS fill (negative for a rollback).
    pub size: Decimal,
    /// The order's cumulative filled size after absorbing it.
    pub cumulative: Decimal,
    /// The cumulative size the venue reported, before the order-size cap.
    pub reported_cumulative: Decimal,
    /// The order's total size.
    pub order_size: Decimal,
    /// The venue reported more than the order could hold — an over-fill.
    pub overfill: bool,
}

/// What one inbound fill did: the canonical delta to project (when it moved
/// size) and, separately, a late/over-reported fill to surface as an alert.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FillOutcome {
    pub delta: Option<FillDelta>,
    pub late_fill: Option<LateFill>,
    /// issue 691: a fill that resolved to NO order at all — buffered for retry
    /// and, on first sight, flagged so the service can audit + alert it. The
    /// shares are real money moving at the venue with no order of ours to
    /// attribute them to; the buffer cap (500, FIFO) means silence is a data
    /// hole, not a safety property.
    pub ghost_fill: Option<GhostFill>,
}

/// A fill the OME could not attribute to any order (`apply_fill_report`'s
/// resolve-miss branch). Pure fact carried up to the service, which owns the
/// alert + audit I/O — the OME itself performs none.
#[derive(Debug, Clone, PartialEq)]
pub struct GhostFill {
    /// Venue trade id / tx hash / synthetic key; `-` in the alert text when the
    /// venue carried neither (the key that dedup would use is empty then).
    pub trade_id: String,
    pub token_id: TokenId,
    pub side: Side,
    /// Cumulative size the venue reported for this trade.
    pub size: Decimal,
    pub price: Decimal,
}

#[derive(Debug, Clone)]
struct AppliedFill {
    applied: Decimal,
    price: Decimal,
    /// Order the trade key belongs to, so eviction can tell a live order's entry
    /// (never evictable) from a terminal one's.
    order_id: OrderId,
    /// Monotone application sequence — the explicit LRU order.
    seq: u64,
}

/// One durable entry (or tombstone) of the applied-fill table.
///
/// Append-only mutation log, folded by `trade_key` in file order exactly like the
/// order log: later records win, `evicted` records remove. Compacted on restore
/// to the surviving entries, so the file tracks the table rather than growing
/// forever.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppliedFillRecord {
    /// The idempotency key: venue trade id, else tx hash, else a synthetic key.
    pub trade_key: String,
    /// Order the key was applied to (missing on a tombstone written by an older
    /// build; harmless, it is only used for eviction ordering).
    #[serde(default)]
    pub order_id: OrderId,
    /// Cumulative size applied for this key (0 on a rollback to nothing).
    #[serde(with = "crate::decimal", default)]
    pub applied: Decimal,
    #[serde(with = "crate::decimal", default)]
    pub price: Decimal,
    /// Application sequence in the writer's session (informational: a replay
    /// re-sequences in file order).
    #[serde(default)]
    pub seq: u64,
    /// Tombstone: the entry was evicted and must not come back on replay.
    #[serde(default)]
    pub evicted: bool,
}

#[derive(Debug, Clone)]
struct PendingFill {
    fill: Fill,
    at_ms: i64,
}

#[derive(Debug, Clone)]
pub struct SubmitParams {
    pub order_id: OrderId,
    pub request: OrderRequest,
    pub submitted_at_ms: i64,
    /// #351 order lifecycle: when the strategy decided (defaults to
    /// `submitted_at_ms` when the caller has nothing earlier to say).
    pub decision_at_ms: i64,
    /// #351: when the order arrives at the venue (`decision_at + taker
    /// latency`). A dry taker with latency fills against the book as it
    /// stands AT this instant.
    pub execute_at_ms: i64,
    /// #351: when the fill report reaches the local book
    /// (`execute_at + rtt`). Ledger/position effects are stamped here.
    pub report_at_ms: i64,
    /// Maker→taker escalation window (ms), three-valued: `0` = the core's
    /// configured default, positive = that clock, negative = never escalate
    /// (see `service::escalation_delay_ms`). Not "0 = no escalation" — that was
    /// the doc, and it was wrong.
    pub maker_timeout_ms: i64,
    /// B1b: the round-length family the order was sized/timed against (the
    /// placement path resolves it three-state off the token's market and the
    /// global grid). `0` = the caller had no family fact (legacy callers /
    /// restored rows): the tracked order keeps the B1a serde default and the
    /// effective duration falls back through the market declaration, exactly
    /// as before this field existed.
    pub round_duration_sec: i64,
}

pub struct Ome {
    orders: HashMap<OrderId, TrackedOrder>,
    by_internal: HashMap<String, OrderId>,
    /// `(submitted_at_ms, order_id)` for every tracked order, kept in sync at
    /// the only two insertion points (`submit`, `restore`). `submitted_at_ms`
    /// is immutable after creation, so the set never needs re-keying. Query
    /// methods iterate this in key order instead of re-sorting the whole
    /// orders map on every call — the per-tick maintenance path calls
    /// `all()`/`live_orders()` several times per tick, and an O(n log n) sort
    /// over an ever-growing order table collapsed replay throughput
    /// (replay profile: ~54% of busy samples in `sort_by_key` under `Ome::all`).
    by_submitted: BTreeSet<(i64, OrderId)>,
    /// The live subset of `by_submitted`, maintained at the status
    /// transition points (`submit`/`restore` insert; `mark_terminal` and a
    /// fill that completes an order remove). `live_orders()` runs several
    /// times per tick on the maintenance path — once per open position —
    /// and iterating the FULL table for it made every tick O(total orders)
    /// with a SipHash lookup per row. The table only grows (one row per
    /// order ever placed, never removed), so long replays kept decaying even
    /// after the per-call sort was removed (profile: hash_one/memcmp leaves
    /// under `Ome::live_for`). Live orders number in the tens; this index
    /// keeps `live_orders()` at O(live).
    live_by_submitted: BTreeSet<(i64, OrderId)>,
    /// Per-token live subset: `token_id -> {(submitted_at_ms, order_id)}`.
    /// `live_for` runs once per open position per tick (exit checks) and on
    /// every fill resolution; after `live_by_submitted` it still filtered the
    /// WHOLE live set per call, leaving an O(positions × live) multiplier on
    /// the maintenance path (profile: the `live_for` filter closure was the
    /// top non-idle frame at 1080 samples). Keyed by token alone — a `&str`
    /// lookup with no per-call allocation — because per-token live orders
    /// number in single digits, so the side filter inside the bucket is
    /// free. Buckets prune to empty on retirement so the map never grows
    /// with historical tokens.
    live_by_token: HashMap<String, BTreeSet<(i64, OrderId)>>,
    /// Per-token completion times of FILLED SELL orders — the E31-b residual
    /// backstop's query ("did a sell on this token complete at/after the
    /// position's entry?") answered in O(log n) instead of a full-table scan
    /// per open position per tick. Append-only: one entry per completed sell.
    filled_sell_times: HashMap<String, BTreeSet<i64>>,
    applied: HashMap<String, AppliedFill>,
    /// Mutations of `applied`, in application order; the caller drains this and
    /// persists it. Empty on a core that never persists (the OME does no I/O).
    applied_journal: Vec<AppliedFillRecord>,
    applied_seq: u64,
    pending_fills: Vec<PendingFill>,
    pending_ttl_ms: i64,
    /// issue 691: trade keys already alerted as unresolvable, so a venue that
    /// re-reports the same ghost trade does not page the operator twice — and
    /// a trade whose order LATER registers clears out and can alert again if
    /// it ever ghosts anew. Bounded in practice by the same venue behaviour
    /// the pending buffer bounds; a `HashSet` would do, but the OME already
    /// imports `BTreeSet` and ghost keys number in single digits.
    ghost_alerted: BTreeSet<String>,
}

impl Ome {
    pub fn new() -> Self {
        Self {
            orders: HashMap::new(),
            by_internal: HashMap::new(),
            by_submitted: BTreeSet::new(),
            live_by_submitted: BTreeSet::new(),
            live_by_token: HashMap::new(),
            filled_sell_times: HashMap::new(),
            applied: HashMap::new(),
            applied_journal: Vec::new(),
            applied_seq: 0,
            pending_fills: Vec::new(),
            pending_ttl_ms: 30_000,
            ghost_alerted: BTreeSet::new(),
        }
    }

    pub fn get(&self, id: &str) -> Option<&TrackedOrder> {
        self.orders.get(id)
    }

    /// issue 561 账户维重置：把 `account` 的全部订单行（含终端态）从内存
    /// 表里整个拿掉，其他账户的行一个不动。清单见 service.rs 重置分支的
    /// 账户维/全局维分表——幂等表 `applied`/`by_internal` 是全局维（键无
    /// 账户归属，整表由重置的其他全局项处理），这里只做订单行本身。返回
    /// 移除的行数。
    ///
    /// 调用契约：目标账户的 resting 订单必须已先经 `cancel`（或已被整替）
    /// ——本方法只负责「连记录都消失」，不负责释放预留/通知 venue。
    pub fn remove_account_orders(&mut self, account: &str) -> usize {
        let doomed: Vec<String> = self
            .orders
            .values()
            .filter(|o| o.account_id.as_str() == account)
            .map(|o| o.order_id.clone())
            .collect();
        let n = doomed.len();
        for id in &doomed {
            self.index_remove(id);
            self.orders.remove(id);
        }
        n
    }

    /// Insert into all order indexes. The sets must agree: an order in
    /// `by_submitted` but missing from the live indexes would vanish from
    /// the live views once it BECAME live again — impossible, statuses never
    /// resurrect — so a drift here is a straight bug, and a test pins it.
    fn index_insert(&mut self, submitted_at_ms: i64, order_id: &str, token_id: &str, live: bool) {
        let key = (submitted_at_ms, order_id.to_string());
        self.by_submitted.insert(key.clone());
        if live {
            self.live_by_submitted.insert(key.clone());
            self.live_by_token
                .entry(token_id.to_string())
                .or_default()
                .insert(key);
        }
    }

    /// Remove an order's keys from all indexes before a map-level overwrite.
    fn index_remove(&mut self, id: &str) {
        let prev = self
            .orders
            .get(id)
            .map(|o| (o.submitted_at_ms, o.order_id.clone(), o.token_id.clone()));
        if let Some((submitted_at_ms, order_id, token_id)) = prev {
            let key = (submitted_at_ms, order_id);
            self.by_submitted.remove(&key);
            self.live_by_submitted.remove(&key);
            if let Some(bucket) = self.live_by_token.get_mut(&token_id) {
                bucket.remove(&key);
                if bucket.is_empty() {
                    self.live_by_token.remove(&token_id);
                }
            }
        }
    }

    /// Re-place one order's live-index membership after a status flip:
    /// `was_live` is the PRE-transition status, the map entry is read for the
    /// POST-transition status. `mutate` and `record_delta` both call this, so
    /// the live index can never drift from `is_live()`.
    fn sync_live_index(&mut self, id: &str, was_live: bool) {
        let Some(o) = self.orders.get(id) else {
            return;
        };
        let is_live = o.status.is_live();
        let key = (o.submitted_at_ms, o.order_id.clone());
        match (was_live, is_live) {
            (false, true) => {
                self.live_by_submitted.insert(key.clone());
                self.live_by_token
                    .entry(o.token_id.clone())
                    .or_default()
                    .insert(key);
            }
            (true, false) => {
                self.live_by_submitted.remove(&key);
                if let Some(bucket) = self.live_by_token.get_mut(&o.token_id) {
                    bucket.remove(&key);
                    if bucket.is_empty() {
                        self.live_by_token.remove(&o.token_id);
                    }
                }
            }
            _ => {}
        }
        // E31-b: a sell just COMPLETED — stamp its completion time (the
        // transition's now_ms, already written into updated_at_ms by the
        // caller) so the residual-backstop query stays O(log n) instead of
        // scanning every order ever placed, per position, per tick.
        if was_live && !is_live && o.side == Side::Sell && o.status == OrderStatus::Filled {
            let token = o.token_id.clone();
            let completed_at = o.updated_at_ms;
            self.filled_sell_times
                .entry(token)
                .or_default()
                .insert(completed_at);
        }
    }

    /// Remaining (still resting) size of an order: `size` is the TOTAL requested
    /// size and `filled_size` is cumulative, so the remainder is their difference
    /// — never either field alone (issue #178).
    pub fn remaining(&self, id: &str) -> Decimal {
        self.orders
            .get(id)
            .map(|o| (o.size - o.filled_size).max(Decimal::ZERO))
            .unwrap_or(Decimal::ZERO)
    }

    /// Rebuild the OME from persisted orders (crash recovery). Orders that were
    /// live when the process stopped stay live, so the startup sweep can
    /// reconcile/cancel anything the venue still holds. Idempotent per order id.
    pub fn restore(&mut self, orders: Vec<TrackedOrder>) {
        for o in orders {
            self.by_internal
                .insert(o.internal_key.clone(), o.order_id.clone());
            // Idempotent per order id: drop stale index keys when the order
            // already exists (a re-restore with a different timestamp would
            // otherwise leave the old keys behind and the query views would
            // yield that order twice). Both indexes clear together.
            let live = o.status.is_live();
            let submitted_at_ms = o.submitted_at_ms;
            let order_id = o.order_id.clone();
            let sell_completed = o.side == Side::Sell && o.status == OrderStatus::Filled;
            let completed_at_ms = o.updated_at_ms;
            let token_id = o.token_id.clone();
            self.index_remove(&order_id);
            self.orders.insert(order_id.clone(), o);
            self.index_insert(submitted_at_ms, &order_id, &token_id, live);
            // The E31-b completion index rebuilds from the snapshot too: a
            // position whose sell completed before a restart must still get
            // the residual re-close after it. `updated_at_ms` is the best
            // completion proxy a snapshot carries (the last delta's now_ms).
            if sell_completed {
                self.filled_sell_times
                    .entry(token_id)
                    .or_default()
                    .insert(completed_at_ms);
            }
        }
    }

    /// Restore the applied-fill idempotency table from its persisted log
    /// (issue #178). Records are folded in file (= application) order: a later
    /// record for a key wins, a tombstone removes it. Sequence numbers are
    /// re-assigned from the fold order, so the restored table evicts in exactly
    /// the order the original would have.
    pub fn restore_applied(&mut self, records: Vec<AppliedFillRecord>) {
        for r in records {
            if r.evicted {
                self.applied.remove(&r.trade_key);
                continue;
            }
            self.applied_seq += 1;
            let seq = self.applied_seq;
            self.applied.insert(
                r.trade_key,
                AppliedFill {
                    applied: r.applied,
                    price: r.price,
                    order_id: r.order_id,
                    seq,
                },
            );
        }
    }

    /// Current table, in application order — the compaction source.
    pub fn applied_snapshot(&self) -> Vec<AppliedFillRecord> {
        let mut v: Vec<AppliedFillRecord> = self
            .applied
            .iter()
            .map(|(trade_key, a)| AppliedFillRecord {
                trade_key: trade_key.clone(),
                order_id: a.order_id.clone(),
                applied: a.applied,
                price: a.price,
                seq: a.seq,
                evicted: false,
            })
            .collect();
        v.sort_by_key(|r| r.seq);
        v
    }

    /// Take the pending mutation journal. Draining is the caller's job — the OME
    /// never touches the filesystem — so a caller that wants restart-safe
    /// idempotency must drain after every applied fill and append the records.
    pub fn take_applied_journal(&mut self) -> Vec<AppliedFillRecord> {
        std::mem::take(&mut self.applied_journal)
    }

    /// Number of trade keys the idempotency table currently holds.
    pub fn applied_len(&self) -> usize {
        self.applied.len()
    }

    /// Order ids the applied-fill table references (issue #701: evidence
    /// attribution — a caller that needs to know WHICH book a fill belongs to
    /// walks these through [`Self::all`]; the raw table length is global and
    /// must never be read as one venue's history).
    pub fn applied_order_ids(&self) -> std::collections::HashSet<&str> {
        self.applied.values().map(|a| a.order_id.as_str()).collect()
    }

    /// True when this trade key was already applied with a non-zero size — the
    /// exact test a re-delivered trade id must fail.
    pub fn already_applied(&self, trade_key: &str) -> bool {
        self.applied
            .get(trade_key)
            .map(|a| a.applied > Decimal::ZERO)
            .unwrap_or(false)
    }

    fn journal(&mut self, rec: AppliedFillRecord) {
        self.applied_journal.push(rec);
        if self.applied_journal.len() > APPLIED_JOURNAL_CAP {
            let drop = self.applied_journal.len() - APPLIED_JOURNAL_CAP / 2;
            tracing::warn!(
                cap = APPLIED_JOURNAL_CAP,
                dropping = drop,
                "applied-fill journal was not drained by the caller; dropping the oldest mutations"
            );
            self.applied_journal.drain(0..drop);
        }
    }

    /// Record the cumulative size applied under one trade key, journalling the
    /// change. Reuses the key's original sequence when the key already exists, so
    /// a status progression (MATCHED→CONFIRMED) does not make an old key look
    /// newly applied.
    fn remember(&mut self, trade_key: String, order_id: &str, applied: Decimal, price: Decimal) {
        if let Some(existing) = self.applied.get_mut(&trade_key) {
            if existing.applied == applied && existing.price == price {
                return;
            }
            existing.applied = applied;
            existing.price = price;
            let (seq, order_id) = (existing.seq, existing.order_id.clone());
            self.journal(AppliedFillRecord {
                trade_key,
                order_id,
                applied,
                price,
                seq,
                evicted: false,
            });
        } else {
            self.applied_seq += 1;
            let seq = self.applied_seq;
            self.applied.insert(
                trade_key.clone(),
                AppliedFill {
                    applied,
                    price,
                    order_id: order_id.to_string(),
                    seq,
                },
            );
            self.journal(AppliedFillRecord {
                trade_key,
                order_id: order_id.to_string(),
                applied,
                price,
                seq,
                evicted: false,
            });
        }
        self.evict_applied();
    }

    /// Explicit LRU eviction (issue #178). The victim is the OLDEST entry by
    /// application sequence — and only among orders that are terminal or no
    /// longer tracked at all. An entry belonging to a live order is never
    /// evicted, whatever its age: it is the only thing standing between that
    /// order and a re-booked fill. When every entry belongs to a live order the
    /// table is allowed to exceed the cap (the live-order count is bounded by the
    /// risk limits), and that is reported rather than silently worked around.
    fn evict_applied(&mut self) {
        let mut evicted = 0usize;
        while self.applied.len() > APPLIED_CAP {
            let victim = self
                .applied
                .iter()
                .filter(|(_, a)| {
                    !self
                        .orders
                        .get(&a.order_id)
                        .map(|o| o.status.is_live())
                        .unwrap_or(false)
                })
                .min_by_key(|(_, a)| a.seq)
                .map(|(k, _)| k.clone());
            let Some(key) = victim else {
                tracing::warn!(
                    len = self.applied.len(),
                    cap = APPLIED_CAP,
                    "applied-fill table is over cap but every entry belongs to a live order; keeping them (evicting one would re-book its fill)"
                );
                break;
            };
            if let Some(a) = self.applied.remove(&key) {
                evicted += 1;
                self.journal(AppliedFillRecord {
                    trade_key: key,
                    order_id: a.order_id,
                    applied: a.applied,
                    price: a.price,
                    seq: a.seq,
                    evicted: true,
                });
            }
        }
        if evicted > 0 {
            tracing::debug!(
                evicted,
                len = self.applied.len(),
                "evicted applied-fill entries (oldest terminal-order keys first)"
            );
        }
    }

    /// Venue order ids the OME tracks — live AND terminal — for orphan
    /// detection and fill back-attribution: any venue-open id NOT in this set
    /// is unknown to us. Terminal ids must stay in the set: a fill that lands
    /// on the venue after our order went terminal (force-closed at round end,
    /// ghost-closed by reconcile, cancelled) still has to attribute through the
    /// FILLED-orders channel, and `apply_fill_report` absorbs it as a late
    /// fill. This is safe for the orphan sweep: a terminal order is not
    /// venue-open, so its id never appears in the venue's open list — and if
    /// it did, cancelling it would be correct anyway.
    pub fn known_venue_ids(&self) -> std::collections::HashSet<String> {
        self.orders
            .values()
            .filter_map(|o| o.venue_order_id.clone())
            .collect()
    }
    pub fn all(&self) -> Vec<&TrackedOrder> {
        self.by_submitted
            .iter()
            .filter_map(|(_, id)| self.orders.get(id))
            .collect()
    }
    pub fn live_orders(&self) -> Vec<&TrackedOrder> {
        self.live_by_submitted
            .iter()
            .filter_map(|(_, id)| self.orders.get(id))
            .filter(|o| o.status.is_live())
            .collect()
    }
    pub fn live_for(&self, token_id: &str, side: Side) -> Vec<&TrackedOrder> {
        self.live_by_token
            .get(token_id)
            .into_iter()
            .flatten()
            .filter_map(|(_, id)| self.orders.get(id))
            .filter(|o| o.status.is_live() && o.side == side)
            .collect()
    }

    /// True when a SELL on `token_id` COMPLETED (Filled) at/after `since_ms`.
    /// The E31-b residual backstop's question — "did a sell on this token
    /// fill at/after the position's entry?" — answered from the completion
    /// index instead of a full-table scan per open position per tick. The
    /// stamped instant is the completion transition's `now_ms` (what
    /// `updated_at_ms` held at Filled); a later absorbed late-fill bump on
    /// the order row does not move it, which is the truer "filled at" anyway.
    pub fn has_filled_sell_since(&self, token_id: &str, since_ms: i64) -> bool {
        self.filled_sell_times
            .get(token_id)
            .is_some_and(|ts| ts.range(since_ms..).next().is_some())
    }

    /// True while an order with this internal key is not terminal (dedup guard).
    pub fn has_pending(&self, internal_key: &str) -> bool {
        self.by_internal
            .get(internal_key)
            .and_then(|id| self.orders.get(id))
            .map(|o| o.status.is_live())
            .unwrap_or(false)
    }

    /// Register a newly submitted order as Pending.
    pub fn submit(&mut self, p: SubmitParams) -> CoreResult<()> {
        if self.has_pending(&p.request.internal_key) {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                format!("duplicate internal key: {}", p.request.internal_key),
            ));
        }
        if self.orders.contains_key(&p.order_id) {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                format!("order already registered: {}", p.order_id),
            ));
        }
        let r = &p.request;
        let order = TrackedOrder {
            order_id: p.order_id.clone(),
            internal_key: r.internal_key.clone(),
            strategy: r.strategy.clone(),
            asset: r.asset.clone(),
            direction: r.direction.clone(),
            token_id: r.token_id.clone(),
            condition_id: r.condition_id.clone(),
            side: r.side,
            mode: r.mode,
            price: r.price,
            size: r.size,
            filled_size: Decimal::ZERO,
            avg_fill_price: None,
            status: OrderStatus::Pending,
            round_slot: r.round_slot,
            submitted_at_ms: p.submitted_at_ms,
            updated_at_ms: p.submitted_at_ms,
            decision_at_ms: if p.decision_at_ms > 0 {
                p.decision_at_ms
            } else {
                p.submitted_at_ms
            },
            execute_at_ms: p.execute_at_ms,
            report_at_ms: p.report_at_ms,
            venue_order_id: None,
            place_uncertain: false,
            escalate_at_ms: None,
            maker_timeout_ms: p.maker_timeout_ms,
            role: OrderRole::Pending,
            // E28 (§9.2): the order belongs to the account its request named.
            account_id: r.account_id.clone(),
            // Derived at read time (list_orders); never stored.
            venue: String::new(),
            // B1b: the placement path's family stamp — `0` when the caller
            // had no family fact, the order then falls back through the
            // market declaration (B1a's tri-state) at every read.
            round_duration_sec: p.round_duration_sec,
        };
        self.by_internal
            .insert(r.internal_key.clone(), p.order_id.clone());
        // Pending is live (is_live = Pending|Live|PartiallyFilled): a fresh
        // order belongs in the live indexes from birth.
        let live = order.status.is_live();
        self.index_insert(p.submitted_at_ms, &p.order_id, &r.token_id, live);
        self.orders.insert(p.order_id.clone(), order);
        Ok(())
    }

    pub fn mark_live(&mut self, id: &str, now_ms: i64) -> CoreResult<()> {
        self.mutate(id, now_ms, |o| {
            if matches!(o.status, OrderStatus::Pending) {
                o.status = OrderStatus::Live;
            }
            Ok(())
        })
    }

    pub fn mark_terminal(&mut self, id: &str, status: OrderStatus, now_ms: i64) -> CoreResult<()> {
        debug_assert!(status.is_terminal());
        self.mutate(id, now_ms, |o| {
            o.status = status;
            Ok(())
        })
    }

    pub fn set_escalation(&mut self, id: &str, at_ms: i64) -> CoreResult<()> {
        self.mutate(id, at_ms, |o| {
            o.escalate_at_ms = Some(at_ms);
            Ok(())
        })
    }

    /// Replay fast path: the earliest armed escalation deadline over live
    /// orders (`i64::MAX` when none). The maintenance sweep may skip the
    /// live-order scan until this instant: `escalate_at_ms` is armed by the
    /// submit path and only ever moved to `now + timeout` (never earlier),
    /// so no deadline can become due between the sweep that noticed it and
    /// the instant this returns.
    pub fn next_escalation_ms(&self) -> i64 {
        self.live_by_submitted
            .iter()
            .filter_map(|(_, id)| self.orders.get(id))
            .filter_map(|o| o.escalate_at_ms)
            .min()
            .unwrap_or(i64::MAX)
    }

    /// Bind a core order to its exchange-assigned id after a LIVE POST acks.
    pub fn bind_venue(&mut self, id: &str, venue_order_id: String, now_ms: i64) -> CoreResult<()> {
        self.mutate(id, now_ms, |o| {
            o.venue_order_id = Some(venue_order_id);
            Ok(())
        })
    }

    /// A place POST TIMED OUT: the venue may or may not hold this order.
    /// Stamping the flag removes it from [`Self::live_orders`]-driven drains
    /// (`Core::pending_unbound`) so the executor stops re-submitting it — a
    /// second signed POST is a second real order at any venue that does not
    /// dedupe by client id (polymarket). The #462 retirement sweep still owns
    /// the order: the reservation stays held and the sweep decides its fate.
    pub fn mark_place_uncertain(&mut self, id: &str, now_ms: i64) -> CoreResult<()> {
        self.mutate(id, now_ms, |o| {
            o.place_uncertain = true;
            Ok(())
        })
    }

    /// B1b: idempotent lazy backfill of the order's round-length family — the
    /// OME-flavoured wrapper over [`TrackedOrder::backfill_round_duration`].
    /// A non-0 stamp is never overwritten (the first family a record was
    /// admitted under stays authoritative), so calling this on every sweep /
    /// fill pass is free and stable. The whole state lives under the
    /// single-threaded Core lock (`ipc/server.rs` serializes every verb and
    /// sweep), so the write-back rides the ordinary field-assignment path —
    /// no new lock.
    pub fn backfill_round_duration(&mut self, id: &str, sec: i64) {
        if sec <= 0 {
            return;
        }
        if let Some(o) = self.orders.get_mut(id) {
            o.backfill_round_duration(sec);
        }
    }

    /// Resolve an exchange order id (or a core id) to our tracked order.
    pub fn by_venue_or_id(&self, external: &str) -> Option<&TrackedOrder> {
        if let Some(o) = self.orders.get(external) {
            return Some(o);
        }
        self.orders
            .values()
            .find(|o| o.venue_order_id.as_deref() == Some(external))
    }

    fn mutate(
        &mut self,
        id: &str,
        now_ms: i64,
        f: impl FnOnce(&mut TrackedOrder) -> CoreResult<()>,
    ) -> CoreResult<()> {
        let was_live = self
            .orders
            .get(id)
            .map(|o| o.status.is_live())
            .ok_or_else(|| {
                CoreError::new(CoreErrorCode::UnknownOrder, format!("unknown order: {id}"))
            })?;
        {
            let o = self.orders.get_mut(id).expect("checked above");
            f(o)?;
            o.updated_at_ms = now_ms;
        }
        self.sync_live_index(id, was_live);
        Ok(())
    }

    /// Resolve an inbound fill to a tracked order: exact core/venue id, else
    /// same token+side live order (SDK user events sometimes only carry token).
    fn resolve(&self, fill: &Fill) -> Option<OrderId> {
        if let Some(o) = self.by_venue_or_id(&fill.order_id) {
            return Some(o.order_id.clone());
        }
        let mut cands: Vec<&TrackedOrder> = self
            .live_for(&fill.token_id, fill.side)
            .into_iter()
            .collect();
        cands.sort_by_key(|o| std::cmp::Reverse(o.submitted_at_ms));
        cands.first().map(|o| o.order_id.clone())
    }

    /// Apply one inbound fill event. Returns a canonical delta when position
    /// state changes (positive fills or FAILED rollbacks); None for duplicate /
    /// status-only updates. Unknown orders buffer the fill for later retry.
    ///
    /// Thin wrapper over [`Ome::apply_fill_report`] for callers with no interest
    /// in the late-fill signal; the service uses the report so a late or
    /// over-reported fill can be surfaced instead of vanishing.
    pub fn apply_fill(&mut self, fill: Fill, now_ms: i64) -> CoreResult<Option<FillDelta>> {
        Ok(self.apply_fill_report(fill, now_ms)?.delta)
    }

    /// Apply one inbound fill event, reporting BOTH what it moved and whether it
    /// was late/over-reported (issue #178).
    pub fn apply_fill_report(&mut self, mut fill: Fill, now_ms: i64) -> CoreResult<FillOutcome> {
        // A trade id already applied for an order the OME no longer tracks is a
        // duplicate, whatever order it resolves to now. The check runs BEFORE
        // resolution: `resolve` falls back to "newest live order on the same
        // token+side", so a late fill for a long-gone order could otherwise be
        // booked a second time onto an unrelated live one — the restart
        // double-count this table exists to stop (issue #178).
        let raw_key = fill
            .trade_id
            .clone()
            .or_else(|| fill.tx_hash.clone())
            .unwrap_or_default();
        if fill.status != FillStatus::Failed
            && !raw_key.is_empty()
            && let Some(prev) = self.applied.get(&raw_key)
            && prev.applied > Decimal::ZERO
            && !self.orders.contains_key(&prev.order_id)
        {
            tracing::debug!(
                trade = %raw_key,
                order = %prev.order_id,
                "duplicate trade for an order the OME no longer tracks; ignored"
            );
            return Ok(FillOutcome::default());
        }

        let order_id = match self.resolve(&fill) {
            Some(id) => {
                // issue 691: the order caught up with the fill (restart replay,
                // a late registration). The ghost alert retires — a trade that
                // resolves is not a ghost, whatever it looked like earlier.
                if let Some(key) = fill
                    .trade_id
                    .clone()
                    .or_else(|| fill.tx_hash.clone())
                    .filter(|k| !k.is_empty())
                {
                    self.ghost_alerted.remove(&key);
                }
                id
            }
            None => {
                // issue 691: an unattributable fill must not vanish into this
                // memory queue (cap 500, FIFO — silence is a data hole, not a
                // safety property). First sight of a trade key flags the
                // outcome; the service turns the flag into the audit row +
                // RiskAlert, exactly like a late fill. A re-report of the same
                // trade stays silent, and a FAILED notice books no money so it
                // never alerts (the same gate the duplicate check above uses).
                let key = fill
                    .trade_id
                    .clone()
                    .or_else(|| fill.tx_hash.clone())
                    .unwrap_or_default();
                let ghost_src = (fill.status != FillStatus::Failed && !key.is_empty()).then(|| {
                    (
                        key.clone(),
                        fill.token_id.clone(),
                        fill.side,
                        fill.size,
                        fill.price,
                    )
                });
                let first_seen = ghost_src.is_some()
                    && self
                        .ghost_alerted
                        .insert(ghost_src.as_ref().unwrap().0.clone());
                let pending_len = {
                    self.pending_fills.push(PendingFill {
                        fill,
                        at_ms: now_ms,
                    });
                    if self.pending_fills.len() > 500 {
                        self.pending_fills.remove(0);
                    }
                    self.pending_fills.len()
                };
                let ghost = first_seen.then(|| {
                    let (key, token_id, side, size, price) = ghost_src.unwrap();
                    tracing::warn!(
                        trade = %key,
                        token = %token_id,
                        side = ?side,
                        size = %size,
                        price = %price,
                        pending = pending_len,
                        "ghost_fill — fill report resolved to NO order of ours; buffered and alerted (issue 691)"
                    );
                    GhostFill {
                        trade_id: key,
                        token_id,
                        side,
                        size,
                        price,
                    }
                });
                return Ok(FillOutcome {
                    ghost_fill: ghost,
                    ..FillOutcome::default()
                });
            }
        };
        fill.order_id = order_id.clone();
        let trade_key = fill
            .trade_id
            .clone()
            .or_else(|| fill.tx_hash.clone())
            .unwrap_or_else(|| format!("{}:{}:{}", order_id, fill.size, fill.price));

        // FAILED: roll back anything provisionally applied under this trade.
        if fill.status == FillStatus::Failed {
            let prev = match self.applied.get(&trade_key) {
                Some(a) if a.applied > Decimal::ZERO => a.clone(),
                _ => return Ok(FillOutcome::default()),
            };
            let (mut delta, late) =
                self.record_delta(&order_id, -prev.applied, prev.price, now_ms, fill.maker)?;
            // F4: the rollback delta carries the key of the provisional fill it
            // reverses, so the caller can pair the reversal with the close
            // bookkeeping that fill already drove.
            if let Some(d) = delta.as_mut() {
                d.trade_id = trade_key.clone();
            }
            self.remember(trade_key.clone(), &order_id, Decimal::ZERO, prev.price);
            return Ok(FillOutcome {
                delta,
                late_fill: late.map(|mut l| {
                    l.trade_id = trade_key;
                    l
                }),
                ghost_fill: None,
            });
        }

        let reported = fill.size.max(Decimal::ZERO);
        let (prev_applied, prev_price) = self
            .applied
            .get(&trade_key)
            .map(|a| (a.applied, a.price))
            .unwrap_or((Decimal::ZERO, fill.price));
        let delta_raw = reported - prev_applied;
        if delta_raw <= Decimal::ZERO {
            // Status progression for an already-seen trade (MATCHED→CONFIRMED):
            // no new size, and a downward revision is ignored rather than booked
            // as a negative delta (the venue re-reporting a smaller cumulative is
            // a correction, not a trade).
            self.remember(trade_key, &order_id, prev_applied.max(reported), prev_price);
            return Ok(FillOutcome::default());
        }

        let (mut delta, late) =
            self.record_delta(&order_id, delta_raw, fill.price, now_ms, fill.maker)?;
        // F4: stamp the producing trade's ledger key on the delta so the caller
        // can pair it with the trade it came from.
        if let Some(d) = delta.as_mut() {
            d.trade_id = trade_key.clone();
        }
        self.remember(trade_key.clone(), &order_id, reported, fill.price);
        Ok(FillOutcome {
            delta,
            late_fill: late.map(|mut l| {
                l.trade_id = trade_key;
                l
            }),
            ghost_fill: None,
        })
    }

    /// Apply a signed size delta to an order, capping cumulative at order size.
    ///
    /// `reported_maker` is the venue's own maker/taker report for this execution
    /// when it has one (E17-b). It is preferred over the order's fill policy,
    /// because the policy is only what we ASKED for: a `MakerThenTaker` order
    /// becomes a taker when it escalates, and a policy cannot describe a fill
    /// that already happened. `None` falls back to the policy.
    ///
    /// Returns the delta to project plus, when the fill could not move the order
    /// the way it normally would, the [`LateFill`] describing that.
    #[allow(clippy::type_complexity)]
    fn record_delta(
        &mut self,
        id: &str,
        signed_delta: Decimal,
        price: Decimal,
        now_ms: i64,
        reported_maker: Option<bool>,
    ) -> CoreResult<(Option<FillDelta>, Option<LateFill>)> {
        let order = self.orders.get_mut(id).ok_or_else(|| {
            CoreError::new(CoreErrorCode::UnknownOrder, format!("unknown order: {id}"))
        })?;
        let was_live = order.status.is_live();

        let prev = order.filled_size;
        let requested = (prev + signed_delta).max(Decimal::ZERO);
        let capped = if order.size > Decimal::ZERO {
            requested.min(order.size)
        } else {
            requested
        };
        let effective = capped - prev;
        let overfill = requested > capped;
        // A fill for an order that already ended: the size is real, the status is
        // NOT ours to rewrite. Absorb it (issue #178).
        let absorbed_by = order.status.is_terminal().then_some(order.status);
        let order_size = order.size;
        order.updated_at_ms = now_ms;
        if effective == Decimal::ZERO {
            // Nothing moved. A venue that still reports more than the order could
            // hold is worth saying out loud even when the cap swallows it whole.
            let late = (overfill && signed_delta > Decimal::ZERO).then(|| LateFill {
                order_id: id.to_string(),
                trade_id: String::new(),
                absorbed_by,
                size: Decimal::ZERO,
                cumulative: order.filled_size,
                reported_cumulative: requested,
                order_size,
                overfill,
            });
            return Ok((None, late));
        }

        // Weighted average only for positive fills.
        if effective > Decimal::ZERO {
            let new_avg = if capped > Decimal::ZERO {
                let cur = order
                    .avg_fill_price
                    .unwrap_or(Decimal::ZERO)
                    .checked_mul(prev);
                let weighted = price.checked_mul(effective);
                match (cur, weighted) {
                    (Some(cur), Some(weighted)) => cur
                        .checked_add(weighted)
                        .and_then(|total| total.checked_div(capped)),
                    _ => None,
                }
            } else {
                Some(price)
            };
            match new_avg {
                Some(avg) => order.avg_fill_price = Some(avg),
                // #779: an out-of-band fill price can overflow the weighted
                // average. SKIP the derived average (the previous one stands)
                // — it is a report value; the fill itself (filled_size, status)
                // and the reservation release downstream must still land, so
                // this is the one term that can safely be skipped.
                None => tracing::error!(
                    order = %id,
                    price = %price,
                    effective = %effective,
                    prev = %prev,
                    capped = %capped,
                    "fill average price overflow — average left unchanged (#779)"
                ),
            }
        }
        order.filled_size = capped;
        if absorbed_by.is_none() {
            order.status = if capped >= order.size {
                OrderStatus::Filled
            } else if capped > Decimal::ZERO {
                OrderStatus::PartiallyFilled
            } else {
                order.status
            };
        } else {
            tracing::warn!(
                order = %id,
                status = ?absorbed_by,
                filled = %capped,
                size = %order_size,
                overfill,
                "late fill absorbed by a terminal order status (not resurrected)"
            );
        }
        let status_after = order.status;

        // Resolve the role from what this fill actually did, then fold it into
        // the order's history (E17-b). The venue's report wins when it made one;
        // otherwise the order's own policy is the only evidence available.
        let role = match reported_maker {
            Some(true) => OrderRole::Maker,
            Some(false) => OrderRole::Taker,
            None => OrderRole::from_fill_policy(order.mode),
        };
        if effective > Decimal::ZERO {
            order.role = order.role.after_fill(role);
        }

        let late = (absorbed_by.is_some() || overfill).then(|| LateFill {
            order_id: id.to_string(),
            trade_id: String::new(),
            absorbed_by,
            size: effective,
            cumulative: capped,
            reported_cumulative: requested,
            order_size,
            overfill,
        });
        let delta = FillDelta {
            order_id: id.to_string(),
            token_id: order.token_id.clone(),
            account_id: order.account_id.clone(),
            side: order.side,
            delta: effective,
            price,
            cumulative: capped,
            strategy: order.strategy.clone(),
            asset: order.asset.clone(),
            direction: order.direction.clone(),
            condition_id: order.condition_id.clone(),
            round_slot: order.round_slot,
            mode: order.mode,
            role,
            limit_price: order.price,
            // The caller (apply_fill) stamps the producing trade's ledger key
            // right after this returns — record_delta has no trade identity.
            trade_id: String::new(),
        };
        debug_assert!(status_after == order.status);
        self.sync_live_index(id, was_live);
        Ok((Some(delta), late))
    }

    /// Retry buffered fills whose order has since registered. Expired ones drop.
    pub fn drain_pending(&mut self, now_ms: i64) -> CoreResult<Vec<FillOutcome>> {
        let pending = std::mem::take(&mut self.pending_fills);
        let mut out = Vec::new();
        for pf in pending {
            if now_ms - pf.at_ms > self.pending_ttl_ms {
                continue;
            }
            // Only dispatch when resolvable; otherwise retain (apply_fill would
            // otherwise re-buffer it and we'd double-count).
            if self.resolve(&pf.fill).is_some() {
                let outcome = self.apply_fill_report(pf.fill, now_ms)?;
                if outcome.delta.is_some() || outcome.late_fill.is_some() {
                    out.push(outcome);
                }
            } else {
                self.pending_fills.push(pf);
            }
        }
        Ok(out)
    }

    pub fn pending_unknown_count(&self) -> usize {
        self.pending_fills.len()
    }
}

impl Default for Ome {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;

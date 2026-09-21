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
use std::collections::HashMap;

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
    /// Maker→taker escalation window (ms); 0 = no escalation.
    pub maker_timeout_ms: i64,
}

pub struct Ome {
    orders: HashMap<OrderId, TrackedOrder>,
    by_internal: HashMap<String, OrderId>,
    applied: HashMap<String, AppliedFill>,
    /// Mutations of `applied`, in application order; the caller drains this and
    /// persists it. Empty on a core that never persists (the OME does no I/O).
    applied_journal: Vec<AppliedFillRecord>,
    applied_seq: u64,
    pending_fills: Vec<PendingFill>,
    pending_ttl_ms: i64,
}

impl Ome {
    pub fn new() -> Self {
        Self {
            orders: HashMap::new(),
            by_internal: HashMap::new(),
            applied: HashMap::new(),
            applied_journal: Vec::new(),
            applied_seq: 0,
            pending_fills: Vec::new(),
            pending_ttl_ms: 30_000,
        }
    }

    pub fn get(&self, id: &str) -> Option<&TrackedOrder> {
        self.orders.get(id)
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
            self.orders.insert(o.order_id.clone(), o);
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

    /// Venue order ids the OME currently believes are resting (for orphan
    /// detection: any venue-open id NOT in this set is unknown to us).
    pub fn known_venue_ids(&self) -> std::collections::HashSet<String> {
        self.orders
            .values()
            .filter(|o| o.status.is_live())
            .filter_map(|o| o.venue_order_id.clone())
            .collect()
    }
    pub fn all(&self) -> Vec<&TrackedOrder> {
        let mut v: Vec<_> = self.orders.values().collect();
        v.sort_by_key(|o| o.submitted_at_ms);
        v
    }
    pub fn live_orders(&self) -> Vec<&TrackedOrder> {
        let mut v: Vec<_> = self
            .orders
            .values()
            .filter(|o| o.status.is_live())
            .collect();
        v.sort_by_key(|o| o.submitted_at_ms);
        v
    }
    pub fn live_for(&self, token_id: &str, side: Side) -> Vec<&TrackedOrder> {
        let mut v: Vec<_> = self
            .orders
            .values()
            .filter(|o| o.status.is_live() && o.token_id == token_id && o.side == side)
            .collect();
        v.sort_by_key(|o| o.submitted_at_ms);
        v
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
            venue_order_id: None,
            escalate_at_ms: None,
            maker_timeout_ms: p.maker_timeout_ms,
            role: OrderRole::Pending,
        };
        self.by_internal
            .insert(r.internal_key.clone(), p.order_id.clone());
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

    /// Bind a core order to its exchange-assigned id after a LIVE POST acks.
    pub fn bind_venue(&mut self, id: &str, venue_order_id: String, now_ms: i64) -> CoreResult<()> {
        self.mutate(id, now_ms, |o| {
            o.venue_order_id = Some(venue_order_id);
            Ok(())
        })
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

    /// Find a live order by venue id, token and side (fallback for user-WS
    /// trade events that only carry taker/maker order ids + token).
    pub fn live_by_venue(&self, venue_id: &str, token: &str, side: Side) -> Option<&TrackedOrder> {
        if let Some(o) = self.by_venue_or_id(venue_id)
            && o.status.is_live()
        {
            return Some(o);
        }
        self.live_for(token, side)
            .into_iter()
            .find(|o| o.venue_order_id.as_deref() == Some(venue_id))
    }

    fn mutate(
        &mut self,
        id: &str,
        now_ms: i64,
        f: impl FnOnce(&mut TrackedOrder) -> CoreResult<()>,
    ) -> CoreResult<()> {
        let o = self.orders.get_mut(id).ok_or_else(|| {
            CoreError::new(CoreErrorCode::UnknownOrder, format!("unknown order: {id}"))
        })?;
        f(o)?;
        o.updated_at_ms = now_ms;
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
            Some(id) => id,
            None => {
                self.pending_fills.push(PendingFill {
                    fill,
                    at_ms: now_ms,
                });
                if self.pending_fills.len() > 500 {
                    self.pending_fills.remove(0);
                }
                return Ok(FillOutcome::default());
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
            let (delta, late) =
                self.record_delta(&order_id, -prev.applied, prev.price, now_ms, fill.maker)?;
            self.remember(trade_key.clone(), &order_id, Decimal::ZERO, prev.price);
            return Ok(FillOutcome {
                delta,
                late_fill: late.map(|mut l| {
                    l.trade_id = trade_key;
                    l
                }),
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

        let (delta, late) =
            self.record_delta(&order_id, delta_raw, fill.price, now_ms, fill.maker)?;
        self.remember(trade_key.clone(), &order_id, reported, fill.price);
        Ok(FillOutcome {
            delta,
            late_fill: late.map(|mut l| {
                l.trade_id = trade_key;
                l
            }),
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
                let cur = order.avg_fill_price.unwrap_or(Decimal::ZERO) * prev;
                (cur + price * effective) / capped
            } else {
                price
            };
            order.avg_fill_price = Some(new_avg);
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
        };
        debug_assert!(status_after == order.status);
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
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn req(internal: &str, size: Decimal) -> OrderRequest {
        OrderRequest {
            token_id: "tok".into(),
            condition_id: "cond".into(),
            side: Side::Buy,
            mode: FillPolicy::Maker,
            price: dec!(0.4),
            size,
            internal_key: internal.into(),
            strategy: "spread_arb".into(),
            asset: "BTC".into(),
            direction: "up".into(),
            round_slot: 7,
        }
    }

    fn fill(
        order_id: &str,
        trade: &str,
        size: Decimal,
        price: Decimal,
        status: FillStatus,
    ) -> Fill {
        Fill {
            order_id: order_id.into(),
            trade_id: Some(trade.into()),
            token_id: "tok".into(),
            side: Side::Buy,
            price,
            size,
            status,
            ts_ms: 1000,
            tx_hash: None,
            // No venue report: the order's own fill policy decides the role.
            maker: None,
        }
    }

    #[test]
    fn cumulative_deltas_idempotent_and_capped() {
        let mut ome = Ome::new();
        ome.submit(SubmitParams {
            order_id: "o1".into(),
            request: req("k1", dec!(10)),
            submitted_at_ms: 1,
            maker_timeout_ms: 0,
        })
        .unwrap();
        ome.mark_live("o1", 2).unwrap();

        // First trade 6 @ 0.40 → +6
        let d1 = ome
            .apply_fill(
                fill("o1", "t1", dec!(6), dec!(0.40), FillStatus::Confirmed),
                3,
            )
            .unwrap()
            .unwrap();
        assert_eq!(d1.delta, dec!(6));
        assert_eq!(d1.price, dec!(0.40));
        assert_eq!(ome.get("o1").unwrap().status, OrderStatus::PartiallyFilled);

        // Duplicate CONFIRMED for same trade → no delta.
        assert!(
            ome.apply_fill(
                fill("o1", "t1", dec!(6), dec!(0.40), FillStatus::Confirmed),
                4
            )
            .unwrap()
            .is_none()
        );

        // Second trade pushes cumulative to 12 but order size caps at 10 → +4.
        let d2 = ome
            .apply_fill(
                fill("o1", "t2", dec!(12), dec!(0.42), FillStatus::Confirmed),
                5,
            )
            .unwrap()
            .unwrap();
        assert_eq!(d2.delta, dec!(4));
        assert_eq!(ome.get("o1").unwrap().filled_size, dec!(10));
        assert_eq!(ome.get("o1").unwrap().status, OrderStatus::Filled);
        // Weighted avg: (0.40*6 + 0.42*4)/10 = 0.408
        assert_eq!(ome.get("o1").unwrap().avg_fill_price, Some(dec!(0.408)));
    }

    #[test]
    fn failed_fill_rolls_back_provisional() {
        let mut ome = Ome::new();
        ome.submit(SubmitParams {
            order_id: "o1".into(),
            request: req("k1", dec!(10)),
            submitted_at_ms: 1,
            maker_timeout_ms: 0,
        })
        .unwrap();
        // MATCHED provisional +5
        let d = ome
            .apply_fill(fill("o1", "t1", dec!(5), dec!(0.4), FillStatus::Matched), 2)
            .unwrap()
            .unwrap();
        assert_eq!(d.delta, dec!(5));
        assert_eq!(ome.get("o1").unwrap().filled_size, dec!(5));
        // FAILED same trade → -5 rollback
        let r = ome
            .apply_fill(fill("o1", "t1", dec!(5), dec!(0.4), FillStatus::Failed), 3)
            .unwrap()
            .unwrap();
        assert_eq!(r.delta, dec!(-5));
        assert_eq!(ome.get("o1").unwrap().filled_size, dec!(0));
    }

    #[test]
    fn unknown_fill_buffers_then_applies_after_submit() {
        let mut ome = Ome::new();
        // Fill arrives before order known → buffered, no delta.
        assert!(
            ome.apply_fill(
                fill("ghost", "t9", dec!(3), dec!(0.4), FillStatus::Confirmed),
                1
            )
            .unwrap()
            .is_none()
        );
        assert_eq!(ome.pending_unknown_count(), 1);
        // Still unknown on drain → retained.
        assert!(ome.drain_pending(2).unwrap().is_empty());
        assert_eq!(ome.pending_unknown_count(), 1);
        // Order registered (resolution falls back to token+side) → buffered fill applies.
        ome.submit(SubmitParams {
            order_id: "o1".into(),
            request: req("k1", dec!(10)),
            submitted_at_ms: 3,
            maker_timeout_ms: 0,
        })
        .unwrap();
        let drained = ome.drain_pending(4).unwrap();
        assert_eq!(drained.len(), 1);
        let d = drained[0].delta.as_ref().expect("delta projected");
        assert_eq!(d.delta, dec!(3));
        assert_eq!(ome.pending_unknown_count(), 0);
    }

    #[test]
    fn duplicate_internal_key_rejected_while_live() {
        let mut ome = Ome::new();
        ome.submit(SubmitParams {
            order_id: "o1".into(),
            request: req("k1", dec!(10)),
            submitted_at_ms: 1,
            maker_timeout_ms: 0,
        })
        .unwrap();
        assert!(
            ome.submit(SubmitParams {
                order_id: "o2".into(),
                request: req("k1", dec!(10)),
                submitted_at_ms: 2,
                maker_timeout_ms: 0,
            })
            .is_err()
        );
        ome.mark_terminal("o1", OrderStatus::Cancelled, 3).unwrap();
        // Same key reusable once terminal.
        assert!(
            ome.submit(SubmitParams {
                order_id: "o3".into(),
                request: req("k1", dec!(10)),
                submitted_at_ms: 4,
                maker_timeout_ms: 0,
            })
            .is_ok()
        );
    }

    // ── #178: restart idempotency ────────────────────────────────────────────

    /// The acceptance criterion: after a restart (a fresh Ome hydrated from the
    /// persisted journal), re-delivering the same trade id must NOT book a second
    /// fill. Before the fix the table was memory-only, so the same trade was
    /// applied twice and the position doubled.
    #[test]
    fn restored_applied_table_makes_redelivery_a_no_op() {
        let mut first = Ome::new();
        first
            .submit(SubmitParams {
                order_id: "o1".into(),
                request: req("k1", dec!(10)),
                submitted_at_ms: 1,
                maker_timeout_ms: 0,
            })
            .unwrap();
        first.mark_live("o1", 2).unwrap();
        let d = first
            .apply_fill(
                fill("o1", "t1", dec!(4), dec!(0.4), FillStatus::Confirmed),
                3,
            )
            .unwrap()
            .expect("first fill books");
        assert_eq!(d.delta, dec!(4));

        // What the service would write to disk: the drained mutation journal.
        let journal = first.take_applied_journal();
        assert_eq_one_record(&journal, "t1", dec!(4));

        // "Restart": a brand-new OME with the order and the table restored.
        let mut second = Ome::new();
        second.restore(first.all().into_iter().cloned().collect());
        second.restore_applied(first.applied_snapshot());
        assert!(second.already_applied("t1"));

        // The venue re-delivers the same trade (its WS replay / REST sweep).
        let repeat = second
            .apply_fill(
                fill("o1", "t1", dec!(4), dec!(0.4), FillStatus::Confirmed),
                9,
            )
            .unwrap();
        assert!(repeat.is_none(), "re-delivered trade must not book again");
        assert_eq!(second.get("o1").unwrap().filled_size, dec!(4));
        assert_eq!(
            second.get("o1").unwrap().status,
            OrderStatus::PartiallyFilled
        );

        // The journal is also empty: nothing to persist for a duplicate.
        assert!(second.take_applied_journal().is_empty());
    }

    /// A late fill for a trade whose order is gone must not be re-booked onto an
    /// unrelated live order that happens to share its token+side. `resolve` has
    /// that token+side fallback, so without the pre-resolution duplicate check the
    /// shares land on the wrong order.
    #[test]
    fn duplicate_trade_for_an_untracked_order_is_not_rebooked_elsewhere() {
        let mut ome = Ome::new();
        // The old order lives, gets a fill, then is dropped (terminal orders are
        // not restored after a restart).
        ome.submit(SubmitParams {
            order_id: "old".into(),
            request: req("k-old", dec!(10)),
            submitted_at_ms: 1,
            maker_timeout_ms: 0,
        })
        .unwrap();
        ome.mark_live("old", 1).unwrap();
        ome.apply_fill(
            fill("old", "t7", dec!(4), dec!(0.4), FillStatus::Confirmed),
            2,
        )
        .unwrap();
        let table = ome.applied_snapshot();

        let mut restarted = Ome::new();
        restarted.restore_applied(table);
        // A fresh live order on the same token+side.
        restarted
            .submit(SubmitParams {
                order_id: "new".into(),
                request: req("k-new", dec!(10)),
                submitted_at_ms: 5,
                maker_timeout_ms: 0,
            })
            .unwrap();
        restarted.mark_live("new", 5).unwrap();
        // The venue re-delivers t7 (addressed to the old venue id).
        let out = restarted
            .apply_fill(
                fill("old", "t7", dec!(4), dec!(0.4), FillStatus::Confirmed),
                6,
            )
            .unwrap();
        assert!(out.is_none());
        assert_eq!(
            restarted.get("new").unwrap().filled_size,
            Decimal::ZERO,
            "the duplicate must not be booked onto the new order"
        );
        assert_eq!(restarted.pending_unknown_count(), 0);
    }

    // ── #178: terminal absorption ────────────────────────────────────────────

    /// The acceptance criterion: a Cancelled order that receives a late partial
    /// fill keeps the terminal status, accrues the size correctly, and reports the
    /// late fill. Before the fix the status was recomputed unconditionally, so the
    /// order "came back to life" as PartiallyFilled.
    #[test]
    fn cancelled_order_absorbs_a_late_fill_and_stays_terminal() {
        let mut ome = Ome::new();
        ome.submit(SubmitParams {
            order_id: "o1".into(),
            request: req("k1", dec!(10)),
            submitted_at_ms: 1,
            maker_timeout_ms: 0,
        })
        .unwrap();
        ome.mark_live("o1", 1).unwrap();
        ome.apply_fill(
            fill("o1", "t1", dec!(2), dec!(0.4), FillStatus::Confirmed),
            2,
        )
        .unwrap();
        ome.mark_terminal("o1", OrderStatus::Cancelled, 3).unwrap();

        let out = ome
            .apply_fill_report(
                fill("o1", "t2", dec!(3), dec!(0.4), FillStatus::Confirmed),
                4,
            )
            .unwrap();
        let d = out.delta.expect("the late fill still moves size");
        assert_eq!(d.delta, dec!(3));
        assert_eq!(d.cumulative, dec!(5));
        let late = out.late_fill.expect("a late fill is reported");
        assert_eq!(late.absorbed_by, Some(OrderStatus::Cancelled));
        assert_eq!(late.trade_id, "t2");
        assert_eq!(late.size, dec!(3));
        assert!(!late.overfill);
        let o = ome.get("o1").unwrap();
        assert_eq!(o.status, OrderStatus::Cancelled, "still terminal");
        assert_eq!(o.filled_size, dec!(5), "size is still accounted for");
        // And the size is still idempotent: a replay of t2 books nothing.
        assert!(
            ome.apply_fill(
                fill("o1", "t2", dec!(3), dec!(0.4), FillStatus::Confirmed),
                5
            )
            .unwrap()
            .is_none()
        );
        assert_eq!(ome.get("o1").unwrap().filled_size, dec!(5));
    }

    /// A venue that reports more than the order could hold is capped today (that
    /// part is correct) but was silently swallowed. It must surface as a LateFill
    /// with `overfill` set — the operator needs to know the venue thinks more
    /// shares exist than we asked for.
    #[test]
    fn over_reported_fill_is_capped_and_surfaced() {
        let mut ome = Ome::new();
        ome.submit(SubmitParams {
            order_id: "o1".into(),
            request: req("k1", dec!(10)),
            submitted_at_ms: 1,
            maker_timeout_ms: 0,
        })
        .unwrap();
        ome.mark_live("o1", 1).unwrap();
        let out = ome
            .apply_fill_report(
                fill("o1", "t1", dec!(12), dec!(0.4), FillStatus::Confirmed),
                2,
            )
            .unwrap();
        let d = out.delta.unwrap();
        assert_eq!(d.delta, dec!(10), "capped at the order size");
        let late = out.late_fill.expect("the over-report is surfaced");
        assert!(late.overfill);
        assert_eq!(late.reported_cumulative, dec!(12));
        assert_eq!(late.cumulative, dec!(10));
        assert_eq!(late.order_size, dec!(10));
        assert_eq!(late.absorbed_by, None, "the order was still live");
        assert_eq!(ome.get("o1").unwrap().status, OrderStatus::Filled);
        // A further over-report on the now-Filled order: no size left to move,
        // still surfaced (and absorbed rather than resurrected).
        let again = ome
            .apply_fill_report(
                fill("o1", "t2", dec!(1), dec!(0.4), FillStatus::Confirmed),
                3,
            )
            .unwrap();
        assert!(again.delta.is_none());
        let late = again.late_fill.expect("still surfaced");
        assert!(late.overfill);
        assert_eq!(late.absorbed_by, Some(OrderStatus::Filled));
        assert_eq!(ome.get("o1").unwrap().status, OrderStatus::Filled);
    }

    /// A rollback (FAILED) on a terminal order must not be booked as a fresh
    /// partial fill either: the cash/shares reverse, the status stays terminal.
    #[test]
    fn cancelled_order_absorbs_a_late_rollback() {
        let mut ome = Ome::new();
        ome.submit(SubmitParams {
            order_id: "o1".into(),
            request: req("k1", dec!(10)),
            submitted_at_ms: 1,
            maker_timeout_ms: 0,
        })
        .unwrap();
        ome.mark_live("o1", 1).unwrap();
        ome.apply_fill(fill("o1", "t1", dec!(4), dec!(0.4), FillStatus::Matched), 2)
            .unwrap();
        ome.mark_terminal("o1", OrderStatus::Rejected, 3).unwrap();
        let out = ome
            .apply_fill_report(fill("o1", "t1", dec!(4), dec!(0.4), FillStatus::Failed), 4)
            .unwrap();
        assert_eq!(out.delta.unwrap().delta, dec!(-4));
        assert_eq!(ome.get("o1").unwrap().filled_size, Decimal::ZERO);
        assert_eq!(ome.get("o1").unwrap().status, OrderStatus::Rejected);
        assert_eq!(
            out.late_fill.unwrap().absorbed_by,
            Some(OrderStatus::Rejected)
        );
    }

    // ── #178: explicit LRU eviction ──────────────────────────────────────────

    /// The acceptance criterion: after 5000+ evictions the EARLIEST ACTIVE trade
    /// id is still idempotent. The old eviction used `HashMap::keys().next()` —
    /// an arbitrary key — so it could drop the live order's entry and let its fill
    /// be booked twice.
    #[test]
    fn eviction_is_lru_over_terminal_orders_and_never_drops_a_live_entry() {
        let mut ome = Ome::new();
        // One long-lived live order whose trade key is the OLDEST entry in the
        // table: exactly the entry the arbitrary-key eviction used to eat.
        ome.submit(SubmitParams {
            order_id: "live".into(),
            request: req("k-live", dec!(1000)),
            submitted_at_ms: 1,
            maker_timeout_ms: 0,
        })
        .unwrap();
        ome.mark_live("live", 1).unwrap();
        ome.apply_fill(
            fill("live", "t-live", dec!(1), dec!(0.4), FillStatus::Confirmed),
            2,
        )
        .unwrap();

        // 5001 more trades, each on its own short-lived order that ends terminal.
        for i in 0..5001 {
            let id = format!("o{i}");
            ome.submit(SubmitParams {
                order_id: id.clone(),
                request: req(&format!("k{i}"), dec!(1)),
                submitted_at_ms: 10,
                maker_timeout_ms: 0,
            })
            .unwrap();
            ome.mark_live(&id, 10).unwrap();
            ome.apply_fill(
                fill(
                    &id,
                    &format!("t{i}"),
                    dec!(1),
                    dec!(0.4),
                    FillStatus::Confirmed,
                ),
                10,
            )
            .unwrap();
            ome.mark_terminal(&id, OrderStatus::Filled, 10).unwrap();
        }

        assert!(ome.applied_len() <= APPLIED_CAP + 1);
        assert!(
            ome.already_applied("t-live"),
            "the live order's key must survive eviction"
        );
        // And it really is idempotent: a replay books nothing.
        assert!(
            ome.apply_fill(
                fill("live", "t-live", dec!(1), dec!(0.4), FillStatus::Confirmed),
                11
            )
            .unwrap()
            .is_none()
        );
        assert_eq!(ome.get("live").unwrap().filled_size, dec!(1));
        // The oldest terminal-order keys are the ones that went.
        assert!(!ome.already_applied("t0"));
    }

    /// A rollback to zero keeps the key (with size 0) so the trade can still be
    /// re-applied if the venue later confirms it — the table must not treat a
    /// zeroed key as "never seen".
    #[test]
    fn rolled_back_key_is_retained_but_zeroed() {
        let mut ome = Ome::new();
        ome.submit(SubmitParams {
            order_id: "o1".into(),
            request: req("k1", dec!(10)),
            submitted_at_ms: 1,
            maker_timeout_ms: 0,
        })
        .unwrap();
        ome.mark_live("o1", 1).unwrap();
        ome.apply_fill(fill("o1", "t1", dec!(5), dec!(0.4), FillStatus::Matched), 2)
            .unwrap();
        ome.apply_fill(fill("o1", "t1", dec!(5), dec!(0.4), FillStatus::Failed), 3)
            .unwrap();
        assert!(!ome.already_applied("t1"));
        // A later CONFIRMED for the same id is a fresh application, not a no-op.
        let again = ome
            .apply_fill(
                fill("o1", "t1", dec!(5), dec!(0.4), FillStatus::Confirmed),
                4,
            )
            .unwrap();
        assert_eq!(again.unwrap().delta, dec!(5));
        assert_eq!(ome.get("o1").unwrap().filled_size, dec!(5));
    }

    /// `size` is total, `filled_size` cumulative: the remaining size a cancel
    /// retires is their difference.
    #[test]
    fn remaining_size_is_size_minus_filled() {
        let mut ome = Ome::new();
        ome.submit(SubmitParams {
            order_id: "o1".into(),
            request: req("k1", dec!(10)),
            submitted_at_ms: 1,
            maker_timeout_ms: 0,
        })
        .unwrap();
        ome.mark_live("o1", 1).unwrap();
        assert_eq!(ome.remaining("o1"), dec!(10));
        ome.apply_fill(
            fill("o1", "t1", dec!(4), dec!(0.4), FillStatus::Confirmed),
            2,
        )
        .unwrap();
        assert_eq!(ome.remaining("o1"), dec!(6));
        assert_eq!(ome.get("o1").unwrap().size, dec!(10), "size is unchanged");
        assert_eq!(ome.remaining("ghost"), Decimal::ZERO);
    }

    /// A restored journal is folded in file order: later records win, tombstones
    /// remove, and the restored table evicts in the same order the writer would.
    #[test]
    fn journal_folds_later_records_and_tombstones() {
        let mut ome = Ome::new();
        ome.restore_applied(vec![
            AppliedFillRecord {
                trade_key: "t1".into(),
                order_id: "o1".into(),
                applied: dec!(2),
                price: dec!(0.4),
                seq: 1,
                evicted: false,
            },
            // The venue later confirmed t1 at a larger size: later wins.
            AppliedFillRecord {
                trade_key: "t1".into(),
                order_id: "o1".into(),
                applied: dec!(5),
                price: dec!(0.4),
                seq: 2,
                evicted: false,
            },
            AppliedFillRecord {
                trade_key: "t2".into(),
                order_id: "o2".into(),
                applied: dec!(1),
                price: dec!(0.5),
                seq: 3,
                evicted: false,
            },
            // ...and t2 was evicted afterwards.
            AppliedFillRecord {
                trade_key: "t2".into(),
                order_id: "o2".into(),
                applied: dec!(1),
                price: dec!(0.5),
                seq: 3,
                evicted: true,
            },
        ]);
        assert!(ome.already_applied("t1"));
        assert!(!ome.already_applied("t2"));
        assert_eq!(ome.applied_len(), 1);
        let snap = ome.applied_snapshot();
        assert_eq!(snap[0].trade_key, "t1");
        assert_eq!(snap[0].applied, dec!(5));
        assert!(snap[0].seq > 0, "restored entries get explicit sequences");
    }

    fn assert_eq_one_record(journal: &[AppliedFillRecord], key: &str, applied: Decimal) {
        assert_eq!(journal.len(), 1, "one mutation journalled per fill");
        assert_eq!(journal[0].trade_key, key);
        assert_eq!(journal[0].applied, applied);
        assert!(!journal[0].evicted);
        assert!(journal[0].seq > 0);
    }
}

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

use crate::model::*;
use rust_decimal::Decimal;
use std::collections::HashMap;

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
}

#[derive(Debug, Clone)]
struct AppliedFill {
    applied: Decimal,
    price: Decimal,
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
}

pub struct Ome {
    orders: HashMap<OrderId, TrackedOrder>,
    by_internal: HashMap<String, OrderId>,
    applied: HashMap<String, AppliedFill>,
    pending_fills: Vec<PendingFill>,
    pending_ttl_ms: i64,
}

impl Ome {
    pub fn new() -> Self {
        Self {
            orders: HashMap::new(),
            by_internal: HashMap::new(),
            applied: HashMap::new(),
            pending_fills: Vec::new(),
            pending_ttl_ms: 30_000,
        }
    }

    pub fn get(&self, id: &str) -> Option<&TrackedOrder> {
        self.orders.get(id)
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
        self.all()
            .into_iter()
            .filter(|o| o.status.is_live())
            .collect()
    }
    pub fn live_for(&self, token_id: &str, side: Side) -> Vec<&TrackedOrder> {
        self.live_orders()
            .into_iter()
            .filter(|o| o.token_id == token_id && o.side == side)
            .collect()
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
        if let Some(o) = self.by_venue_or_id(venue_id) {
            if o.status.is_live() {
                return Some(o);
            }
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
    pub fn apply_fill(&mut self, mut fill: Fill, now_ms: i64) -> CoreResult<Option<FillDelta>> {
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
                return Ok(None);
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
                _ => return Ok(None),
            };
            let delta =
                self.record_delta(&order_id, -prev.applied, prev.price, now_ms, fill.maker)?;
            self.applied.insert(
                trade_key,
                AppliedFill {
                    applied: Decimal::ZERO,
                    price: prev.price,
                },
            );
            return Ok(delta);
        }

        let reported = fill.size.max(Decimal::ZERO);
        let (prev_applied, prev_price) = self
            .applied
            .get(&trade_key)
            .map(|a| (a.applied, a.price))
            .unwrap_or((Decimal::ZERO, fill.price));
        let delta_raw = reported - prev_applied;
        if delta_raw <= Decimal::ZERO {
            // Status progression for an already-seen trade (MATCHED→CONFIRMED): no delta.
            self.applied.insert(
                trade_key.clone(),
                AppliedFill {
                    applied: prev_applied.max(reported),
                    price: prev_price,
                },
            );
            return Ok(None);
        }

        let delta = self.record_delta(&order_id, delta_raw, fill.price, now_ms, fill.maker)?;
        self.applied.insert(
            trade_key.clone(),
            AppliedFill {
                applied: reported,
                price: fill.price,
            },
        );
        if self.applied.len() > 5000 {
            if let Some(oldest) = self.applied.keys().next().cloned() {
                self.applied.remove(&oldest);
            }
        }
        Ok(delta)
    }

    /// Apply a signed size delta to an order, capping cumulative at order size.
    ///
    /// `reported_maker` is the venue's own maker/taker report for this execution
    /// when it has one (E17-b). It is preferred over the order's fill policy,
    /// because the policy is only what we ASKED for: a `MakerThenTaker` order
    /// becomes a taker when it escalates, and a policy cannot describe a fill
    /// that already happened. `None` falls back to the policy.
    fn record_delta(
        &mut self,
        id: &str,
        signed_delta: Decimal,
        price: Decimal,
        now_ms: i64,
        reported_maker: Option<bool>,
    ) -> CoreResult<Option<FillDelta>> {
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
        order.updated_at_ms = now_ms;
        if effective == Decimal::ZERO {
            return Ok(None);
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
        order.status = if capped >= order.size {
            OrderStatus::Filled
        } else if capped > Decimal::ZERO {
            OrderStatus::PartiallyFilled
        } else {
            order.status
        };

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

        Ok(Some(FillDelta {
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
        }))
    }

    /// Retry buffered fills whose order has since registered. Expired ones drop.
    pub fn drain_pending(&mut self, now_ms: i64) -> CoreResult<Vec<FillDelta>> {
        let pending = std::mem::take(&mut self.pending_fills);
        let mut out = Vec::new();
        for pf in pending {
            if now_ms - pf.at_ms > self.pending_ttl_ms {
                continue;
            }
            // Only dispatch when resolvable; otherwise retain (apply_fill would
            // otherwise re-buffer it and we'd double-count).
            if self.resolve(&pf.fill).is_some() {
                if let Some(d) = self.apply_fill(pf.fill, now_ms)? {
                    out.push(d);
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
        })
        .unwrap();
        let drained = ome.drain_pending(4).unwrap();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].delta, dec!(3));
        assert_eq!(ome.pending_unknown_count(), 0);
    }

    #[test]
    fn duplicate_internal_key_rejected_while_live() {
        let mut ome = Ome::new();
        ome.submit(SubmitParams {
            order_id: "o1".into(),
            request: req("k1", dec!(10)),
            submitted_at_ms: 1,
        })
        .unwrap();
        assert!(
            ome.submit(SubmitParams {
                order_id: "o2".into(),
                request: req("k1", dec!(10)),
                submitted_at_ms: 2
            })
            .is_err()
        );
        ome.mark_terminal("o1", OrderStatus::Cancelled, 3).unwrap();
        // Same key reusable once terminal.
        assert!(
            ome.submit(SubmitParams {
                order_id: "o3".into(),
                request: req("k1", dec!(10)),
                submitted_at_ms: 4
            })
            .is_ok()
        );
    }
}

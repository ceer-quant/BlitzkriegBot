//! Reconciliation — compare the local OME against an authoritative exchange
//! snapshot (current open orders + recent trades) and repair drift.
//!
//! Pure logic over snapshots so it is unit-testable and identical for the
//! live REST sweep and a replayed/offline source. It handles:
//!
//!  - missed fills: a trade exists at the venue for our order but the OME never
//!    applied it (WS gap) → synthesise an authoritative cumulative Fill
//!  - ghost orders: local thinks an order is LIVE but the venue reports it gone
//!    AND it has a fill that completed it → mark Filled; otherwise it was
//!    cancelled away → mark Cancelled. We never invent a resting order.
//!
//! Caller is responsible for fetching the snapshot and executing any venue
//! cancel for true ghosts; this module returns explicit actions.

use crate::model::*;
use crate::ome::{FillDelta, Ome};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct VenueTrade {
    pub venue_order_id: String,
    pub trade_id: String,
    pub token_id: TokenId,
    pub side: Side,
    /// Per-trade executed size (NOT cumulative).
    pub size: rust_decimal::Decimal,
    pub price: rust_decimal::Decimal,
    pub ts_ms: i64,
    pub tx_hash: Option<String>,
    /// The venue's maker/taker report for this trade, when it has one. A gap
    /// fill synthesized from it must charge the fee the venue actually charged,
    /// so the role travels with the trade rather than being re-guessed (E17).
    pub maker: Option<bool>,
}

#[derive(Debug, Clone, Default)]
pub struct VenueSnapshot {
    /// Exchange order ids currently resting/open.
    pub open_order_ids: Vec<String>,
    /// Recent trades for our orders (any status); used to fill gaps.
    pub trades: Vec<VenueTrade>,
    pub now_ms: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ReconcileAction {
    /// A fill missing from the local OME was applied (WS gap repaired).
    FilledGap {
        core_order_id: OrderId,
        delta: FillDelta,
    },
    /// Local-live order is fully filled per the venue (no longer open).
    MarkedFilled { core_order_id: OrderId },
    /// Local-live order vanished from the venue with no fill → marked cancelled.
    MarkedCancelled { core_order_id: OrderId },
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReconcileReport {
    pub actions: Vec<ReconcileAction>,
    /// Local-live orders with no venue presence and no completing fill; a real
    /// ghost that may warrant a cancel if the venue snapshot is trustworthy.
    pub suspect_ghost_ids: Vec<OrderId>,
}

/// Reconcile the OME against an authoritative snapshot.
///
/// `ome.apply_fill` stays the only mutator of fill state. Status repair for
/// ghosts is done through mark_terminal so the state machine stays consistent.
pub fn reconcile(ome: &mut Ome, snap: &VenueSnapshot) -> CoreResult<ReconcileReport> {
    let mut report = ReconcileReport::default();
    let open: std::collections::HashSet<&str> =
        snap.open_order_ids.iter().map(String::as_str).collect();

    // 1) Repair missed fills. Aggregate per order so a cumulative Fill reflects
    //    everything the venue saw, then push through the idempotent ledger.
    let mut by_order: HashMap<String, (rust_decimal::Decimal, rust_decimal::Decimal, &VenueTrade)> =
        HashMap::new();
    for t in &snap.trades {
        let entry = by_order
            .entry(t.venue_order_id.clone())
            .or_insert_with(|| (rust_decimal::Decimal::ZERO, rust_decimal::Decimal::ZERO, t));
        entry.0 += t.size;
        // volume-weighted price accumulator (numerator)
        entry.1 += t.price * t.size;
    }

    for (venue_id, (total_size, price_num, last)) in by_order {
        let Some(order) = ome.by_venue_or_id(&venue_id).cloned() else {
            continue;
        };
        let core_id = order.order_id.clone();
        // Only repair if the venue reports more filled size than the OME knows.
        if total_size <= order.filled_size {
            continue;
        }
        let avg_price = if total_size > rust_decimal::Decimal::ZERO {
            price_num / total_size
        } else {
            last.price
        };
        let fill = Fill {
            order_id: core_id.clone(),
            trade_id: Some(format!("recon:{}:{}", core_id, snap.now_ms)),
            token_id: order.token_id.clone(),
            side: order.side,
            price: avg_price,
            size: total_size, // cumulative
            status: FillStatus::Confirmed,
            ts_ms: last.ts_ms,
            tx_hash: last.tx_hash.clone(),
            // Carry the venue's role through the synthesized gap fill.
            maker: last.maker,
        };
        if let Some(delta) = ome.apply_fill(fill, snap.now_ms)? {
            report.actions.push(ReconcileAction::FilledGap {
                core_order_id: core_id.clone(),
                delta,
            });
        }
    }

    // 2) Ghost detection over orders we still consider live. Snapshot the
    //    candidate ids first so the OME is not borrowed while we mutate it.
    let candidates: Vec<(OrderId, Option<OrderId>)> = ome
        .live_orders()
        .into_iter()
        .map(|o| (o.order_id.clone(), o.venue_order_id.clone()))
        .collect();

    for (core_id, venue_id) in candidates {
        let Some(venue_id) = venue_id else {
            // Dry orders have no venue id; never touch them from a venue snapshot.
            continue;
        };
        if open.contains(venue_id.as_str()) {
            continue; // venue still has it open — fine
        }
        // Gone from the venue. Refresh: a fill may have completed it in step 1.
        let fresh_status = ome.get(&core_id).map(|o| (o.status, o.filled_size, o.size));
        match fresh_status {
            Some((status, filled, size)) if status == OrderStatus::Filled || filled >= size => {
                if status != OrderStatus::Filled {
                    ome.mark_terminal(&core_id, OrderStatus::Filled, snap.now_ms)?;
                }
                report.actions.push(ReconcileAction::MarkedFilled {
                    core_order_id: core_id,
                });
            }
            Some((_, filled, _)) if filled > rust_decimal::Decimal::ZERO => {
                ome.mark_terminal(&core_id, OrderStatus::Cancelled, snap.now_ms)?;
                report.actions.push(ReconcileAction::MarkedCancelled {
                    core_order_id: core_id,
                });
            }
            _ => {
                ome.mark_terminal(&core_id, OrderStatus::Cancelled, snap.now_ms)?;
                report.suspect_ghost_ids.push(core_id.clone());
                report.actions.push(ReconcileAction::MarkedCancelled {
                    core_order_id: core_id,
                });
            }
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ome::SubmitParams;
    use rust_decimal_macros::dec;

    fn req(key: &str, size: rust_decimal::Decimal) -> OrderRequest {
        OrderRequest {
            token_id: "tok".into(),
            condition_id: "cond".into(),
            side: Side::Buy,
            mode: FillPolicy::Taker,
            price: dec!(0.4),
            size,
            internal_key: key.into(),
            strategy: "s".into(),
            asset: "BTC".into(),
            direction: "up".into(),
            round_slot: 1,
        }
    }

    fn live_order(ome: &mut Ome, key: &str, core_id: &str, venue_id: &str, now: i64) {
        ome.submit(SubmitParams {
            order_id: core_id.into(),
            request: req(key, dec!(10)),
            submitted_at_ms: now,
        })
        .unwrap();
        ome.bind_venue(core_id, venue_id.into(), now).unwrap();
        ome.mark_live(core_id, now).unwrap();
    }

    #[test]
    fn repairs_missed_fill_from_rest_trade() {
        let mut ome = Ome::new();
        live_order(&mut ome, "k1", "c1", "v1", 1);
        // WS gap: no fill applied locally. Venue reports a full 10 filled.
        let snap = VenueSnapshot {
            open_order_ids: vec![],
            trades: vec![VenueTrade {
                venue_order_id: "v1".into(),
                trade_id: "t1".into(),
                token_id: "tok".into(),
                side: Side::Buy,
                size: dec!(10),
                price: dec!(0.4),
                ts_ms: 2,
                tx_hash: None,
                maker: Some(true),
            }],
            now_ms: 3,
        };
        let report = reconcile(&mut ome, &snap).unwrap();
        assert!(
            report
                .actions
                .iter()
                .any(|a| matches!(a, ReconcileAction::FilledGap { .. }))
        );
        assert_eq!(ome.get("c1").unwrap().status, OrderStatus::Filled);
        assert_eq!(ome.get("c1").unwrap().filled_size, dec!(10));
    }

    #[test]
    fn detects_ghost_order_without_fill() {
        let mut ome = Ome::new();
        live_order(&mut ome, "k1", "c1", "v1", 1);
        // Venue no longer lists it open and there are no trades → ghost/cancelled.
        let snap = VenueSnapshot {
            open_order_ids: vec![],
            trades: vec![],
            now_ms: 5,
        };
        let report = reconcile(&mut ome, &snap).unwrap();
        assert_eq!(report.suspect_ghost_ids, vec!["c1".to_string()]);
        assert_eq!(ome.get("c1").unwrap().status, OrderStatus::Cancelled);
    }

    #[test]
    fn leaves_open_orders_untouched_and_is_idempotent() {
        let mut ome = Ome::new();
        live_order(&mut ome, "k1", "c1", "v1", 1);
        let snap = VenueSnapshot {
            open_order_ids: vec!["v1".into()],
            trades: vec![],
            now_ms: 2,
        };
        let r1 = reconcile(&mut ome, &snap).unwrap();
        assert!(r1.actions.is_empty());
        assert_eq!(ome.get("c1").unwrap().status, OrderStatus::Live);
        // Idempotent: still nothing on a second identical sweep.
        let r2 = reconcile(&mut ome, &snap).unwrap();
        assert!(r2.actions.is_empty());
    }

    #[test]
    fn dry_orders_without_venue_id_are_ignored() {
        let mut ome = Ome::new();
        ome.submit(SubmitParams {
            order_id: "dry1".into(),
            request: req("k9", dec!(10)),
            submitted_at_ms: 1,
        })
        .unwrap();
        ome.mark_live("dry1", 1).unwrap();
        let snap = VenueSnapshot {
            open_order_ids: vec![],
            trades: vec![],
            now_ms: 2,
        };
        let report = reconcile(&mut ome, &snap).unwrap();
        assert!(report.actions.is_empty());
        assert_eq!(ome.get("dry1").unwrap().status, OrderStatus::Live);
    }
}

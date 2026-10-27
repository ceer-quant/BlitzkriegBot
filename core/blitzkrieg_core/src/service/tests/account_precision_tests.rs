#![cfg(all(test, feature = "account-precision"))]
//! ── E17: accounting precision ────────────────────────────────────────────────
//!
//! The invariant under test is the version plan's core E17 acceptance criterion:
//! with nothing reserved and no position open,
//!
//! ```text
//!     ledger.balance() == seed + Σ closed.net_pnl_usd
//! ```
//!
//! Every scenario is driven through the REAL fill path (dry matcher, live
//! user-WS ingest, or a reconciliation gap — all three meet at
//! `apply_delta_effects`), so what is asserted is the production arithmetic and
//! not a reimplementation of it in the test.
//!
//! The matrix covers the maker/taker × partial-fill combinations the plan names
//! (1/10, 5/10, 9.99/10, 10/10) on BOTH legs, plus the escalation path that used
//! to produce the 0.0817722 drift. Enabled by the `account-precision` feature,
//! which is on by default so a plain `cargo test` covers it too.

use super::*;
use crate::model::OrderRole;
use rust_decimal_macros::dec;

const SEED: Decimal = dec!(1000);

fn core() -> Core {
    let mut c = Core::new(CoreConfig {
        risk: RiskConfig {
            max_order_notional: dec!(100),
            ..Default::default()
        },
        dry_seed_balance: SEED,
        // Keep the fixture's arithmetic to the position, not the scheduler.
        auto_exits_enabled: false,
        ..Default::default()
    });
    c.set_balance(SEED);
    c
}

fn req(side: Side, mode: FillPolicy, price: Decimal, size: Decimal, key: &str) -> OrderRequest {
    OrderRequest {
        token_id: "tok".into(),
        condition_id: "cond".into(),
        side,
        mode,
        price,
        size,
        internal_key: key.into(),
        strategy: "acc".into(),
        asset: "BTC".into(),
        direction: "up".into(),
        round_slot: 1,
        account_id: crate::model::default_account_id(),
    }
}

fn fill(order_id: &str, trade: &str, side: Side, price: Decimal, size: Decimal) -> Fill {
    Fill {
        order_id: order_id.into(),
        trade_id: Some(trade.into()),
        token_id: "tok".into(),
        side,
        price,
        size,
        status: FillStatus::Confirmed,
        ts_ms: 0,
        tx_hash: None,
        // The harness reports no role, so the OME falls back to the order's
        // fill policy — correct here, because these fills are the ones the
        // policy would produce. A test that needs the VENUE to overrule the
        // policy uses `fill_as` below.
        maker: None,
        account_id: crate::model::default_account_id(),
    }
}

/// A fill carrying the venue's OWN maker/taker report, which outranks the
/// order's fill policy (E17-b).
fn fill_as(
    order_id: &str,
    trade: &str,
    side: Side,
    price: Decimal,
    size: Decimal,
    maker: bool,
) -> Fill {
    Fill {
        maker: Some(maker),
        ..fill(order_id, trade, side, price, size)
    }
}

/// Σ net PnL over the closed book.
fn realized(c: &Core) -> Decimal {
    c.positions()
        .closed_positions()
        .iter()
        .map(|p| p.net_pnl_usd)
        .sum()
}

/// The E17 identity, asserted with the offending numbers in the message so a
/// regression is diagnosable from CI output alone.
fn assert_reconciled(c: &Core, scenario: &str) {
    assert_eq!(
        c.positions().open_positions().len(),
        0,
        "[{scenario}] the scenario must end flat"
    );
    let net = realized(c);
    let detail: Vec<String> = c
        .positions()
        .closed_positions()
        .iter()
        .map(|p| {
            format!(
                "{} entry={} exit={} shares={} basis={} entryFee={} exitFee={} entryRole={:?} exitRole={:?} dust={} gross={} net={}",
                p.id, p.entry_price, p.exit_price, p.shares, p.cost_usd,
                p.entry_fee_pct, p.exit_fee_pct, p.entry_role, p.exit_role,
                p.dust_shares, p.pnl_usd, p.net_pnl_usd
            )
        })
        .collect();
    assert_eq!(
        c.ledger().balance(),
        SEED + net,
        "[{scenario}] balance must equal seed + realized net; closed={detail:?}"
    );
}

/// A BUY that only partly filled still holds the UNFILLED remainder's
/// reservation until the order is cancelled — a real (pre-existing) property
/// of the ledger, since the remainder is still committed on the book. The
/// E17 identity is about cash, so the harness releases the remainder the way
/// the venue would (cancel what will never fill) before asserting.
fn cancel_open_buys(c: &mut Core, now_ms: i64) {
    let residual: Vec<String> = c
        .ome()
        .live_orders()
        .into_iter()
        .filter(|o| o.side == Side::Buy && o.filled_size < o.size)
        .map(|o| o.order_id.clone())
        .collect();
    for id in residual {
        c.cancel(&id, now_ms).unwrap();
    }
}

/// A round trip at an explicit fill shape, driven the way the VENUE drives it:
/// the order is registered pending (no dry auto-fill) and the fills arrive as
/// cumulative reports. The entry delivers `entry_partial` of the requested
/// size; the exit is then sized to what was actually held and delivers
/// `exit_partial` of that, with any remainder closed by a second exit fill —
/// so the position ends flat however the venue rations the fills.
fn dry_round_trip(
    entry_mode: FillPolicy,
    entry_price: Decimal,
    entry_partial: Decimal,
    exit_mode: FillPolicy,
    exit_price: Decimal,
    exit_partial: Decimal,
    size: Decimal,
) -> Core {
    let mut c = core();
    let (id, _) = c
        .place_pending(req(Side::Buy, entry_mode, entry_price, size, "entry"), 1)
        .unwrap();
    c.confirm_live(&id, 1).unwrap();
    c.ingest_fill(fill(&id, "e1", Side::Buy, entry_price, entry_partial), 2)
        .unwrap();

    let held = c.positions().open_positions()[0].shares;
    assert_eq!(
        held, entry_partial,
        "the venue's partial decides the size, not the request"
    );
    // The unfilled part of the entry will never fill; let it go so only cash
    // and the position are compared.
    cancel_open_buys(&mut c, 3);

    let (sid, _) = c
        .place_pending(req(Side::Sell, exit_mode, exit_price, held, "exit"), 4)
        .unwrap();
    c.confirm_live(&sid, 4).unwrap();
    c.ingest_fill(fill(&sid, "x1", Side::Sell, exit_price, exit_partial), 5)
        .unwrap();
    // Any shares the first exit did not cover are closed by a second fill.
    let left: Decimal = c
        .positions()
        .open_positions()
        .iter()
        .map(|p| p.shares)
        .sum();
    if left > Decimal::ZERO {
        let (rid, _) = c
            .place_pending(req(Side::Sell, exit_mode, exit_price, left, "exit-2"), 6)
            .unwrap();
        c.confirm_live(&rid, 6).unwrap();
        c.ingest_fill(fill(&rid, "x2", Side::Sell, exit_price, left), 7)
            .unwrap();
    }
    cancel_open_buys(&mut c, 8);
    c
}

/// The plan's named partials on both legs, across all four fill policies.
#[test]
fn every_maker_taker_partial_shape_reconciles() {
    let shapes: [(&str, Decimal); 4] = [
        ("1/10", dec!(1)),
        ("5/10", dec!(5)),
        ("9.99/10", dec!(9.99)),
        ("10/10", dec!(10)),
    ];
    let mut checked = 0usize;
    for (entry_label, entry_partial) in shapes {
        for (exit_label, exit_partial) in shapes {
            // An exit can never exceed what the entry actually delivered.
            if exit_partial > entry_partial {
                continue;
            }
            for em in [FillPolicy::Maker, FillPolicy::Taker] {
                for xm in [FillPolicy::Maker, FillPolicy::Taker] {
                    let c = dry_round_trip(
                        em,
                        dec!(0.43),
                        entry_partial,
                        xm,
                        dec!(0.95),
                        exit_partial,
                        dec!(10),
                    );
                    assert_reconciled(
                        &c,
                        &format!("entry {entry_label} {em:?} / exit {exit_label} {xm:?}"),
                    );
                    checked += 1;
                }
            }
        }
    }
    assert_eq!(checked, 40, "the whole named matrix must run");
}

/// The VENUE's report outranks the order's fill policy (E17-b). A
/// `MakerThenTaker` order can end up filling as a pure maker (it rested and
/// was hit before the timer) or as a pure taker (it escalated); the policy
/// says nothing about which happened, so the report must win and the fee must
/// follow it — in both directions, so this cannot pass by always picking one.
#[test]
fn the_venue_role_report_outranks_the_orders_policy() {
    // A MakerThenTaker order that the venue says CROSSED → taker fee.
    let mut crossed = core();
    let (id, _) = crossed
        .place_pending(
            req(
                Side::Buy,
                FillPolicy::MakerThenTaker,
                dec!(0.70),
                dec!(10),
                "c",
            ),
            1,
        )
        .unwrap();
    crossed.confirm_live(&id, 1).unwrap();
    crossed
        .ingest_fill(
            fill_as(&id, "c1", Side::Buy, dec!(0.70), dec!(10), false),
            2,
        )
        .unwrap();
    let pos = crossed.positions().open_positions()[0].clone();
    assert_eq!(pos.entry_role, OrderRole::Taker, "the report said taker");
    assert!(
        pos.flows.entry_fee_usd > Decimal::ZERO,
        "a crossing fill pays the taker fee even though the policy rested first"
    );

    // A plain Maker order the venue says CROSSED → also a taker fee. Under
    // policy-only resolution this silently paid nothing.
    let mut surprised = core();
    let (id2, _) = surprised
        .place_pending(
            req(Side::Buy, FillPolicy::Maker, dec!(0.70), dec!(10), "s"),
            1,
        )
        .unwrap();
    surprised.confirm_live(&id2, 1).unwrap();
    surprised
        .ingest_fill(
            fill_as(&id2, "s1", Side::Buy, dec!(0.70), dec!(10), false),
            2,
        )
        .unwrap();
    let pos2 = surprised.positions().open_positions()[0].clone();
    assert_eq!(pos2.entry_role, OrderRole::Taker);
    assert_eq!(
        pos2.flows.entry_fee_usd,
        (crate::exit_policy::taker_fee_pct(dec!(0.70)) / Decimal::ONE_HUNDRED) * dec!(7)
    );

    // The reverse: a TAKER order the venue says RESTED → no fee. (Rebates are
    // not modelled, but a maker fill is genuinely free.)
    let mut rested = core();
    let (id3, _) = rested
        .place_pending(
            req(Side::Buy, FillPolicy::Taker, dec!(0.70), dec!(10), "r"),
            1,
        )
        .unwrap();
    rested.confirm_live(&id3, 1).unwrap();
    rested
        .ingest_fill(
            fill_as(&id3, "r1", Side::Buy, dec!(0.70), dec!(10), true),
            2,
        )
        .unwrap();
    let pos3 = rested.positions().open_positions()[0].clone();
    assert_eq!(pos3.entry_role, OrderRole::Maker, "the report said maker");
    assert_eq!(
        pos3.flows.entry_fee_usd,
        Decimal::ZERO,
        "a resting fill is charged nothing"
    );
}

/// The exact fixture from the plan's background section: a `MakerThenTaker`
/// entry that rests at the bid, is hit, and exits at a profit. This is the
/// case that measured a 0.0817722 gap before E17.
#[test]
fn maker_then_taker_entry_and_profit_exit_reconcile() {
    let mut c = core();
    let (id, _) = c
        .place(
            req(
                Side::Buy,
                FillPolicy::MakerThenTaker,
                dec!(0.43),
                dec!(10),
                "entry",
            ),
            0,
            1,
        )
        .unwrap();
    // The resting bid is hit in full → a MAKER entry fill (no fee).
    c.ingest_fill(fill(&id, "mtt-1", Side::Buy, dec!(0.43), dec!(10)), 2)
        .unwrap();
    let pos = c.positions().open_positions()[0].clone();
    assert_eq!(pos.entry_role, OrderRole::Maker, "rested, so maker");
    assert_eq!(
        pos.flows.entry_fee_usd,
        Decimal::ZERO,
        "a maker entry pays no fee"
    );

    // Profit target exits as a maker.
    let (sid, _) = c
        .place(
            req(Side::Sell, FillPolicy::Maker, dec!(0.95), dec!(10), "exit"),
            0,
            3,
        )
        .unwrap();
    c.ingest_fill(fill(&sid, "mtt-2", Side::Sell, dec!(0.95), dec!(10)), 4)
        .unwrap();

    assert_reconciled(&c, "MakerThenTaker hit → maker profit exit");
    let closed = &c.positions().closed_positions()[0];
    // 10 shares × (0.95 − 0.43) = 5.20, with no fees on either leg.
    assert_eq!(closed.pnl_usd, dec!(5.20));
    assert_eq!(closed.net_pnl_usd, dec!(5.20));
    assert_eq!(closed.entry_role, OrderRole::Maker);
    assert_eq!(closed.exit_role, OrderRole::Maker);
    assert_eq!(closed.dust_shares, Decimal::ZERO);
}

/// Mixed roles across legs of ONE position: a maker leg that partially fills,
/// then the escalated taker leg that completes the entry. This drives the REAL
/// escalation (maker timeout → cancel → cross the remainder) rather than
/// hand-placing the second order, so it also pins that a partially filled
/// maker leg still escalates instead of being rejected as a duplicate entry.
/// The fee must follow each leg's own role, not the original request — the
/// core of the E17 defect.
#[test]
fn a_mixed_role_position_charges_each_leg_by_what_it_did() {
    let mut c = core();
    let (id, _) = c
        .place(
            req(
                Side::Buy,
                FillPolicy::MakerThenTaker,
                dec!(0.43),
                dec!(10),
                "entry",
            ),
            1_000,
            1,
        )
        .unwrap();
    assert!(c.ome().get(&id).unwrap().status.is_live(), "it must rest");

    // Maker leg: 4 of 10 shares are hit at the resting bid.
    c.ingest_fill(fill(&id, "leg-1", Side::Buy, dec!(0.43), dec!(4)), 2)
        .unwrap();
    let half = c.positions().open_positions()[0].clone();
    assert_eq!(half.shares, dec!(4));
    assert_eq!(half.entry_role, OrderRole::Maker);
    assert_eq!(
        half.flows.entry_fee_usd,
        Decimal::ZERO,
        "the maker leg pays no fee"
    );

    // The maker window elapses: the kernel cancels the rest of the resting
    // order and crosses the remaining 6 shares as a TAKER. The ask rests
    // ABOVE the resting bid (the matcher stays quiet) and deep enough
    // for the honest FOK walk to cover all 6 shares.
    c.book_snapshot("tok", vec![], vec![(dec!(0.45), dec!(500))], 1_001);
    c.tick(1_002).unwrap();
    let pos = c.positions().open_positions()[0].clone();
    assert_eq!(pos.shares, dec!(10), "both legs accrue onto one position");
    assert_eq!(
        pos.entry_role,
        OrderRole::MakerThenTaker,
        "the position records that both kinds of fill happened"
    );
    // Basis is the real money spent: 4×0.43 rested, 6×0.45 crossed at the
    // book's ask (the escalated leg pays the walk price, not the maker's).
    assert_eq!(pos.cost_usd, dec!(4) * dec!(0.43) + dec!(6) * dec!(0.45));
    // The fee covers the TAKER leg only — not all 10 shares at the taker rate.
    let expected_fee = (crate::exit_policy::taker_fee_pct(dec!(0.45)) / Decimal::ONE_HUNDRED)
        * dec!(0.45)
        * dec!(6);
    assert_eq!(pos.flows.entry_fee_usd, expected_fee);
    assert!(expected_fee > Decimal::ZERO);

    // Exit in full as a taker at a profit.
    c.book_snapshot("tok", vec![(dec!(0.95), dec!(500))], vec![], 4);
    let (sid, _) = c
        .place(
            req(Side::Sell, FillPolicy::Taker, dec!(0.95), dec!(10), "exit"),
            0,
            5,
        )
        .unwrap();
    assert_eq!(c.ome().get(&sid).unwrap().status, OrderStatus::Filled);
    assert_reconciled(&c, "mixed maker/taker entry, taker exit");

    let closed = &c.positions().closed_positions()[0];
    assert_eq!(closed.entry_role, OrderRole::MakerThenTaker);
    assert_eq!(closed.exit_role, OrderRole::Taker);
    assert_eq!(closed.shares, dec!(10));
    // Gross = proceeds − real basis: 9.5 − (4×0.43 + 6×0.45), minus the
    // taker legs' fees (the escalated entry leg and the exit).
    let exit_fee =
        (crate::exit_policy::taker_fee_pct(dec!(0.95)) / Decimal::ONE_HUNDRED) * dec!(9.5);
    assert_eq!(
        closed.net_pnl_usd,
        dec!(9.5) - (dec!(4) * dec!(0.43) + dec!(6) * dec!(0.45)) - expected_fee - exit_fee
    );
}

/// Dry and LIVE must produce a bit-identical ledger. In DRY the core
/// synthesises the fills; in LIVE the venue adapter reports them. Both meet
/// at `apply_delta_effects`, so identical inputs must yield identical money.
/// This is the `account:parity` gate expressed as a unit test.
#[test]
fn dry_and_live_ledgers_agree_bit_for_bit() {
    for (em, xm) in [
        (FillPolicy::Maker, FillPolicy::Taker),
        (FillPolicy::Taker, FillPolicy::Maker),
        (FillPolicy::Maker, FillPolicy::Maker),
        (FillPolicy::MakerThenTaker, FillPolicy::Taker),
    ] {
        // DRY: submit at a NON-crossing book (post-only would be refused
        // on a crossing one — #464), so the fill below is the venue's
        // own report either way and the parity subject is the LEDGER
        // arithmetic, not the matcher's fill decision.
        let mut dry = core();
        dry.book_snapshot(
            "tok",
            vec![(dec!(0.43), dec!(100))],
            vec![(dec!(0.44), dec!(100))],
            1,
        );
        let (did, _) = dry
            .place(req(Side::Buy, em, dec!(0.43), dec!(10), "entry"), 0, 1)
            .unwrap();
        if dry.ome().get(&did).unwrap().filled_size < dec!(10) {
            // It rested without crossing; drive the same fill the venue would.
            dry.ingest_fill(fill(&did, "d1", Side::Buy, dec!(0.43), dec!(10)), 2)
                .unwrap();
        }
        let (dxid, _) = dry
            .place(req(Side::Sell, xm, dec!(0.95), dec!(10), "exit"), 0, 3)
            .unwrap();
        if dry.ome().get(&dxid).unwrap().filled_size < dec!(10) {
            dry.ingest_fill(fill(&dxid, "d2", Side::Sell, dec!(0.95), dec!(10)), 4)
                .unwrap();
        }

        // LIVE: register pending, venue acks, then the same fills stream in.
        let mut live = core();
        let (lid, _) = live
            .place_pending(req(Side::Buy, em, dec!(0.43), dec!(10), "entry"), 1)
            .unwrap();
        live.confirm_live(&lid, 1).unwrap();
        live.ingest_fill(fill(&lid, "l1", Side::Buy, dec!(0.43), dec!(10)), 2)
            .unwrap();
        let (lxid, _) = live
            .place_pending(req(Side::Sell, xm, dec!(0.95), dec!(10), "exit"), 3)
            .unwrap();
        live.confirm_live(&lxid, 3).unwrap();
        live.ingest_fill(fill(&lxid, "l2", Side::Sell, dec!(0.95), dec!(10)), 4)
            .unwrap();

        assert_eq!(
            dry.ledger().balance(),
            live.ledger().balance(),
            "[{em:?}→{xm:?}] dry and live cash must be bit-identical"
        );
        assert_eq!(
            realized(&dry),
            realized(&live),
            "[{em:?}→{xm:?}] dry and live realized PnL must be bit-identical"
        );
        assert_reconciled(&dry, &format!("dry parity {em:?}→{xm:?}"));
        assert_reconciled(&live, &format!("live parity {em:?}→{xm:?}"));
    }
}

/// A rollback (`FAILED`) must not leave phantom cash, a phantom fee, or a
/// phantom position behind.
#[test]
fn a_rolled_back_fill_leaves_the_ledger_clean() {
    let mut c = core();
    // Rest the order so the venue, not the matcher, reports the fills — the
    // rollback must target the SAME trade the provisional fill created.
    let (id, _) = c
        .place_pending(
            req(Side::Buy, FillPolicy::Maker, dec!(0.43), dec!(10), "entry"),
            1,
        )
        .unwrap();
    c.confirm_live(&id, 1).unwrap();
    c.ingest_fill(fill(&id, "r1", Side::Buy, dec!(0.43), dec!(10)), 2)
        .unwrap();
    let provisioned = c.ledger().balance();
    assert!(provisioned < SEED, "the provisional fill moved cash");

    // The venue reports that same trade as FAILED: it is fully rolled back.
    c.ingest_fill(
        Fill {
            status: FillStatus::Failed,
            ..fill(&id, "r1", Side::Buy, dec!(0.43), dec!(10))
        },
        3,
    )
    .unwrap();
    c.tick(4).unwrap();

    let net = realized(&c);
    assert_eq!(
        c.ledger().balance(),
        SEED + net,
        "a rolled-back trade must not move cash"
    );
    let held: Decimal = c
        .positions()
        .open_positions()
        .iter()
        .map(|p| p.shares)
        .sum();
    assert_eq!(held, Decimal::ZERO, "nothing may be held after a rollback");
    assert_eq!(net, Decimal::ZERO, "a rollback realizes no PnL");
}

/// Reconciliation-gap fills (the third producer of deltas) reconcile too:
/// the venue reports a partial the core never saw, and the position is sized
/// from what was actually confirmed, not from what was requested.
#[test]
fn reconciliation_gap_fills_reconcile() {
    let mut c = core();
    // The order goes out and the venue ack'd it; the fill arrives only later
    // and only in part — the classic reconciliation gap.
    let (id, _) = c
        .place_pending(
            req(Side::Buy, FillPolicy::Taker, dec!(0.62), dec!(10), "entry"),
            1,
        )
        .unwrap();
    c.confirm_live(&id, 1).unwrap();
    c.ingest_fill(fill(&id, "gap-1", Side::Buy, dec!(0.62), dec!(4)), 2)
        .unwrap();
    let held = c.positions().open_positions()[0].shares;
    assert_eq!(held, dec!(4), "only what the venue confirmed");
    cancel_open_buys(&mut c, 3);

    // Close the whole holding as a taker at a loss. The 0.40 bid is deep
    // enough for the honest FOK exit to fill at the walked price.
    c.book_snapshot("tok", vec![(dec!(0.40), dec!(500))], vec![], 3);
    let (sid, _) = c
        .place(
            req(Side::Sell, FillPolicy::Taker, dec!(0.40), held, "exit"),
            0,
            4,
        )
        .unwrap();
    assert_eq!(c.ome().get(&sid).unwrap().status, OrderStatus::Filled);
    assert_reconciled(&c, "reconciliation gap partial entry");
    assert!(realized(&c) < Decimal::ZERO, "bought 0.62, sold 0.40");
}

/// The E17 identity asserted MID-FLIGHT on a half-exited position — the case
/// the flat-book form cannot see.
///
/// The view exposes two different entry figures and only one of them is cash:
/// `flows.entry_cost_usd` is the TOTAL notional paid in, while `cost_usd` is
/// the basis of the shares STILL HELD. A partial exit moves basis out of
/// `cost_usd` and returns it inside `proceeds_usd`, so an identity that sums
/// `cost_usd` counts that release twice and reports a phantom drift exactly
/// equal to the released basis. Both facts are pinned here, because the
/// distinction is invisible until a position is partly exited — which is why
/// an external monitor summing `cost_usd` (as the first draft of
/// `account-drift-check.mjs` did) reports drift on a correct ledger.
#[test]
fn the_identity_holds_mid_flight_and_entry_cost_is_not_the_held_basis() {
    let mut c = core();
    let (id, _) = c
        .place(
            req(Side::Buy, FillPolicy::Taker, dec!(0.43), dec!(10), "in"),
            0,
            1,
        )
        .unwrap();
    c.ingest_fill(
        fill_as(&id, "mf-1", Side::Buy, dec!(0.43), dec!(10), false),
        2,
    )
    .unwrap();

    // Sell 4 of the 10 — the position stays open with both cash legs non-zero.
    let (sid, _) = c
        .place(
            req(Side::Sell, FillPolicy::Taker, dec!(0.60), dec!(4), "out"),
            0,
            3,
        )
        .unwrap();
    c.ingest_fill(
        fill_as(&sid, "mf-2", Side::Sell, dec!(0.60), dec!(4), false),
        4,
    )
    .unwrap();

    let p = c.positions().open_positions()[0].clone();
    assert_eq!(p.shares, dec!(6), "4 of 10 sold, so 6 remain");
    assert!(p.flows.proceeds_usd > Decimal::ZERO, "cash came back in");
    assert_eq!(
        p.flows.entry_cost_usd,
        dec!(4.30),
        "the TOTAL paid in never shrinks with a partial exit"
    );
    assert_eq!(
        p.cost_usd,
        dec!(2.58),
        "the held basis does shrink: 4.30 − (4.30/10 × 4)"
    );
    assert!(
        p.flows.entry_cost_usd > p.cost_usd,
        "this is exactly why the identity may not use the held basis"
    );

    let cash_in = p.flows.entry_cost_usd + p.flows.entry_fee_usd;
    let cash_back = p.flows.proceeds_usd - p.flows.exit_fee_usd;
    let realized: Decimal = c
        .positions()
        .closed_positions()
        .iter()
        .map(|p| p.net_pnl_usd)
        .sum();
    assert_eq!(
        c.ledger().balance(),
        SEED + realized - cash_in + cash_back,
        "the identity must hold mid-flight, not only once flat"
    );
    // ...and the held-basis form must NOT hold, or this test would be pinning
    // nothing. It overstates cash by the released basis, to the last digit.
    let wrong = SEED + realized - (p.cost_usd + p.flows.entry_fee_usd) + cash_back;
    assert_eq!(
        wrong - c.ledger().balance(),
        dec!(1.72),
        "summing the held basis double-counts the 4-share release"
    );
}

use super::*;

/// F2, counterexample: our maker BUY bought 10 shares @ 0.40 (cost 4);
/// the counterparty taker SOLD 100 shares in that same match, the other 90
/// bought by other makers. A venue SELL leg that cannot be attributed to
/// our inventory must not mint cash.
///
/// (a) a SELL with NO open position for the token: booked nowhere;
/// (b) an oversized SELL against 10 held shares: size AND proceeds are
///     truncated to the attributable inventory — proceeds 4, net 0, cash
///     back to seed — NOT the audit's 40 proceeds / +36 net (882%).
#[test]
fn external_sell_beyond_attributable_inventory_never_mints_cash() {
    let mut c = core();

    // (a) no position: the SELL matches nothing and must change nothing.
    c.reconcile(external_sell(dec!(100), dec!(0.40), 1_000))
        .unwrap();
    assert_eq!(c.positions().open_positions().len(), 0);
    assert_eq!(c.positions().closed_positions().len(), 0);
    assert_eq!(c.ledger().balance(), SEED, "no position, no booking");
    assert_eq!(c.positions().daily_pnl_for("default"), Decimal::ZERO);

    // (b) hold 10 shares @ 0.40 (maker entry, cost 4, no fee).
    open_maker_position(&mut c, dec!(0.40), dec!(10), 4_000);
    assert_eq!(c.positions().open_positions()[0].shares, dec!(10));
    assert_eq!(c.positions().open_positions()[0].cost_usd, dec!(4));
    assert_eq!(c.ledger().balance(), SEED - dec!(4));
    let bal_after_entry = c.ledger().balance();

    // The counterparty's 100-share sell: clamped to the 10 shares we hold.
    c.reconcile(external_sell(dec!(100), dec!(0.40), 6_000))
        .unwrap();

    let closed = &c.positions().closed_positions();
    assert_eq!(closed.len(), 1, "the held shares were sold off");
    assert_eq!(closed[0].shares, dec!(10));
    // Proceeds truncated to inventory value: 10 × 0.40 = 4, not 100 × 0.40.
    assert_eq!(closed[0].pnl_usd, dec!(4) - dec!(4), "proceeds capped at 4");
    assert_eq!(
        closed[0].net_pnl_usd,
        Decimal::ZERO,
        "no phantom profit from the counterparty's size"
    );
    // Cash: entry cost 4, proceeds 4 → back to seed. NOT seed + 36.
    assert_eq!(
        c.ledger().balance(),
        bal_after_entry + dec!(4),
        "the ledger received exactly the attributable proceeds"
    );
    assert_eq!(c.positions().daily_pnl_for("default"), Decimal::ZERO);
    // Strategy accounting: a flat close is NOT a +36 win.
    let acc = c
        .strategy_accounting
        .get(&strategy_acc_key(STRATEGY, "default"))
        .expect("accounting row");
    assert_eq!(acc.net_pnl_usd, Decimal::ZERO);
    assert_eq!(acc.wins, 1);
    assert_eq!(acc.losses, 0);
}

/// F4, counterexample: maker BUY 10 @ 0.40, maker SELL 10 @ 0.80 booked
/// +4 realized on MATCHED; the venue then reports the same trade FAILED.
/// The sale never happened: inventory must come back and every ledger the
/// close touched must reverse.
#[test]
fn failed_status_after_full_close_restores_inventory_and_reverses_profit() {
    let log = tmp_trade_log("f4");
    let mut c = core_with_trade_log(Some(log.clone()));

    open_maker_position(&mut c, dec!(0.40), dec!(10), 1_000);
    let (sid, _) = c
        .place_pending(
            req(Side::Sell, FillPolicy::Maker, dec!(0.80), dec!(10), "exit"),
            2_000,
        )
        .unwrap();
    c.confirm_live(&sid, 2_000).unwrap();
    c.ingest_fill(
        fill(
            &sid,
            "x1",
            Side::Sell,
            dec!(0.80),
            dec!(10),
            FillStatus::Confirmed,
        ),
        3_000,
    )
    .unwrap();

    // The close was fully booked.
    assert!(c.positions().open_positions().is_empty());
    let closed = &c.positions().closed_positions()[0];
    assert_eq!(closed.net_pnl_usd, dec!(4));
    assert_eq!(c.ledger().balance(), SEED + dec!(4));
    assert_eq!(c.positions().daily_pnl_for("default"), dec!(4));
    {
        let acc = c
            .strategy_accounting
            .get(&strategy_acc_key(STRATEGY, "default"))
            .expect("accounting row");
        assert_eq!(acc.closed_trades, 1);
        assert_eq!(acc.wins, 1);
        assert_eq!(acc.net_pnl_usd, dec!(4));
    }
    assert_eq!(c.trade_db.as_ref().unwrap().summary().total_trades, 1);
    assert_eq!(c.trade_db.as_ref().unwrap().summary().total_net_pnl, 4.0);

    // FAILED arrives for the same trade → the rollback must unwind it all.
    c.ingest_fill(
        fill(
            &sid,
            "x1",
            Side::Sell,
            dec!(0.80),
            dec!(10),
            FillStatus::Failed,
        ),
        4_000,
    )
    .unwrap();

    // Inventory restored exactly: 10 shares @ 0.40 basis.
    assert_eq!(
        c.positions().open_positions().len(),
        1,
        "the unsold position is back on the books"
    );
    let pos = &c.positions().open_positions()[0];
    assert_eq!(pos.shares, dec!(10));
    assert_eq!(pos.cost_usd, dec!(4));
    assert_eq!(pos.entry_price, dec!(0.40));
    // The phantom close is gone from the realized book.
    assert!(
        c.positions().closed_positions().is_empty(),
        "a trade that never happened must not stay realized"
    );
    // Cash refunded to post-entry; profit reversed.
    assert_eq!(c.ledger().balance(), SEED - dec!(4));
    assert_eq!(c.positions().daily_pnl_for("default"), Decimal::ZERO);
    {
        let acc = c
            .strategy_accounting
            .get(&strategy_acc_key(STRATEGY, "default"))
            .expect("accounting row");
        assert_eq!(acc.closed_trades, 0);
        assert_eq!(acc.wins, 0);
        assert_eq!(acc.fees_usd, Decimal::ZERO);
        assert_eq!(acc.net_pnl_usd, Decimal::ZERO);
    }
    // TradeDb: record withdrawn, summary back to zero.
    assert_eq!(c.trade_db.as_ref().unwrap().summary().total_trades, 0);
    assert_eq!(c.trade_db.as_ref().unwrap().summary().total_net_pnl, 0.0);
    assert!(
        std::fs::read_to_string(&log)
            .unwrap_or_default()
            .trim()
            .is_empty(),
        "the retracted trade must not remain in the JSONL"
    );

    // E17 cash identity over the whole round trip: cash + open basis
    // equals the seed — nothing leaked in either direction.
    assert_eq!(
        c.ledger().balance() + c.positions().open_positions()[0].cost_usd,
        SEED
    );

    let _ = std::fs::remove_file(&log);
}

/// F10, counterexample: the strategy booked an automatic close, then the
/// operator flat-closed a position manually on the venue at a LOSS. The
/// loss must reach the SAME close ledger: TradeDb record, strategy net
/// PnL, win/loss counts and the PositionClosed event — not just cash.
#[test]
fn manual_venue_loss_reaches_the_trade_ledger() {
    let log = tmp_trade_log("f10");
    let mut c = core_with_trade_log(Some(log.clone()));
    let (tx, mut rx) = mpsc::unbounded_channel();
    c.set_event_sink(tx);

    // Hold 10 @ 0.60 (cost 6).
    open_maker_position(&mut c, dec!(0.60), dec!(10), 1_000);
    assert_eq!(c.ledger().balance(), SEED - dec!(6));

    // Operator sells flat on the venue at 0.40: a −2 realized LOSS.
    c.reconcile(external_sell(dec!(10), dec!(0.40), 6_000))
        .unwrap();

    let closed = &c.positions().closed_positions();
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0].exit_reason, ExitReason::Manual);
    assert_eq!(closed[0].net_pnl_usd, dec!(-2));

    // TradeDb received the manual close (audit F10: it used to be missed).
    assert_eq!(c.trade_db.as_ref().unwrap().summary().total_trades, 1);
    assert_eq!(c.trade_db.as_ref().unwrap().summary().losses, 1);
    assert_eq!(c.trade_db.as_ref().unwrap().summary().total_net_pnl, -2.0);

    // Strategy accounting: the loss IS in the strategy's net PnL.
    let acc = c
        .strategy_accounting
        .get(&strategy_acc_key(STRATEGY, "default"))
        .expect("accounting row");
    assert_eq!(acc.closed_trades, 1);
    assert_eq!(acc.losses, 1);
    assert_eq!(acc.wins, 0);
    assert_eq!(acc.net_pnl_usd, dec!(-2));

    // Daily PnL sees it too, and the PositionClosed event fired.
    assert_eq!(c.positions().daily_pnl_for("default"), dec!(-2));
    let position_closed = {
        let mut found = false;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, Event::PositionClosed { ref net_pnl_usd, .. } if *net_pnl_usd == dec!(-2))
            {
                found = true;
            }
        }
        found
    };
    assert!(
        position_closed,
        "the manual close must emit PositionClosed like engine closes do"
    );

    // Cash identity: SEED + realized net.
    assert_eq!(c.ledger().balance(), SEED + dec!(-2));

    let _ = std::fs::remove_file(&log);
}

// ── E28 (G5): a cross-account close is refused, never re-routed ─────────

#[test]
fn a_cross_account_close_is_refused_not_rerouted() {
    let mut c = two_book_core();
    // The position belongs to `default` (the req helper names it).
    open_maker_position(&mut c, dec!(0.40), dec!(10), 1);

    // The WRONG account tries the close: refused, naming both sides.
    let mut wrong = req(
        Side::Sell,
        FillPolicy::Taker,
        dec!(0.90),
        dec!(10),
        "exit:tok:StopLoss",
    );
    wrong.account_id = crate::account::AccountId::from("paper");
    let err = c.place(wrong, 0, 2).unwrap_err();
    assert!(
        err.message.contains("cross-account close refused"),
        "the refusal must name the fault: {err}"
    );
    assert!(
        err.message.contains("`default`") && err.message.contains("`paper`"),
        "the refusal must name whose money is whose: {err}"
    );
    // Nothing settled: the position still open.
    assert_eq!(c.positions().open_positions().len(), 1);

    // The RIGHT account closes: a bid rests so the dry taker fills.
    c.book_snapshot("tok", vec![(dec!(0.90), dec!(500))], vec![], 3);
    let right = req(
        Side::Sell,
        FillPolicy::Taker,
        dec!(0.90),
        dec!(10),
        "exit:tok:Manual2", // a different key: the refused one is in cooldown
    );
    c.place(right, 0, 4).unwrap();
    assert!(
        c.positions().open_positions().is_empty(),
        "the position's own account may close it"
    );
    // The proceeds landed in the book that PAID for the shares; the
    // other book never moved.
    let paper = c
        .accounts()
        .ledger_of(&crate::account::AccountId::from("paper"))
        .unwrap()
        .balance();
    assert_eq!(paper, SEED, "paper's book is untouched by default's close");
    assert!(
        c.ledger().balance() > SEED,
        "default's book took the profit"
    );
}

// ── Account↔venue binding (first batch) ─────────────────────────────────

#[test]
fn an_order_for_another_venue_is_refused_when_the_account_is_bound() {
    let mut c = venue_bound_core();
    // `default` is pinned to polymarket; the kalshi token belongs to
    // another venue's round market — refused, naming BOTH venues.
    let err = c.place(bound_order("kal-up", "k1"), 0, 1).unwrap_err();
    assert!(err.message.contains("cross-venue order refused"), "{err}");
    assert!(
        err.message.contains("`polymarket`") && err.message.contains("`kalshi`"),
        "the refusal must name both venues: {err}"
    );
    // The order never reached the book.
    assert!(c.list_orders().is_empty());
}

/// P0 (v0.4.2 audit): with several executors live, each one's reconcile
/// snapshot only speaks for ITS venue. Before the scope, the kalshi sweep
/// judged this polymarket order (absent from kalshi's own open list) as a
/// ghost and force-closed it LOCALLY while it kept resting at polymarket —
/// unmanaged, and skipped by the orphan sweep because `known_venue_ids`
/// still listed it.
///
/// Negative: on the pre-fix code the order becomes `ForceClosed` here.
#[test]
fn reconcile_from_another_venue_leaves_the_resting_order_alone() {
    let mut c = venue_bound_core();
    let (id, _) = c.place(bound_order("poly-up", "k1"), 0, 1).unwrap();
    c.bind_venue(&id, "poly-1".into(), 2).unwrap();
    assert_eq!(c.ome().get(&id).unwrap().status, OrderStatus::Live);

    // The kalshi sweep cannot see a polymarket resting id.
    let foreign = c
        .reconcile_for_venue(
            crate::reconcile::VenueSnapshot {
                open_order_ids: vec![],
                trades: vec![],
                now_ms: 3,
            },
            "kalshi",
        )
        .unwrap();
    assert!(
        foreign.actions.is_empty(),
        "another venue's snapshot must not judge this order: {:?}",
        foreign.actions
    );
    assert_eq!(
        c.ome().get(&id).unwrap().status,
        OrderStatus::Live,
        "the order still rests at polymarket — a local force-close strands it"
    );

    // Its OWN sweep still repairs a real ghost (the scope is not a blanket skip).
    let own = c
        .reconcile_for_venue(
            crate::reconcile::VenueSnapshot {
                open_order_ids: vec![],
                trades: vec![],
                now_ms: 4,
            },
            "polymarket",
        )
        .unwrap();
    assert_eq!(own.suspect_ghost_ids, vec![id.clone()]);
    assert_eq!(c.ome().get(&id).unwrap().status, OrderStatus::ForceClosed);
}

#[test]
fn a_bound_account_trades_its_own_venue_and_an_unbound_one_trades_anywhere() {
    let mut c = venue_bound_core();
    // The account's OWN venue: passes.
    c.place(bound_order("poly-up", "k1"), 0, 1).unwrap();
    // The UNBOUND account on the OTHER venue: passes — binding restricts
    // only the account that declares it.
    let mut paper = bound_order("kal-up", "k2");
    paper.account_id = crate::account::AccountId::from("paper");
    c.place(paper, 0, 2).unwrap();
    assert_eq!(c.list_orders().len(), 2);
}

#[test]
fn an_order_on_an_unlabelled_token_passes_the_venue_guard() {
    // A token that belongs to NO live labelled market carries no venue
    // evidence — the guard cannot judge it, so it passes (the legacy /
    // unlabelled-host behaviour). Whatever refuses it must be another
    // gate with its own reason.
    let mut c = venue_bound_core();
    let out = c.place(bound_order("ghost-token", "k1"), 0, 1);
    assert!(
        out.is_ok() || !out.unwrap_err().message.contains("cross-venue"),
        "an untagged token is not venue evidence"
    );
}

/// The sim account (`venue = "polysimulator"`) trades byte-identical tokens
/// polymarket also lists (the mirror property). Under the RFC-661 M1 dual-key
/// table the sim batch lands its OWN rows — the venue guard judges the
/// order's EXECUTION venue off the placing account's own (venue, token) row,
/// no declaration involved.
#[test]
fn a_mirror_bound_account_places_orders_on_mirror_tokens() {
    let mut c = venue_bound_core();
    // The polysimulator batch declares the SAME token pair polymarket
    // already listed: a same-slot merge whose fresh set is the sim venue's
    // OWN tokens — the rows land under their own (venue, token) keys
    // (RFC-661 M1; no mirror declaration participates).
    let now = 1_000_000;
    let slot = now / 1000 / 900;
    let sim = crate::model::CryptoMarket {
        asset: "BTC".into(),
        condition_id: "cond-polymarket".into(),
        question_id: "q".into(),
        up_token_id: "poly-up".into(),
        down_token_id: "poly-down".into(),
        up_price: dec!(0.6),
        down_price: dec!(0.4),
        expires_at_ms: (slot + 1) * 900 * 1000,
        round_slot: slot,
        round_duration_sec: 900,
        archive_verdict: false,
        venue: "polysimulator".into(),
        neg_risk: false,
        question: "?".into(),
    };
    c.engine_on_data(
        crate::engine::DataEvent::RoundMarkets {
            markets: vec![sim],
            now_ms: now + 1,
        },
        now + 1,
    );
    // A poly-sim account bound to polysimulator: its mirror-token order
    // passes the venue guard — the binding reads its OWN (venue, token)
    // row through execution_venue_of.
    c.accounts_mut()
        .insert(crate::account::Account {
            id: crate::account::AccountId::from("sim"),
            name: "sim".into(),
            market_type: blitzkrieg_market_api::MarketType::Prediction,
            status: crate::account::AccountStatus::Active,
            credential_keys: crate::account::CredentialKeys::default(),
            venue: "polysimulator".into(),
            trading_enabled: true,
            strategies: Vec::new(),
            rounds: Vec::new(),
            updated_at_ms: 0,
        })
        .unwrap();
    c.accounts_mut()
        .get_mut(&crate::account::AccountId::from("sim"))
        .unwrap()
        .set_balance(dec!(10));
    // The book's binding reaches the engine's scope map on the evaluate
    // tick (engine_evaluate re-pushes every account's venue) — the same
    // sync the runtime relies on.
    c.engine_evaluate(now + 2);
    let mut order = bound_order("poly-up", "sim-1");
    order.account_id = crate::account::AccountId::from("sim");
    c.place(order, 0, 1).unwrap();
    assert_eq!(c.list_orders().len(), 1, "the mirror order passes");

    // And the execution venue reads the placing binding's own dual-key row —
    // the same fact the executor drain and the cancel queue key on. Two
    // accounts on the byte-identical token each read their OWN venue.
    assert_eq!(
        c.execution_venue_of("sim", "poly-up"),
        "polysimulator",
        "execution follows the placing account's own dual-key row"
    );
    assert_eq!(
        c.execution_venue_of("default", "poly-up"),
        "polymarket",
        "the PM-bound account reads the PM row — the two never collapse"
    );

    // The position (and the trade row it closes into) stamps the same
    // EXECUTION venue: a fill opened under a venue-bound account inherits
    // the row's own venue tag (polysimulator), so the panel's account-bound
    // venue filter finds the row the badge counted.
    let sim_id = c.ome().all()[0].order_id.clone();
    c.ingest_fill(
        crate::model::Fill {
            order_id: sim_id,
            trade_id: Some("sim-t1".into()),
            token_id: "poly-up".into(),
            side: Side::Buy,
            price: dec!(0.6),
            size: dec!(5),
            status: FillStatus::Confirmed,
            ts_ms: now + 3,
            tx_hash: None,
            maker: Some(false),
            account_id: crate::account::AccountId::from("sim"),
        },
        now + 4,
    )
    .unwrap();
    let opened = c.positions().open_positions();
    assert_eq!(opened.len(), 1, "the mirror fill opened the position");
    assert_eq!(
        opened[0].venue, "polysimulator",
        "the position stamps the execution venue, not the table row"
    );

    // The kalshi token is in no mirror declaration: the sim account
    // still cannot reach it.
    let mut kal = bound_order("kal-up", "sim-2");
    kal.account_id = crate::account::AccountId::from("sim");
    let err = c.place(kal, 0, 2).unwrap_err();
    assert!(err.message.contains("cross-venue order refused"), "{err}");
}

/// RFC-661 §五.1 面板 bug 的根修钉（M1 形态）：polysimulator 执行
/// polymarket 字节同 token 的单，成交行/持仓行/trade 行的 venue 必须
/// ≡ 执行身份（polysimulator）——M1 双键表让 poly 的行自己落表（不
/// 再需要 mirror 声明），入场戳（`execution_venue_of`）按绑定读它
/// 自己的 (venue, token) 行，从开仓到 trade_db 全链同戳。面板
/// `venue:polysimulator` 滤网筛出行数 ≡ 徽标计数——「汇总 1 单、
/// 筛后 0 行」的截图 bug 在自然行形态下不可复现。开仓→SELL 平仓→
/// trade 行全链钉死。
#[test]
fn a_polysimulator_executed_polymarket_token_stamps_execution_identity_end_to_end() {
    let mut c = venue_bound_core();
    let now = 1_000_000;
    let slot = now / 1000 / 900;
    // M1 双键形态：polysimulator 的批次带着与 polymarket 无关的自身行
    // 落表（这里 token 与 PM 的 poly-up 同字节——镜像的构造性事实保
    // 留，但表不再 token 唯一），没有任何 mirror 声明参与。
    let sim = crate::model::CryptoMarket {
        asset: "BTC".into(),
        condition_id: "cond-polymarket".into(),
        question_id: "q".into(),
        up_token_id: "poly-up".into(),
        down_token_id: "poly-down".into(),
        up_price: dec!(0.6),
        down_price: dec!(0.4),
        expires_at_ms: (slot + 1) * 900 * 1000,
        round_slot: slot,
        round_duration_sec: 900,
        archive_verdict: false,
        venue: "polysimulator".into(),
        neg_risk: false,
        question: "?".into(),
    };
    c.engine_on_data(
        crate::engine::DataEvent::RoundMarkets {
            markets: vec![sim],
            now_ms: now + 1,
        },
        now + 1,
    );
    c.accounts_mut()
        .insert(crate::account::Account {
            id: crate::account::AccountId::from("sim"),
            name: "sim".into(),
            market_type: blitzkrieg_market_api::MarketType::Prediction,
            status: crate::account::AccountStatus::Active,
            credential_keys: crate::account::CredentialKeys::default(),
            venue: "polysimulator".into(),
            trading_enabled: true,
            strategies: Vec::new(),
            rounds: Vec::new(),
            updated_at_ms: 0,
        })
        .unwrap();
    c.accounts_mut()
        .get_mut(&crate::account::AccountId::from("sim"))
        .unwrap()
        .set_balance(dec!(10));
    // The book's binding reaches the engine's scope map on the evaluate
    // tick — the same sync the runtime relies on.
    c.engine_evaluate(now + 2);

    // Precondition: the dual-key table carries BOTH venues' rows for the
    // byte-identical token (M1: two instruments).
    let e = c.engine.as_ref().expect("engine");
    assert!(
        e.market_for_token_on_venue("polymarket", "poly-up")
            .is_some()
            && e.market_for_token_on_venue("polysimulator", "poly-up")
                .is_some(),
        "the dual-key table holds both venues' rows for the byte-identical token"
    );

    // The sim account's own row admits and routes it — NO declaration.
    let mut order = bound_order("poly-up", "sim-1");
    order.account_id = crate::account::AccountId::from("sim");
    c.place(order, 0, 1).unwrap();
    assert_eq!(
        c.execution_venue_of("sim", "poly-up"),
        "polysimulator",
        "execution follows the placing account's own dual-key row"
    );

    // The fill opens the position stamped with the EXECUTION identity.
    let sim_id = c.ome().all()[0].order_id.clone();
    c.ingest_fill(
        crate::model::Fill {
            order_id: sim_id,
            trade_id: Some("sim-t1".into()),
            token_id: "poly-up".into(),
            side: Side::Buy,
            price: dec!(0.6),
            size: dec!(5),
            status: FillStatus::Confirmed,
            ts_ms: now + 3,
            tx_hash: None,
            maker: Some(false),
            account_id: crate::account::AccountId::from("sim"),
        },
        now + 4,
    )
    .unwrap();
    let opened = c.positions().open_positions();
    assert_eq!(opened.len(), 1, "the fill opened the position");
    assert_eq!(
        opened[0].venue, "polysimulator",
        "the position stamps the EXECUTION venue — the panel's venue column"
    );

    // The close turns the position into a trade row; the trade_db record
    // carries the SAME venue (the field the panel's venue filter reads).
    let pos_id = opened[0].id.clone();
    let closed = c
        .positions_mut()
        .close(
            &pos_id,
            dec!(0.8),
            crate::model::ExitReason::Manual,
            true,
            now + 5,
        )
        .expect("the full-size close lands");
    assert_eq!(
        closed.venue, "polysimulator",
        "the closed trade keeps the execution identity"
    );
    c.on_position_closed(&closed, now + 6);
    let trades = c.recent_trades(50);
    assert_eq!(trades.len(), 1, "one trade row exists");
    assert_eq!(
        trades[0].get("venue").and_then(|v| v.as_str()),
        Some("polysimulator"),
        "the trade row's venue ≡ the execution identity — the venue:polysimulator \
         filter now finds the rows the badge counted (the panel bug's root fix)"
    );
}

/// The cancel queue tags by EXECUTION venue: a live order cancelled by its
/// polysimulator account must land in the polysimulator executor's drain,
/// never the real venue's.
#[tokio::test]
async fn a_mirror_order_cancel_queues_to_the_mirror_executor() {
    let mut c = venue_bound_core();
    let now = 1_000_000;
    let slot = now / 1000 / 900;
    // The sim batch lands its OWN dual-key rows (RFC-661 M1) — the cancel
    // queue reads the placing binding's own (venue, token) row; no
    // declaration participates.
    let sim = crate::model::CryptoMarket {
        asset: "BTC".into(),
        condition_id: "cond-polymarket".into(),
        question_id: "q".into(),
        up_token_id: "poly-up".into(),
        down_token_id: "poly-down".into(),
        up_price: dec!(0.6),
        down_price: dec!(0.4),
        expires_at_ms: (slot + 1) * 900 * 1000,
        round_slot: slot,
        round_duration_sec: 900,
        archive_verdict: false,
        venue: "polysimulator".into(),
        neg_risk: false,
        question: "?".into(),
    };
    c.engine_on_data(
        crate::engine::DataEvent::RoundMarkets {
            markets: vec![sim],
            now_ms: now + 1,
        },
        now + 1,
    );
    c.accounts_mut()
        .insert(crate::account::Account {
            id: crate::account::AccountId::from("sim"),
            name: "sim".into(),
            market_type: blitzkrieg_market_api::MarketType::Prediction,
            status: crate::account::AccountStatus::Active,
            credential_keys: crate::account::CredentialKeys::default(),
            venue: "polysimulator".into(),
            trading_enabled: true,
            strategies: Vec::new(),
            rounds: Vec::new(),
            updated_at_ms: 0,
        })
        .unwrap();
    c.accounts_mut()
        .get_mut(&crate::account::AccountId::from("sim"))
        .unwrap()
        .set_balance(dec!(10));
    c.engine_evaluate(now + 2);
    let mut order = bound_order("poly-up", "sim-1");
    order.account_id = crate::account::AccountId::from("sim");
    let (id, _) = c.place(order, 0, 1).unwrap();
    c.bind_venue(&id, "sim-v1".into(), 2).unwrap();
    c.cancel(&id, 3).unwrap();
    assert_eq!(
        c.take_pending_venue_cancels_for_venue("polysimulator", false),
        vec!["sim-v1".to_string()],
        "the mirror order's cancel rides the polysimulator drain"
    );
    assert!(
        c.take_pending_venue_cancels_for_venue("polymarket", false)
            .is_empty()
    );
}

#[test]
fn account_views_carry_the_venue_binding() {
    let c = venue_bound_core();
    let views = c.account_views(1);
    let by_id = |id: &str| {
        views
            .iter()
            .find(|v| v.id == id)
            .unwrap_or_else(|| panic!("no view for {id}"))
    };
    assert_eq!(by_id("default").venue, "polymarket");
    assert_eq!(
        by_id("paper").venue,
        "",
        "unbound renders as the empty string"
    );
}

// ── issue 787 + 749: a live book with no venue report projects ZERO ────────

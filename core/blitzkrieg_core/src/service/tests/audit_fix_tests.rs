#![cfg(test)]
//! ── Audit F2/F10/F4: reverse acceptance tests ───────────────────────────────
//!
//! One test per audit finding, each driving the exact counterexample from
//! `BlitzkriegBot_核心逻辑审计.md` through the REAL production path and
//! asserting the account does NOT move the way the bug moved it:
//!
//!  - F2: a counterparty's 100-share SELL must never book against a 10-share
//!    inventory — the clamp truncates size AND proceeds; nothing attributable
//!    to inventory is never booked at all.
//!  - F4: a fully-closing SELL that later arrives FAILED must restore the
//!    position inventory and reverse the profit (TradeDb, strategy PnL, daily
//!    PnL, breaker) instead of leaving phantom profit behind.
//!  - F10: a manual (external) full close at a LOSS must reach the SAME close
//!    ledger the engine's own fills do — TradeDb record, strategy net PnL,
//!    win/loss counts and the PositionClosed event.

use super::*;
use rust_decimal_macros::dec;

const SEED: Decimal = dec!(1000);
const STRATEGY: &str = "acc";

fn core_with_trade_log(path: Option<std::path::PathBuf>) -> Core {
    let mut cfg = CoreConfig {
        risk: RiskConfig {
            max_order_notional: dec!(100),
            ..Default::default()
        },
        dry_seed_balance: SEED,
        auto_exits_enabled: false,
        ..Default::default()
    };
    cfg.trade_log_path = path.map(|p| p.to_string_lossy().into_owned());
    let mut c = Core::new(cfg);
    c.set_balance(SEED);
    c
}

fn core() -> Core {
    core_with_trade_log(None)
}

fn tmp_trade_log(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    // TradeDb keeps summary.json beside the log, so each fixture needs its
    // own DIRECTORY — a bare file in the shared temp root would make every
    // test load (and persist onto) the same summary.json.
    let dir = std::env::temp_dir().join(format!("bk-audit-{tag}-{nanos}"));
    dir.join("trades.jsonl")
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
        strategy: STRATEGY.into(),
        asset: "BTC".into(),
        direction: "up".into(),
        round_slot: 1,
        account_id: crate::model::default_account_id(),
    }
}

fn fill(
    order_id: &str,
    trade: &str,
    side: Side,
    price: Decimal,
    size: Decimal,
    status: FillStatus,
) -> Fill {
    Fill {
        order_id: order_id.into(),
        trade_id: Some(trade.into()),
        token_id: "tok".into(),
        side,
        price,
        size,
        status,
        ts_ms: 0,
        tx_hash: None,
        // Maker-mode orders with no venue report → the policy decides, and
        // these fixtures are exactly what the policy would produce.
        maker: None,
        account_id: crate::model::default_account_id(),
    }
}

/// Open a maker position of `size` shares at `price` through the REAL
/// live fill path (register → venue ack → user-WS fill).
fn open_maker_position(c: &mut Core, price: Decimal, size: Decimal, now: i64) {
    let (id, _) = c
        .place_pending(req(Side::Buy, FillPolicy::Maker, price, size, "entry"), now)
        .unwrap();
    c.confirm_live(&id, now).unwrap();
    c.ingest_fill(
        fill(&id, "e1", Side::Buy, price, size, FillStatus::Confirmed),
        now + 1,
    )
    .unwrap();
}

fn external_sell(size: Decimal, price: Decimal, ts_ms: i64) -> crate::reconcile::VenueSnapshot {
    crate::reconcile::VenueSnapshot {
        open_order_ids: vec![],
        trades: vec![crate::reconcile::VenueTrade {
            venue_order_id: "0xmanual".into(),
            trade_id: "t-manual".into(),
            token_id: "tok".into(),
            side: Side::Sell,
            size,
            price,
            ts_ms,
            tx_hash: None,
            maker: Some(true),
        }],
        now_ms: ts_ms + 1_000,
    }
}

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

/// A two-account core built through the injection constructor — no test
/// touches the real `user_layer/configs/accounts.toml` the cwd decides.
/// Both books are seeded (§9.3: separate wallets, not aliases).
fn two_book_core() -> Core {
    let book = crate::account::AccountLedgers::new(
        vec![
            crate::account::Account {
                id: crate::account::AccountId::from("default"),
                name: "default".into(),
                market_type: blitzkrieg_market_api::MarketType::Prediction,
                status: crate::account::AccountStatus::Active,
                credential_keys: crate::account::CredentialKeys::default(),
                venue: String::new(),
                trading_enabled: true,
                strategies: Vec::new(),
                rounds: Vec::new(),
                updated_at_ms: 0,
            },
            crate::account::Account {
                id: crate::account::AccountId::from("paper"),
                name: "paper".into(),
                market_type: blitzkrieg_market_api::MarketType::Prediction,
                status: crate::account::AccountStatus::Active,
                credential_keys: crate::account::CredentialKeys::default(),
                venue: String::new(),
                trading_enabled: true,
                strategies: Vec::new(),
                rounds: Vec::new(),
                updated_at_ms: 0,
            },
        ],
        crate::account::AccountId::from("default"),
    );
    let cfg = CoreConfig {
        risk: RiskConfig {
            max_order_notional: dec!(100),
            ..Default::default()
        },
        dry_seed_balance: SEED,
        auto_exits_enabled: false,
        // The cwd-relative default would silently read a REAL data/trades
        // log if one happens to sit beside the crate (the summary tests
        // must count their own closes, not history) — same discipline as
        // `core_with_trade_log`.
        trade_log_path: None,
        ..Default::default()
    };
    Core::with_account_book(cfg, book)
}

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

/// A two-account core where `default` is PINNED to venue `polymarket` and
/// `paper` is unbound. The engine carries two labelled round markets so
/// the token→venue lookup has something real to read.
fn venue_bound_core() -> Core {
    venue_bound_core_opts(None)
}

/// [`venue_bound_core`] with a durable order log — the persistence-path
/// tests read what `place` appends.
fn venue_bound_core_opts(order_log_path: Option<String>) -> Core {
    let book = crate::account::AccountLedgers::new(
        vec![
            crate::account::Account {
                id: crate::account::AccountId::from("default"),
                name: "default".into(),
                market_type: blitzkrieg_market_api::MarketType::Prediction,
                status: crate::account::AccountStatus::Active,
                credential_keys: crate::account::CredentialKeys::default(),
                venue: "polymarket".into(),
                trading_enabled: true,
                strategies: Vec::new(),
                rounds: Vec::new(),
                updated_at_ms: 0,
            },
            crate::account::Account {
                id: crate::account::AccountId::from("paper"),
                name: "paper".into(),
                market_type: blitzkrieg_market_api::MarketType::Prediction,
                status: crate::account::AccountStatus::Active,
                credential_keys: crate::account::CredentialKeys::default(),
                venue: String::new(),
                trading_enabled: true,
                strategies: Vec::new(),
                rounds: Vec::new(),
                updated_at_ms: 0,
            },
        ],
        crate::account::AccountId::from("default"),
    );
    let mut c = Core::with_account_book(
        CoreConfig {
            risk: RiskConfig {
                max_order_notional: dec!(100),
                ..Default::default()
            },
            dry_seed_balance: SEED,
            auto_exits_enabled: false,
            // These fixtures PLACE orders: the default config's relative
            // data/ log paths would append into whatever the process cwd
            // is at this instant (parallel tests move it), re-dirtying
            // the reset tests' self-check. A fixture that trades carries
            // no durable logs — same contract as `hot_reload_fixture`.
            trade_log_path: None,
            order_log_path,
            position_log_path: None,
            ..Default::default()
        },
        book,
    );
    c.enable_engine(crate::engine::Engine::new(crate::engine::EngineConfig {
        scanner: crate::scanner::ScannerConfig {
            assets: vec!["BTC".into()],
            round_duration_sec: 900,
            min_round_age_sec: 0,
            min_time_left_sec: 0,
        },
        ..Default::default()
    }));
    let now: i64 = 1_000_000;
    let slot = now / 1000 / 900;
    let market = |venue: &str, up: &str, down: &str| crate::model::CryptoMarket {
        asset: "BTC".into(),
        condition_id: format!("cond-{venue}"),
        question_id: "q".into(),
        up_token_id: up.into(),
        down_token_id: down.into(),
        up_price: dec!(0.6),
        down_price: dec!(0.4),
        expires_at_ms: (slot + 1) * 900 * 1000,
        round_slot: slot,
        round_duration_sec: 900,
        archive_verdict: false,
        venue: venue.into(),
        neg_risk: false,
        question: "?".into(),
    };
    c.engine_on_data(
        crate::engine::DataEvent::RoundMarkets {
            markets: vec![
                market("polymarket", "poly-up", "poly-down"),
                market("kalshi", "kal-up", "kal-down"),
            ],
            now_ms: now,
        },
        now,
    );
    c
}

fn bound_order(token: &str, key: &str) -> OrderRequest {
    OrderRequest {
        token_id: token.into(),
        condition_id: "cond".into(),
        side: Side::Buy,
        mode: FillPolicy::Maker,
        price: dec!(0.40),
        size: dec!(1),
        internal_key: key.into(),
        strategy: STRATEGY.into(),
        asset: "BTC".into(),
        direction: "up".into(),
        round_slot: 1,
        account_id: crate::model::default_account_id(),
    }
}

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

// ── B1c: refuse_unsubscribed_duration — the admission gate's teeth ─────

#[test]
fn an_unsubscribed_family_order_is_refused_with_account_limit() {
    // The gate's core verdict: with a declared subscription set that does
    // NOT contain the order's (venue, duration) family, the order is
    // refused with the AccountLimit vocabulary, naming the family. The
    // chain position (right after the venue binding guard, before risk)
    // means the refusal surfaces before any notional is reserved.
    let mut c = venue_bound_core();
    // The set declares ONLY polymarket 15m; the kalshi token belongs to
    // (kalshi, 900) — unsubscribed. The kalshi attempt runs from the
    // UNBOUND account (the venue binding guard would refuse it first
    // for the bound one — earlier gate in the chain, unchanged); the
    // duration gate fires for it because the empty-venue account still
    // reads the shared staged set.
    c.stage_duration_subscriptions_for_test(HashSet::from([("polymarket".to_string(), 900)]));
    let mut kal = bound_order("kal-up", "k1");
    kal.account_id = crate::account::AccountId::from("paper");
    let err = c.place(kal, 0, 1).unwrap_err();
    assert_eq!(err.code, CoreErrorCode::AccountLimit, "{err:?}");
    assert!(err.message.contains("unsubscribed round family"), "{err}");
    assert!(
        err.message.contains("`kalshi`") && err.message.contains("900"),
        "the refusal names the offending family: {err}"
    );
    assert!(c.list_orders().is_empty(), "nothing reached the book");
    // A family the account DID subscribe passes (from the bound account,
    // its own venue and its declared family).
    c.place(bound_order("poly-up", "k2"), 0, 2).unwrap();
    assert_eq!(c.list_orders().len(), 1);
}

#[test]
fn a_close_intent_is_exempt_from_the_duration_gate() {
    // The #174 hatch, copied verbatim: a position admitted while the
    // family was subscribed must never be trapped by an unsubscribe —
    // closing intents always pass the family gate, even for a family the
    // account never declared.
    let mut c = venue_bound_core();
    // The declared set contains nothing for polymarket's own family — a
    // non-close BUY on poly-up would be refused. The token still belongs
    // to the bound account's own venue, so the earlier venue binding
    // guard has no verdict either: only the duration gate could stop
    // this order, and the close intent must sail past it.
    c.stage_duration_subscriptions_for_test(HashSet::from([("kalshi".to_string(), 900)]));
    // A close SELL on a polymarket token (no open position — the exit for
    // a token with none flows through, same as the cross-account guard's
    // precedent; the gate under test must not be the thing that stops it).
    let mut close = bound_order("poly-up", "exit:probe-1");
    close.side = Side::Sell;
    c.place(close, 0, 1).unwrap();
    assert_eq!(c.list_orders().len(), 1, "the close passed the family gate");
}

#[test]
fn an_empty_subscription_set_admits_everything() {
    // Today's behaviour (and the zero-config shape): no declared set ⇒
    // no restriction ⇒ every family passes. The production gate reads a
    // constant empty set until B1d wires the real assembly, so this is
    // also the byte-compatibility proof for every existing corpus.
    let mut c = venue_bound_core();
    // No staging: the gate reads the empty set. Both families pass —
    // the bound account through its own venue, the unbound one anywhere.
    c.place(bound_order("poly-up", "k1"), 0, 1).unwrap();
    let mut kal = bound_order("kal-up", "k2");
    kal.account_id = crate::account::AccountId::from("paper");
    c.place(kal, 0, 2).unwrap();
    assert_eq!(c.list_orders().len(), 2);
    // Explicitly staged EMPTY set: the same verdict.
    let mut c2 = venue_bound_core();
    c2.stage_duration_subscriptions_for_test(HashSet::new());
    c2.place(bound_order("poly-up", "k3"), 0, 1).unwrap();
    assert_eq!(c2.list_orders().len(), 1);
}

#[test]
fn a_duration_mismatch_within_one_venue_is_refused() {
    // The gate is a (venue, duration) PAIR check, not a venue check:
    // polymarket 15m subscribed does not admit a polymarket 1h token —
    // the duration axis is the new dimension this gate adds.
    let mut c = venue_bound_core();
    c.stage_duration_subscriptions_for_test(HashSet::from([("polymarket".to_string(), 900)]));
    // A 1h polymarket market, same venue, different family.
    let now = 1_000_000;
    let end1h = 2 * 3600 * 1000;
    let market = crate::model::CryptoMarket {
        asset: "BTC".into(),
        condition_id: "cond-poly-3600".into(),
        question_id: "q".into(),
        up_token_id: "poly-1h-up".into(),
        down_token_id: "poly-1h-down".into(),
        up_price: dec!(0.6),
        down_price: dec!(0.4),
        expires_at_ms: end1h,
        round_slot: end1h / 1000 / 3600,
        round_duration_sec: 3600,
        archive_verdict: false,
        venue: "polymarket".into(),
        neg_risk: false,
        question: "?".into(),
    };
    c.engine_on_data(
        crate::engine::DataEvent::RoundMarkets {
            markets: vec![market],
            now_ms: now,
        },
        now,
    );
    let mut order = bound_order("poly-1h-up", "k1");
    order.token_id = "poly-1h-up".into();
    let err = c.place(order, 0, 1).unwrap_err();
    assert_eq!(err.code, CoreErrorCode::AccountLimit, "{err:?}");
    assert!(
        err.message.contains("3600"),
        "the refusal names the 1h family: {err}"
    );
}

/// The order's venue tag is the venue binding of the account it was
/// placed from — the same fact `account.list` reports. An order placed
/// by an account bound to kalshi reads `venue: "kalshi"`; an order
/// placed by an UNBOUND account carries NO venue key at all on the wire
/// (empty string is skipped), so the panel's venue filter keeps the row
/// visible (an untagged row must not be silently dropped).
#[test]
fn order_views_carry_the_account_venue_binding() {
    let mut c = venue_bound_core();
    // `default` is bound to polymarket: its order tags polymarket.
    c.place(bound_order("poly-up", "k1"), 0, 1).unwrap();
    // `paper` is unbound: its order carries no venue tag.
    let mut paper = bound_order("kal-up", "k2");
    paper.account_id = crate::account::AccountId::from("paper");
    c.place(paper, 0, 2).unwrap();
    assert_eq!(c.list_orders().len(), 2);

    let mut rows = c.list_orders();
    rows.sort_by(|a, b| a.internal_key.cmp(&b.internal_key));
    assert_eq!(rows[0].internal_key, "k1");
    assert_eq!(
        rows[0].venue, "polymarket",
        "the tag is the placing account's binding, not the token's venue"
    );
    assert_eq!(rows[1].internal_key, "k2");
    assert_eq!(rows[1].venue, "", "an unbound account's order is untagged");
    // The wire contract: the empty tag is ABSENT (skip_serializing_if),
    // the non-empty one present. The panel's filterByVenue keeps rows
    // without the key visible.
    let wire: Vec<serde_json::Value> = rows
        .iter()
        .map(|o| serde_json::to_value(o).unwrap())
        .collect();
    assert_eq!(
        wire[0]["venue"],
        serde_json::json!("polymarket"),
        "the bound order's venue key is present"
    );
    assert!(
        wire[1].get("venue").is_none(),
        "the unbound order's wire row carries NO venue key: {}",
        wire[1]
    );
}

/// B1d (轮次多族): the gate's subscription set assembles from the
/// ACCOUNT's own `rounds` declaration (the real seam, not the test
/// override). A bound account's set is (venue × every declared
/// duration); a declared family the account did not list is refused.
#[test]
fn the_duration_gate_assembles_from_the_account_rounds_declaration() {
    let mut c = venue_bound_core();
    // `default` is bound to polymarket; declare ONLY the 900s family —
    // every poly market in this fixture IS 900s, so its orders sail.
    c.accounts_mut()
        .set_rounds(
            &crate::account::AccountId::from("default"),
            vec![900],
            crate::account::config::TEST_ALLOWED_ROUNDS,
        )
        .unwrap();
    c.place(bound_order("poly-up", "k1"), 0, 1).unwrap();
    assert_eq!(c.list_orders().len(), 1, "the declared family passes");
    // A poly 1h token is (polymarket, 3600) — declared? No. Refused.
    let now = 1_000_000;
    let end1h = 2 * 3600 * 1000;
    let m = crate::model::CryptoMarket {
        asset: "BTC".into(),
        condition_id: "cond-poly-1h".into(),
        question_id: "q".into(),
        up_token_id: "poly-1h-up".into(),
        down_token_id: "poly-1h-down".into(),
        up_price: dec!(0.6),
        down_price: dec!(0.4),
        expires_at_ms: end1h,
        round_slot: end1h / 1000 / 3600,
        round_duration_sec: 3600,
        archive_verdict: false,
        venue: "polymarket".into(),
        neg_risk: false,
        question: "?".into(),
    };
    c.engine_on_data(
        crate::engine::DataEvent::RoundMarkets {
            markets: vec![m],
            now_ms: now,
        },
        now,
    );
    let err = c.place(bound_order("poly-1h-up", "k2"), 0, 2).unwrap_err();
    assert_eq!(err.code, CoreErrorCode::AccountLimit, "{err:?}");
    assert!(err.message.contains("3600"), "{err}");
    // Declaring the 1h family too re-opens it — the assembly is the
    // declaration's product, venue × every listed duration.
    c.accounts_mut()
        .set_rounds(
            &crate::account::AccountId::from("default"),
            vec![900, 3600],
            crate::account::config::TEST_ALLOWED_ROUNDS,
        )
        .unwrap();
    c.place(bound_order("poly-1h-up", "k3"), 0, 3).unwrap();
    assert_eq!(c.list_orders().len(), 2);
}

/// B1d: an UNBOUND account's `rounds` declaration speaks for EVERY
/// venue the live book currently labels at that duration — the set is
/// the declaration expanded across the live venues, so the account
/// trades its declared families everywhere without naming venues.
#[test]
fn an_unbound_accounts_rounds_span_every_live_venue_at_the_declared_duration() {
    let mut c = venue_bound_core();
    // `paper` is unbound; it declares 900s only.
    c.accounts_mut()
        .set_rounds(
            &crate::account::AccountId::from("paper"),
            vec![900],
            crate::account::config::TEST_ALLOWED_ROUNDS,
        )
        .unwrap();
    // Both live venues carry 900s markets: both are admitted.
    let mut p1 = bound_order("poly-up", "k1");
    p1.account_id = crate::account::AccountId::from("paper");
    c.place(p1, 0, 1).unwrap();
    let mut p2 = bound_order("kal-up", "k2");
    p2.account_id = crate::account::AccountId::from("paper");
    c.place(p2, 0, 2).unwrap();
    assert_eq!(c.list_orders().len(), 2, "900s on both venues passes");
    // The poly 1h family is not declared: refused from the same account.
    let now = 1_000_000;
    let end1h = 2 * 3600 * 1000;
    let m = crate::model::CryptoMarket {
        asset: "BTC".into(),
        condition_id: "cond-poly-1h".into(),
        question_id: "q".into(),
        up_token_id: "poly-1h-up".into(),
        down_token_id: "poly-1h-down".into(),
        up_price: dec!(0.6),
        down_price: dec!(0.4),
        expires_at_ms: end1h,
        round_slot: end1h / 1000 / 3600,
        round_duration_sec: 3600,
        archive_verdict: false,
        venue: "polymarket".into(),
        neg_risk: false,
        question: "?".into(),
    };
    c.engine_on_data(
        crate::engine::DataEvent::RoundMarkets {
            markets: vec![m],
            now_ms: now,
        },
        now,
    );
    let mut p3 = bound_order("poly-1h-up", "k3");
    p3.account_id = crate::account::AccountId::from("paper");
    let err = c.place(p3, 0, 3).unwrap_err();
    assert_eq!(err.code, CoreErrorCode::AccountLimit, "{err:?}");
    // A bound account with an EMPTY rounds set stays unrestricted —
    // the historical shape (no declaration = no gate).
    c.accounts_mut()
        .set_rounds(
            &crate::account::AccountId::from("default"),
            Vec::new(),
            crate::account::config::TEST_ALLOWED_ROUNDS,
        )
        .unwrap();
    c.place(bound_order("poly-1h-up", "k4"), 0, 4).unwrap();
    assert_eq!(c.list_orders().len(), 3);
}

/// The venue tag is a DERIVED read, never stored: the persisted order
/// log stays byte-identical to its pre-#427-orders shape (empty tag is
/// skipped on write), and a rebinding changes what `orders.list` reads
/// without touching any stored row.
#[test]
fn the_order_venue_tag_is_derived_not_stored() {
    let dir = std::env::temp_dir().join(format!(
        "bk-order-venue-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let mut c = venue_bound_core_opts(Some(
        dir.join("orders.jsonl").to_string_lossy().into_owned(),
    ));
    c.place(bound_order("poly-up", "k1"), 0, 1).unwrap();

    // The persisted line carries no venue key (skip_serializing_if).
    let line = std::fs::read_to_string(dir.join("orders.jsonl")).unwrap();
    let doc: serde_json::Value = serde_json::from_str(line.lines().next().unwrap()).unwrap();
    assert!(
        doc.get("venue").is_none(),
        "the persisted order line must not gain a venue key: {doc}"
    );

    // Rebinding is a fact about the ACCOUNT, not the order: the same
    // stored order now reads with the new tag, nothing rewritten.
    c.accounts_mut()
        .set_venue(&crate::account::AccountId::from("default"), "kalshi", 2)
        .unwrap();
    assert_eq!(c.list_orders()[0].venue, "kalshi");
    let line2 = std::fs::read_to_string(dir.join("orders.jsonl")).unwrap();
    assert_eq!(
        line, line2,
        "the order log is untouched by a venue rebinding"
    );
}

/// RFC-661 §五.5 回放半边（M1）：单键时代（venue A）落盘的 live 订单
/// 重启恢复（`restore_orders` → OME restore）后，其执行路由仍走
/// `execution_venue_of` 的 token-only 解析。双键时代同一字节 token
/// 若两所各持一行（A→B 换 venue：B 的批次后到，token-only 查找变
/// 歧义），恢复的单绝不能按插入序被路由到先到的 A 执行器 —— 歧义
/// 解析为空（UNTAGGED），UNTAGGED 单只进 active-plugin 桶；账户绑定
/// （B）的单仍路由到它自己的执行器。A→B 不串账，B→A 对称（同一规
/// 则，插入序无关）。
#[test]
fn a_restored_legacy_order_routes_by_binding_not_by_insertion_order() {
    let dir = std::env::temp_dir().join(format!(
        "bk-restore-dualkey-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();

    // Era 1 (venue A = polymarket only): a live order rests on the shared
    // token and is persisted.
    let mut c = venue_bound_core_opts(Some(
        dir.join("orders.jsonl").to_string_lossy().into_owned(),
    ));
    c.place(bound_order("poly-up", "k1"), 0, 1).unwrap();
    assert_eq!(c.list_orders().len(), 1);
    drop(c);

    // Era 2: the core reboots and restores. The era-2 fixture's round
    // list still names ONLY polymarket's row for the token: the
    // token-only lookup has a UNIQUE candidate and resolves to it — the
    // legacy archive replays byte-identically. (With the engine fully
    // down the same lookup is UNTAGGED `""` — every executor sees the
    // order, the pre-routing bridge behaviour.)
    let mut reborn = venue_bound_core_opts(Some(
        dir.join("orders.jsonl").to_string_lossy().into_owned(),
    ));
    assert_eq!(reborn.restore_orders(), 1, "the live order is re-adopted");
    assert_eq!(
        reborn.pending_unbound_for_venue("polymarket").len(),
        1,
        "a unique candidate routes the restored order to its venue's drain"
    );
    assert!(reborn.pending_unbound_for_venue("kalshi").is_empty());

    // A→B: the B venue (kalshi here, any sibling) now lands a batch
    // carrying byte-identical tokens. The token-only lookup turns
    // AMBIGUOUS — the restored order must NOT ride the insertion-order
    // first-hit into the wrong executor; the ACCOUNT's binding
    // (polymarket) still owns the order through the dual-key row.
    let now = 1_000_000;
    let slot = now / 1000 / 900;
    let twin = crate::model::CryptoMarket {
        asset: "BTC".into(),
        condition_id: "cond-twin".into(),
        question_id: "q".into(),
        up_token_id: "poly-up".into(),
        down_token_id: "poly-down".into(),
        up_price: dec!(0.6),
        down_price: dec!(0.4),
        expires_at_ms: (slot + 1) * 900 * 1000,
        round_slot: slot,
        round_duration_sec: 900,
        archive_verdict: false,
        venue: "twinvenue".into(),
        neg_risk: false,
        question: "?".into(),
    };
    reborn.engine_on_data(
        crate::engine::DataEvent::RoundMarkets {
            markets: vec![twin],
            now_ms: now + 1,
        },
        now + 1,
    );
    // The binding reaches the engine's scope map on the evaluate tick —
    // the same sync the runtime relies on.
    reborn.engine_evaluate(now + 2);
    assert_eq!(
        reborn.execution_venue_of("default", "poly-up"),
        "polymarket",
        "the account binding reads its OWN dual-key row — ambiguity of the \
         token-only lookup never re-routes a bound account"
    );
    assert_eq!(
        reborn.pending_unbound_for_venue("polymarket").len(),
        1,
        "the bound account's restored order drains to ITS venue"
    );
    assert!(
        reborn.pending_unbound_for_venue("twinvenue").is_empty(),
        "the sibling executor never sees another venue's bound order"
    );
    // The ambiguity itself is real and refused (never first-hit): an
    // UNBOUND account's token-only resolution gets nothing.
    assert_eq!(
        reborn.execution_venue_of("paper", "poly-up"),
        "",
        "an ambiguous token resolves for no unbound account"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// #463: the close-credential journal survives a restart. A push is
/// journalled, a consumption tombstones it, and a fresh Core built on the
/// same data dir replays exactly the still-live credentials — the window
/// between a close fill and the venue's FAILED no longer dies with the
/// process.
#[test]
fn the_close_credential_journal_survives_a_restart() {
    let dir = std::env::temp_dir().join(format!(
        "bk-cred-journal-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let order_log = dir.join("orders.jsonl");
    let journal = dir.join("close-credentials.jsonl");

    let cred = |shares: rust_decimal::Decimal| CloseCredential {
        restored: crate::position::OpenPosition {
            id: "hft-1".into(),
            strategy: "spread_arb".into(),
            asset: "BTC".into(),
            direction: crate::model::SignalDirection::Up,
            token_id: "tok".into(),
            condition_id: "cond".into(),
            entry_price: dec!(0.43),
            current_price: dec!(0.45),
            prev_price: dec!(0.44),
            shares,
            cost_usd: dec!(4.3),
            was_maker_entry: true,
            entry_fee_pct: rust_decimal::Decimal::ZERO,
            entry_role: crate::model::OrderRole::Maker,
            exit_role: crate::model::OrderRole::Pending,
            target_exit_price: None,
            entered_at_ms: 1000,
            expires_at_ms: 900_000,
            account_id: crate::model::default_account_id(),
            venue: String::new(),
            partner_token_id: String::new(),
            last_book_ts: 0,
            round_duration_sec: 0,
            state: crate::exit_policy::ExitState::new(dec!(0.43), 1000),
            flows: crate::position::CashFlows {
                entry_cost_usd: dec!(4.3),
                opened_shares: shares,
                ..Default::default()
            },
        },
        closed: crate::position::ClosedPosition {
            id: "hft-1".into(),
            strategy: "spread_arb".into(),
            asset: "BTC".into(),
            direction: crate::model::SignalDirection::Up,
            token_id: "tok".into(),
            condition_id: "cond".into(),
            entry_price: dec!(0.43),
            exit_price: dec!(0.50),
            shares,
            cost_usd: dec!(4.3),
            was_maker_entry: true,
            was_maker_exit: false,
            entry_fee_pct: rust_decimal::Decimal::ZERO,
            exit_fee_pct: rust_decimal::Decimal::ZERO,
            pnl_usd: dec!(0.7),
            pnl_pct: dec!(16.27),
            net_pnl_usd: dec!(0.7),
            net_pnl_pct: dec!(16.27),
            high_pnl_pct: dec!(0),
            low_pnl_pct: dec!(0),
            hold_time_sec: 30,
            exit_reason: crate::model::ExitReason::TakeProfit,
            entered_at_ms: 1000,
            exited_at_ms: 4000,
            entry_role: crate::model::OrderRole::Maker,
            exit_role: crate::model::OrderRole::Taker,
            dust_shares: rust_decimal::Decimal::ZERO,
            account_id: crate::model::default_account_id(),
            venue: String::new(),
        },
        streak_before: 2,
        account_streak_before: 1,
    };

    // Boot one core with persistence on, push two credentials, consume one.
    let mut core = Core::new(CoreConfig {
        order_log_path: Some(order_log.to_string_lossy().to_string()),
        ..Default::default()
    });
    core.push_close_credential("trade-a".into(), cred(dec!(10)));
    core.push_close_credential("trade-b".into(), cred(dec!(4)));
    assert_eq!(core.close_credentials.len(), 2);

    // Consume trade-b the way a FAILED status would: tombstone + remove.
    let pos = core
        .close_credentials
        .iter()
        .position(|(tid, _)| tid == "trade-b")
        .unwrap();
    let _ = core.close_credentials.remove(pos).unwrap();
    if let Some(path) = &core.close_credential_log {
        crate::jsonl::append(
            path,
            &CloseCredentialLine::Consumed {
                trade_id: "trade-b".into(),
            },
        );
    }

    // "Restart": a fresh Core on the same data dir, recovery re-run the way
    // the IPC server does at boot. Only trade-a may re-enter the queue;
    // the tombstone must keep trade-b out.
    let mut reborn = Core::new(CoreConfig {
        order_log_path: Some(order_log.to_string_lossy().to_string()),
        ..Default::default()
    });
    assert!(
        reborn.close_credentials.is_empty(),
        "the constructor reads NOTHING — replay is recovery, not boot"
    );
    reborn.restore_orders();
    assert_eq!(
        reborn.close_credentials.len(),
        1,
        "exactly the unconsumed credential replays"
    );
    assert_eq!(reborn.close_credentials[0].0, "trade-a");
    assert_eq!(reborn.close_credentials[0].1.restored.shares, dec!(10));

    // The journal file exists beside the order log and every line parses.
    let text = std::fs::read_to_string(&journal).unwrap();
    let lines = text.lines().filter(|l| !l.trim().is_empty()).count();
    assert!(lines >= 3, "push, push, tombstone all landed: {lines}");

    std::fs::remove_dir_all(&dir).ok();
}

// ── PR H (账户筛选修复): per-(strategy, account) accounting ──────────────

/// The audit `req` helper, retargeted at an account. Everything else
/// (token, strategy, slot) stays shared — the exact shape that used to
/// merge two accounts into one accounting row.
fn req_for(
    side: Side,
    mode: FillPolicy,
    price: Decimal,
    size: Decimal,
    key: &str,
    account: &str,
) -> OrderRequest {
    let mut r = req(side, mode, price, size, key);
    r.account_id = crate::account::AccountId::from(account);
    r
}

/// Both accounts trade the SAME strategy on the SAME token. Before PR H
/// the accounting key was the bare strategy name, so the second account's
/// fills landed on the FIRST account's counters (merged PnL, doubled
/// closed_trades) and `strategy_stats` could not tell the accounts apart.
/// The fix keys the map by (strategy, \u{1}, account); each slice must
/// carry only its own account's activity.
#[test]
fn two_accounts_same_strategy_keep_separate_accounting_rows() {
    let mut c = two_book_core();

    // default opens at 0.40 and closes at 0.50 (+1.00 gross before fees).
    open_maker_position(&mut c, dec!(0.40), dec!(10), 1);
    c.book_snapshot("tok", vec![(dec!(0.50), dec!(500))], vec![], 3);
    c.place(
        req_for(
            Side::Sell,
            FillPolicy::Taker,
            dec!(0.50),
            dec!(10),
            "exit:d1",
            "default",
        ),
        0,
        4,
    )
    .unwrap();
    assert!(c.positions().open_positions().is_empty());

    // paper opens the same token at 0.60 and closes at 0.40 (−2.00 gross).
    let mut paper_open = req_for(
        Side::Buy,
        FillPolicy::Maker,
        dec!(0.60),
        dec!(10),
        "entry:p1",
        "paper",
    );
    paper_open.internal_key = "entry:p1:paper".into(); // distinct slot key
    let (pid, _) = c.place_pending(paper_open, 10).unwrap();
    c.confirm_live(&pid, 10).unwrap();
    c.ingest_fill(
        fill(
            &pid,
            "e-p1",
            Side::Buy,
            dec!(0.60),
            dec!(10),
            FillStatus::Confirmed,
        ),
        11,
    )
    .unwrap();
    c.book_snapshot("tok", vec![(dec!(0.40), dec!(500))], vec![], 13);
    c.place(
        req_for(
            Side::Sell,
            FillPolicy::Taker,
            dec!(0.40),
            dec!(10),
            "exit:p1",
            "paper",
        ),
        0,
        14,
    )
    .unwrap();

    // Each account's slice: exactly its own trade, its own net.
    let dflt = c
        .strategy_accounting
        .get(&strategy_acc_key(STRATEGY, "default"))
        .expect("default's accounting row");
    assert_eq!(dflt.closed_trades, 1, "default sees only its own close");
    assert_eq!(dflt.wins, 1);
    assert!(
        dflt.net_pnl_usd > Decimal::ZERO,
        "default won: {}",
        dflt.net_pnl_usd
    );

    let paper = c
        .strategy_accounting
        .get(&strategy_acc_key(STRATEGY, "paper"))
        .expect("paper's accounting row");
    assert_eq!(paper.closed_trades, 1, "paper sees only its own close");
    assert_eq!(paper.losses, 1);
    assert!(
        paper.net_pnl_usd < Decimal::ZERO,
        "paper lost: {}",
        paper.net_pnl_usd
    );

    // THE TOOTH: with a bare-name key this lookup returned default's row
    // (first writer) and paper's activity was invisible/doubled. The two
    // slices must never be the same row, and the folded strategy_stats
    // row must name BOTH accounts.
    assert_ne!(
        strategy_acc_key(STRATEGY, "default"),
        strategy_acc_key(STRATEGY, "paper")
    );
    let rows = c.strategy_stats();
    let row = rows
        .iter()
        .find(|r| r["name"] == STRATEGY)
        .expect("one folded row per strategy name");
    let accounts: Vec<String> = serde_json::from_value(row["accounts"].clone()).unwrap();
    assert_eq!(accounts, vec!["default".to_string(), "paper".to_string()]);
    assert_eq!(row["closedTrades"], 2, "the fold sums both slices");
    assert_eq!(row["wins"], 1);
    assert_eq!(row["losses"], 1);

    // The per-account view: paper's page shows paper's slice only.
    let for_paper = c.strategy_stats_for_account("paper");
    let prow = for_paper
        .iter()
        .find(|r| r["name"] == STRATEGY)
        .expect("the strategy row on paper's page");
    assert_eq!(prow["closedTrades"], 1);
    assert_eq!(prow["losses"], 1);
    assert_eq!(prow["wins"], 0);
}

/// 账户↔venue 解耦（投影层牙）: two accounts hold the SAME token at the
/// SAME time. The fill projection keys positions by (ACCOUNT, token) —
/// the old token-only merge landed paper's fill onto default's row
/// (one merged position, paper's book empty), so this tooth holds both
/// legs simultaneously and demands: two rows, each with its own basis,
/// each paid from its own ledger, and each account's close resolving to
/// its OWN row while the sibling's survives untouched.
#[test]
fn two_accounts_holding_the_same_token_keep_separate_position_rows() {
    let mut c = two_book_core();

    // default holds 10 @ 0.40 (cost 4); paper holds 10 @ 0.60 (cost 6) —
    // same token, overlapping holds. Maker fills: no fees anywhere.
    open_maker_position(&mut c, dec!(0.40), dec!(10), 1);
    let mut paper_open = req_for(
        Side::Buy,
        FillPolicy::Maker,
        dec!(0.60),
        dec!(10),
        "entry:p1",
        "paper",
    );
    paper_open.internal_key = "entry:p1:paper".into(); // distinct slot key
    let (pid, _) = c.place_pending(paper_open, 2).unwrap();
    c.confirm_live(&pid, 2).unwrap();
    c.ingest_fill(
        fill(
            &pid,
            "e-p1",
            Side::Buy,
            dec!(0.60),
            dec!(10),
            FillStatus::Confirmed,
        ),
        3,
    )
    .unwrap();

    // THE TOOTH: two rows, one per account, each with its own basis.
    let rows = c.positions().open_positions();
    assert_eq!(
        rows.len(),
        2,
        "same token, two accounts: two rows, never one merged position"
    );
    let dflt = rows
        .iter()
        .find(|p| p.account_id.as_str() == "default")
        .expect("default's own row");
    let paper = rows
        .iter()
        .find(|p| p.account_id.as_str() == "paper")
        .expect("paper's own row");
    assert_eq!(
        dflt.entry_price,
        dec!(0.40),
        "default's basis is default's fill"
    );
    assert_eq!(
        paper.entry_price,
        dec!(0.60),
        "paper's basis is paper's fill — a token-only merge would have stamped 0.60 onto \
         default's (first) row and left paper with nothing"
    );
    // Each book paid for its own leg.
    assert_eq!(
        c.accounts
            .ledger_of(&crate::account::AccountId::from("default"))
            .unwrap()
            .balance(),
        SEED - dec!(4),
    );
    assert_eq!(
        c.accounts
            .ledger_of(&crate::account::AccountId::from("paper"))
            .unwrap()
            .balance(),
        SEED - dec!(6),
    );

    // The RIGHT account's close resolves to ITS row: paper closes at 0.50
    // (−1.00 gross, no fee) and default's 0.40 row survives untouched.
    let (sid, _) = c
        .place_pending(
            req_for(
                Side::Sell,
                FillPolicy::Maker,
                dec!(0.50),
                dec!(10),
                "exit:p1",
                "paper",
            ),
            4,
        )
        .unwrap();
    c.confirm_live(&sid, 4).unwrap();
    c.ingest_fill(
        fill(
            &sid,
            "x-p1",
            Side::Sell,
            dec!(0.50),
            dec!(10),
            FillStatus::Confirmed,
        ),
        5,
    )
    .unwrap();
    let rows = c.positions().open_positions();
    assert_eq!(rows.len(), 1, "paper's close removed exactly paper's row");
    assert_eq!(rows[0].account_id.as_str(), "default");
    assert_eq!(
        rows[0].entry_price,
        dec!(0.40),
        "default's row is the survivor, byte-for-byte its own basis"
    );
    assert_eq!(
        c.accounts
            .ledger_of(&crate::account::AccountId::from("paper"))
            .unwrap()
            .balance(),
        SEED - dec!(1),
        "paper's close settled into paper's book"
    );
    assert_eq!(
        c.accounts
            .ledger_of(&crate::account::AccountId::from("default"))
            .unwrap()
            .balance(),
        SEED - dec!(4),
        "default's cash never moved for paper's trade"
    );
    // And the close landed on PAPER's accounting slice.
    let acc = c
        .strategy_accounting
        .get(&strategy_acc_key(STRATEGY, "paper"))
        .expect("paper's accounting row");
    assert_eq!(acc.closed_trades, 1);
    assert_eq!(acc.losses, 1);
    assert!(
        !c.strategy_accounting
            .contains_key(&strategy_acc_key(STRATEGY, "default")),
        "default has no closed trade yet — its slice must not exist"
    );
}

/// `trades.summary` with an `accountId` filters the row set before
/// aggregating — the wallet page's headline must belong to the active
/// account, not to every account at once. The no-arg call keeps the
/// persisted global headline byte-for-byte.
#[test]
fn trade_summary_scopes_to_one_account() {
    let mut c = two_book_core();

    open_maker_position(&mut c, dec!(0.40), dec!(10), 1);
    c.book_snapshot("tok", vec![(dec!(0.50), dec!(500))], vec![], 3);
    c.place(
        req_for(
            Side::Sell,
            FillPolicy::Taker,
            dec!(0.50),
            dec!(10),
            "exit:d1",
            "default",
        ),
        0,
        4,
    )
    .unwrap();

    let mut paper_open = req_for(
        Side::Buy,
        FillPolicy::Maker,
        dec!(0.60),
        dec!(10),
        "entry:p1",
        "paper",
    );
    paper_open.internal_key = "entry:p1:paper".into();
    let (pid, _) = c.place_pending(paper_open, 10).unwrap();
    c.confirm_live(&pid, 10).unwrap();
    c.ingest_fill(
        fill(
            &pid,
            "e-p1",
            Side::Buy,
            dec!(0.60),
            dec!(10),
            FillStatus::Confirmed,
        ),
        11,
    )
    .unwrap();
    c.book_snapshot("tok", vec![(dec!(0.40), dec!(500))], vec![], 13);
    c.place(
        req_for(
            Side::Sell,
            FillPolicy::Taker,
            dec!(0.40),
            dec!(10),
            "exit:p1",
            "paper",
        ),
        0,
        14,
    )
    .unwrap();

    // Global: two closed trades (the persisted headline keeps folding
    // everything — the snapshot's `tradeSummary` shape is unchanged).
    assert_eq!(c.trade_summary(None)["totalTrades"], 2);

    // Scoped: one trade each, opposite signs.
    let d = c.trade_summary(Some("default"));
    assert_eq!(d["totalTrades"], 1);
    assert_eq!(d["wins"], 1);
    let p = c.trade_summary(Some("paper"));
    assert_eq!(p["totalTrades"], 1);
    assert_eq!(p["losses"], 1);
    assert!(
        p["totalNetPnl"].as_f64().unwrap() < 0.0,
        "paper's slice is the losing one: {}",
        p["totalNetPnl"]
    );

    // An unknown account answers an EMPTY summary, not the global one.
    let ghost = c.trade_summary(Some("ghost"));
    assert_eq!(ghost["totalTrades"], 0);
}

/// PR H: two scopes of the SAME strategy name (default + paper) must not
/// duplicate the `strategy.list` row — the panel table is per-NAME, and
/// the pre-fix kernel listed the name once per scope (the 伪影 bug). The
/// engine's raw list keeps both (registration order is a tie-break
/// elsewhere); the service layer is where the per-NAME projection lives.
#[test]
fn strategy_names_dedupe_across_account_scopes() {
    let mut c = two_book_core();
    let cfg = c.config.clone();
    cfg.install_engine(&mut c)
        .expect("test engine (no strategy_dir: registers nothing itself)");
    let engine = c.engine.as_mut().expect("test engine");
    crate::strategies::test_support::host_disabled(engine, Default::default(), Default::default());
    // The SAME name again, into the paper scope: the raw engine list now
    // repeats every name.
    let dup = crate::strategies::test_support::TestSpreadArb::new(
        crate::signal::TrendConfig::default(),
        crate::signal::SpreadArbConfig::default(),
    );
    engine
        .register_scoped_strategy(Box::new(dup), "test".into(), "paper")
        .expect("second scope of the same package");

    let raw = engine.supported_strategies();
    assert!(
        raw.len() > raw.iter().collect::<std::collections::BTreeSet<_>>().len(),
        "the raw engine list must repeat names across scopes for this test to have a tooth"
    );
    let names = c.strategy_names();
    let mut sorted = names.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(
        names, sorted,
        "strategy_names must answer each name exactly once: {names:?}"
    );
    // And the stats surface — same projection — shows one row per name.
    assert_eq!(
        c.strategy_stats().len(),
        names.len(),
        "one stats row per distinct strategy"
    );
}

/// 策略全局可见（2026-10 操作者裁决）：boot 把每个包注册 DISABLED，随后
/// 账户启用集把未指派的 scope 整拆（rebuild DROP）。行集合若只读活
/// scope，策略页就缩成「被启用的那几个」——面板的启用按钮（assignable
/// 集合 = `strategy.list`）再也无法点亮其余策略：死锁。本牙钉死：拆除后
/// `strategy_names` / `strategy_stats` 仍报**安装面**全量，拆掉的名字行
/// `accounts` 为空、source 保留 sticky 溯源；enable 集合照旧只数活 scope。
#[test]
fn torn_down_scopes_stay_visible_on_the_strategies_surface() {
    let mut c = two_book_core();
    let cfg = c.config.clone();
    cfg.install_engine(&mut c)
        .expect("test engine (no strategy_dir: registers nothing itself)");
    let engine = c.engine.as_mut().expect("test engine");
    // Three packages installed DISABLED (the boot scan's landing state).
    crate::strategies::test_support::host_disabled(engine, Default::default(), Default::default());
    assert_eq!(c.strategy_names().len(), 3, "installed = visible");
    assert_eq!(c.strategy_stats().len(), 3);

    // The account enablement set keeps ONE strategy: the rebuild's DROP
    // phase tears the other two flat scopes down (the boot fan-out's
    // exact move; held = [] — nothing is demoted). In-process core has no
    // accounts_config_path, so the persisting half is skipped — the book
    // write + the engine rebuild are the semantic body the persisting
    // wrapper shares (persistence has its own teeth in the config tests).
    let default_id = crate::account::AccountId::from("default");
    let held: Vec<String> = Vec::new();
    c.accounts
        .set_strategies(&default_id, vec!["spread_arb".into()], 1)
        .expect("the book write lands");
    let kept = c.rebuild_account_scopes(&default_id, &["spread_arb".to_string()], &held);
    assert!(kept.is_empty(), "nothing held ⇒ no exits-only promotion");

    // The visibility contract: every INSTALLED strategy stays on the
    // surface, enabled or not.
    let names = c.strategy_names();
    assert_eq!(
        names.len(),
        3,
        "torn-down scopes must stay visible (package_sources is the installed set): {names:?}"
    );
    assert_eq!(c.strategy_stats().len(), 3, "same projection for the stats");
    for row in c.strategy_stats() {
        let name = row["name"].as_str().expect("row name");
        let accounts: Vec<&str> = row["accounts"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        if name == "spread_arb" {
            assert_eq!(
                accounts,
                vec!["default"],
                "the enabled strategy names its scope"
            );
        } else {
            assert!(
                accounts.is_empty(),
                "torn-down {name} has no live scope ⇒ empty accounts, still visible"
            );
            let source = row["source"].as_str().unwrap_or("");
            assert_eq!(
                source, "test",
                "torn-down {name} keeps its sticky provenance, not a blank source"
            );
        }
    }
    // The engine's own enablement read is untouched: one live scope, on.
    assert_eq!(
        c.enabled_strategy_names(),
        vec!["spread_arb".to_string()],
        "visibility must not resurrect the torn-down scopes as running"
    );
}

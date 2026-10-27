#![cfg(test)]

//! B1b: the round-duration family reaches the durable records — the placed
//! order carries its family stamp, the opened position carries its own, the
//! legacy rows backfill idempotently at the expiry consumer, and the held
//! position view derives its slot on the POSITION'S OWN family grid.

use super::*;
use rust_decimal_macros::dec;

const SEED: Decimal = dec!(1000);
const STRATEGY: &str = "acc";

/// Two live round markets on two families (polymarket 900s, kalshi 3600s),
/// engine enabled, accounts seeded — the B1b write paths have something
/// real to resolve against. `two_family_core()` is the double-family
/// deployment (polymarket 900s + kalshi 3600s); `single_family_core()`
/// feeds BOTH venues at 900s — the pre-B1b world the equivalence test
/// replays.
fn two_family_core() -> Core {
    family_core(&[("polymarket", "poly", 900), ("kalshi", "kal", 3600)])
}

fn single_family_core() -> Core {
    family_core(&[("polymarket", "poly", 900), ("kalshi", "kal", 900)])
}

fn family_core(families: &[(&str, &str, i64)]) -> Core {
    let book = crate::account::AccountLedgers::new(
        vec![crate::account::Account {
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
        }],
        crate::model::default_account_id(),
    );
    let mut c = Core::with_account_book(
        CoreConfig {
            risk: RiskConfig {
                max_order_notional: dec!(100),
                ..Default::default()
            },
            dry_seed_balance: SEED,
            auto_exits_enabled: false,
            trade_log_path: None,
            order_log_path: None,
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
    let market = |venue: &str, up: &str, down: &str, dur: i64| crate::model::CryptoMarket {
        asset: "BTC".into(),
        condition_id: format!("cond-{venue}-{dur}"),
        question_id: "q".into(),
        up_token_id: up.into(),
        down_token_id: down.into(),
        up_price: dec!(0.6),
        down_price: dec!(0.4),
        expires_at_ms: now / 1000 / dur * dur + dur * 1000,
        round_slot: now / 1000 / dur,
        round_duration_sec: dur,
        archive_verdict: false,
        venue: venue.into(),
        neg_risk: false,
        question: "?".into(),
    };
    let markets: Vec<crate::model::CryptoMarket> = families
        .iter()
        .map(|(venue, stem, dur)| {
            market(venue, &format!("{stem}-up"), &format!("{stem}-down"), *dur)
        })
        .collect();
    c.engine_on_data(
        crate::engine::DataEvent::RoundMarkets {
            markets,
            now_ms: now,
        },
        now,
    );
    c
}

fn order(token: &str, key: &str) -> OrderRequest {
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

fn open_params(token: &str, venue: &str, dur: i64, entered_ms: i64) -> crate::position::OpenParams {
    crate::position::OpenParams {
        strategy: STRATEGY.into(),
        asset: "BTC".into(),
        direction: crate::model::SignalDirection::Up,
        token_id: token.into(),
        condition_id: "cond".into(),
        account_id: crate::model::default_account_id(),
        venue: venue.into(),
        partner_token_id: String::new(),
        entry_price: dec!(0.40),
        expires_at_ms: entered_ms + dur * 1000,
        was_maker: false,
        target_exit_price: None,
        round_duration_sec: dur,
    }
}

#[test]
fn a_placed_order_carries_its_family_stamp() {
    let mut c = two_family_core();
    // The polymarket token's market declares 900s — the tracked order is
    // stamped 900 at submit (its family, matching the global grid here
    // only by coincidence).
    c.place(order("poly-up", "k1"), 0, 1).unwrap();
    let o = &c.list_orders()[0];
    assert_eq!(
        o.round_duration_sec, 900,
        "stamped from the market declaration"
    );
    // The kalshi token declares 3600s — DIFFERENT family, own stamp.
    c.place(order("kal-up", "k2"), 0, 2).unwrap();
    let o = &c.list_orders()[1];
    assert_eq!(
        o.round_duration_sec, 3600,
        "the second family stamps its own duration"
    );
    // A token belonging to NO live market falls back to the global grid.
    c.place(order("ghost-up", "k3"), 0, 3).unwrap();
    let o = &c.list_orders()[2];
    assert_eq!(o.round_duration_sec, 900, "no market: the global grid");
}

#[test]
fn an_opened_position_carries_its_family_stamp() {
    let mut c = two_family_core();
    // The production open path (project_fill_delta) resolves the family
    // through OpenParams; here the same shape opens unstamped-vs-stamped
    // rows and the manager carries them through.
    let p = c
        .positions
        .open(open_params("kal-up", "kalshi", 3600, 1_000), 1_000);
    assert_eq!(
        p.round_duration_sec, 3600,
        "the position opens with its family"
    );
    let p2 = c
        .positions
        .open(open_params("poly-up", "polymarket", 0, 1_100), 1_100);
    assert_eq!(
        p2.round_duration_sec, 0,
        "an unstamped caller opens a legacy row"
    );
}

#[test]
fn legacy_positions_backfill_idempotently_at_the_expiry_consumer() {
    let mut c = two_family_core();
    // A legacy snapshot: restored with the B1a serde default (0).
    let p = c
        .positions
        .open(open_params("poly-up", "polymarket", 0, 1_000), 1_000);
    assert_eq!(p.round_duration_sec, 0, "legacy row opens unstamped");
    // One sweep pass backfills it from the market declaration.
    c.run_expiry_sweep(1_001);
    let p = &c.positions.open_positions()[0];
    assert_eq!(
        p.round_duration_sec, 900,
        "backfilled from the market lookup"
    );
    // A second pass is a no-op (the non-0 stamp is never overwritten) —
    // and a sibling family row stays on its own family.
    c.positions
        .open(open_params("kal-up", "kalshi", 3600, 1_100), 1_100);
    c.run_expiry_sweep(1_002);
    let rows: Vec<(bool, i64)> = c
        .positions
        .open_positions()
        .iter()
        .map(|p| (p.token_id.contains("kal"), p.round_duration_sec))
        .collect();
    assert!(
        rows.contains(&(false, 900)) && rows.contains(&(true, 3600)),
        "each row keeps its own family: {rows:?}"
    );
}

#[test]
fn the_held_position_view_derives_the_slot_on_the_own_family_grid() {
    let mut c = two_family_core();
    // The view's slot formula works in SECONDS (entered_ms/1000/dur):
    // 3_600_000 ms is slot 1 on the position's OWN 3600s grid, slot 4 on
    // the 900s GLOBAL grid — the stamped row must read its own.
    c.positions
        .open(open_params("kal-up", "kalshi", 3600, 3_600_000), 3_600_000);
    let held = c.held_position_view();
    assert_eq!(held.len(), 1);
    assert_eq!(
        held[0].round_slot, 1,
        "slot derived on the position's OWN 3600s grid, not the 900s global"
    );
    // A legacy (unstamped) row still derives on the global grid.
    c.positions.open(
        open_params("poly-up", "polymarket", 0, 3_600_000),
        3_600_000,
    );
    let held = c.held_position_view();
    assert_eq!(held.len(), 2);
    let legacy_slot = held
        .iter()
        .find(|h| h.token_id.contains("poly"))
        .map(|h| h.round_slot)
        .unwrap();
    assert_eq!(legacy_slot, 4, "no stamp: the historic global-grid formula");
}

// ── 消费点 2/5：到期判定按各订单自己的族钟 ────────────────────────────
// （先红后绿：升级前这两处读 `self.config.round_duration_sec`，3600s
// 订单会在 1_800_000ms 被误杀 / 误放行——把 eff 改回全局即红。）

/// A 900s order and a 3600s order rest on slot 1 SIMULTANEOUSLY: the
/// sweep at 1.8M ms force-closes only the 900s order — the 3600s order
/// keeps living on its own family clock until its own hour ends.
#[test]
fn the_expiry_sweep_judges_each_order_on_its_own_family_clock() {
    let mut c = two_family_core();
    // Slot 1: [900s, 1800s) on the poly grid, [3600s, 7200s) on kalshi's.
    // Place both INSIDE their rounds (place stamps the family itself).
    let poly = c.place(order("poly-up", "k1"), 0, 901_000).unwrap().0;
    let kal = c.place(order("kal-up", "k2"), 0, 902_000).unwrap().0;
    assert_eq!(c.ome().get(&poly).unwrap().round_duration_sec, 900);
    assert_eq!(c.ome().get(&kal).unwrap().round_duration_sec, 3600);
    // 1.8M ms: the poly round has ENDED, the kalshi round has 30min left.
    c.run_expiry_sweep(1_800_000);
    assert_eq!(
        c.ome().get(&poly).unwrap().status,
        OrderStatus::ForceClosed,
        "the 900s order expires on its own clock"
    );
    assert_eq!(
        c.ome().get(&kal).unwrap().status,
        OrderStatus::Live,
        "the 3600s order must NOT be judged on the 900s grid"
    );
    // The sweep pass also stamped the survivor's family (lazy backfill).
    assert_eq!(
        c.ome().get(&kal).unwrap().round_duration_sec,
        3600,
        "the survivor carries its own family stamp"
    );
    // 7.2M ms: now the kalshi slot-1 round has ended too (slot 1 covers
    // [3600s, 7200s) on the hour grid).
    c.run_expiry_sweep(7_200_000);
    assert_eq!(
        c.ome().get(&kal).unwrap().status,
        OrderStatus::ForceClosed,
        "the 3600s order expires on ITS clock, one grid later"
    );
}

/// The manual verb refuses a 3600s order whose round is still live at
/// 1.8M ms (the 900s grid alone would call it expired) and accepts it
/// once ITS hour has passed.
#[test]
fn force_close_judges_the_order_s_own_round_clock() {
    let mut c = two_family_core();
    let kal = c.place(order("kal-up", "k1"), 0, 901_000).unwrap().0;
    let err = c
        .force_close_order(&kal, 1_800_000, "manual test")
        .unwrap_err();
    assert_eq!(
        err.code,
        CoreErrorCode::InvalidOrderState,
        "a 3600s round is still live at the 900s grid's end"
    );
    assert_eq!(c.ome().get(&kal).unwrap().status, OrderStatus::Live);
    // Past its OWN hour (slot 1 ends at 7.2M ms) the verb succeeds —
    // and stamps the family.
    c.force_close_order(&kal, 7_200_001, "manual test").unwrap();
    assert_eq!(c.ome().get(&kal).unwrap().status, OrderStatus::ForceClosed);
    assert_eq!(c.ome().get(&kal).unwrap().round_duration_sec, 3600);
}

// ── 消费点 1：fill 投影按订单自己的族钟 ───────────────────────────────

/// A 3600s taker fill opens a position that expires at the 3600s slot's
/// END — the 900s global grid would put the expiry a quarter into the
/// round and force-exit the hold immediately.
#[test]
fn project_fill_delta_projects_expiry_on_the_order_s_own_family() {
    let mut c = two_family_core();
    let kal = c.place(order("kal-up", "k1"), 0, 901_000).unwrap().0;
    // Cross the book: the resting-ish taker fills at its limit.
    c.book_snapshot("kal-up", vec![], vec![(dec!(0.39), dec!(500))], 901_500);
    c.ingest_fill(
        crate::model::Fill {
            order_id: kal.clone(),
            trade_id: Some("t1".into()),
            token_id: "kal-up".into(),
            side: Side::Buy,
            price: dec!(0.40),
            size: dec!(1),
            status: crate::model::FillStatus::Matched,
            ts_ms: 901_500,
            tx_hash: None,
            maker: None,
            account_id: crate::model::default_account_id(),
        },
        901_500,
    )
    .unwrap();
    let pos = c
        .positions()
        .open_positions()
        .iter()
        .find(|p| p.token_id == "kal-up")
        .expect("the fill opened the position");
    assert_eq!(
        pos.round_duration_sec, 3600,
        "the position carries the order's family"
    );
    assert_eq!(
        pos.expires_at_ms, 7_200_000,
        "slot 1 on the 3600s grid ends at 2h — not 1.8M ms (the 900s grid's end)"
    );
}

// ── 回填幂等（放置路径 + sweep 双写点）───────────────────────────────

/// Backfill twice (place stamp, then two sweep passes over the survivor)
/// changes nothing after the first write: the family a record was
/// admitted under stays authoritative.
#[test]
fn the_family_stamp_is_idempotent_across_repeat_passes() {
    let mut c = two_family_core();
    let kal = c.place(order("kal-up", "k1"), 0, 901_000).unwrap().0;
    assert_eq!(c.ome().get(&kal).unwrap().round_duration_sec, 3600);
    // Two more sweep passes — the stamp must not move (idempotent), and
    // the order stays live (its round has not ended on its own clock).
    c.run_expiry_sweep(1_800_000);
    c.run_expiry_sweep(1_800_500);
    let o = c.ome().get(&kal).unwrap();
    assert_eq!(o.round_duration_sec, 3600, "non-0 stamp never overwritten");
    assert_eq!(o.status, OrderStatus::Live);
}

// ── 单族等价：全 900s 部署与升级前逐字节同判 ─────────────────────────

/// Every market declares the configured grid (or nothing): sweep,
/// force-close and the fill projection must land exactly where the
/// pre-B1b code put them — the same expiry second, the same refusal
/// boundary, the same slot.
#[test]
fn single_family_deployment_matches_the_pre_b1b_verdicts() {
    // A single-family core: BOTH markets declare 900 == the global grid.
    let mut c = single_family_core();
    // Sweep rides the same grid: a slot-1 order placed at 901s survives
    // 1.7M ms and dies at exactly 1.8M ms — the #457 matrix unchanged.
    let id = c.place(order("kal-up", "k1"), 0, 901_000).unwrap().0;
    assert_eq!(c.ome().get(&id).unwrap().round_duration_sec, 900);
    c.run_expiry_sweep(1_700_000);
    assert_eq!(c.ome().get(&id).unwrap().status, OrderStatus::Live);
    c.run_expiry_sweep(1_800_000);
    assert_eq!(c.ome().get(&id).unwrap().status, OrderStatus::ForceClosed);
    // Force-close's refusal boundary sits at the same second.
    let id3 = c.place(order("poly-up", "k3"), 0, 901_000).unwrap().0;
    assert!(c.force_close_order(&id3, 1_799_999, "manual").is_err());
    c.force_close_order(&id3, 1_800_000, "manual").unwrap();
    // The fill projection lands on the same end-of-slot second.
    let id2 = c.place(order("kal-up", "k2"), 0, 901_000).unwrap().0;
    c.book_snapshot("kal-up", vec![], vec![(dec!(0.39), dec!(500))], 901_500);
    c.ingest_fill(
        crate::model::Fill {
            order_id: id2,
            trade_id: Some("t2".into()),
            token_id: "kal-up".into(),
            side: Side::Buy,
            price: dec!(0.40),
            size: dec!(1),
            status: crate::model::FillStatus::Matched,
            ts_ms: 901_500,
            tx_hash: None,
            maker: None,
            account_id: crate::model::default_account_id(),
        },
        901_500,
    )
    .unwrap();
    let pos = &c.positions().open_positions()[0];
    assert_eq!(
        pos.expires_at_ms, 1_800_000,
        "single family: the historic end-of-slot projection"
    );
    assert_eq!(pos.round_duration_sec, 900);
}

// ── E2（多族漏网 ②）：top-up 判定按 (slot, duration) 对 + kill 按族圈定 ──

/// A polymarket 15m leg is IN FLIGHT (latency dials engaged) when the same
/// venue's 1h family feeds its NEW round. The pre-E2 mirror predicate was
/// a BARE slot equality — the feeding batch's slot (a 3600s-grid number)
/// matches nothing in the table, so the batch was misread as a ROLLOVER,
/// and the kill arm scoped by the feeding VENUE alone murdered the 15m
/// leg whose round was still running (`taker_killed_round_ended`). After
/// the fix the predicate is the engine's (slot, duration) pair and the
/// kill arm is family-scoped: the 1h rollover still happens (its old row
/// is replaced by its own feed — the correct B1c behaviour), but the 15m
/// leg survives to its fill.
#[test]
fn a_foreign_family_first_feed_does_not_kill_a_sibling_family_s_inflight_taker() {
    let mut c = family_core(&[("polymarket", "pm15", 900)]);
    c.config_mut().fill_model = crate::sim::FillModel {
        taker_latency_ms: 100,
        taker_rtt_ms: 50,
        ..Default::default()
    };
    // Timeline: 900s slot 8 = [7_200_000, 8_100_000); 3600s slot 1 =
    // [3_600_000, 7_200_000) is ALREADY OVER at t0 — its stale row still
    // bridges in the scanner until its own feed replaces it (the same
    // staggered shape a live 1h discovery produces at the boundary).
    let row = |slot: i64, dur: i64, up: &str| crate::model::CryptoMarket {
        asset: "BTC".into(),
        condition_id: format!("cond-pm-{slot}-{dur}"),
        question_id: "q".into(),
        up_token_id: up.into(),
        down_token_id: format!("{up}-d"),
        up_price: dec!(0.6),
        down_price: dec!(0.4),
        expires_at_ms: (slot + 1) * dur * 1000,
        round_slot: slot,
        round_duration_sec: dur,
        archive_verdict: false,
        venue: "polymarket".into(),
        neg_risk: false,
        question: "?".into(),
    };
    let t0 = 7_599_900i64;
    // The live 15m round and the ALREADY-EXPIRED 1h row (bridge state).
    c.engine_on_data(
        crate::engine::DataEvent::RoundMarkets {
            markets: vec![row(8, 900, "pm15-up")],
            now_ms: t0,
        },
        t0,
    );
    c.engine_on_data(
        crate::engine::DataEvent::RoundMarkets {
            markets: vec![row(1, 3600, "pm1h-old")],
            now_ms: t0,
        },
        t0,
    );
    // The 15m leg goes in flight: matches at t0+100, reports at t0+150.
    c.book_snapshot("pm15-up", vec![], vec![(dec!(0.40), dec!(500))], t0);
    let req = OrderRequest {
        token_id: "pm15-up".into(),
        condition_id: "cond-pm-8-900".into(),
        side: Side::Buy,
        mode: FillPolicy::Taker,
        price: dec!(0.40),
        size: dec!(1),
        internal_key: "k-inflight".into(),
        strategy: STRATEGY.into(),
        asset: "BTC".into(),
        direction: "up".into(),
        round_slot: 8,
        account_id: crate::model::default_account_id(),
    };
    let (id, st) = c.place(req, 0, t0).unwrap();
    assert_eq!(st, OrderStatus::Pending, "the leg is in flight");

    // t0+20: the SAME venue's 1h family feeds its NEW round (slot 2). A
    // bare-slot mirror predicate matches nothing → rollover arm → the
    // venue-scoped kill murdered the 15m leg here (pre-E2). The family
    // scope lets the leg through.
    let t1 = t0 + 20;
    c.engine_on_data(
        crate::engine::DataEvent::RoundMarkets {
            markets: vec![row(2, 3600, "pm1h-new")],
            now_ms: t1,
        },
        t1,
    );
    // The 1h rollover DID happen: the stale slot-1 row is replaced by the
    // feeding family's own slot-2 row.
    let e = c.engine.as_ref().expect("engine");
    assert!(
        e.scanner()
            .markets()
            .iter()
            .any(|m| m.up_token_id == "pm1h-new"),
        "the 1h feed still rolls its own family over"
    );
    assert!(
        !e.scanner()
            .markets()
            .iter()
            .any(|m| m.up_token_id == "pm1h-old"),
        "the expired 1h row leaves the table"
    );
    // Past report_at: the 15m leg FILLS — no taker_killed_round_ended
    // rejection may exist for it.
    c.tick(t0 + 160).unwrap();
    let o = c.ome().get(&id).unwrap();
    assert_eq!(
        o.status,
        OrderStatus::Filled,
        "the 15m in-flight leg survives the sibling family's rollover"
    );
    assert!(
        c.positions()
            .open_positions()
            .iter()
            .any(|p| p.token_id == "pm15-up"),
        "the fill books a position"
    );
}

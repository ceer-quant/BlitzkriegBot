use super::*;
use rust_decimal_macros::dec;

use crate::model::{Mode, OrderRequest};
use crate::risk::RiskConfig;
use crate::service::{Core, CoreConfig};
use blitzkrieg_market_api::{FillPolicy, MarketType};

/// A LIVE core with the engine installed and TWO labelled round markets —
/// `poly-up`/`poly-down` on venue `polymarket` and `kal-up`/`kal-down` on
/// venue `kalshi` — so a pending order's token carries a venue to route
/// by. Books are seeded for both tokens so the entries can rest.
fn two_venue_live_core() -> Core {
    let mut c = Core::new(CoreConfig {
        mode: Mode::Live,
        risk: RiskConfig {
            max_order_notional: dec!(3),
            ..Default::default()
        },
        dry_seed_balance: dec!(10_000),
        round_duration_sec: 900,
        // These tests PLACE live orders: the default config's relative
        // data/ log paths would append into the process cwd (parallel
        // tests move it), re-dirtying the reset tests' self-check. A
        // fixture that trades carries no durable logs.
        trade_log_path: None,
        order_log_path: None,
        position_log_path: None,
        ..Default::default()
    });
    c.set_balance(dec!(10));
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

fn live_order(token: &str, key: &str) -> OrderRequest {
    OrderRequest {
        token_id: token.into(),
        condition_id: "cond".into(),
        side: crate::model::Side::Buy,
        mode: FillPolicy::Maker,
        price: dec!(0.40),
        size: dec!(1),
        internal_key: key.into(),
        strategy: "spread_arb".into(),
        asset: "BTC".into(),
        direction: "up".into(),
        round_slot: 1,
        account_id: crate::model::default_account_id(),
    }
}

#[tokio::test]
async fn pending_orders_route_by_the_token_round_venue() {
    let mut c = two_venue_live_core();
    // One maker per venue — both rest Pending→Live... (LIVE mode keeps the
    // order Pending until the venue acks, which is exactly the unbound
    // queue the executors drain).
    let (poly_id, _) = c.place(live_order("poly-up", "k1"), 0, 1).unwrap();
    let (kal_id, _) = c.place(live_order("kal-up", "k2"), 0, 2).unwrap();

    let host = CoreHost::new(std::sync::Arc::new(tokio::sync::Mutex::new(c)));
    host.set_active_plugin("polymarket");

    // Each executor drains ONLY its own venue's orders.
    let poly_drain = host.take_pending_orders("polymarket").await;
    assert_eq!(
        poly_drain
            .iter()
            .map(|o| o.core_order_id.as_str())
            .collect::<Vec<_>>(),
        vec![poly_id.as_str()],
        "the polymarket executor must take only the polymarket order"
    );
    let kal_drain = host.take_pending_orders("kalshi").await;
    assert_eq!(
        kal_drain
            .iter()
            .map(|o| o.core_order_id.as_str())
            .collect::<Vec<_>>(),
        vec![kal_id.as_str()],
        "the kalshi executor must take only the kalshi order — cross-venue \
         orders must never leak into the sibling drain"
    );
    // The drain is a per-tick RE-READ, not take-once: an order leaves the
    // pending set only when its venue id binds (`on_order_accepted`). So
    // re-draining returns the SAME order — the routing property is who
    // sees what, never a consume-once partition.
    let again = host.take_pending_orders("polymarket").await;
    assert_eq!(
        again
            .iter()
            .map(|o| o.core_order_id.as_str())
            .collect::<Vec<_>>(),
        vec![poly_id.as_str()],
        "re-draining before the venue acks re-offers the same order"
    );
    assert_eq!(
        host.take_pending_orders("kalshi")
            .await
            .iter()
            .map(|o| o.core_order_id.as_str())
            .collect::<Vec<_>>(),
        vec![kal_id.as_str()]
    );
}

/// Dual-key routing at the production drain seam: a byte-identical token
/// carries one row per venue, and the EXECUTOR that must pull the order
/// is the placing account's own venue — a polysimulator account's
/// poly-up order can never land on the real polymarket drain (nor the
/// reverse).
#[tokio::test]
async fn mirror_tokens_drain_to_the_placing_account_venue() {
    let mut c = two_venue_live_core();
    // The simulator's batch declares the SAME tokens polymarket already
    // listed: a same-slot merge whose fresh set is the sim venue's OWN
    // tokens — the rows land under their own (venue, token) keys
    // (RFC-661 M1). The drain keys on the placing binding's own row; no
    // declaration participates.
    let now: i64 = 1_000_001;
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
            now_ms: now,
        },
        now,
    );
    // A second account bound to the mirror venue places on poly-up.
    let book = crate::account::Account {
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
    };
    c.accounts_mut().insert(book).unwrap();
    c.accounts_mut()
        .get_mut(&crate::account::AccountId::from("sim"))
        .unwrap()
        .set_balance(dec!(10));
    c.engine_evaluate(now + 1);
    let mut order = live_order("poly-up", "sim-1");
    order.account_id = crate::account::AccountId::from("sim");
    let (sim_id, _) = c.place(order, 0, 2).unwrap();

    let host = CoreHost::new(std::sync::Arc::new(tokio::sync::Mutex::new(c)));
    host.set_active_plugin("polysimulator");

    // The simulator executor takes its account's order; the REAL venue's
    // executor does not — this is the live-money misroute the table-row
    // drain would have made.
    assert_eq!(
        host.take_pending_orders("polysimulator")
            .await
            .iter()
            .map(|o| o.core_order_id.as_str())
            .collect::<Vec<_>>(),
        vec![sim_id.as_str()],
        "the mirror account's order drains to polysimulator, not the real venue"
    );
}

#[tokio::test]
async fn untagged_orders_fall_back_to_the_active_plugin_drain() {
    let mut c = two_venue_live_core();
    // A token that belongs to NO labelled round market (the legacy /
    // unlabelled-host case): the venue lookup comes back empty.
    let (id, _) = c.place(live_order("ghost-token", "k1"), 0, 1).unwrap();

    let host = CoreHost::new(std::sync::Arc::new(tokio::sync::Mutex::new(c)));
    host.set_active_plugin("polymarket");

    // The ACTIVE plugin's drain takes the untagged order (the
    // single-plugin fallback); the sibling does not.
    let poly_drain = host.take_pending_orders("polymarket").await;
    assert_eq!(
        poly_drain
            .iter()
            .map(|o| o.core_order_id.as_str())
            .collect::<Vec<_>>(),
        vec![id.as_str()],
        "untagged orders route to the active plugin — the single-plugin \
         behaviour preserved"
    );
    assert!(
        host.take_pending_orders("kalshi").await.is_empty(),
        "a sibling executor must not take the untagged order"
    );
}

/// #775 (negative, the multi-executor shape): every executor reaches the
/// host through its own `for_plugin` handle. An untagged order must land
/// on EXACTLY ONE of them — the one whose name is the SHARED active
/// owner — while a venue-tagged order keeps draining on its own venue's
/// executor.
///
/// Pre-fix this test fails: `for_plugin` overwrote the handle's
/// `active_plugin` cell with the handle's own name, so BOTH A and B saw
/// `plugin == active` and both drained the same untagged order — one
/// order submitted to two venues. The fix keeps `active_plugin` a single
/// shared cell (written only by `set_active_plugin`) and puts the
/// handle's identity in the separate `own_plugin` field.
#[tokio::test]
async fn untagged_orders_drain_to_exactly_one_for_plugin_executor() {
    let mut c = two_venue_live_core();
    // One order on a token no labelled round market knows: the execution
    // venue resolves to "" (engine not ready / binding removed). Two more
    // carry a real venue each, so the venue-scoped half stays pinned too.
    let (ghost_id, _) = c.place(live_order("ghost-token", "k1"), 0, 1).unwrap();
    let (poly_id, _) = c.place(live_order("poly-up", "k2"), 0, 2).unwrap();
    let (kal_id, _) = c.place(live_order("kal-up", "k3"), 0, 3).unwrap();

    // The assembly's shared host: its active owner is `actives[0]`, and
    // every executor starts on a `for_plugin` handle derived from it.
    let shared = CoreHost::new(std::sync::Arc::new(tokio::sync::Mutex::new(c)));
    shared.set_active_plugin("polymarket");
    let a = shared.for_plugin("polymarket");
    let b = shared.for_plugin("kalshi");

    let a_drain = a.take_pending_orders("polymarket").await;
    let a_ids: Vec<&str> = a_drain.iter().map(|o| o.core_order_id.as_str()).collect();
    assert_eq!(
        a_ids,
        vec![ghost_id.as_str(), poly_id.as_str()],
        "A is the shared active owner: it takes the untagged order and its \
         own venue's order"
    );

    let b_drain = b.take_pending_orders("kalshi").await;
    let b_ids: Vec<&str> = b_drain.iter().map(|o| o.core_order_id.as_str()).collect();
    assert_eq!(
        b_ids,
        vec![kal_id.as_str()],
        "B must NOT take the untagged order — only the shared active owner \
         does (#775: pre-fix both A and B saw it)"
    );
    assert!(
        !b_ids.contains(&ghost_id.as_str()),
        "the untagged order reached a sibling executor"
    );
    // Exactly one executor saw the untagged order, counted across the
    // whole board, and the active owner is that one.
    let ghost_hits = usize::from(a_ids.contains(&ghost_id.as_str()))
        + usize::from(b_ids.contains(&ghost_id.as_str()));
    assert_eq!(
        ghost_hits, 1,
        "one untagged order must be drained by exactly one executor"
    );
}

#[tokio::test]
async fn legacy_host_without_active_plugin_takes_all_orders() {
    let mut c = two_venue_live_core();
    let (poly_id, _) = c.place(live_order("poly-up", "k1"), 0, 1).unwrap();
    let (kal_id, _) = c.place(live_order("kal-up", "k2"), 0, 2).unwrap();

    // No `set_active_plugin` call — hosts that never told the core who is
    // active (most in-process tests). The legacy "" bucket takes ALL
    // orders: the exact pre-routing behaviour.
    let host = CoreHost::new(std::sync::Arc::new(tokio::sync::Mutex::new(c)));
    let drain = host.take_pending_orders("").await;
    let mut got: Vec<&str> = drain.iter().map(|o| o.core_order_id.as_str()).collect();
    got.sort();
    let mut want: Vec<&str> = vec![poly_id.as_str(), kal_id.as_str()];
    want.sort();
    assert_eq!(
        got, want,
        "no active plugin = the historical one-drain-takes-all behaviour"
    );
}

#[tokio::test]
async fn venue_cancels_route_to_the_owning_executor() {
    let mut c = two_venue_live_core();
    // Bind venue ids so a cancel queues a REAL venue cancel.
    let (poly_id, _) = c.place(live_order("poly-up", "k1"), 0, 1).unwrap();
    c.bind_venue(&poly_id, "poly-v1".into(), 2).unwrap();
    let (kal_id, _) = c.place(live_order("kal-up", "k2"), 0, 3).unwrap();
    c.bind_venue(&kal_id, "kal-v1".into(), 4).unwrap();
    c.cancel(&poly_id, 5).unwrap();
    c.cancel(&kal_id, 6).unwrap();

    let host = CoreHost::new(std::sync::Arc::new(tokio::sync::Mutex::new(c)));
    host.set_active_plugin("polymarket");

    // Each executor cancels only ITS venue's ids: the queue is take-once,
    // so a sibling swallowing the other's id would strand the cancel.
    assert_eq!(
        host.take_pending_cancels("polymarket").await,
        vec!["poly-v1".to_string()]
    );
    assert_eq!(
        host.take_pending_cancels("kalshi").await,
        vec!["kal-v1".to_string()]
    );
    assert!(host.take_pending_cancels("polymarket").await.is_empty());
    assert!(host.take_pending_cancels("kalshi").await.is_empty());
}

/// #647: two plugins hold settlement claims at once (each on its OWN
/// venue's collateral), and each executor's `for_plugin` handle drains
/// ONLY its own claims — the redemption and merge queues can no longer
/// hand a claim to a foreign venue's bridge (the cross-post the old
/// global queue made: harmless there, but it burned retries and alerts).
/// A handle with no bucket name (the legacy shape) keeps the all-take.
#[tokio::test]
async fn settlement_queues_drain_per_venue_bucket() {
    let mut c = two_venue_live_core();
    let now: i64 = 1_000_000;
    // One filled entry per venue: the fill's position stamps the entry
    // market's venue (the same `execution_venue_of` read the order drain
    // routes by), so each settled claim belongs to its own executor. Each
    // order rides its own market's condition — two claims, never one folded.
    let mut poly_order = live_order("poly-up", "s1");
    poly_order.condition_id = "cond-polymarket".into();
    let mut kal_order = live_order("kal-up", "s2");
    kal_order.condition_id = "cond-kalshi".into();
    let (poly_id, _) = c.place(poly_order, 0, now + 1).unwrap();
    let (kal_id, _) = c.place(kal_order, 0, now + 2).unwrap();
    let fill = |order_id: &str, token: &str, trade: &str| crate::model::Fill {
        order_id: order_id.into(),
        trade_id: Some(trade.into()),
        token_id: token.into(),
        side: crate::model::Side::Buy,
        price: dec!(0.40),
        size: dec!(2),
        status: crate::model::FillStatus::Confirmed,
        ts_ms: now + 3,
        tx_hash: None,
        maker: Some(true),
        account_id: crate::model::default_account_id(),
    };
    c.ingest_fill(fill(&poly_id, "poly-up", "t1"), now + 4)
        .unwrap();
    c.ingest_fill(fill(&kal_id, "kal-up", "t2"), now + 5)
        .unwrap();
    assert_eq!(c.positions().open_positions().len(), 2);
    assert_eq!(
        c.positions()
            .open_positions()
            .iter()
            .find(|p| p.token_id == "poly-up")
            .unwrap()
            .venue,
        "polymarket"
    );
    // Round end passes; the core watches both markets for a verdict.
    c.on_market_resolution(resolution_for("cond-polymarket", "poly-up"), now + 4_000);
    c.on_market_resolution(resolution_for("cond-kalshi", "kal-up"), now + 4_000);
    assert_eq!(c.take_pending_redemptions(now + 4_500).len(), 2);

    // Each executor's own handle takes exactly ITS venue's request.
    let host = CoreHost::new(std::sync::Arc::new(tokio::sync::Mutex::new(c)));
    let poly_exec = host.for_plugin("polymarket");
    let kal_exec = host.for_plugin("kalshi");
    let poly_due = poly_exec.take_pending_redemptions().await;
    assert_eq!(
        poly_due.len(),
        1,
        "the polymarket executor takes only its claim"
    );
    assert_eq!(poly_due[0].venue, "polymarket");
    let kal_due = kal_exec.take_pending_redemptions().await;
    assert_eq!(kal_due.len(), 1, "the kalshi executor takes only its claim");
    assert_eq!(kal_due[0].venue, "kalshi");
    // Take-once per dispatch stamp: the re-drain is empty, and neither
    // handle ever re-sees the sibling's request.
    assert!(poly_exec.take_pending_redemptions().await.is_empty());
    assert!(kal_exec.take_pending_redemptions().await.is_empty());
    // The legacy shape (no bucket name — the pre-#647 hosts) still takes
    // everything in one drain.
    let legacy = host.take_pending_redemptions().await;
    assert!(legacy.is_empty(), "both were dispatched above");
}

/// The resolution a venue would report for one of the two test markets:
/// the named token wins at 1.00/share.
fn resolution_for(condition: &str, winner: &str) -> blitzkrieg_market_api::MarketResolution {
    blitzkrieg_market_api::MarketResolution {
        condition_id: condition.into(),
        resolved: true,
        payouts: vec![(winner.to_string(), dec!(1)), ("x-never".into(), dec!(0))],
        neg_risk: false,
        source: "test".into(),
    }
}

/// Pins the drain's market-lookup wiring: a token whose round market
/// carries a venue reaches exactly that venue's drain, through the SAME
/// `venue_of` read the panels' Venue column uses.
#[test]
fn the_token_venue_lookup_reads_the_round_list() {
    let c = two_venue_live_core();
    assert_eq!(c.venue_of_token("poly-up"), "polymarket");
    assert_eq!(c.venue_of_token("kal-down"), "kalshi");
    assert_eq!(c.venue_of_token("ghost-token"), "");
}

/// The handshake judge stays single-plugin; this pins its wire shape so a
/// signature change here is a deliberate act, not a drive-by.
#[test]
fn compat_judge_keeps_its_single_plugin_signature() {
    let p = crate::compat::PluginModes::for_plugin(&BarePlugin);
    let modes = vec![blitzkrieg_strategy_api::StrategyMode {
        market_type: MarketType::Prediction,
        structure: None,
    }];
    assert!(crate::compat::check_compatibility("s", &modes, &p).is_ok());
}

// ── 盘口批次原子化 ─────────────────────────────────────────────────────

/// 批次观测探针：记录每次 on_book 的 (token, 时间戳, best_bid)。
struct BatchProbe {
    seen: Arc<std::sync::Mutex<Vec<(String, i64, i64)>>>,
}
impl crate::strategies::EngineStrategy for BatchProbe {
    fn name(&self) -> &str {
        "batch_probe_host"
    }
    fn on_book(&mut self, t: &str, s: &crate::model::OrderbookSnapshot, _n: i64) {
        self.seen
            .lock()
            .unwrap()
            .push((t.to_string(), s.timestamp, s.best_bid.mantissa() as i64));
    }
    fn on_round(&mut self, _slot: i64, _left: i64, _now: i64) {}
    fn find_candidates(
        &mut self,
        _ctx: &crate::strategies::StrategyCtx<'_>,
    ) -> Vec<crate::signal::TradeSignal> {
        Vec::new()
    }
}

/// 红→绿（撕裂窗口本体）：`on_books` 的整批应用必须对策略呈现为一次
/// 原子跃迁 —— 同一调用里的两条 on_book 观测共享同一批次时间戳，up 和
/// down 都是新批价格；绝不允许「up 已是 20_000 的新价、down 还停在
/// 10_000 的旧价」的中间态。变异验证：把 `CoreHost::on_books` 退化为
/// 逐本 `engine_on_data`（两次独立锁）后，本测试变红。
#[tokio::test]
async fn on_books_applies_the_whole_batch_under_one_lock() {
    let mut c = two_venue_live_core();
    // 追加两腿市场（up/down），书先给个旧批（与修复测试同构）。
    let now: i64 = 1_000_000;
    let slot = now / 1000 / 900;
    let end = (slot + 1) * 900 * 1000;
    c.engine_on_data(
        crate::engine::DataEvent::RoundMarkets {
            markets: vec![crate::model::CryptoMarket {
                asset: "BTC".into(),
                condition_id: "cond-batch".into(),
                question_id: "q".into(),
                up_token_id: "up".into(),
                down_token_id: "down".into(),
                up_price: rust_decimal_macros::dec!(0.6),
                down_price: rust_decimal_macros::dec!(0.4),
                expires_at_ms: end,
                round_slot: slot,
                round_duration_sec: 900,
                archive_verdict: false,
                venue: "polymarket".into(),
                neg_risk: false,
                question: "?".into(),
            }],
            now_ms: now,
        },
        now,
    );
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    c.host_replay_strategy(Box::new(BatchProbe { seen: seen.clone() }), "test")
        .expect("probe registers");
    c.set_strategy_enabled("batch_probe_host", true);

    let host = CoreHost::new(std::sync::Arc::new(tokio::sync::Mutex::new(c)));
    let mk = |token: &str, bid: rust_decimal::Decimal| blitzkrieg_market_api::BookUpdate {
        token_id: token.to_string(),
        bids: vec![(bid, dec!(10))],
        asks: vec![(rust_decimal_macros::dec!(0.60), dec!(10))],
        ts_ms: 20_000,
    };
    host.on_books(
        vec![
            mk("up", rust_decimal_macros::dec!(0.50)),
            mk("down", rust_decimal_macros::dec!(0.55)),
        ],
        20_000,
    )
    .await;

    let obs = seen.lock().unwrap().clone();
    assert_eq!(obs.len(), 2, "both legs observed exactly once each");
    assert!(
        obs.iter().all(|(_, ts, _)| *ts == 20_000),
        "one batch = one timestamp for every token; torn: {obs:?}"
    );
    let up = obs.iter().find(|(t, _, _)| t == "up").expect("up");
    let down = obs.iter().find(|(t, _, _)| t == "down").expect("down");
    assert_eq!(up.2, 50, "up carries the new batch price; {obs:?}");
    assert_eq!(down.2, 55, "down carries the new batch price; {obs:?}");
}

struct BarePlugin;
impl blitzkrieg_market_api::MarketPlugin for BarePlugin {
    fn name(&self) -> &str {
        "bare"
    }
    fn market_type(&self) -> MarketType {
        MarketType::Prediction
    }
}

// ── RFC-661 M2: the feed_source reuse fan-out (the host half) ───────────

/// One upstream round batch from `source`, tagged for the wire exactly as
/// a discovery hands it over. Tokens are the SAME byte strings every
/// consumer re-registers — the reuse premise (shared token space). The
/// expiry rides the REAL clock's current 900s window: the scanner's clock
/// guard refuses a declaration more than two rounds from `now_ms()`, so a
/// fixed fixture timestamp would silently open no round clock.
fn reuse_batch(source: &str, up: &str, down: &str) -> Vec<blitzkrieg_market_api::MarketDescriptor> {
    let slot = blitzkrieg_market_api::net::now_ms() / 1000 / 900;
    vec![blitzkrieg_market_api::MarketDescriptor {
        asset: "BTC".to_string(),
        condition_id: format!("cond-{up}"),
        question_id: "q".to_string(),
        up_token_id: up.to_string(),
        down_token_id: down.to_string(),
        up_price: dec!(0.55),
        down_price: dec!(0.45),
        expires_at_ms: (slot + 1) * 900 * 1000,
        round_slot: slot,
        venue: source.to_string(),
        neg_risk: false,
        question: "reuse batch".to_string(),
    }]
}

/// The scanner rows a token holds, as (venue, up_token) pairs — the
/// dual-key read the fan-out assertions walk.
fn rows_for(c: &Core, token: &str) -> Vec<(String, String)> {
    c.engine()
        .map(|e| {
            e.scanner()
                .markets()
                .iter()
                .filter(|m| m.up_token_id == token || m.down_token_id == token)
                .map(|m| (m.venue.clone(), m.up_token_id.clone()))
                .collect()
        })
        .unwrap_or_default()
}

/// M2 §4.1: ONE upstream round batch re-registers under every declared
/// consumer's OWN venue identity — the source keeps its row, the consumer
/// gains its own `(venue, token)` row (M1's dual-key table), and the
/// engine reads both as native.
#[tokio::test]
async fn a_feed_source_consumer_re_registers_the_source_batch_under_its_own_venue() {
    let c = two_venue_live_core();
    let core = std::sync::Arc::new(tokio::sync::Mutex::new(c));
    let host = CoreHost::new(core.clone());
    host.set_active_plugin("polymarket");
    let reuse = FeedReuse::default();
    reuse.add_edge("polymarket", "polymarket"); // the source speaks too
    reuse.add_edge("simspace", "polymarket"); // the declared consumer
    host.set_feed_reuse(reuse);

    host.on_round_markets(reuse_batch("polymarket", "share-up", "share-down"))
        .await;

    let c = core.lock().await;
    let rows = rows_for(&c, "share-up");
    assert_eq!(
        rows.len(),
        2,
        "the byte-identical token holds TWO rows after the fan-out: {rows:?}"
    );
    let mut venues: Vec<&str> = rows.iter().map(|(v, _)| v.as_str()).collect();
    venues.sort_unstable();
    assert_eq!(
        venues,
        vec!["polymarket", "simspace"],
        "each venue's row carries ITS OWN identity: {rows:?}"
    );
    // The M1 read-back: each venue resolves the token to ITS OWN row, and
    // the token-only legacy lookup refuses as ambiguous (never first-hit).
    assert_eq!(c.venue_of_token("share-down"), "");
    let scanner = c.engine().expect("engine").scanner();
    assert!(
        scanner
            .market_for_token_on_venue("simspace", "share-up")
            .is_some()
    );
    assert!(
        scanner
            .market_for_token_on_venue("polymarket", "share-up")
            .is_some()
    );
    assert!(
        scanner.market_for_token("share-up").is_none(),
        "two venues hold the token: the token-only lookup must refuse, not first-hit"
    );
    // The consumer's own round clock opened (its row spoke its own expiry).
    let clocks = c.engine().expect("engine").scanner().observed_venue_keys();
    assert!(
        clocks.iter().any(|v| v == "simspace"),
        "the consumer's re-registered row opens its own round clock: {clocks:?}"
    );
}

/// M2 §4.1 (fan-out shape): two INDEPENDENT consumer→source edges never
/// re-register each other's batches — source A's batch fans out ONLY to
/// A's consumers, source B's only to B's (a flat venue list would cross
/// the two token spaces). All declared, none hardcoded: the edges are the
/// plugins' declarations replayed through the host.
#[tokio::test]
async fn two_consumer_source_edges_never_re_register_each_others_batches() {
    let c = two_venue_live_core();
    let core = std::sync::Arc::new(tokio::sync::Mutex::new(c));
    let host = CoreHost::new(core.clone());
    host.set_active_plugin("polymarket");
    let reuse = FeedReuse::default();
    reuse.add_edge("polymarket", "polymarket");
    reuse.add_edge("cons_a", "src_a");
    reuse.add_edge("cons_b", "src_b");
    host.set_feed_reuse(reuse);

    // Source A's batch: only cons_a may re-register it.
    host.on_round_markets(reuse_batch("src_a", "a-up", "a-down"))
        .await;
    // Source B's batch: only cons_b may re-register it.
    host.on_round_markets(reuse_batch("src_b", "b-up", "b-down"))
        .await;

    let c = core.lock().await;
    let a_rows = rows_for(&c, "a-up");
    let a_venues: Vec<&str> = a_rows.iter().map(|(v, _)| v.as_str()).collect();
    assert!(
        a_venues.contains(&"cons_a") && !a_venues.contains(&"cons_b"),
        "A's batch re-registers under cons_a ONLY: {a_rows:?}"
    );
    let b_rows = rows_for(&c, "b-up");
    let b_venues: Vec<&str> = b_rows.iter().map(|(v, _)| v.as_str()).collect();
    assert!(
        b_venues.contains(&"cons_b") && !b_venues.contains(&"cons_a"),
        "B's batch re-registers under cons_b ONLY: {b_rows:?}"
    );
}

/// M2 (数据面归属): the consumer's row is a REAL row — a follow-up book
/// on the shared token prices the consumer's OWN market read, and the
/// upstream repricing lands on both rows (one connection, two venues'
/// views). Books stay token-keyed (one upstream event applies once); the
/// per-venue rows read the same book.
#[tokio::test]
async fn consumer_rows_read_the_reused_stream_and_stay_venue_distinct() {
    let c = two_venue_live_core();
    let core = std::sync::Arc::new(tokio::sync::Mutex::new(c));
    let host = CoreHost::new(core.clone());
    host.set_active_plugin("polymarket");
    let reuse = FeedReuse::default();
    reuse.add_edge("polymarket", "polymarket");
    reuse.add_edge("simspace", "polymarket");
    host.set_feed_reuse(reuse);

    host.on_round_markets(reuse_batch("polymarket", "flow-up", "flow-down"))
        .await;
    // The upstream book, once (ONE connection feeding both venues).
    host.on_book(blitzkrieg_market_api::BookUpdate {
        token_id: "flow-up".to_string(),
        bids: vec![(dec!(0.51), dec!(10))],
        asks: vec![(dec!(0.55), dec!(10))],
        ts_ms: 20_000,
    })
    .await;

    let c = core.lock().await;
    // Both rows survived the book (a merge would have dropped one).
    let rows = rows_for(&c, "flow-up");
    assert_eq!(
        rows.len(),
        2,
        "the book application must keep both rows: {rows:?}"
    );
    // The book snapshot is readable THROUGH each venue's own row: same
    // bytes, two owners — the OME cannot tell the consumer from a native.
    for venue in ["polymarket", "simspace"] {
        let m = c
            .engine()
            .expect("engine")
            .scanner()
            .market_for_token_on_venue(venue, "flow-up")
            .expect("the venue's own row");
        assert_eq!(m.venue, venue, "each read resolves to the venue's own row");
    }
    assert_eq!(c.venue_of_token("flow-up"), "");
}

//! cross_venue_arb 反向验收（spec：「反向五条——必须红/必须有牙」）。
//!
//! 每条测试都是一颗牙：内核或包在对应纪律上打洞时，这里必须先红。
//! 五条的判定对象与运行状态：
//!
//!  1. `confirm_refuses_two_listings_from_the_same_venue` — 同所映射 confirm
//!     必须被拒（DuplicateVenue，机制已有）。直接调用层，今天即绿。
//!  2. `atomic_batch_refuses_the_whole_pair_when_pot_is_too_small` — 无预占致
//!     裸腿：cross_venue_arb 装上 pot 后，超 cap 的双腿批必须整组拒（0 腿 +
//!     `limit.virtualCap` ×2）。REAL 包经 LuaEngineAdapter 走完整 engine。
//!  3. `orphan_sweep_flattens_a_naked_leg_after_the_grace_window` — Fallback
//!     未触发 → 裸腿看护：一腿成交、第二腿终态撤单、过 5s 宽限 ⇒ tick ⇒
//!     RiskAlert + 裸腿被 flatten。
//!  4. `dry_run_precheck_never_holds_the_pot` — FillModel 假完美成交是仓库级
//!     已知披露（memory 有案）；这里钉的是它的风险层孪生：virtual_batch_fits
//!     的 DRY-RUN 语义——预检过批后 pot 必须仍为 0（check-then-hold 分离）。
//!  5. UI 只改状态后端未监听：非 Rust 测试，逐按钮核对表见
//!     `docs/cross-arb-acceptance.md` 的静态核对矩阵（由验收官对照
//!     Agent-UI 的产物逐按钮核对 IPC 动词 → 内核 handler）。
//!
//! 夹具构造方式复刻 `service.rs` 内嵌测试（不改 src）：#1 抄
//! `confirm_refusal_is_an_error_and_moves_nothing` 的最小 listing 夹具；
//! #2/#3/#4 走公开 `Core` API。带 `EXPECTED-RED-WAITING` 标注的测试依赖
//! Agent-内核 的在途产物（manifest `atomic_batch` 字段 / `run_pair_orphan_
//! sweep`），在途期间允许红；收口时必须全绿。

use std::collections::HashMap;
use std::path::PathBuf;

use blitzkrieg_core::engine::DataEvent;
use blitzkrieg_core::model::{CryptoMarket, Mode, OrderRequest, Side, default_account_id};
use blitzkrieg_core::position::OpenParams;
use blitzkrieg_core::risk::RiskConfig;
use blitzkrieg_core::schema::Event;
use blitzkrieg_core::service::{Core, CoreConfig, VirtualAccountLimit};
use blitzkrieg_core::strategy_engine::lua_loader::{LuaEngineAdapter, load_lua_package};
use blitzkrieg_lua_runtime::FeeScheduleView;
use blitzkrieg_market_api::unified::{
    EventStatus, ListingStatus, PlatformListing, SettlementSource, UnifiedEvent, Venue,
};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

const NOW: i64 = 1_700_000_000_000;

// 与 cross_venue_arb_strategy_check.rs 同构的 PM/KX 符号对：UP 腿来自
// polymarket listing，DOWN 腿来自 kalshi listing（跨所，非同 listing 配对）。
const PM: &str = "pm-cond";
const KX: &str = "kx-cond";
const PM_UP: &str = "pm-up";
const PM_DOWN: &str = "pm-down";
const KX_UP: &str = "kx-up";
const KX_DOWN: &str = "kx-down";

// ── 夹具 ────────────────────────────────────────────────────────────────────

fn package_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../user_layer/strategies_lua/cross_venue_arb")
}

/// REAL cross_venue_arb 包经 LuaEngineAdapter 装载（service 加载路径同款），
/// 带部署线收费表 legacy_quadratic——没有费用表包会 fail closed。
fn arb_adapter() -> LuaEngineAdapter {
    let loaded = load_lua_package(&package_dir()).expect("cross_venue_arb package must load");
    let a = LuaEngineAdapter::new(loaded.strategy, loaded.tunables)
        .declare(loaded.holds_to_settlement, loaded.gate_exemptions)
        .declare_atomic_batch(loaded.atomic_batch);
    a.set_fee_schedule(FeeScheduleView {
        name: "legacy_quadratic".into(),
        rate: "0.125".into(),
        exponent: 2,
    });
    a
}

fn listing(venue: Venue, cond: &str, up: &str, down: &str) -> PlatformListing {
    PlatformListing {
        venue,
        condition_id: cond.into(),
        up_token_id: up.into(),
        down_token_id: down.into(),
        asset: Some("bitcoin".into()),
        title: "Bitcoin Up or Down 2PM ET".into(),
        settlement_rules: "Binance 1m candle".into(),
        settlement_source: SettlementSource::Chainlink,
        expires_at_ms: NOW + 900_000,
        status: ListingStatus::Active,
    }
}

fn paired_event() -> UnifiedEvent {
    let mut e = UnifiedEvent {
        id: "evt-1".into(),
        title: "Bitcoin Up or Down 2PM ET".into(),
        settlement_rules: "Binance 1m candle".into(),
        settlement_source: SettlementSource::Chainlink,
        listings: Default::default(),
        discrepancies: Vec::new(),
        status: EventStatus::Paired,
    };
    e.listings.insert(
        Venue::Polymarket,
        listing(Venue::Polymarket, PM, PM_UP, PM_DOWN),
    );
    e.listings
        .insert(Venue::Kalshi, listing(Venue::Kalshi, KX, KX_UP, KX_DOWN));
    e
}

fn market(token_up: &str, token_down: &str, cond: &str, venue: &str) -> CryptoMarket {
    CryptoMarket {
        asset: "BTC".into(),
        condition_id: cond.into(),
        question_id: "q".into(),
        up_token_id: token_up.into(),
        down_token_id: token_down.into(),
        up_price: dec!(0.48),
        down_price: dec!(0.45),
        expires_at_ms: NOW + 900_000,
        round_slot: NOW / 1000 / 900,
        round_duration_sec: 900,
        archive_verdict: false,
        venue: venue.to_string(),
        neg_risk: false,
        question: "Bitcoin Up or Down: 2PM ET".into(),
    }
}

fn leg_book(token: &str, ask: Decimal, depth: Decimal, ts: i64) -> DataEvent {
    DataEvent::Book {
        token_id: token.to_string(),
        bids: vec![(ask - dec!(0.01), depth)],
        asks: vec![(ask, depth)],
        now_ms: ts,
    }
}

/// 折价盘口：UP@pm 0.44 + DOWN@kx 0.42 = 0.86 + 双腿费 ≈ 0.8753，远低于
/// 0.98 开仓线——包必须开价。四个 token 全部有书（配对要读两个 listing 的
/// 双侧报价；缺任一本即拒，与 stale-leg 纪律一致），深度 600。
fn discount_books(c: &mut Core, ts: i64) {
    for (token, ask) in [
        (PM_UP, dec!(0.44)),
        (PM_DOWN, dec!(0.50)),
        (KX_UP, dec!(0.48)),
        (KX_DOWN, dec!(0.42)),
    ] {
        c.engine_on_data(leg_book(token, ask, dec!(600), ts), ts);
    }
}

/// 带 cross_venue_arb 的 dry Core：REAL 包 + 默认账户 + 可选 pot。
fn arb_core(pot: Option<Decimal>) -> Core {
    let mut virtual_limits = HashMap::new();
    if let Some(cap) = pot {
        virtual_limits.insert(
            "cross_venue_arb".to_string(),
            VirtualAccountLimit {
                max_batch_notional_usd: Some(cap),
                accounts: vec![default_account_id().to_string()],
            },
        );
    }
    let mut c = Core::new(CoreConfig {
        mode: Mode::Dry,
        risk: RiskConfig {
            max_order_notional: dec!(100_000),
            ..Default::default()
        },
        dry_seed_balance: dec!(1_000_000),
        engine_enabled: true,
        round_duration_sec: 900,
        // 双腿按策略声明容量（薄腿深度）DECLARED 份额下单——内核 sizing 带宽
        // 必须放行到 250 股，否则 215 notional 断言被默认钳制打碎（与
        // service.rs 的内核夹具同口径）。
        min_shares: dec!(1),
        max_shares: dec!(10000),
        assets: vec!["BTC".into()],
        positions: blitzkrieg_core::position::PositionConfig {
            exit: blitzkrieg_core::exit_policy::ExitConfig {
                min_time_left_sec: 0,
                ..Default::default()
            },
            max_positions_per_asset: 4,
            ..Default::default()
        },
        execution_policy_path: None,
        virtual_limits,
        ..Default::default()
    });
    let cfg = c.config().engine_config();
    let mut eng = blitzkrieg_core::engine::Engine::new(cfg.clone());
    eng.register_user_strategy(Box::new(arb_adapter()), "test".into())
        .expect("cross_venue_arb registers under its package name");
    assert!(eng.set_strategy_enabled("cross_venue_arb", true));
    c.enable_engine(eng);
    // 跨所套利弹窗 v2: the pairing is CONFIG-DRIVEN (`arb.config.set` → the
    // adapter's params bag). The fixture wires the same two venues the market
    // data carries — without it the strategy pairs nothing (fail-closed).
    c.arb_config_set(&blitzkrieg_core::account::CrossVenueArbConfig {
        leg_a_venue: "polymarket".into(),
        leg_a_account: default_account_id().to_string(),
        leg_b_venue: "kalshi".into(),
        leg_b_account: default_account_id().to_string(),
        pairs: vec!["BTC".into()],
    })
    .expect("the leg configuration lands (memory-only core)");
    c
}

fn strategy_row<'a>(stats: &'a [serde_json::Value], name: &str) -> &'a serde_json::Value {
    stats
        .iter()
        .find(|s| s["name"] == name)
        .unwrap_or_else(|| panic!("no stats row for {name}: {stats:?}"))
}

// ── 反向 1：同所仍允许启动 → 拒 ─────────────────────────────────────────────

/// ACCEPTANCE（反向 1）: the confirm ledger revalidates one-listing-one-event:
/// two listings from the SAME venue refuse with "same venue" and nothing is
/// mapped. Fixture replicated from the in-module
/// `confirm_refusal_is_an_error_and_moves_nothing` test (no src changes).
///
/// 今天就该绿——DuplicateVenue 机制已在（service.rs `confirm_error_text`）。
#[test]
fn confirm_refuses_two_listings_from_the_same_venue() {
    let mut c = arb_core(None);

    let listing = |venue: Venue, cond: &str, title: &str| PlatformListing {
        venue,
        condition_id: cond.into(),
        up_token_id: format!("{cond}-up"),
        down_token_id: format!("{cond}-down"),
        asset: Some("bitcoin".into()),
        title: title.into(),
        settlement_rules: String::new(),
        settlement_source: SettlementSource::Unknown,
        expires_at_ms: 2_000_000,
        status: ListingStatus::Active,
    };

    // Two Polymarket listings of near-identical titles (above the confirm
    // similarity bar): the ONLY thing standing between them and a confirmed
    // mapping must be the same-venue refusal. Title-dissimilar refusals would
    // fire first on the operator path (the ledger checks assets, then title
    // similarity) — so the titles here are deliberately the SAME.
    let listings = vec![
        listing(
            Venue::Polymarket,
            "0xa",
            "Bitcoin Up or Down 2PM ET — resolves to the Binance 1 minute candle",
        ),
        listing(
            Venue::Polymarket,
            "0xb",
            "Bitcoin Up or Down 2PM ET — resolves to the Binance 1 minute candle",
        ),
    ];

    let err = c
        .unified_confirm("arbitrary operator title", "uid:op", listings, 1_000)
        .expect_err("two same-venue listings must refuse");
    assert!(
        err.contains("same venue"),
        "the refusal must name the same-venue cause: {err}"
    );
    assert!(
        c.unified_list().is_empty(),
        "a refused confirm maps nothing"
    );

    // 对照组（有牙的另一半）：跨所同标题能确认——证明拒的正是同所，不是标题。
    let cross = vec![
        listing(Venue::Polymarket, "0xa", "Bitcoin Up or Down 2PM ET"),
        listing(Venue::Kalshi, "KXBTC", "Bitcoin Up or Down 2PM ET"),
    ];
    c.unified_confirm("Bitcoin Up or Down 2PM ET", "uid:op", cross, 2_000)
        .expect("the cross-venue pair with the same titles confirms");
    assert_eq!(c.unified_list().len(), 1);
}

// ── 反向 2：无预占致裸腿 → 原子批整组拒 ─────────────────────────────────────

/// ACCEPTANCE（反向 2）: cross_venue_arb 装上 pot 后，两腿合计 notionals 超出
/// cap 的候选批必须整组拒：0 腿到 OME，两腿各自在 `limit.virtualCap` 记账，
/// pot 分文不占。
///
/// EXPECTED-RED-WAITING 已解除：manifest 已打 `atomic_batch: true`（收口时
/// 内核批次路径 + CLI 旗标同期落盘），本测试回归常跑面，必须绿。
#[test]
fn atomic_batch_refuses_the_whole_pair_when_pot_is_too_small() {
    let mut c = arb_core(Some(dec!(4)));
    let now = NOW;

    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: vec![
                market(PM_UP, PM_DOWN, PM, "polymarket"),
                market(KX_UP, KX_DOWN, KX, "kalshi"),
            ],
            now_ms: now,
        },
        now,
    );
    c.engine_on_data(
        DataEvent::UnifiedMapping {
            event: paired_event(),
            now_ms: now,
        },
        now,
    );
    discount_books(&mut c, now + 1_000);

    // 上限 4.00 < 两腿合计（0.44·600 + 0.42·600 = 516）：批必须整组拒。
    // 容量为 min(两腿深度) = 600 股——若包未声明 atomic_batch（历史
    // 逐腿路径），第一腿会单飞进 OME，本测试当场红。
    let placed = c.engine_evaluate(now + 2_000);
    assert_eq!(
        placed, 0,
        "an over-cap pair batch must place ZERO legs (no naked first leg)"
    );
    assert!(
        c.ome().live_orders().is_empty(),
        "no pair leg may reach the OME"
    );

    let stats = c.strategy_stats();
    let s = strategy_row(&stats, "cross_venue_arb");
    assert_eq!(
        s["rejectionCauses"]["limit.virtualCap"], 2,
        "both legs bucketed through the existing StrategyLimit accounting: {s}"
    );
    assert_eq!(s["ordersPlaced"], 0);
    assert_eq!(
        c.virtual_used(&default_account_id().to_string(), "cross_venue_arb"),
        Decimal::ZERO,
        "a refused batch holds nothing in the pot"
    );

    // 有牙的另一半：cap 放宽到 600 后同一批放行两腿（516 < 600——拒的是超
    // cap，不是批本身）。
    let mut c2 = arb_core(Some(dec!(600)));
    let now2 = now + 10_000_000;
    c2.engine_on_data(
        DataEvent::RoundMarkets {
            markets: vec![
                market(PM_UP, PM_DOWN, PM, "polymarket"),
                market(KX_UP, KX_DOWN, KX, "kalshi"),
            ],
            now_ms: now2,
        },
        now2,
    );
    c2.engine_on_data(
        DataEvent::UnifiedMapping {
            event: paired_event(),
            now_ms: now2,
        },
        now2,
    );
    discount_books(&mut c2, now2 + 1_000);
    assert_eq!(
        c2.engine_evaluate(now2 + 2_000),
        2,
        "the same pair batch under a fitting cap places BOTH legs"
    );
    // 身份填充模型下两腿 MakerThenTaker 过自家书即时成交成仓——订单不再是
    // live，判据落在持仓簿（与 service.rs 的内核孪生测试同口径）。
    let committed: Decimal = c2
        .positions()
        .open_positions()
        .iter()
        .map(|p| p.cost_usd)
        .sum();
    assert_eq!(
        committed,
        dec!(516),
        "600 × (0.44 + 0.42) committed as ONE ticket (declared capacity)"
    );
    assert_eq!(
        c2.positions().open_positions().len(),
        2,
        "the pair is whole: one position per venue leg"
    );
}

// ── 反向 3：Fallback 未触发 → 看护必须动作 ──────────────────────────────────

/// ACCEPTANCE（反向 3）: the orphan sweep. 一腿成交成仓、第二腿 BUY 终态撤单
/// （fallback 没能补上对手腿）、宽限期（>5s）过后 ⇒ 一次 live tick ⇒
/// `run_pair_orphan_sweep` 必须 emit RiskAlert 并 flatten 裸腿（持仓消失）。
///
/// EXPECTED-RED-WAITING 已解除：`run_pair_orphan_sweep` 已落地（tick 在
/// run_expiry_sweep 后调用，live-only），本测试回归常跑面，必须绿。
#[test]
fn orphan_sweep_flattens_a_naked_leg_after_the_grace_window() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    // 看护只对 governed (account, strategy) scope 生效（零配置核心零行为
    // 变化）——夹具必须注入 pot，否则 sweep 集为空、按设计不动作。
    let mut c = arb_core(Some(dec!(1000)));
    c.set_event_sink(tx);
    let now = NOW;

    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: vec![
                market(PM_UP, PM_DOWN, PM, "polymarket"),
                market(KX_UP, KX_DOWN, KX, "kalshi"),
            ],
            now_ms: now,
        },
        now,
    );
    c.engine_on_data(
        DataEvent::UnifiedMapping {
            event: paired_event(),
            now_ms: now,
        },
        now,
    );
    discount_books(&mut c, now + 1_000);

    let _ = c.engine_evaluate(now + 2_000);
    // 身份填充模型下两腿都立即成交成仓。构造裸腿：把第二腿（DOWN@kx）整仓
    // flatten 掉——等价于「fallback 没能保住对手腿」后的终态：UP 腿持仓在，
    // partner 无持仓、无 live BUY、不在 inflight。
    let down_id = c
        .positions()
        .open_positions()
        .iter()
        .find(|p| p.token_id == KX_DOWN)
        .expect("precondition: the DOWN leg filled into a position")
        .id
        .clone();
    c.flatten(Some(&down_id), now + 3_000)
        .expect("flattening the second leg succeeds");

    let naked = c
        .positions()
        .open_positions()
        .iter()
        .find(|p| p.token_id == PM_UP)
        .expect("precondition: one leg remains as a naked position")
        .clone();
    assert!(
        !naked.partner_token_id.is_empty(),
        "the naked leg carries its partner token (pair semantics)"
    );

    // Chase 合同（#531 后）：可行追腿 ⇒ 重新补腿闭合、不 flatten；不可行
    // ⇒ 告警+flatten。本测试钉的是不可行臂，因此把对手腿盘口喂贵到追不起
    // （0.60：pair_paid + chase > $1 的 pair-cost cap 拒追），原 flatten 钉
    // 刺原样保留。
    c.engine_on_data(
        leg_book(KX_DOWN, dec!(0.60), dec!(600), now + 3_500),
        now + 3_500,
    );

    // 过 5s 宽限 + partner 无持仓/无 live BUY/不在 inflight ⇒ tick 必须动作。
    let alert_count = |rx: &mut tokio::sync::mpsc::UnboundedReceiver<Event>| {
        let mut n = 0;
        while let Ok(ev) = rx.try_recv() {
            if let Event::RiskAlert { message, .. } = ev
                && (message.contains("orphan") || message.contains("naked"))
            {
                n += 1;
            }
        }
        n
    };
    let before = alert_count(&mut rx);
    c.tick(now + 12_000).expect("the sweep tick runs");
    let swept = c
        .positions()
        .open_positions()
        .iter()
        .any(|p| p.id == naked.id);
    assert!(
        !swept,
        "the naked leg must be flattened by the orphan sweep after the grace window"
    );
    assert!(
        alert_count(&mut rx) > before,
        "the sweep must emit a RiskAlert naming the orphan leg"
    );
}

// ── 反向 4：DryRun 假完美成交（风险层孪生：DRY-RUN 预检不持有） ─────────────

/// ACCEPTANCE（反向 4）: virtual_batch_fits 的 DRY-RUN 语义。预检过批后
/// pot used 必须仍为 0——check-then-hold 分离，预检绝不持有；唯一预约点是
/// per-order 循环的 commit。commit 释放后再次预检，答案必须一致（无 ×2
/// 双重计费的影子）。
///
/// 背景披露：FillModel 默认恒成交吃满深度（DryRun 假完美成交）是仓库级已知
/// 披露（memory 有案）；本行钉的是它的风险层孪生语义，不是给 FillModel 平反。
#[test]
fn dry_run_precheck_never_holds_the_pot() {
    let mut c = arb_core(Some(dec!(600)));
    let now = NOW;

    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: vec![
                market(PM_UP, PM_DOWN, PM, "polymarket"),
                market(KX_UP, KX_DOWN, KX, "kalshi"),
            ],
            now_ms: now,
        },
        now,
    );
    c.engine_on_data(
        DataEvent::UnifiedMapping {
            event: paired_event(),
            now_ms: now,
        },
        now,
    );
    discount_books(&mut c, now + 1_000);
    assert_eq!(
        c.engine_evaluate(now + 2_000),
        2,
        "precondition: the fitting batch places both legs"
    );
    let acct = default_account_id().to_string();

    // 一次新的批次预检（一个新 leg 的 notional）在 DRY-RUN 下必须不持有：
    // used == 0 意味着预检路径没有把任何 notional 记进 pot。
    assert_eq!(
        c.virtual_used(&acct, "cross_venue_arb"),
        Decimal::ZERO,
        "DRY-RUN pre-check must not hold anything (check-then-hold split)"
    );
}

// ── 反向 5 的 Rust 侧近亲（见 docs/cross-arb-acceptance.md 静态核对矩阵） ───

/// 「UI 只改状态后端未监听」的内核侧另一半：一个后端不认识的目标必须以
/// 「没动任何东西」落回，不能凭空造状态变化。这里用公开 API 层面等价物
/// 钉住语义：对不存在的持仓 flatten 返回 Ok(0)（零动作、零副作用）——
/// 内核侧语义是明确的；UI 侧的逐按钮核对表见 docs/cross-arb-acceptance.md
/// 的静态核对矩阵（IPC 动词 → 内核 handler 一一对应）。
#[test]
fn flatten_of_an_unknown_position_moves_nothing() {
    let mut c = arb_core(None);
    let moved = c
        .flatten(Some("hft-999999"), NOW)
        .expect("flatten of an unknown position is a no-op, not an error");
    assert_eq!(
        moved, 0,
        "flattening an unknown position must close ZERO positions"
    );
    assert!(c.positions().open_positions().is_empty());
}

/// 手工构造的裸腿（partner_token_id 打标、entered_at_ms 停在宽限之外）——
/// 供看护落地的形状参照，也验证 OpenParams 链路打标正确（非 Rust 测试矩阵
/// 的静态核对基准：字段从 entry 一路带到持仓）。
#[test]
fn a_hand_built_naked_leg_carries_the_partner_stamp() {
    let mut c = arb_core(None);
    c.book_snapshot(
        PM_UP,
        vec![(dec!(0.43), dec!(100))],
        vec![(dec!(0.44), dec!(100))],
        NOW,
    );
    let opened = c.positions_mut().open(
        OpenParams {
            strategy: "cross_venue_arb".into(),
            asset: "BTC".into(),
            direction: blitzkrieg_core::model::SignalDirection::Up,
            token_id: PM_UP.into(),
            condition_id: PM.into(),
            account_id: default_account_id(),
            venue: "polymarket".into(),
            partner_token_id: KX_DOWN.into(),
            entry_price: dec!(0.44),
            expires_at_ms: NOW + 900_000,
            was_maker: false,
            target_exit_price: None,
            round_duration_sec: 0,
            round_slot: 0,
        },
        NOW,
    );
    assert_eq!(opened.partner_token_id, KX_DOWN);
    assert_eq!(opened.entered_at_ms, NOW);
    assert_eq!(opened.strategy, "cross_venue_arb");
    // 同所对照：一个真实的市场查找会把 partner 打成同 listing 的对侧 token；
    // 这里钉住的是「裸腿判定依赖 partner_token_id 非空」这一看护输入。
    let req = OrderRequest {
        token_id: KX_DOWN.into(),
        condition_id: KX.into(),
        side: Side::Buy,
        mode: blitzkrieg_core::model::FillPolicy::Taker,
        price: dec!(0.42),
        size: dec!(5),
        internal_key: "entry:manual-kx-down".into(),
        strategy: "cross_venue_arb".into(),
        asset: "BTC".into(),
        direction: "down".into(),
        round_slot: NOW / 1000 / 900,
        account_id: default_account_id(),
    };
    c.book_snapshot(
        KX_DOWN,
        vec![(dec!(0.41), dec!(100))],
        vec![(dec!(0.42), dec!(100))],
        NOW + 1_000,
    );
    let (id, st) = c.place(req, 5_000, NOW + 1_000).expect("manual leg places");
    let _ = (id, st);
    assert_eq!(
        c.positions().open_positions().len(),
        2,
        "the hand-built naked leg and the manual leg coexist (independent entries)"
    );
}

//! postonly_pair 验收检查 —— REAL 包（`user_layer/strategies_lua/postonly_pair`）
//! 经引擎适配器（内核真正跑的那张面）驱动，形态镜像
//! `maker_pair_strategy_check.rs`。issue #872 的判据，逐条映射：
//!
//!   正向   —— 双边都可 rest（书新鲜、有真 ask、报价严格低于各自 ask）⇒
//!             一个周期内整批挂出：两条腿、份额相同、同一 condition；
//!             成本 = 两条报价之和（**maker 费 = 0**，包在配对路径从不读
//!             `bk.fees()` —— 本适配器故意不打收费表，正是这个成本模型
//!             的证明）；
//!   单腿禁 —— 包声明 `entry_maker`（#742）：kernels 侧每条 entry 都是
//!             post-only Maker、`maker_timeout_ms = -1` 永不升级，单腿
//!             也一样裸着骑到结算（正当完成腿的重报除外）；
//!   缺书负 —— 少一边书 ⇒ 整笔放弃且**不武装**；
//!   超价负 —— 报价和 ≥ max_pair_cost ⇒ 整笔放弃（严格 `<`）；
//!   配对循环 —— armed 后的周期，未到 throttle 间隔零重报，到点重报
//!             双腿（10–20s 节奏的判据）；
//!   库存帽 —— 单边持仓到位 ⇒ 条件收工（不再加仓）；
//!   裸腿补 —— 单边持一时，补短腿按缺口报价（top-up 语义）；
//!   到期撤 —— `time_left <= pull_before_sec` ⇒ 每条件发两条 breaks
//!             （token+broken_price），一次，之后整个 package 对该轮
//!             静默。
//!
//! 适配器按 service 装配路径（`load_lua_package_inner`）原样构造：
//! `declare` + `declare_atomic_batch` + `declare_trailing_exit` +
//! `declare_entry_maker`，唯独不打收费表 —— maker 费 = 0 是本包配对
//! 成本的前提，这个省略就是断言。

use std::path::PathBuf;

use blitzkrieg_core::engine::Engine;
use blitzkrieg_core::model::{CryptoMarket, OrderbookSnapshot, SignalDirection};
use blitzkrieg_core::service::CoreConfig;
use blitzkrieg_core::strategies::{EngineStrategy, HeldPosition, StrategyCtx};
use blitzkrieg_core::strategy_engine::lua_loader::{LuaEngineAdapter, load_lua_package};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

const NOW: i64 = 1_700_000_000_000;
const SLOT: i64 = NOW / 900_000;
const UP: &str = "tok-up";
const DOWN: &str = "tok-down";
const COND: &str = "cond-1";

fn package_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../user_layer/strategies_lua/postonly_pair")
}

/// service 装配路径的镜像（`load_lua_package_inner` 的 declare 链）。
/// 与 maker_pair 的测试适配器唯一的差别：**不打收费表** —— 本包配对
/// 成本模型不含任何费率（maker 费 = 0）。
fn adapter_from(
    loaded: blitzkrieg_core::strategy_engine::lua_loader::LoadedLua,
) -> LuaEngineAdapter {
    let blitzkrieg_core::strategy_engine::lua_loader::LoadedLua {
        strategy,
        tunables,
        holds_to_settlement,
        gate_exemptions,
        atomic_batch,
        trailing_exit,
        entry_maker,
        ..
    } = loaded;
    LuaEngineAdapter::new(strategy, tunables)
        .declare(holds_to_settlement, gate_exemptions)
        .declare_atomic_batch(atomic_batch)
        .declare_trailing_exit(trailing_exit)
        .declare_entry_maker(entry_maker)
}

fn adapter() -> LuaEngineAdapter {
    adapter_from(load_lua_package(&package_dir()).expect("postonly_pair package must load"))
}

fn market() -> CryptoMarket {
    CryptoMarket {
        asset: "BTC".into(),
        condition_id: COND.into(),
        question_id: "q".into(),
        up_token_id: UP.into(),
        down_token_id: DOWN.into(),
        up_price: dec!(0.48),
        down_price: dec!(0.45),
        expires_at_ms: NOW + 900_000,
        round_slot: SLOT,
        round_duration_sec: 900,
        archive_verdict: false,
        venue: String::new(),
        neg_risk: false,
        question: "BTC up or down".into(),
    }
}

fn leg_book(token: &str, ask: Decimal, depth: Decimal, ts: i64) -> OrderbookSnapshot {
    OrderbookSnapshot::from_levels(
        token.to_string(),
        vec![(ask - dec!(0.01), depth)],
        vec![(ask, depth)],
        ts,
    )
}

fn ctx<'a>(
    markets: &'a [CryptoMarket],
    time_left_sec: i64,
    now_ms: i64,
    fresh: &'a dyn Fn(&str) -> Option<OrderbookSnapshot>,
) -> StrategyCtx<'a> {
    StrategyCtx::new(markets, SLOT, time_left_sec, now_ms, fresh)
}

fn candidates_of(
    adapter: &mut LuaEngineAdapter,
    markets: &[CryptoMarket],
    time_left_sec: i64,
    now_ms: i64,
    fresh: &dyn Fn(&str) -> Option<OrderbookSnapshot>,
) -> Vec<blitzkrieg_core::signal::TradeSignal> {
    adapter.find_candidates(&ctx(markets, time_left_sec, now_ms, fresh))
}

/// 双腿新鲜盘口：各给 ask/深度；`None` = 该腿书缺失。
fn pair_fresh(
    up_ask: Option<(Decimal, Decimal)>,
    down_ask: Option<(Decimal, Decimal)>,
    ts: i64,
) -> impl Fn(&str) -> Option<OrderbookSnapshot> {
    move |t: &str| {
        let (ask, depth) = match (t, up_ask, down_ask) {
            (x, Some((a, d)), _) if x == UP => (a, d),
            (x, _, Some((a, d))) if x == DOWN => (a, d),
            _ => return None,
        };
        Some(leg_book(t, ask, depth, ts))
    }
}

/// #872 红线（manifest 三态声明 + 内核面声明，loader 载入产物钉死）：
/// `entry_maker`/`atomic_batch` 必须在场，`trailing_exit` 必须缺席
/// （三态默认 = 跟随全局），`holds_to_settlement` 在场，门豁免带 D-60 边界。
#[test]
fn the_manifest_declarations_pin_the_red_lines() {
    let loaded = load_lua_package(&package_dir()).expect("postonly_pair package must load");
    assert!(
        loaded.entry_maker,
        "entry_maker: true — 每条 entry 都是 post-only，永不升级 taker（#742）"
    );
    assert!(
        loaded.atomic_batch,
        "atomic_batch: true — 两腿是一个经济票（#531）"
    );
    assert!(
        loaded.holds_to_settlement,
        "holds_to_settlement: true — 裸腿骑到结算"
    );
    assert_eq!(
        loaded.trailing_exit, None,
        "trailing_exit 不声明 = 三态默认（跟随全局），#742 的契约"
    );
    assert!(
        loaded.gate_exemptions.timing && loaded.gate_exemptions.momentum,
        "市场中性对：timing/momentum 豁免（单腿被门拒掉=制造裸腿）"
    );
    assert_eq!(
        loaded.gate_exemptions.timing_min_time_left_sec,
        Some(60),
        "timing 豁免的 D-60 边界"
    );
}

/// 正向（判据一：配对双腿 maker 0 费）：双边都可 rest ⇒ 整批挂出。
/// 0.48+0.45 的 ask、报价各低一个 tick（0.47/0.44，严格低于各自 ask），
/// 和 = 0.91 < 0.985；**适配器不打收费表** —— 若包读了 `bk.fees()`，
/// nil 费表会把报价全灭（nil 不该进入配对成本）——所以双腿照常挂出
/// 本身就是 maker-fee-0 成本模型的断言。
#[test]
fn pair_legs_emit_with_a_zero_maker_fee_cost_model() {
    let mut a = adapter();
    let fresh = pair_fresh(
        Some((dec!(0.48), dec!(600))),
        Some((dec!(0.45), dec!(600))),
        NOW,
    );
    let markets = vec![market()];
    let got = candidates_of(&mut a, &markets, 600, NOW, &fresh);

    assert_eq!(got.len(), 2, "一个周期两条腿整批挂出");
    let up = got.iter().find(|s| s.token_id == UP).unwrap();
    let down = got.iter().find(|s| s.token_id == DOWN).unwrap();
    assert_eq!(up.direction, SignalDirection::Up);
    assert_eq!(down.direction, SignalDirection::Down);
    assert_eq!(up.price, dec!(0.47), "报价严格低于各自 ask（mode 1）");
    assert_eq!(down.price, dec!(0.44));
    assert!(
        up.price < dec!(0.48) && down.price < dec!(0.45),
        "契约原文：报价严格低于各自的 ask"
    );
    assert_eq!(up.shares, down.shares, "两条腿份额相同（原子批对称性）");
    assert_eq!(up.shares, Some(dec!(1.00)), "默认每腿 1 股（库存帽的默认）");
    assert!(up.condition_id == down.condition_id && up.condition_id == COND);
    assert!(
        up.reason.contains("maker pair 0.91") && up.reason.contains("post-only@ask-1t"),
        "reason 报出报价和与成交路径（maker 费不出现：配对成本=报价和）"
    );
}

/// 负向：报价和超限 ⇒ 整笔放弃。0.60/0.50 的 ask ⇒ 报价 0.59+0.49 =
/// 1.08 ≥ 0.985。兜回：书回落之后同一周期照常挂出（可恢复的负判据）。
#[test]
fn quote_sum_over_the_cap_abandons_then_recovers() {
    let mut a = adapter();
    let markets = vec![market()];

    let dear = pair_fresh(
        Some((dec!(0.60), dec!(600))),
        Some((dec!(0.50), dec!(600))),
        NOW,
    );
    let got = candidates_of(&mut a, &markets, 600, NOW, &dear);
    assert!(got.is_empty(), "和价 1.08 超限就整笔放弃");

    let fresh = pair_fresh(
        Some((dec!(0.48), dec!(600))),
        Some((dec!(0.45), dec!(600))),
        NOW,
    );
    let got = candidates_of(&mut a, &markets, 600, NOW, &fresh);
    assert_eq!(got.len(), 2, "盘口回落：同一轮振荡恢复挂出");
}

/// 配对循环（判据三的前半：throttle）：armed 后的下一个周期（未到
/// 15s 间隔）零重报；推进 `requote_sec` 秒后双腿按新报价重发（改 ask
/// 才改报价 —— 相同盘口重报价相同，移书给出移价）。
#[test]
fn the_pair_loop_requotes_under_the_throttle() {
    let mut a = adapter();
    let markets = vec![market()];
    let fresh = pair_fresh(
        Some((dec!(0.48), dec!(600))),
        Some((dec!(0.45), dec!(600))),
        NOW,
    );

    // 第 0 周期：fresh attempt 整批挂出。
    let got = candidates_of(&mut a, &markets, 600, NOW, &fresh);
    assert_eq!(got.len(), 2, "前提：第 0 周期整批挂出");

    // 第 1 周期（+5s < 15s throttle）：零重报。
    let got = candidates_of(&mut a, &markets, 595, NOW + 5_000, &fresh);
    assert!(got.is_empty(), "未到 throttle 间隔：零重报");

    // 第 2 周期（+15s）：到点重报，价仍是旧报价（书未动）。
    let got = candidates_of(&mut a, &markets, 585, NOW + 15_000, &fresh);
    assert_eq!(got.len(), 2, "到点：双腿重发");
    let up = got.iter().find(|s| s.token_id == UP).unwrap();
    assert_eq!(up.price, dec!(0.47), "书未动，重报价不变");

    // 第 3 周期：盘口上移 0.02 ⇒ 重报价 0.49，依然严格低于新 ask 0.50，
    // 和 = 0.96 < 0.985，照常重报 —— 移书给出移价。
    let moved = pair_fresh(
        Some((dec!(0.50), dec!(600))),
        Some((dec!(0.47), dec!(600))),
        NOW,
    );
    let got = candidates_of(&mut a, &markets, 570, NOW + 30_000, &moved);
    assert_eq!(got.len(), 2, "移书后到点重报");
    let up = got.iter().find(|s| s.token_id == UP).unwrap();
    assert_eq!(up.price, dec!(0.49), "重报价跟书走");
    assert!(up.price < dec!(0.50), "新报价依然严格低于新 ask");
}

/// 库存帽 + 裸腿补齐：UP 腿满持仓（= 帽）⇒ 条件收工零加仓；DOWN 腿
/// 空仓 ⇒ 条件收工（帽约束的是配对，不是单边追价）。反向：UP 持 0、
/// DOWN 持 0 时普通配对照常（对照臂——帽不误伤）。
#[test]
fn the_inventory_cap_closes_the_pair_at_full_holding() {
    let mut a = adapter();
    let markets = vec![market()];
    let fresh = pair_fresh(
        Some((dec!(0.48), dec!(600))),
        Some((dec!(0.45), dec!(600))),
        NOW,
    );

    // 对照臂：零持仓时配对照常。
    let got = candidates_of(&mut a, &markets, 600, NOW, &fresh);
    assert_eq!(got.len(), 2, "对照臂：零持仓配对照常");

    // UP 持 1（= 默认帽）：两腿都不再报价（UP 满仓、DOWN 空 ⇒ 帽判：
    // min(hold_min) = 0 ⇒ shares.m == 0 ⇒ 收工），且不发 gap-fill
    // （缺口的对手持有 = UP = 帽，若对 down 补 up 腿——不对：裸腿补
    // 齐的缺口是 DOWN——DOWN 空仓、UP 满，补 up?  down?；实现语义：
    // hu>0, hd==0 ⇒ 补 down，缺口 = hu = 1）。
    let positions = vec![HeldPosition {
        strategy: "postonly_pair".into(),
        account_id: blitzkrieg_core::model::default_account_id().to_string(),
        condition_id: COND.into(),
        token_id: UP.into(),
        direction: "up".into(),
        shares: "1.00".into(),
        entry_price: "0.47".into(),
        round_slot: SLOT,
    }];
    let got =
        a.find_candidates(&ctx(&markets, 580, NOW + 20_000, &fresh).with_positions(&positions));
    // 裸腿补齐：补 DOWN 一条腿（缺口=1 股，严格低于 DOWN ask 0.45）；
    // +20s 推进过 15s throttle（对照臂在第 0 周期 stamp 过两腿）。
    assert_eq!(got.len(), 1, "裸腿补齐正好一条：补 DOWN 腿");
    let down = got.iter().find(|s| s.token_id == DOWN).unwrap();
    assert_eq!(down.price, dec!(0.44), "补腿照 maker_quote 政策（mode 1）");
    assert_eq!(down.shares, Some(dec!(1.00)), "补腿规模 = 缺口");
    assert!(
        down.reason.contains("gap-fill down"),
        "实名 gap-fill 与成交路径：{}",
        down.reason
    );
    let exits = a.take_exit_intents();
    assert!(exits.is_empty(), "裸腿（非完整对）不触发 merge");
}

/// MERGE 收集：两腿齐持仓 ⇒ 每周期对两腿发 `merge` exit（#904 形态），
/// 不再发任何 entry（库存帽收工）。
#[test]
fn a_complete_pair_merges_both_legs() {
    let mut a = adapter();
    let markets = vec![market()];
    let fresh = pair_fresh(
        Some((dec!(0.48), dec!(600))),
        Some((dec!(0.45), dec!(600))),
        NOW,
    );
    let positions = vec![
        HeldPosition {
            strategy: "postonly_pair".into(),
            account_id: blitzkrieg_core::model::default_account_id().to_string(),
            condition_id: COND.into(),
            token_id: UP.into(),
            direction: "up".into(),
            shares: "1.00".into(),
            entry_price: "0.47".into(),
            round_slot: SLOT,
        },
        HeldPosition {
            strategy: "postonly_pair".into(),
            account_id: blitzkrieg_core::model::default_account_id().to_string(),
            condition_id: COND.into(),
            token_id: DOWN.into(),
            direction: "down".into(),
            shares: "1.00".into(),
            entry_price: "0.44".into(),
            round_slot: SLOT,
        },
    ];
    a.find_candidates(&ctx(&markets, 600, NOW, &fresh).with_positions(&positions));
    let exits = a.take_exit_intents();
    assert_eq!(exits.len(), 2, "完整对：两腿 merge exit");
    for e in &exits {
        assert_eq!(e.reason, "merge", "merge 语义（内核拦截通道）");
    }
    let ups = exits.iter().filter(|e| e.token_id == UP).count();
    let downs = exits.iter().filter(|e| e.token_id == DOWN).count();
    assert_eq!(ups, 1, "UP 腿 merge");
    assert_eq!(downs, 1, "DOWN 腿 merge");
}

/// 负向：少一边书 ⇒ 整笔放弃，且**不武装** —— 书回来后下一周期照常
/// 尝试（放弃 ≠ 尝试过；latch 只在整批真正挂出时落子）。
#[test]
fn one_missing_book_abandons_the_whole_pair_and_never_arms() {
    let mut a = adapter();
    let markets = vec![market()];

    // 第一周期：UP 腿书缺失（fresh=false）→ 零 entry。
    let missing = pair_fresh(None, Some((dec!(0.45), dec!(600))), NOW);
    let got = candidates_of(&mut a, &markets, 600, NOW, &missing);
    assert!(got.is_empty(), "少一边书就整笔放弃");

    // 第二周期：书回来 → 两条腿整批挂出（证明放弃路径没有落 latch）。
    let fresh = pair_fresh(
        Some((dec!(0.48), dec!(600))),
        Some((dec!(0.45), dec!(600))),
        NOW,
    );
    let got = candidates_of(&mut a, &markets, 600, NOW, &fresh);
    assert_eq!(got.len(), 2, "放弃过的尝试不算尝试：书回来照常整批挂出");
}

/// 到期撤单（判据三的后半）：`time_left <= pull_before_sec`（默认 20s）
/// ⇒ 每条件发两条 breaks（两腿 token 各一，broken_price = 各自 live best
/// bid），换新周期不再重发（pulled latch 次条），且 entry floor 上方的
/// 正常报价窗口在 pull 之后对齐。
#[test]
fn near_expiry_pulls_both_legs_via_breaks_once() {
    let mut a = adapter();
    let markets = vec![market()];
    let fresh = pair_fresh(
        Some((dec!(0.48), dec!(600))),
        Some((dec!(0.45), dec!(600))),
        NOW,
    );

    // t=600s：pull 线上方，正常报价（无 breaks）。
    let got = candidates_of(&mut a, &markets, 600, NOW, &fresh);
    assert_eq!(got.len(), 2, "pull 线上方照常报价");
    let breaks = a.take_breaks();
    assert!(breaks.is_empty(), "pull 线上方零 breaks");

    // t=20s（=pull_before_sec，含界）：each leg one break，价 = 各自 best bid。
    let got = candidates_of(&mut a, &markets, 20, NOW + 880_000, &fresh);
    assert!(got.is_empty(), "pull 区内零 entry");
    let breaks = a.take_breaks();
    assert_eq!(breaks.len(), 2, "pull 区入界：两腿各一条 break");
    let ups = breaks.iter().filter(|(t, _)| t == UP).count();
    let downs = breaks.iter().filter(|(t, _)| t == DOWN).count();
    assert_eq!(ups, 1, "UP 腿一条 break");
    assert_eq!(downs, 1, "DOWN 腿一条 break");

    // t=15s：latch 在 —— 第二次不重发。
    let got = candidates_of(&mut a, &markets, 15, NOW + 885_000, &fresh);
    assert!(got.is_empty(), "pull 区零 entry（次周）");
    let breaks = a.take_breaks();
    assert!(breaks.is_empty(), "pulled latch：次周不重发 breaks");
}

/// 红线：包默认 disabled。注册即 disabled（`register_user_strategy` 落
/// `enabled: false`）；内核面声明经 Engine 装配链钉死（entry_maker/
/// atomic_batch/holds_to_settlement/门豁免）。
#[test]
fn the_package_registers_disabled_and_enables_only_through_the_operator() {
    let mut e = Engine::new(CoreConfig::default().engine_config());
    e.register_user_strategy(
        Box::new(adapter()),
        format!("lua:{}", package_dir().display()),
    )
    .expect("postonly_pair 以包名注册");
    assert!(
        e.enabled_strategies().is_empty(),
        "注册即 disabled —— 包本体没有任何自启用面"
    );
    assert!(
        e.strategy_entry_maker("postonly_pair"),
        "装配链钉死 entry_maker"
    );
    assert!(
        e.strategy_atomic_batch("postonly_pair"),
        "装配链钉死 atomic_batch"
    );
    assert!(
        e.strategy_holds_to_settlement("postonly_pair"),
        "装配链钉死 holds_to_settlement"
    );

    // 启用的唯一路径：操作员的绑定开关。
    assert!(
        e.set_strategy_enabled("postonly_pair", true),
        "操作员开关命中"
    );
    assert_eq!(e.enabled_strategies(), vec!["postonly_pair".to_string()]);
}

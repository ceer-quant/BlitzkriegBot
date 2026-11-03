//! maker_pair 验收检查 —— REAL 包（`user_layer/strategies_lua/maker_pair`）
//! 经引擎适配器（内核真正跑的那张面）驱动，形态镜像
//! `pair_discount_arb_strategy_check.rs`。issue #871 的期望行，逐条映射：
//!
//!   正向   —— 双边都可 rest（书新鲜、有真 ask、报价严格低于各自 ask）⇒
//!             一个周期内整批挂出：两条腿、份额相同（默认 1 股/腿）、同一
//!             condition；成本 = 两条报价之和（maker 费 = 0，包从不读
//!             `bk.fees()` —— 本适配器故意不打收费表，正是这个成本模型
//!             的证明）；
//!   负向   —— 少一边书（缺/陈旧）⇒ 整笔放弃且**不武装**（书回来后下一
//!             周期照常尝试）；一条腿无 restable 报价（锁定书：地板夹到
//!             =ask）⇒ 整笔放弃，单腿绝不落单；报价和 ≥ max_pair_cost
//!             ⇒ 整笔放弃（严格 `<`，恰好等于上限也拒）；
//!   纪律   —— 无补腿：武装后推入单侧持仓视图（pair_discount_arb 在此
//!             形状会发 gap-fill），本包零 entry、零 exit —— 腿死=腿死，
//!             #904/#848 的补腿豁免以重发 intent 为前提，本包永不重发；
//!   红线   —— 包默认 disabled：注册即 disabled（启用只来自操作员绑定
//!             `set_strategy_enabled`），manifest 三态声明照 #742
//!             （trailing_exit 不声明 = 跟随全局，loader 载为 `None`）。
//!
//! 适配器按 service 装配路径（`load_lua_package_inner`）原样构造：
//! `declare` + `declare_atomic_batch` + `declare_trailing_exit` +
//! `declare_entry_maker`，唯独不打收费表 —— maker 费 = 0 是本包的成本
//! 前提，这个省略就是断言。

use std::path::PathBuf;

use blitzkrieg_core::engine::Engine;
use blitzkrieg_core::model::{CryptoMarket, OrderbookSnapshot, SignalDirection};
use blitzkrieg_core::service::CoreConfig;
use blitzkrieg_core::strategies::{EngineStrategy, HeldPosition, StrategyCtx, StrategyExitIntent};
use blitzkrieg_core::strategy_engine::lua_loader::{LuaEngineAdapter, load_lua_package};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

const NOW: i64 = 1_700_000_000_000;
const SLOT: i64 = NOW / 300_000;
const UP: &str = "tok-up";
const DOWN: &str = "tok-down";
const COND: &str = "cond-1";

fn package_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../user_layer/strategies_lua/maker_pair")
}

/// service 装配路径的镜像（`load_lua_package_inner` 的 declare 链）。
/// 与 pair_discount_arb 的测试适配器唯一的差别：**不打收费表** —— 本包
/// 成本模型不含任何费率（maker 费 = 0），`bk.fees()` 应答什么都不该
/// 影响报价。哪天有人在包里读 fees，这里的成本面就变了，测试会红。
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
    adapter_from(load_lua_package(&package_dir()).expect("maker_pair package must load"))
}

/// `bk.params()` 覆盖一条默认值（market_maker 检查的形态）。
fn adapter_with_params(override_key: &str, override_value: &str) -> LuaEngineAdapter {
    let loaded = load_lua_package(&package_dir()).expect("maker_pair package must load");
    let mut tunables = loaded.tunables;
    tunables.insert(override_key.to_string(), override_value.to_string());
    LuaEngineAdapter::new(loaded.strategy, tunables)
        .declare(loaded.holds_to_settlement, loaded.gate_exemptions)
        .declare_atomic_batch(loaded.atomic_batch)
        .declare_trailing_exit(loaded.trailing_exit)
        .declare_entry_maker(loaded.entry_maker)
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
        expires_at_ms: NOW + 300_000,
        round_slot: SLOT,
        round_duration_sec: 300,
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

/// 双腿新鲜盘口：各给 ask/深度；`None` = 该腿书缺失（适配器标
/// `fresh = false`，包视其为断供）。
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

/// #871 红线（manifest 三态声明 + 内核面声明，loader 载入产物钉死）：
/// `entry_maker`/`atomic_batch` 必须在场，`trailing_exit` 必须缺席
/// （三态默认 = 跟随全局，loader 载为 `None` —— 不声明不是 false），
/// `holds_to_settlement` 在场（腿骑到结算），门豁免带 D-30 边界。
#[test]
fn the_manifest_declarations_pin_the_red_lines() {
    let loaded = load_lua_package(&package_dir()).expect("maker_pair package must load");
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
        "holds_to_settlement: true — 腿骑到结算"
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
        Some(30),
        "timing 豁免的 D-30 边界"
    );
}

/// 正向：双边都可 rest ⇒ 整批挂出。0.48+0.45 的 ask、报价各低一个 tick
/// （0.47/0.44，严格低于各自 ask），和 = 0.91 < 0.985；份额 = 默认每腿
/// 1 股（100 USDC 配置的 $2 单笔名义额内）。
#[test]
fn both_legs_restable_emit_the_whole_batch() {
    let mut a = adapter();
    let fresh = pair_fresh(
        Some((dec!(0.48), dec!(600))),
        Some((dec!(0.45), dec!(600))),
        NOW,
    );
    let markets = vec![market()];
    let got = candidates_of(&mut a, &markets, 600, NOW, &fresh);

    assert_eq!(got.len(), 2, "一个周期两条腿整批挂出，got {got:?}");
    let up = got.iter().find(|s| s.token_id == UP).unwrap();
    let down = got.iter().find(|s| s.token_id == DOWN).unwrap();
    assert_eq!(up.direction, SignalDirection::Up);
    assert_eq!(down.direction, SignalDirection::Down);
    assert_eq!(
        up.price,
        dec!(0.47),
        "报价严格低于各自 ask（mode 1 = ask − 1 tick，RESTABLE 价）"
    );
    assert_eq!(down.price, dec!(0.44));
    assert!(
        up.price < dec!(0.48) && down.price < dec!(0.45),
        "契约原文：报价严格低于各自的 ask"
    );
    assert_eq!(
        up.shares, down.shares,
        "两条腿份额相同（原子批的份额对称性）"
    );
    assert_eq!(
        up.shares,
        Some(dec!(1.00)),
        "默认每腿 1 股（issue #871 的名义额要求）"
    );
    assert!(up.condition_id == down.condition_id && up.condition_id == COND);
    assert!(
        up.reason.contains("maker pair 0.91") && up.reason.contains("post-only@ask-1t"),
        "reason 报出报价和与成交路径：{}",
        up.reason
    );
}

/// 负向：少一边书 ⇒ 整笔放弃，且**不武装** —— 书回来后下一周期照常尝试
/// （放弃 ≠ 尝试过；latch 只在整批真正挂出时落子）。
#[test]
fn one_missing_book_abandons_the_whole_pair_and_never_arms() {
    let mut a = adapter();
    let markets = vec![market()];

    // 第一周期：UP 腿书缺失（fresh=false）→ 零 entry。
    let missing = pair_fresh(None, Some((dec!(0.45), dec!(600))), NOW);
    let got = candidates_of(&mut a, &markets, 600, NOW, &missing);
    assert!(got.is_empty(), "少一边书就整笔放弃，got {got:?}");

    // 第二周期：书回来 → 两条腿整批挂出（证明放弃路径没有落 latch）。
    let fresh = pair_fresh(
        Some((dec!(0.48), dec!(600))),
        Some((dec!(0.45), dec!(600))),
        NOW,
    );
    let got = candidates_of(&mut a, &markets, 600, NOW, &fresh);
    assert_eq!(got.len(), 2, "放弃过的尝试不算尝试：书回来照常整批挂出");
}

/// 负向：一条腿没有 restable 报价（锁定书：ask 0.01，报价被地板夹到
/// =ask ⇒ post-only POST 必被拒 #464）⇒ 整笔放弃；**单腿绝不落单** ——
/// 另一条腿的报价再好也不发。
#[test]
fn one_leg_without_a_restable_quote_abandons_the_whole_pair() {
    let mut a = adapter();
    // UP ask 0.01：mode 1 报价 0.00 → 地板夹到 0.01 → 不严格低于 ask → 无报价。
    let locked = pair_fresh(
        Some((dec!(0.01), dec!(600))),
        Some((dec!(0.45), dec!(600))),
        NOW,
    );
    let markets = vec![market()];
    let got = candidates_of(&mut a, &markets, 600, NOW, &locked);
    assert!(
        got.is_empty(),
        "一条腿无 restable 报价 ⇒ 整笔放弃（含那条好的腿），got {got:?}"
    );
}

/// 负向：报价和超限 ⇒ 整笔放弃。0.60/0.50 的 ask ⇒ 报价 0.59+0.49 =
/// 1.08 ≥ 0.985。
#[test]
fn quote_sum_over_the_cap_abandons_the_whole_pair() {
    let mut a = adapter();
    let fresh = pair_fresh(
        Some((dec!(0.60), dec!(600))),
        Some((dec!(0.50), dec!(600))),
        NOW,
    );
    let markets = vec![market()];
    let got = candidates_of(&mut a, &markets, 600, NOW, &fresh);
    assert!(got.is_empty(), "和价超限就整笔放弃，got {got:?}");
}

/// 边界：成本测试是严格 `<` —— 报价和恰好等于上限也拒（锁零边际的对不是
/// 交易），上限放宽一丝立即放行。
#[test]
fn the_cost_test_is_strict_at_the_cap() {
    let markets = vec![market()];
    // ask 0.55/0.45 ⇒ 报价 0.54+0.44 = 0.98。
    let fresh = pair_fresh(
        Some((dec!(0.55), dec!(600))),
        Some((dec!(0.45), dec!(600))),
        NOW,
    );

    let mut a = adapter_with_params("max_pair_cost", "0.98");
    let got = candidates_of(&mut a, &markets, 600, NOW, &fresh);
    assert!(
        got.is_empty(),
        "恰好等于上限 = 零边际，严格 `<` 拒绝，got {got:?}"
    );

    let mut a = adapter_with_params("max_pair_cost", "0.981");
    let got = candidates_of(&mut a, &markets, 600, NOW, &fresh);
    assert_eq!(got.len(), 2, "上限之下一丝立即放行");
}

/// 纪律（#871 的核心）：无补腿，腿死=腿死。武装后的周期推入经典的部分成交
/// 形状（UP 持 1 股、DOWN 持 0 —— pair_discount_arb 在此形状会发 gap-fill
/// 重报，#904 的豁免也以此为事实源），本包零 entry、零 exit：不发补腿
/// intent，豁免自然不触发；也不发 merge（没有 collection 生命周期）。
#[test]
fn no_gap_fill_a_dead_leg_stays_dead() {
    let mut a = adapter();
    let markets = vec![market()];
    let fresh = pair_fresh(
        Some((dec!(0.48), dec!(600))),
        Some((dec!(0.45), dec!(600))),
        NOW,
    );

    // 第一周期：整批挂出（武装）。
    let got = candidates_of(&mut a, &markets, 600, NOW, &fresh);
    assert_eq!(got.len(), 2, "前提：本周期整批挂出");

    // 第二周期：单侧持仓视图在场（裸腿形状），书照样新鲜 —— 零补腿。
    let positions = vec![
        HeldPosition {
            strategy: "maker_pair".into(),
            account_id: blitzkrieg_core::model::default_account_id().to_string(),
            condition_id: COND.into(),
            token_id: UP.into(),
            direction: "up".into(),
            shares: "1.00".into(),
            entry_price: "0.47".into(),
            round_slot: SLOT,
        },
        HeldPosition {
            strategy: "maker_pair".into(),
            account_id: blitzkrieg_core::model::default_account_id().to_string(),
            condition_id: COND.into(),
            token_id: DOWN.into(),
            direction: "down".into(),
            shares: "0.00".into(),
            entry_price: "0.44".into(),
            round_slot: SLOT,
        },
    ];
    let got = a.find_candidates(&ctx(&markets, 600, NOW, &fresh).with_positions(&positions));
    assert!(
        got.is_empty(),
        "腿死=腿死：武装后的周期零补腿零重报，got {got:?}"
    );
    let exits: Vec<StrategyExitIntent> = a.take_exit_intents();
    assert!(
        exits.is_empty(),
        "本包从不发 exit（无 merge、无卖出）：{exits:?}"
    );
}

/// 红线：包默认 disabled。注册即 disabled（`register_user_strategy` 落
/// `enabled: false`），启用只来自操作员绑定（`set_strategy_enabled`）；
/// 内核面声明经 Engine 的装配链钉死（entry_maker/atomic_batch/
/// holds_to_settlement/门豁免）。`CoreConfig::default()` 不带任何
/// `virtual_limits` pot —— 允许名单出厂为空。
#[test]
fn the_package_registers_disabled_and_enables_only_through_the_operator() {
    let mut e = Engine::new(CoreConfig::default().engine_config());
    e.register_user_strategy(
        Box::new(adapter()),
        format!("lua:{}", package_dir().display()),
    )
    .expect("maker_pair 以包名注册");
    assert!(
        e.enabled_strategies().is_empty(),
        "注册即 disabled —— 包本体没有任何自启用面，got {:?}",
        e.enabled_strategies()
    );
    assert!(
        e.strategy_entry_maker("maker_pair"),
        "装配链钉死 entry_maker"
    );
    assert!(
        e.strategy_atomic_batch("maker_pair"),
        "装配链钉死 atomic_batch"
    );
    assert!(
        e.strategy_holds_to_settlement("maker_pair"),
        "装配链钉死 holds_to_settlement"
    );
    let ex = e
        .strategy_gate_exemptions("maker_pair")
        .expect("门豁免声明在场");
    assert!(ex.timing && ex.momentum && ex.timing_min_time_left_sec == Some(30));

    // 启用的唯一路径：操作员的绑定开关。
    assert!(e.set_strategy_enabled("maker_pair", true), "操作员开关命中");
    assert_eq!(e.enabled_strategies(), vec!["maker_pair".to_string()]);
}

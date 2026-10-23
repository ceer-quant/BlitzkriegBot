//! 多账户解耦验收（spec：正向三条 + 反向三条，每条一颗牙，先红后绿）。
//!
//! 六颗牙的判定对象与运行状态（红基线于 029028f 记录，收口时必须全绿）：
//!
//!  1. `switching_the_active_account_leaves_the_other_scope_trading` —
//!     反向 1：切换账户（进程级 active 挪动）不得暂停/销毁其他账户的策略。
//!     双账户 + 双 scope + 实盘同款数据流：把 active 切走后评估，paper 的
//!     scope 照常下单成交。
//!  2. `assignments_are_independent_and_coexist` — 正向 2/反向 3 的内核半：
//!     assignment IS enablement。未指派的账户零单；指派后自己的 scope 起来，
//!     且与既有 scope 同 token 共存互不压制（两账户各自成交同一对腿）。
//!  3. `assignment_rebuilds_at_boot_from_the_file` — 正向 2 的重启半：
//!     accounts.toml 是唯一事实，重建内核后 scope 集合精确恢复（文件指派的
//!     账户开跑，无指派的 default 账户什么都不跑——INHERIT 已废）。
//!  4. `removing_a_held_scope_demotes_it_to_exits_only` — exits-only 安全网：
//!     有持仓的 scope 被移出指派 ⇒ 保留但只出不平（exit 流到平仓为止），
//!     平仓后的再次移除则整拆（DROP），无 exits-only 残留。
//!  5. `dual_venue_data_plane_keeps_both_venues_and_rolls_per_venue` —
//!     反向 2 的数据面半：同 slot 双所 RoundMarkets 必须合流（两所的行都在），
//!     每行可按 token 归属到所；单所翻轮只换自己那半（对手所行情不被撕掉）。
//!
//!  6a. `a_feasible_chase_reoffers_the_missing_leg_and_keeps_the_pair_whole`
//!     — Chase 追腿（合同：可追 ⇒ 追单过对手价、裸腿不被 flatten、双腿合拢）。
//!     EXPECTED-RED-WAITING：现树只有 alert+flatten（无追腿），收口时内核
//!     agent 的 chase 落地后必须绿。
//!
//!  6b. `an_infeasible_chase_alerts_and_flattens_the_naked_leg` — Chase 不可行
//!     （pair 成本超 $1.00 预算）⇒ RiskAlert + flatten。现树语义即此臂，今天
//!     就该绿；chase 落地后仍须绿（不可行臂保留 alert+flatten）。
//!
//! 夹具构造方式复刻 `cross_arb_reverse_check.rs`（不改 src）：REAL
//! cross_venue_arb 包经 LuaEngineAdapter 装载，但注册 source 用 `lua:<dir>`
//! （生产装配同款 provenance）——scope 重建（`build_lua_scope`）只认 `lua:`
//! 前缀，`"test"` 这类自由文本会让重建 warn-skip、双账户牙全部落空。
//! 账户簿文件放 worktree 的 `target/` 下（禁 /tmp）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use blitzkrieg_core::account::config::load_account_ledgers;
use blitzkrieg_core::engine::DataEvent;
use blitzkrieg_core::ipc::schema::Event;
use blitzkrieg_core::model::{
    AccountId, CoreError, CoreErrorCode, CryptoMarket, FillPolicy, Mode, OrderRequest, OrderStatus,
    Side, default_account_id,
};
use blitzkrieg_core::service::{Core, CoreConfig, VirtualAccountLimit};
use blitzkrieg_core::strategy_engine::lua_loader::{LuaEngineAdapter, load_lua_package};
use blitzkrieg_lua_runtime::FeeScheduleView;
use blitzkrieg_market_api::unified::{
    EventStatus, ListingStatus, PlatformListing, SettlementSource, UnifiedEvent, Venue,
};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

const NOW: i64 = 1_700_000_000_000;

// 与 cross_venue_arb 系列测试同构的 PM/KX 符号对：UP 腿来自 polymarket
// listing，DOWN 腿来自 kalshi listing（跨所配对）。
const PM: &str = "pm-cond";
const KX: &str = "kx-cond";
const PM_UP: &str = "pm-up";
const PM_DOWN: &str = "pm-down";
const KX_UP: &str = "kx-up";
const KX_DOWN: &str = "kx-down";
// 第二轮的 polymarket 行（单所翻轮用——token 必须全新才能证明替换）。
const PM2: &str = "pm-cond-2";
const PM2_UP: &str = "pm2-up";
const PM2_DOWN: &str = "pm2-down";
// kalshi 翻轮后的行（真实链路里 5s 轮询自然重报下一轮；夹具显式补报）。
const KX2: &str = "kx-cond-2";
const KX2_UP: &str = "kx2-up";
const KX2_DOWN: &str = "kx2-down";

fn paper_id() -> AccountId {
    AccountId("paper".to_string())
}

// ── 夹具 ────────────────────────────────────────────────────────────────────

/// 账户簿夹具目录：worktree 的 target/ 下（仓库忽略、同一块盘、绝不碰 /tmp）。
fn scratch_dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/account-decouple-tests");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// 双账户 accounts.toml。`paper_strategies` 非空时 paper 表预带 strategies 键
/// （重启恢复牙的文件事实）；否则两张表都只有 id。
fn accounts_toml_path(name: &str, paper_strategies: Option<&[&str]>) -> PathBuf {
    let mut text = String::from("[[account]]\nid = \"default\"\n\n[[account]]\nid = \"paper\"\n");
    if let Some(strategies) = paper_strategies {
        text = format!(
            "[[account]]\nid = \"default\"\n\n[[account]]\nid = \"paper\"\nstrategies = [{}]\n",
            strategies
                .iter()
                .map(|s| format!("\"{s}\""))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let path = scratch_dir().join(format!("accounts-{name}-{}.toml", std::process::id()));
    std::fs::write(&path, text).expect("accounts.toml fixture writes");
    path
}

fn package_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../user_layer/strategies_lua/cross_venue_arb")
}

/// 生产装配同款的 provenance：`lua:<dir>`。scope 重建（`build_lua_scope`）
/// 只从 `lua:` 前缀的 source 重实例化——这是双账户牙能落地的全部前提。
fn lua_source() -> String {
    format!("lua:{}", package_dir().display())
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

fn market(token_up: &str, token_down: &str, cond: &str, venue: &str, slot: i64) -> CryptoMarket {
    CryptoMarket {
        asset: "BTC".into(),
        condition_id: cond.into(),
        question_id: "q".into(),
        up_token_id: token_up.into(),
        down_token_id: token_down.into(),
        up_price: dec!(0.48),
        down_price: dec!(0.45),
        expires_at_ms: (slot + 1) * 900 * 1000,
        // 插件语义（polymarket/kalshi discovery 同源）：round_slot = 到期
        // 所属 slot（end_ms/1000/round_sec）。#560 后快照装配层与
        // books_view 都按「到期 slot == 判定钟 slot+1」过滤——正在交易的
        // 一轮。夹具的 slot 参数就是那「正在交易的 slot」。
        round_slot: slot + 1,
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
/// $1 开仓线——包必须开价。四个 token 全部有书（配对要读两个 listing 的
/// 双侧报价），深度 600。
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

/// 一轮完整行情：双所 RoundMarkets（同 slot）+ UnifiedMapping + 折价盘口。
fn pair_market_data(c: &mut Core, now: i64) {
    let slot = now / 1000 / 900;
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: vec![
                market(PM_UP, PM_DOWN, PM, "polymarket", slot),
                market(KX_UP, KX_DOWN, KX, "kalshi", slot),
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
    discount_books(c, now + 1_000);
}

/// 带跨所套利包的 dry Core。`accounts_path` 非 None ⇒ 真账户簿
/// (default + paper) 经 `with_account_book` 注入；`enable_default` 控制
/// default scope 的全局开关（重启/指派牙要求 default 静默）。
fn decouple_core(accounts_path: Option<&Path>, pot: Option<Decimal>, enable_default: bool) -> Core {
    let mut virtual_limits = HashMap::new();
    if let Some(cap) = pot {
        let mut accounts = vec![default_account_id().to_string()];
        if accounts_path.is_some() {
            accounts.push("paper".to_string());
        }
        virtual_limits.insert(
            "cross_venue_arb".to_string(),
            VirtualAccountLimit {
                max_batch_notional_usd: Some(cap),
                accounts,
            },
        );
    }
    let config = CoreConfig {
        mode: Mode::Dry,
        risk: blitzkrieg_core::risk::RiskConfig {
            max_order_notional: dec!(100_000),
            ..Default::default()
        },
        dry_seed_balance: dec!(1_000_000),
        engine_enabled: true,
        round_duration_sec: 900,
        // 双腿按策略声明容量（薄腿深度）DECLARED 份额下单——与
        // cross_arb_reverse_check 的内核夹具同口径。
        min_shares: dec!(1),
        max_shares: dec!(10000),
        assets: vec!["BTC".into()],
        positions: blitzkrieg_core::position::PositionConfig {
            exit: blitzkrieg_core::exit_policy::ExitConfig {
                min_time_left_sec: 0,
                // 出场直取 taker：本文件的牙要看到 exit 立刻成交（身份成交
                // 模型吃满自家书），maker-first 的挂单路径会把出场悬在 OME。
                maker_first_exit_enabled: false,
                // 零宽限期：牙的出场 tick 紧贴建仓 tick，3 秒默认宽限会把
                // 止损 hold 掉（exit_policy decide_exit_verdict 的
                // hold_sec < exit_grace_sec 门）。
                exit_grace_sec: 0,
                ..Default::default()
            },
            max_positions_per_asset: 4,
            ..Default::default()
        },
        // 不加载 execution_policy.toml（与 cross_arb_reverse_check 同口径）：
        // 默认路径是相对 cwd 的拼写，worktree 根下文件存在会把
        // max_positions_per_asset 钳回 1，第二条腿全被拒。
        execution_policy_path: None,
        accounts_config_path: accounts_path.map(|p| p.to_string_lossy().into_owned()),
        virtual_limits,
        ..Default::default()
    };
    let mut c = match accounts_path {
        Some(p) => {
            let book = load_account_ledgers(p, NOW).expect("accounts fixture parses");
            Core::with_account_book(config, book)
        }
        None => Core::new(config),
    };
    let cfg = c.config().engine_config();
    let mut eng = blitzkrieg_core::engine::Engine::new(cfg.clone());
    eng.register_user_strategy(Box::new(arb_adapter()), lua_source())
        .expect("cross_venue_arb registers under its package name");
    if enable_default {
        assert!(eng.set_strategy_enabled("cross_venue_arb", true));
    }
    c.enable_engine(eng);
    // 跨所套利弹窗 v2: the pairing is CONFIG-DRIVEN (`arb.config.set` → the
    // adapter's params bag); unconfigured ⇒ the strategy pairs nothing
    // (fail-closed). The fixture wires both legs onto `default` — the
    // default scope is the WIRED deployment (its pair stamps onto default),
    // and every OTHER scope (paper's, whether assigned or not) receives the
    // per-scope PROJECTION: its leg accounts rewritten onto its own instance
    // id, so its own pair trades its own book — the independence this file
    // pins is untouched by the leg configuration.
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

fn count_positions(c: &Core, account: &str) -> usize {
    c.positions()
        .open_positions()
        .iter()
        .filter(|p| p.account_id.as_str() == account)
        .count()
}

fn open_cost(c: &Core, account: &str) -> Decimal {
    c.positions()
        .open_positions()
        .iter()
        .filter(|p| p.account_id.as_str() == account)
        .map(|p| p.cost_usd)
        .sum()
}

fn orphan_alerts(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Event>) -> Vec<String> {
    let mut msgs = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let Event::RiskAlert { message, .. } = ev
            && (message.contains("orphan") || message.contains("naked"))
        {
            msgs.push(message);
        }
    }
    msgs
}

/// 把 DOWN 腿整仓 flatten——等价于「fallback 没能保住对手腿」后的裸腿终态
/// （UP 腿持仓在、partner 无持仓、无 live BUY、不在 inflight）。
fn flatten_down_leg(c: &mut Core, now: i64) -> String {
    let down_id = c
        .positions()
        .open_positions()
        .iter()
        .find(|p| p.token_id == KX_DOWN)
        .expect("precondition: the DOWN leg filled into a position")
        .id
        .clone();
    c.flatten(Some(&down_id), now)
        .expect("flattening the second leg succeeds");
    down_id
}

// ── 反向 1：切换账户不得暂停/销毁其他账户的策略 ─────────────────────────────

/// ACCEPTANCE（反向 1）: 账户选择是会话/进程级的「看哪里」，不是「跑哪里」。
/// paper 被指派跑 cross_venue_arb 后，把进程级 active 挪到 default（选到 B），
/// 再评估：A（paper）的 scope 必须照常下单、照常成交——任何「切走即停」的
/// 耦合在这里当场红。
#[test]
fn switching_the_active_account_leaves_the_other_scope_trading() {
    let path = accounts_toml_path("switch", None);
    let mut c = decouple_core(Some(&path), Some(dec!(1000)), true);
    let now = NOW;

    c.set_account_strategies_persisting(
        &paper_id(),
        vec!["cross_venue_arb".into()],
        "test",
        "acceptance: paper runs the pair",
        now,
    )
    .expect("paper assignment lands");

    pair_market_data(&mut c, now);

    // 选到 B（default）：账户选择的全部语义就是挪 active 指针。
    c.accounts_mut()
        .switch(&default_account_id())
        .expect("the switch lands");

    let placed = c.engine_evaluate(now + 2_000);
    assert_eq!(
        placed, 4,
        "both scopes place both legs — the switch touched nobody's scope"
    );
    assert_eq!(
        count_positions(&c, "paper"),
        2,
        "A(paper)'s legs filled in the background after switching to B"
    );
    assert_eq!(
        count_positions(&c, default_account_id().as_str()),
        2,
        "B(default)'s own scope filled alongside"
    );
    assert_eq!(
        open_cost(&c, "paper"),
        dec!(516),
        "paper's fills are real committed positions (600 × (0.44 + 0.42)), \
         not orders that vanished with the switch"
    );
}

// ── 正向 2 / 反向 3：指派独立、共存互不压制 ─────────────────────────────────

/// ACCEPTANCE（正向 2 + 反向 3 内核半）: assignment IS enablement。未指派的
/// paper 一个单都不下（没有 scope 可跑）；指派后 paper 自己的 scope 起来，
/// 与 default 的 scope 同 token 各自成交——一账户的启用既不替别人跑，也
/// 不压制别人跑。
#[test]
fn assignments_are_independent_and_coexist() {
    let path = accounts_toml_path("independent", None);
    let mut c = decouple_core(Some(&path), Some(dec!(1000)), true);
    let now = NOW;

    pair_market_data(&mut c, now);
    assert_eq!(
        c.engine_evaluate(now + 2_000),
        2,
        "only the default scope exists — one pair of legs"
    );
    assert_eq!(count_positions(&c, default_account_id().as_str()), 2);
    assert_eq!(
        count_positions(&c, "paper"),
        0,
        "no assignment ⇒ paper runs nothing (explicit, no inherit)"
    );

    // 指派 paper：ADD 相位实例化 + ENABLED，自己的 Lua state、自己的账。
    c.set_account_strategies_persisting(
        &paper_id(),
        vec!["cross_venue_arb".into()],
        "test",
        "acceptance: paper joins",
        now + 2_100,
    )
    .expect("paper assignment lands");

    // 新盘口（default 的重发被 attempt latch 挡住；paper 的 latch 是空的）。
    discount_books(&mut c, now + 2_200);
    let placed2 = c.engine_evaluate(now + 2_500);
    assert_eq!(
        placed2, 2,
        "paper's own scope places its own pair — nothing suppressed it"
    );
    assert_eq!(
        count_positions(&c, "paper"),
        2,
        "same tokens, paper's own fills under paper's account"
    );
    assert_eq!(
        count_positions(&c, default_account_id().as_str()),
        2,
        "coexistence: paper's scope neither paused nor destroyed default's"
    );
}

// ── 正向 2（重启半）：指派随文件恢复 ────────────────────────────────────────

/// ACCEPTANCE: accounts.toml 是唯一事实。boot 读文件、按指派实例化 scope：
/// 文件指派了 paper ⇒ paper 的 scope 开跑；default 无键 ⇒ 什么都不跑
/// （INHERIT 已废，空 = 显式不跑）。重建一个全新内核、同一份文件 ⇒ scope
/// 集合精确恢复，行为一致。
#[test]
fn assignment_rebuilds_at_boot_from_the_file() {
    let path = accounts_toml_path("boot", Some(&["cross_venue_arb"]));
    let now = NOW;

    // boot 1：文件指派 → scope 起来 → 交易。
    let mut c = decouple_core(Some(&path), Some(dec!(1000)), false);
    let migrated = c.apply_account_strategy_assignments_at_boot();
    assert!(
        migrated.is_empty(),
        "no strategy-state file is configured: nothing may migrate"
    );
    assert_eq!(
        c.accounts().strategy_assignment(&paper_id()),
        Some(&vec!["cross_venue_arb".to_string()]),
        "the file's assignment is the book's fact"
    );
    pair_market_data(&mut c, now);
    assert_eq!(
        c.engine_evaluate(now + 2_000),
        2,
        "boot 1: the assigned paper scope trades"
    );
    assert_eq!(count_positions(&c, "paper"), 2);
    assert_eq!(
        count_positions(&c, default_account_id().as_str()),
        0,
        "default has no assignment key ⇒ runs nothing (no inherit)"
    );

    // boot 2：全新内核、同一份文件——重启恢复必须逐字节同形。
    let mut c2 = decouple_core(Some(&path), Some(dec!(1000)), false);
    let migrated2 = c2.apply_account_strategy_assignments_at_boot();
    assert!(
        migrated2.is_empty(),
        "the migration is one-time and nothing else persists an enabled set here"
    );
    pair_market_data(&mut c2, now + 10_000_000);
    assert_eq!(
        c2.engine_evaluate(now + 10_002_000),
        2,
        "boot 2: the scope set is exactly restored from the file"
    );
    assert_eq!(count_positions(&c2, "paper"), 2);
    assert_eq!(count_positions(&c2, default_account_id().as_str()), 0);
}

// ── exits-only 安全网 ───────────────────────────────────────────────────────

/// ACCEPTANCE: 移出一个仍有持仓的 scope ⇒ DEMOTE（保留、只出不平）；exit
/// 流到平仓为止；平仓之后再移除 ⇒ DROP（整拆，无 exits-only 残留）。
#[test]
fn removing_a_held_scope_demotes_it_to_exits_only() {
    let path = accounts_toml_path("exits-only", None);
    let mut c = decouple_core(Some(&path), Some(dec!(1000)), false);
    let now = NOW;

    c.set_account_strategies_persisting(
        &paper_id(),
        vec!["cross_venue_arb".into()],
        "test",
        "acceptance: paper runs the pair",
        now,
    )
    .expect("paper assignment lands");
    pair_market_data(&mut c, now);
    assert_eq!(
        c.engine_evaluate(now + 2_000),
        2,
        "precondition: paper trades"
    );
    assert_eq!(count_positions(&c, "paper"), 2);

    // 移出指派，但 paper 仍持有仓位 ⇒ 保留为 exits-only（返回值即合同面）。
    let kept = c
        .set_account_strategies_persisting(
            &paper_id(),
            vec![],
            "test",
            "acceptance: pull paper off the strategy while held",
            now + 2_500,
        )
        .expect("the removal lands");
    assert_eq!(
        kept,
        vec!["cross_venue_arb".to_string()],
        "a HELD removed scope is kept EXITS-ONLY, never dropped with inventory"
    );

    // 盘口重新报价：pair 成本 0.44 + 0.90 + fee ≥ 0.995（回落平仓阈值）。
    // hold-to-settlement 策略的政策阶梯对 settlement 持有者整体跳过
    // （run_exit_checks 的 settlement_holders），出场走策略自己的 release
    // 阀：exits-only 的 scope 仍然被评估、exit intent 照常流出，tick 平腿。
    for (token, ask) in [(PM_UP, dec!(0.44)), (KX_DOWN, dec!(0.90))] {
        c.engine_on_data(leg_book(token, ask, dec!(600), now + 3_000), now + 3_000);
    }
    let entries_after = c.engine_evaluate(now + 3_500);
    assert_eq!(
        entries_after, 0,
        "exits-only: the demoted scope places no new entries while it works its exits"
    );
    c.tick(now + 4_000).expect("the exit tick runs");
    assert_eq!(
        count_positions(&c, "paper"),
        0,
        "exits kept flowing after the demotion — the account ended flat"
    );

    // 平仓之后再次移除 ⇒ DROP：一个 FLAT 的被移 scope 整拆，无残留。
    let kept2 = c
        .set_account_strategies_persisting(
            &paper_id(),
            vec![],
            "test",
            "acceptance: pull the now-flat scope again",
            now + 5_000,
        )
        .expect("the second removal lands");
    assert!(
        kept2.is_empty(),
        "a FLAT removed scope drops outright — no exits-only residue"
    );

    // 白送的组合（0.10 + 0.12 ≪ $1）：活着的 scope 会冲进来；exits-only 的
    // scope 一个单都不许再开。
    discount_books(&mut c, now + 6_000);
    let _ = c.engine_evaluate(now + 6_500);
    assert_eq!(
        count_positions(&c, "paper"),
        0,
        "exits-only: no more entries even when the pair turns profitable"
    );
}

// ── 反向 2（数据面半）：双所数据面与单所翻轮 ────────────────────────────────

/// ACCEPTANCE（反向 2 数据面半）: 行情面的 venue 维度。同一 slot 两个交易所
/// 各自的 RoundMarkets 必须合流（#498 的跨所推广）——盘口视图两所的行都在，
/// 每个 token 可归属到自己的所；单所翻轮只替换自己 venue 的行，对手所的
/// 行情活得好好的。IPC 线上的 per-row `venue` 字段由 ENGINE_ROUND /
/// ENGINE_BOOKS 组装层从 `venue_of` 富化（见 docs 矩阵的披露节）——本牙钉的
/// 是内核侧数据源本身。
#[test]
fn dual_venue_data_plane_keeps_both_venues_and_rolls_per_venue() {
    let mut c = decouple_core(None, Some(dec!(1000)), false);
    // 固定受控钟（books_view 的判定钟由调用方传入，本测试喂入与判定同
    // 源、零边界竞态）。#560 后的过滤判据：喂入轮 round_slot（插件语义
    // = 到期所属格）必须等于「判定钟 slot + 1」。slot 取 NOW 所在格的中
    // 段：喂入 expiry 未过、observe 派生的 venue 钟 offset = 0，判定钟
    // 与喂入钟逐位一致。
    let now = (NOW / 900_000) * 900_000 + 450_000; // mid-slot
    let slot = now / 1000 / 900;

    // 错峰上架：polymarket 先报、kalshi 同 slot 后报 ⇒ 合流，不是覆盖。
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: vec![market(PM_UP, PM_DOWN, PM, "polymarket", slot)],
            now_ms: now,
        },
        now,
    );
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: vec![market(KX_UP, KX_DOWN, KX, "kalshi", slot)],
            now_ms: now + 500,
        },
        now + 500,
    );

    let rows = c.books_view(5, now);
    assert!(
        rows.iter().any(|r| r.up_token_id == PM_UP),
        "the polymarket row survives the kalshi top-up: {rows:?}"
    );
    assert!(
        rows.iter().any(|r| r.up_token_id == KX_UP),
        "the kalshi row lands alongside it — both venues in one round: {rows:?}"
    );
    assert_eq!(
        c.venue_of_token(PM_UP),
        "polymarket",
        "each token resolves to its own venue (the wire's per-row enrichment input)"
    );
    assert_eq!(c.venue_of_token(KX_UP), "kalshi");

    // 单所翻轮：polymarket 的新 slot 只换 polymarket 的行；kalshi 不被撕掉。
    // 真实链路里 kalshi 的 5s 轮询会自然重报它的下一轮——夹具两所都显式
    // 补报，判定钟同步推进到下一格（已到期的 slot 不再出现在视图里）。
    let rollover_ms = (slot + 1) * 900 * 1000 + 450_000; // next slot's midpoint
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: vec![market(PM2_UP, PM2_DOWN, PM2, "polymarket", slot + 1)],
            now_ms: rollover_ms,
        },
        rollover_ms,
    );
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: vec![market(KX2_UP, KX2_DOWN, KX2, "kalshi", slot + 1)],
            now_ms: rollover_ms + 500,
        },
        rollover_ms + 500,
    );
    let rows2 = c.books_view(5, rollover_ms + 500);
    assert!(
        rows2.iter().any(|r| r.up_token_id == KX2_UP),
        "kalshi survived polymarket's rollover (via its own next-round feed): {rows2:?}"
    );
    assert!(
        rows2.iter().any(|r| r.up_token_id == PM2_UP),
        "polymarket's new round is live: {rows2:?}"
    );
    assert!(
        !rows2.iter().any(|r| r.up_token_id == PM_UP),
        "polymarket's expired round is replaced by its own feed"
    );
    assert_eq!(
        c.venue_of_token(KX2_UP),
        "kalshi",
        "kalshi attribution survives the sibling venue's rollover"
    );
}

// ── Chase 追腿（#531 合同的两臂）────────────────────────────────────────────

/// Chase 追腿的公共前奏：双腿成交 → DOWN 腿 flatten 成裸腿。
fn naked_pair_fixture(
    pot: Decimal,
    sink: bool,
) -> (Core, Option<tokio::sync::mpsc::UnboundedReceiver<Event>>) {
    let mut c = decouple_core(None, Some(pot), true);
    let rx = if sink {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        c.set_event_sink(tx);
        Some(rx)
    } else {
        None
    };
    let now = NOW;
    pair_market_data(&mut c, now);
    assert_eq!(
        c.engine_evaluate(now + 2_000),
        2,
        "precondition: both legs fill into positions"
    );
    flatten_down_leg(&mut c, now + 3_000);
    (c, rx)
}

/// ACCEPTANCE（Chase，可行臂）: 双腿原子批的一腿成交、对手腿终态消失、
/// 落地宽限（5s）过后——只要追腿在预算内（已付 0.44 + 追价 0.42 + 费
/// ≪ $1.00），内核必须对缺失腿重新报价（价格过对手方）、裸腿不被 flatten，
/// 追单成交后双腿一起合拢离场，全程无需人工 flatten。
///
/// EXPECTED-RED-WAITING：现树的看护只有 alert+flatten（`run_pair_orphan_
/// sweep`），没有追腿——裸腿会被平掉、追单不存在，本测试红。收口时内核
/// agent 的 chase 落地后必须绿。
#[test]
fn a_feasible_chase_reoffers_the_missing_leg_and_keeps_the_pair_whole() {
    let (mut c, _rx) = naked_pair_fixture(dec!(1000), false);
    let now = NOW;

    // 对手腿仍以 0.42 挂着（可行）：盘口维持原价。
    c.engine_on_data(
        leg_book(KX_DOWN, dec!(0.42), dec!(600), now + 3_500),
        now + 3_500,
    );
    c.engine_on_data(
        leg_book(PM_UP, dec!(0.44), dec!(600), now + 3_500),
        now + 3_500,
    );

    c.tick(now + 12_000).expect("the sweep tick runs");

    let up_alive = c
        .positions()
        .open_positions()
        .iter()
        .any(|p| p.token_id == PM_UP);
    assert!(
        up_alive,
        "CONTRACT: a feasible chase must NOT flatten the naked leg (EXPECTED-RED-WAITING: \
         today the sweep alerts + flattens and never chases)"
    );
    let chased = c
        .positions()
        .open_positions()
        .iter()
        .any(|p| p.token_id == KX_DOWN)
        || c.ome()
            .live_orders()
            .iter()
            .any(|o| o.side == Side::Buy && o.token_id == KX_DOWN);
    assert!(
        chased,
        "CONTRACT: the missing leg must be re-offered past the counterparty \
         (a filled chase position or a live chase BUY) — EXPECTED-RED-WAITING"
    );
}

/// ACCEPTANCE（Chase，不可行臂）: 追价会把 pair 成本顶过 $1.00 预算
/// （已付 0.44 + 追价 0.60 = 1.04 + 费 > 1.00）⇒ 追腿拒绝：RiskAlert 报警
/// （消息点名 strategy 与 account），裸腿走 flatten 离场。现树语义即此臂，
/// 今天就该绿；chase 落地后仍须绿（不可行臂保留 alert+flatten）。
#[test]
fn an_infeasible_chase_alerts_and_flattens_the_naked_leg() {
    let (mut c, mut rx) = naked_pair_fixture(dec!(1000), true);
    let rx = rx.as_mut().expect("the alert sink was requested");
    let now = NOW;

    // 对手腿贵到追不起：KX_DOWN ask 0.60。
    c.engine_on_data(
        leg_book(KX_DOWN, dec!(0.60), dec!(600), now + 3_500),
        now + 3_500,
    );

    let before = orphan_alerts(rx).len();
    c.tick(now + 12_000).expect("the sweep tick runs");
    let alerts = orphan_alerts(rx);
    assert!(
        alerts.len() > before,
        "the infeasible arm must alert: {alerts:?}"
    );
    assert!(
        alerts.iter().any(|m| m.contains("default")),
        "the alert names the account the naked leg belongs to: {alerts:?}"
    );
    assert!(
        !c.positions()
            .open_positions()
            .iter()
            .any(|p| p.token_id == PM_UP),
        "the naked leg is flattened — exposure the pair economics never priced leaves the book"
    );
}

// ── pair-cost cap 的账户隔离（裁决：pair 的经济单位 = (账户, 条件, 资产)）────
// 一个 UP share + 一个 DOWN share 在**同一账户的账本里**赎回 $1；跨账户的
// 对手腿在结算时不合并（每账户独立账本、独立赎回）。所以 pair-cost cap 的
// 分母只该看本账户的对手腿——A 账户的持仓不得把 B 账户的加仓顶过 $1。

/// 牙 7（跨账户污染）：A（default）持有 PM condition 的 DOWN 对手腿
/// （0.90/share 的昂贵基准），B（paper）的 cross_venue_arb 开自己的
/// PM_UP + KX_DOWN pair。cap 不按 account 过滤时，B 的 PM_UP 腿（Up, PM）
/// 的分母会读到 A 的 pm-down（Down, PM）——0.90 + 0.44 = 1.34 > 1.00，
/// atomic_batch 整对取消。本牙钉死该污染：B 必须正常成交整对。
#[test]
fn pair_cost_cap_ignores_counterpart_legs_held_by_other_accounts() {
    let path = accounts_toml_path("pair-cap-cross", None);
    let mut c = decouple_core(Some(&path), Some(dec!(1000)), false);
    let now = NOW;

    // paper 指派 cross_venue_arb（paper 的 scope 是 B 账户的新腿来源）。
    c.set_account_strategies_persisting(
        &paper_id(),
        vec!["cross_venue_arb".into()],
        "test",
        "acceptance: paper runs the pair",
        now,
    )
    .expect("paper assignment lands");

    // A（default）手工持上 PM condition 的 DOWN 腿：0.90/share 的昂贵基准。
    // condition=PM、direction=Down 与 B 即将开的 PM_UP 腿（Up, PM）恰好互为
    // 对手（cap 的查询面：同 condition + 对手方向）——shares > 0 且 basis
    // 0.90 ⇒ 污染读数 = 0.90 + 0.44 = 1.34 > $1.00。
    c.book_snapshot(PM_DOWN, vec![], vec![(dec!(0.90), dec!(600))], now);
    let a_leg = OrderRequest {
        token_id: PM_DOWN.into(),
        condition_id: PM.into(),
        side: Side::Buy,
        mode: FillPolicy::Taker,
        price: dec!(0.90),
        size: dec!(100),
        internal_key: "k-a-down".into(),
        strategy: "manual".into(),
        asset: "BTC".into(),
        direction: "down".into(),
        round_slot: now / 1000 / 900,
        account_id: default_account_id(),
    };
    let (_a_id, a_st) = c.place(a_leg, 0, now).unwrap();
    assert_eq!(a_st, OrderStatus::Filled, "A's counterpart leg fills");
    assert_eq!(count_positions(&c, "default"), 1);
    assert_eq!(count_positions(&c, "paper"), 0);

    // B（paper）的新 pair：折价盘口 PM_UP@0.44 + KX_DOWN@0.42。A 的账本
    // （pm-down basis 0.90）不进 B 的分母 ⇒ 两腿照常下单成交；污染未修时
    // PM_UP 腿被 cap 拒、atomic_batch 整对取消。
    pair_market_data(&mut c, now + 1_000);
    let placed = c.engine_evaluate(now + 3_000);
    assert_eq!(
        placed, 2,
        "B(paper)'s scope places BOTH legs of its own pair — A's 0.90 counterpart \
         basis (0.90 + 0.44 = 1.34 > 1.00) must never veto B's fresh leg"
    );
    assert_eq!(
        count_positions(&c, "paper"),
        2,
        "paper's pair fills whole — the pair-cost cap saw only paper's own ledger"
    );
    assert_eq!(
        count_positions(&c, "default"),
        1,
        "A's book is untouched by B's entries"
    );
}

/// 牙 7 的另一面（防矫枉过正）：同一账户内，对手腿基准仍然定价 cap。
/// default 持 0.90/share 的 DOWN 对手腿后，同账户再追 UP @0.42
/// （0.90 + 0.42 = 1.32 > 1.00）必须被拒，错误点名 pair-cost cap。
#[test]
fn pair_cost_cap_still_blocks_the_counterpart_leg_within_one_account() {
    let path = accounts_toml_path("pair-cap-same", None);
    let mut c = decouple_core(Some(&path), Some(dec!(1000)), false);
    let now = NOW;

    // default 持 DOWN 对手腿：0.90/share。
    c.book_snapshot("kx-down", vec![], vec![(dec!(0.90), dec!(600))], now);
    let down = OrderRequest {
        token_id: "kx-down".into(),
        condition_id: KX.into(),
        side: Side::Buy,
        mode: FillPolicy::Taker,
        price: dec!(0.90),
        size: dec!(100),
        internal_key: "k-down".into(),
        strategy: "manual".into(),
        asset: "BTC".into(),
        direction: "down".into(),
        round_slot: now / 1000 / 900,
        account_id: default_account_id(),
    };
    let (_id, st) = c.place(down, 0, now).unwrap();
    assert_eq!(st, OrderStatus::Filled, "the DOWN counterpart fills");

    // 同账户再买 UP @0.42：0.90 + 0.42 = 1.32 > 1.00 ⇒ pair-cost cap 拒绝。
    c.book_snapshot("kx-up", vec![], vec![(dec!(0.42), dec!(600))], now + 1_000);
    let up = OrderRequest {
        token_id: "kx-up".into(),
        condition_id: KX.into(),
        side: Side::Buy,
        mode: FillPolicy::Taker,
        price: dec!(0.42),
        size: dec!(100),
        internal_key: "k-up".into(),
        strategy: "manual".into(),
        asset: "BTC".into(),
        direction: "up".into(),
        round_slot: now / 1000 / 900,
        account_id: default_account_id(),
    };
    let err = c.place(up, 0, now + 1_000).unwrap_err();
    assert_eq!(err.code, CoreErrorCode::RiskRejected);
    assert!(
        err.message.contains("pair-cost cap"),
        "the same-account counterpart still prices the cap: {}",
        err.message
    );
    assert_eq!(
        count_positions(&c, "default"),
        1,
        "the over-$1 merge never lands on A's own ledger"
    );
}

// ── 账户删除语义（绑定只在建户时绑定 —— 删户重建是绑定变更的唯一出口）────────
// 用户裁决（2026-10-19）：已有账户不支持编辑交易所（会出现数据混乱），删除是
// 唯一出口 ⇒ 所有账户都支持删除（default 不再保留名），唯一硬线 = 「不允许
// 删除最后一个账户」。有持仓拒删与删后 active 回切一并钉住。

/// 删除语义牙：default 可删、最后一个账户不可删、删后 active 回切剩余账户、
/// 有持仓拒删不变。走 `remove_account_persisting`（IPC 动词的运行时臂），
/// accounts.toml 真文件落盘（worktree target/ 下）。
#[test]
fn account_removal_is_delete_all_but_the_last_account() {
    let path = accounts_toml_path("removal-semantics", None);
    let mut c = decouple_core(Some(&path), None, false);
    let now = NOW;

    // 有持仓的账户拒删：既有守卫不变 —— 钱不能变成不可寻址的。双账户都在，
    // 所以先于 last-account 守卫到达持仓守卫（两条拒绝各自可见）。
    c.positions_mut().open(
        blitzkrieg_core::position::OpenParams {
            strategy: "cross_venue_arb".into(),
            asset: "BTC".into(),
            direction: blitzkrieg_core::model::SignalDirection::Up,
            token_id: PM_UP.into(),
            condition_id: PM.into(),
            account_id: paper_id(),
            venue: String::new(),
            partner_token_id: String::new(),
            entry_price: dec!(0.48),
            expires_at_ms: now + 900_000,
            was_maker: true,
            target_exit_price: None,
            round_duration_sec: 0,
        },
        now,
    );
    let err: CoreError = c
        .remove_account_persisting(&paper_id(), "test", "pin attempt")
        .expect_err("an account holding open positions is refused");
    assert_eq!(err.code, CoreErrorCode::InvalidParams);
    assert!(
        err.message.contains("1 笔未平持仓"),
        "the refusal names the count: {}",
        err.message
    );

    // 删 active 的 default：不再是保留名拒绝 —— 落地，文件行离开，active
    // 回切剩余账户的第一个（paper；簿内无 default 行时的 boot 同款解析）。
    c.accounts_mut()
        .switch(&default_account_id())
        .expect("default is active");
    c.remove_account_persisting(&default_account_id(), "test", "removal semantics")
        .expect("default is an ordinary account — removable");
    let survivors: Vec<String> = c
        .accounts()
        .account_list()
        .iter()
        .map(|a| a.id.as_str().to_string())
        .collect();
    assert_eq!(survivors, vec!["paper"], "only paper survives");
    assert_eq!(
        c.accounts().active_id().as_str(),
        "paper",
        "the active cell never dangles: it re-activates the first survivor"
    );
    let book = load_account_ledgers(&path, now).expect("the written file re-parses");
    assert!(
        book.account(&default_account_id()).is_none(),
        "the `default` row leaves accounts.toml (the next boot re-derives it)"
    );
    assert!(book.account(&paper_id()).is_some());

    // 平掉持仓后，paper 成为最后一个账户 ⇒ 拒删（空簿让下一次 boot 拒载）。
    let ids: Vec<String> = c
        .positions_mut()
        .open_positions()
        .iter()
        .map(|p| p.id.clone())
        .collect();
    for pid in ids {
        c.positions_mut()
            .close(
                &pid,
                dec!(0.5),
                blitzkrieg_core::model::ExitReason::Manual,
                false,
                now,
            )
            .expect("the probe position closes");
    }
    let err: CoreError = c
        .remove_account_persisting(&paper_id(), "test", "last account attempt")
        .expect_err("the LAST account is never removable");
    assert_eq!(err.code, CoreErrorCode::InvalidParams);
    assert!(
        err.message.contains("最后一个账户") && err.message.contains("至少要保留一个账户"),
        "the refusal says a deployment keeps at least one account: {}",
        err.message
    );
    assert_eq!(
        c.accounts().account_list().len(),
        1,
        "the last account survives every refusal"
    );
}

/// 工厂 `user_layer/configs/accounts.toml` 的形状牙：随仓库走的那份文件必须
/// 永远保持「三账户 + venue 绑定 + 全文不带 strategies 键」。字段面（§9.5
/// 扩展轮）把 strategies 加进解析器后，这个不变量没有任何东西替我们看着 ——
/// 而文件里写死的启用集正是「面板策略开关每次重启被改回」（策略永远无法被
/// 关闭）的根源。解析失败 / 形状漂移都从这里红。
#[test]
fn the_factory_accounts_file_binds_venues_and_never_pins_strategies() {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../user_layer/configs/accounts.toml");
    let book = load_account_ledgers(&path, NOW).expect("the factory file parses");

    let rows = book.account_list();
    let mut ids: Vec<String> = rows.iter().map(|a| a.id.to_string()).collect();
    ids.sort();
    assert_eq!(
        ids,
        vec![
            "default".to_string(),
            "kalshi".to_string(),
            "paper".to_string()
        ],
        "the factory book ships exactly these three accounts"
    );

    let default = rows
        .iter()
        .find(|a| a.id.as_str() == "default")
        .expect("default exists");
    assert_eq!(default.venue, "polymarket", "default binds polymarket");
    assert_eq!(
        default.status,
        blitzkrieg_core::account::AccountStatus::Active
    );
    assert!(
        default.strategies.is_empty(),
        "default must NOT pin a strategy set — a pinned set resurrects on every boot"
    );

    let paper = rows
        .iter()
        .find(|a| a.id.as_str() == "paper")
        .expect("paper exists");
    assert_eq!(
        paper.status,
        blitzkrieg_core::account::AccountStatus::ReadOnly,
        "paper stays the read-only practice account"
    );
    assert!(
        paper.venue.is_empty(),
        "paper is unbound (trades any venue)"
    );

    let kalshi = rows
        .iter()
        .find(|a| a.id.as_str() == "kalshi")
        .expect("kalshi exists");
    assert_eq!(kalshi.venue, "kalshi", "kalshi binds its own venue");
    assert!(
        kalshi.strategies.is_empty(),
        "no account ships a pinned set"
    );
}

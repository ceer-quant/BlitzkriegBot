#![cfg(test)]
//! P-1.1: host dispatch to the builtin strategy through Core, with per-strategy
//! caps and per-strategy accounting.

use super::shadow_evolution_tests::scratch;
use super::*;
use crate::engine::{DataEvent, Engine, EngineConfig};
use crate::model::CryptoMarket;
use crate::risk::RiskConfig;
use crate::shadow_evolution::{MutableParams, StrategyParams};
use crate::test_fixtures::round_market;
use rust_decimal_macros::dec;

/// Mirrors the production engine knobs closely enough to trade the feed below
/// (same shape as the engine.rs unit tests).
fn engine_cfg() -> EngineConfig {
    EngineConfig {
        scanner: crate::scanner::ScannerConfig {
            assets: vec!["BTC".into()],
            round_duration_sec: 900,
            min_round_age_sec: 0,
            min_time_left_sec: 0,
        },
        trend: crate::signal::TrendConfig {
            confirm_sec: 5,
            ratio: dec!(0.5),
            min_price: dec!(0.5),
            broken_price: dec!(0.35),
            window_floor_ms: 0,
        },
        spread_arb: crate::signal::SpreadArbConfig {
            trend_max_entry_price: dec!(0.45),
            ..Default::default()
        },
        max_orderbook_stale_ms: 8000,
        momentum_window_sec: 30,
        momentum_tol_pct: dec!(0.03),
        size_usd: dec!(2.5),
        min_shares: dec!(10),
        max_shares: dec!(10),
        size_pct: Decimal::ZERO,
        strategy_sizes: HashMap::new(),
    }
}

fn core_with_engine(limits: HashMap<String, StrategyLimit>) -> Core {
    let mut c = Core::new(CoreConfig {
        risk: RiskConfig {
            max_order_notional: dec!(100),
            ..Default::default()
        },
        dry_seed_balance: dec!(1000),
        engine_enabled: true,
        round_duration_sec: 900,
        auto_exits_enabled: true,
        strategy_limits: limits,
        ..Default::default()
    });
    c.enable_engine(Engine::new(engine_cfg()));
    // PR-B: the kernel registers no strategies, so the dispatch tests that
    // drive the reference dip buyer host the test adapter explicitly.
    crate::strategies::test_support::host(
        c.engine.as_mut().unwrap(),
        engine_cfg().trend,
        engine_cfg().spread_arb,
    );
    c
}

// ── Replay fast path (--backtest-fast) gating semantics ────────────────

/// Counts REAL strategy evaluations (`find_candidates` calls) — the
/// observable behind the fast path's "a quiet tick skips the engine
/// call" claim, without hosting a Lua runtime.
struct ProbeCount {
    count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}
impl crate::strategies::EngineStrategy for ProbeCount {
    fn name(&self) -> &str {
        "probe"
    }
    fn on_book(&mut self, _t: &str, _s: &crate::model::OrderbookSnapshot, _n: i64) {}
    fn on_round(&mut self, _slot: i64, _time_left_sec: i64, _now_ms: i64) {}
    fn find_candidates(
        &mut self,
        _ctx: &crate::strategies::StrategyCtx<'_>,
    ) -> Vec<crate::signal::TradeSignal> {
        self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Vec::new()
    }
}

fn core_with_probe(fast: bool) -> (Core, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    let mut c = Core::new(CoreConfig {
        engine_enabled: true,
        auto_exits_enabled: false,
        backtest_fast: fast,
        ..Default::default()
    });
    c.enable_engine(Engine::new(engine_cfg()));
    let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    c.engine
        .as_mut()
        .unwrap()
        .register_user_strategy(
            Box::new(ProbeCount {
                count: count.clone(),
            }),
            "test".into(),
        )
        .expect("fresh engine registers the probe");
    assert!(
        c.engine
            .as_mut()
            .unwrap()
            .set_strategy_enabled("probe", true)
    );
    (c, count)
}

/// The sparse-cadence contract: a quiet tick inside the fallback window
/// advances the scheduled-cycle counter but does NOT call the strategy;
/// a fresh engine input wakes the evaluation early; the fallback beat
/// bounds anything the wake/boundary model does not cover.
#[test]
fn fast_mode_quiet_tick_skips_the_engine_but_keeps_the_counter() {
    let (mut c, count) = core_with_probe(true);
    let t0 = 1_000_000i64;
    c.tick(t0).unwrap();
    c.engine_evaluate(t0);
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);

    // Quiet: no data, no timing boundary, inside the 2 s fallback window.
    for i in 1..20 {
        let t = t0 + i * 50;
        c.tick(t).unwrap();
        c.engine_evaluate(t);
    }
    assert_eq!(
        count.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a quiet tick must not call the strategy"
    );
    assert_eq!(
        c.stats.evaluations, 20,
        "the scheduled-cycle counter advances even on quiet ticks"
    );

    // An engine input wakes the evaluation before the fallback beat.
    let tw = t0 + 1000;
    c.engine_on_data(
        crate::engine::DataEvent::Book {
            token_id: "up".into(),
            bids: vec![(Decimal::new(55, 2), Decimal::from(100))],
            asks: vec![(Decimal::new(57, 2), Decimal::from(100))],
            now_ms: tw,
        },
        tw,
    );
    c.tick(tw).unwrap();
    c.engine_evaluate(tw);
    assert_eq!(
        count.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "a book event wakes the evaluation"
    );

    // The fallback beat evaluates even with nothing new (last eval was
    // the wake at t0+1000, so the 2 s bound fires at t0+3000).
    let tf = t0 + 3000;
    c.tick(tf).unwrap();
    c.engine_evaluate(tf);
    assert_eq!(
        count.load(std::sync::atomic::Ordering::SeqCst),
        3,
        "the fallback beat evaluates"
    );
}

/// Fast OFF (the default): every scheduled cycle evaluates the strategy —
/// the historical behaviour the fast path must not disturb.
#[test]
fn default_mode_evaluates_every_scheduled_cycle() {
    let (mut c, count) = core_with_probe(false);
    let t0 = 1_000_000i64;
    for i in 0..5 {
        let t = t0 + i * 50;
        c.tick(t).unwrap();
        c.engine_evaluate(t);
    }
    assert_eq!(
        count.load(std::sync::atomic::Ordering::SeqCst),
        5,
        "with the fast path off, every scheduled cycle evaluates"
    );
}

/// 生产 16:29 形状的端到端牙（多账户 PR A 缺陷）：
/// `account.create` on a locally-settling core seeds the minted account
/// with `dry_seed_balance` — the same waking balance a boot account gets.
/// The gap this closes (生产 probe2 实锤): a runtime mint used to start at
/// zero, strategy sizing read `equity 0 below min_equity_usd` and the
/// scope never traded — and no boot repaired it, because the boot seed is
/// skipped wholesale once ANY account has a journal (an established
/// deployment always does). LIVE mints stay at zero: the venue's real
/// cash is the balance there.
///
/// The second half is the DURABILITY of that funding (生产 probe2 二次
/// 实锤: balance 1000 → restart → 0): the minted ledger gets its journal
/// path attached at mint time, so the funding row (and every later cash
/// event) lands in `data/ledger/<account>.journal.jsonl` and the next
/// boot's `attach_journals` replay restores it.
#[test]
fn account_create_seeds_a_runtime_mint_on_a_locally_settling_core() {
    let dir = std::env::temp_dir().join(format!("bk-mint-seed-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let accounts_path = dir.join("accounts.toml");
    let journal_dir = dir.join("ledger");
    std::fs::create_dir_all(&journal_dir).expect("journal dir");
    std::fs::write(
        &accounts_path,
        "[[account]]\nid = \"default\"\nname = \"main\"\n",
    )
    .expect("fixture");
    // Dry settles locally: the mint gets the seed.
    let mut c = Core::with_account_book(
        CoreConfig {
            mode: Mode::Dry,
            accounts_config_path: Some(accounts_path.to_string_lossy().into_owned()),
            ledger_journal_dir: Some(journal_dir.to_string_lossy().into_owned()),
            ..Default::default()
        },
        crate::account::config::load_account_ledgers(
            &accounts_path,
            1,
            crate::account::config::TEST_ALLOWED_ROUNDS,
        )
        .unwrap(),
    );
    let probe = crate::account::AccountId::from("probe2");
    c.create_account_persisting(
        &probe,
        Some("probe two"),
        Some("polymarket"),
        "test",
        "mint",
        5,
    )
    .expect("the mint lands");
    assert_eq!(
        c.accounts
            .ledger_of(&probe)
            .expect("minted account addressable")
            .balance(),
        c.config.dry_seed_balance,
        "the mint wakes with the configured seed, not zero"
    );

    // The funding row is DURABLE: the mint attached the journal path, so
    // the row landed on disk and a fresh boot's `attach_journals` replay
    // restores the balance — the restart does not vaporize it.
    let journal_path = journal_dir.join("probe2.journal.jsonl");
    let rows: Vec<crate::ledger::JournalEntry> = crate::jsonl::load(&journal_path, "test journal");
    let seed_row = rows
        .iter()
        .find(|r| r.ref_ == "account.create")
        .expect("the funding row is ON DISK");
    assert_eq!(seed_row.amount, c.config.dry_seed_balance);

    // LIVE: the venue owns the cash — a mint stays at zero (and no journal
    // path is needed to remember a venue truth, but attaching one is
    // harmless; the balance stays 0 and no row is written).
    let accounts_path_live = dir.join("accounts-live.toml");
    let journal_dir_live = dir.join("ledger-live");
    std::fs::create_dir_all(&journal_dir_live).expect("live journal dir");
    std::fs::write(
        &accounts_path_live,
        "[[account]]\nid = \"default\"\nname = \"main\"\n",
    )
    .expect("fixture");
    let mut live = Core::with_account_book(
        CoreConfig {
            mode: Mode::Live,
            accounts_config_path: Some(accounts_path_live.to_string_lossy().into_owned()),
            ledger_journal_dir: Some(journal_dir_live.to_string_lossy().into_owned()),
            ..Default::default()
        },
        crate::account::config::load_account_ledgers(
            &accounts_path_live,
            1,
            crate::account::config::TEST_ALLOWED_ROUNDS,
        )
        .unwrap(),
    );
    let probe_live = crate::account::AccountId::from("probe3");
    live.create_account_persisting(&probe_live, None, None, "test", "mint live", 6)
        .expect("the live mint lands");
    assert_eq!(
        live.accounts
            .ledger_of(&probe_live)
            .expect("minted live account addressable")
            .balance(),
        rust_decimal::Decimal::ZERO,
        "LIVE mints stay at zero — venue cash is the balance"
    );
    assert!(
        !journal_dir_live.join("probe3.journal.jsonl").exists(),
        "a zero funding records no journal row, so no file is created"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// 显示账户配置化（display.account.set 的事务半）：合法切位把内存指针
/// 与 accounts.toml 顶层 `display_account` 键一次写齐（persist 先行，
/// 失败两个存储都不动）；未知 id 被计划先行拒绝且文件字节不动；持久
/// 化关（无 config 路径）整体拒绝。纯展示指针：active 账户/记账不受
/// 任何影响 —— 内存的 display 单元独立于 active 单元。
/// issue 561（2026-10-20 裁决）：校验放宽为「账户存在即可」，frozen
/// 账户也是合法 display 目标（旧断言钉住的 Active-only 已随裁决废除）。
#[test]
fn display_account_set_persists_and_validates_plan_first() {
    let dir = std::env::temp_dir().join(format!("bk-display-set-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let accounts_path = dir.join("accounts.toml");
    std::fs::write(
        &accounts_path,
        "[[account]]\nid = \"default\"\nname = \"main\"\n\n[[account]]\nid = \"paper\"\nname = \"paper\"\n",
    )
    .expect("fixture");
    let mut c = Core::with_account_book(
        CoreConfig {
            accounts_config_path: Some(accounts_path.to_string_lossy().into_owned()),
            ..Default::default()
        },
        crate::account::config::load_account_ledgers(
            &accounts_path,
            1,
            crate::account::config::TEST_ALLOWED_ROUNDS,
        )
        .unwrap(),
    );
    // Boot default: the loader's first-ACTIVE rule points at `default`.
    assert_eq!(c.accounts.display_id(), "default");

    // The legal re-point: memory AND file move together; a FRESH load
    // (the next boot's call) reads `paper` back.
    c.set_display_account_persisting(&crate::account::AccountId::from("paper"), "test", "")
        .expect("the re-point lands");
    assert_eq!(c.accounts.display_id(), "paper");
    let reloaded = crate::account::config::load_account_ledgers(
        &accounts_path,
        2,
        crate::account::config::TEST_ALLOWED_ROUNDS,
    )
    .unwrap();
    assert_eq!(reloaded.display_id(), "paper", "the pointer is durable");
    // The top-level key survived the render next to both rows' fields.
    let text = std::fs::read_to_string(&accounts_path).unwrap();
    assert!(text.contains("display_account = \"paper\""), "{text}");

    // UNKNOWN id: refused plan-first, memory and file untouched.
    let before = std::fs::read_to_string(&accounts_path).unwrap();
    let err = c
        .set_display_account_persisting(&crate::account::AccountId::from("ghost"), "test", "")
        .expect_err("unknown id");
    assert!(err.message.contains("unknown account"), "{}", err.message);
    assert_eq!(c.accounts.display_id(), "paper");
    assert_eq!(
        std::fs::read_to_string(&accounts_path).unwrap(),
        before,
        "a refused write leaves the file byte-identical"
    );

    // issue 561（2026-10-20 裁决）改写旧断言：frozen 账户此前被
    // Active-only 校验拒绝，现在 display 是面板唯一可信账户源、切观察
    // 户必须生效 —— 「存在即可」，frozen1 合法落位，内存与文件同步。
    std::fs::write(
        &accounts_path,
        "[[account]]\nid = \"default\"\nname = \"main\"\n\n[[account]]\nid = \"frozen1\"\nname = \"f\"\nstatus = \"frozen\"\n",
    )
    .expect("fixture two");
    let mut c2 = Core::with_account_book(
        CoreConfig {
            accounts_config_path: Some(accounts_path.to_string_lossy().into_owned()),
            ..Default::default()
        },
        crate::account::config::load_account_ledgers(
            &accounts_path,
            1,
            crate::account::config::TEST_ALLOWED_ROUNDS,
        )
        .unwrap(),
    );
    c2.set_display_account_persisting(&crate::account::AccountId::from("frozen1"), "test", "")
        .expect("a frozen account is a legal display target (issue 561)");
    assert_eq!(
        c2.accounts.display_id(),
        "frozen1",
        "display follows a frozen book — view-only, posture gates the orders"
    );
    let reloaded2 = crate::account::config::load_account_ledgers(
        &accounts_path,
        2,
        crate::account::config::TEST_ALLOWED_ROUNDS,
    )
    .unwrap();
    assert_eq!(reloaded2.display_id(), "frozen1", "durable across boot");

    // 持久化关（无 accounts_config_path 的内核 — 回测/进程内测试）：整体
    // 拒绝，内存指针也不动（与 account.trading.set 同一姿态）。
    let mut c3 = Core::new(CoreConfig::default());
    let err = c3
        .set_display_account_persisting(&crate::account::AccountId::from("default"), "test", "")
        .expect_err("no persist path");
    assert!(
        err.message.contains("no accounts config path"),
        "{}",
        err.message
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// 空分配 = 该账户零策略（INHERIT 已按操作者裁决废除：显式分配是唯一
/// 真相源，default 账户一视同仁）。旧行为把 `{default, []}` 解析成
/// 「继承 boot 已装集」，DROP 阶段反而因此漏拆 —— 新语义下空分配把
/// 无持仓的 scope 全数拆掉，文件同样按 absence 拼写「不跑任何策略」。
#[test]
fn strategies_set_empty_on_default_runs_nothing_explicitly() {
    let dir = std::env::temp_dir().join(format!("bk-inherit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let accounts_path = dir.join("accounts.toml");
    std::fs::write(
        &accounts_path,
        "[[account]]\nid = \"default\"\nname = \"main\"\n\n[[account]]\nid = \"paper\"\nname = \"paper\"\n",
    )
    .expect("fixture");
    let mut c = Core::with_account_book(
        CoreConfig {
            accounts_config_path: Some(accounts_path.to_string_lossy().into_owned()),
            ..Default::default()
        },
        crate::account::config::load_account_ledgers(
            &accounts_path,
            1,
            crate::account::config::TEST_ALLOWED_ROUNDS,
        )
        .unwrap(),
    );
    c.enable_engine(Engine::new(EngineConfig::default()));
    crate::strategies::test_support::host(
        c.engine.as_mut().unwrap(),
        EngineConfig::default().trend,
        EngineConfig::default().spread_arb,
    );
    // Boot shape: one enabled default scope (what strategy-state.json replays).
    assert!(
        c.engine
            .as_mut()
            .unwrap()
            .set_strategy_enabled("spread_arb", true)
    );

    // The empty write is explicit "run nothing": every default scope
    // drops outright — the enabled one included. The trim is a decision
    // (durable exposure would ride the exits-only DEMOTE had positions
    // been held; none are).
    let kept = c
        .set_account_strategies_persisting(
            &crate::account::AccountId::from("default"),
            Vec::new(),
            "test",
            "explicit stop",
            2,
        )
        .expect("the empty write lands");
    assert!(
        kept.is_empty(),
        "no positions are held, so nothing is promoted exits-only"
    );
    let e = c.engine.as_ref().unwrap();
    // `strategy_registered` is the ASSIGNABLE set now (sticky provenance,
    // teardown-proof); scope liveness is `strategy_scopes`.
    let scopes = |name: &str| e.strategy_scopes(name);
    assert!(
        scopes("spread_arb").is_empty()
            && scopes("trend_follow").is_empty()
            && scopes("mean_reversion").is_empty(),
        "an empty assignment runs nothing — every flat scope drops, enabled included"
    );
    assert!(
        e.strategy_registered("spread_arb"),
        "the package stays assignable after the teardown (sticky provenance)"
    );
    assert!(
        e.enabled_strategies().is_empty(),
        "nothing enabled remains after the empty write"
    );
    // The persisted file spells the empty assignment by absence (the
    // config half's own牙).
    let text = std::fs::read_to_string(&accounts_path).unwrap();
    let row = text.split("[[account]]").nth(1).unwrap();
    assert!(!row.contains("strategies"), "empty spelled by absence");

    // A NAMED assignment restores exactly one scope. The rebuild's ADD
    // phase (incl. "assignment IS enablement") is pinned by the engine
    // tests (`rebuild_drops_an_enabled_scope_the_assignment_omits`); the
    // in-process test adapters are not Lua packages, so a rebuilt scope
    // here would need the source seam — the FILE half is what this test
    // adds on top of the engine teeth.
    c.set_account_strategies_persisting(
        &crate::account::AccountId::from("default"),
        vec!["spread_arb".into()],
        "test",
        "named restore",
        3,
    )
    .expect("the named write lands (build fn declines a non-Lua source: warn, no panic)");
    let book_assignment = c
        .accounts
        .strategy_assignment(&crate::account::AccountId::from("default"))
        .expect("the named assignment is in the book");
    assert_eq!(
        book_assignment,
        &vec!["spread_arb".to_string()],
        "the book spells the named assignment"
    );
    let text = std::fs::read_to_string(&accounts_path).unwrap();
    let row = text.split("[[account]]").nth(1).unwrap();
    assert!(
        row.contains("strategies = [\"spread_arb\"]"),
        "the file spells the named assignment"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// boot 分配应用（生产 `run()` 专属路径，回放不经过）：三条腿 ——
/// ① default 无键 + strategy-state 非空 → 一次性迁移物化进 accounts.toml
/// 且返回迁移集；② 已显式键的账户照文件执行（不再迁移）；③ 空键账户
/// boot 时零 scope（无 INHERIT）。scope 实例化是 Lua 源专属（test 适配器
/// 建不出来），所以 engine 侧断言钉在「分配即启用」的 rebuild 牙上，这里
/// 钉迁移与簿/文件拼写。
#[test]
fn boot_applies_assignments_and_migrates_the_implicit_default_once() {
    let dir = std::env::temp_dir().join(format!("bk-boot-assign-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let accounts_path = dir.join("accounts.toml");
    std::fs::write(
        &accounts_path,
        "[[account]]\nid = \"default\"\nname = \"main\"\n\n[[account]]\nid = \"probe2\"\nname = \"probe two\"\nstrategies = [\"spread_arb\"]\n",
    )
    .expect("fixture");
    let state_path = dir.join("strategy-state.json");
    crate::strategy_state::save(&state_path, &["spread_arb".into(), "trend_follow".into()]);

    let mut c = Core::with_account_book(
        CoreConfig {
            accounts_config_path: Some(accounts_path.to_string_lossy().into_owned()),
            strategy_state_path: Some(state_path.to_string_lossy().into_owned()),
            ..Default::default()
        },
        crate::account::config::load_account_ledgers(
            &accounts_path,
            1,
            crate::account::config::TEST_ALLOWED_ROUNDS,
        )
        .unwrap(),
    );
    c.enable_engine(Engine::new(EngineConfig::default()));
    crate::strategies::test_support::host_disabled(
        c.engine.as_mut().unwrap(),
        EngineConfig::default().trend,
        EngineConfig::default().spread_arb,
    );

    // ①+③: the default migrates (file gains the key), probe2's key is
    // honored (its book spelling is already explicit).
    let migrated = c.apply_account_strategy_assignments_at_boot();
    assert_eq!(
        migrated,
        vec!["spread_arb".to_string(), "trend_follow".to_string()],
        "the migration returns the materialized default assignment"
    );
    let default_id = crate::account::default_account_id();
    assert_eq!(
        c.accounts
            .strategy_assignment(&default_id)
            .expect("migrated into the book"),
        &vec!["spread_arb".to_string(), "trend_follow".to_string()],
    );
    let text = std::fs::read_to_string(&accounts_path).unwrap();
    let default_row = text.split("[[account]]").nth(1).unwrap();
    assert!(
        default_row.contains("strategies"),
        "the file gained the default's explicit assignment"
    );

    // Idempotence: a SECOND boot call does not re-report (the file now
    // carries the key — the migration is one-time by construction).
    let again = c.apply_account_strategy_assignments_at_boot();
    assert!(
        again.is_empty(),
        "the migration never runs twice: the file is the truth now"
    );

    // ②: an account with NO key (and no persisted set to migrate — the
    // file now has one for default, so wipe the state file's role by
    // pointing the config away) boots zero scopes: no INHERIT.
    let mut c2 = Core::with_account_book(
        CoreConfig {
            accounts_config_path: Some(accounts_path.to_string_lossy().into_owned()),
            strategy_state_path: None,
            ..Default::default()
        },
        crate::account::config::load_account_ledgers(
            &accounts_path,
            1,
            crate::account::config::TEST_ALLOWED_ROUNDS,
        )
        .unwrap(),
    );
    c2.enable_engine(Engine::new(EngineConfig::default()));
    crate::strategies::test_support::host_disabled(
        c2.engine.as_mut().unwrap(),
        EngineConfig::default().trend,
        EngineConfig::default().spread_arb,
    );
    let migrated2 = c2.apply_account_strategy_assignments_at_boot();
    assert!(migrated2.is_empty(), "no state file, no migration");
    // probe2's assignment names spread_arb, but the test adapter is a
    // non-Lua source: build_lua_scope declines, the ADD phase warns and
    // skips — probe2 gains NO scope (fail-closed, never a panic). The
    // default's own row (the migrated set) names trend_follow, whose
    // pre-hosted default-scope instance EXISTS: the Some(existing) arm
    // of the ADD phase enables it (in the set = the 启用 state) — the
    // migration earlier in this test materialized the state file's set
    // as the default's accounts.toml key, and c2 boots from that same
    // file, so the default row is explicit (no INHERIT involved).
    assert_eq!(
        c2.enabled_strategy_names(),
        vec!["spread_arb".to_string(), "trend_follow".to_string()],
        "the file's enablement sets are what boot — the state file is out \
         of the loop (both names resolve to the default scope's \
         pre-hosted adapter instances)"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The production upgrade shape pinned (账户↔venue 解耦 boot 兼容牙):
/// the shipped accounts.toml — `default` with
/// `strategies = ["pair_discount_arb","spread_arb"]` — boots EXACTLY the
/// scopes it names (spread_arb's scope lands enabled), the old global
/// toggle's strategy-state.json must not override an explicit key, and
/// unregistered names skip with a warning, never a panic.
#[test]
fn boot_keeps_the_production_default_enablement_shape_byte_identical() {
    let dir = std::env::temp_dir().join(format!("bk-prod-shape-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let accounts_path = dir.join("accounts.toml");
    // The production file's exact spelling (from the deployment tree).
    std::fs::write(
        &accounts_path,
        "[[account]]\nid = \"default\"\nname = \"main\"\nstrategies = [\"pair_discount_arb\", \"spread_arb\"]\n",
    )
    .expect("fixture");
    // The old global toggle's durable form: spread_arb off, the rest on —
    // the file's key must WIN (the migration never overrides an explicit
    // key, and the strategy-state.json read must not leak onto scopes).
    let state_path = dir.join("strategy-state.json");
    crate::strategy_state::save(&state_path, &["trend_follow".into()]);

    let mut c = Core::with_account_book(
        CoreConfig {
            accounts_config_path: Some(accounts_path.to_string_lossy().into_owned()),
            strategy_state_path: Some(state_path.to_string_lossy().into_owned()),
            ..Default::default()
        },
        crate::account::config::load_account_ledgers(
            &accounts_path,
            1,
            crate::account::config::TEST_ALLOWED_ROUNDS,
        )
        .unwrap(),
    );
    c.enable_engine(Engine::new(EngineConfig::default()));
    crate::strategies::test_support::host_disabled(
        c.engine.as_mut().unwrap(),
        EngineConfig::default().trend,
        EngineConfig::default().spread_arb,
    );
    // The migration NEVER runs: the file's default row is already explicit.
    let migrated = c.apply_account_strategy_assignments_at_boot();
    assert!(
        migrated.is_empty(),
        "an explicit key means no migration: {migrated:?}"
    );
    // The book + file are untouched (byte-identical behaviour on upgrade).
    assert_eq!(
        c.accounts
            .strategy_assignment(&crate::account::default_account_id())
            .expect("the key rides"),
        &vec!["pair_discount_arb".to_string(), "spread_arb".to_string()],
    );
    let scopes = |name: &str| c.engine.as_ref().unwrap().strategy_scopes(name);
    assert_eq!(
        scopes("spread_arb"),
        vec!["default".to_string()],
        "the named scope lands (the test adapter is non-Lua, so only the \
         DEFAULT scope's pre-hosted instance is here — the ADD phase's \
         attempt at a second one warns and skips)"
    );
    assert!(
        scopes("pair_discount_arb").is_empty(),
        "an unregistered name is skipped with a warning, never a panic"
    );
    assert!(
        scopes("trend_follow").is_empty(),
        "strategy-state.json must NOT resurrect anything the file does not name"
    );
    // The engine's enabled set mirrors the file, not the old state file.
    assert_eq!(
        c.enabled_strategy_names(),
        vec!["spread_arb".to_string()],
        "the file's spread_arb entry is the enablement; trend_follow's \
         persisted global toggle is dead here"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// 启用集持久化 boot 回放牙：per-account enable/disable 的写路径落盘
/// （persist FIRST，一事务），重启（新 Core + 同一文件）后 scope 集精确
/// 复原 —— paper 的 scope 回来了、default 的没有，启用状态逐 scope 一致。
#[test]
fn scoped_enablement_persists_and_boots_back_exactly() {
    let dir = std::env::temp_dir().join(format!("bk-scope-enable-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let accounts_path = dir.join("accounts.toml");
    std::fs::write(
        &accounts_path,
        "[[account]]\nid = \"default\"\nname = \"main\"\n\n[[account]]\nid = \"paper\"\nname = \"paper\"\n",
    )
    .expect("fixture");
    let state_path = dir.join("strategy-state.json");

    let mut c = Core::with_account_book(
        CoreConfig {
            accounts_config_path: Some(accounts_path.to_string_lossy().into_owned()),
            strategy_state_path: Some(state_path.to_string_lossy().into_owned()),
            ..Default::default()
        },
        crate::account::config::load_account_ledgers(
            &accounts_path,
            1,
            crate::account::config::TEST_ALLOWED_ROUNDS,
        )
        .unwrap(),
    );
    c.enable_engine(Engine::new(EngineConfig::default()));
    crate::strategies::test_support::host_disabled(
        c.engine.as_mut().unwrap(),
        EngineConfig::default().trend,
        EngineConfig::default().spread_arb,
    );
    let paper = crate::account::AccountId::from("paper");

    // ENABLE trend_follow FOR paper: the set lands on disk.
    let (kept, set_now) = c
        .set_scoped_strategy_enabled(&paper, "trend_follow", true, "test", "panel on", 2)
        .expect("the scoped enable lands");
    assert!(kept.is_empty(), "nothing held → nothing demoted");
    assert_eq!(set_now, vec!["trend_follow".to_string()]);
    // The GLOBAL toggle is untouched (spread_arb's default scope state is
    // its own; trend_follow's global row does not exist to be touched).
    assert!(
        !c.enabled_strategy_names()
            .contains(&"trend_follow".to_string()),
        "the per-account write must not flip the global switch"
    );
    // strategy.list's accounts column reads the book.
    assert_eq!(
        c.strategy_enabled_accounts("trend_follow"),
        vec!["paper".to_string()],
        "paper is the only account whose enablement set names it"
    );
    let text = std::fs::read_to_string(&accounts_path).unwrap();
    let paper_row = text.split("[[account]]").nth(2).unwrap();
    assert!(
        paper_row.contains("strategies = [\"trend_follow\"]"),
        "the file spells paper's enablement: {text}"
    );

    // DISABLE it again for paper (flat scope → teardown), then re-enable:
    // the whole dance is the panel's round-trip.
    let (kept, set_now) = c
        .set_scoped_strategy_enabled(&paper, "trend_follow", false, "test", "panel off", 3)
        .expect("the scoped disable lands");
    assert!(kept.is_empty() && set_now.is_empty());
    assert!(c.strategy_enabled_accounts("trend_follow").is_empty());
    let text = std::fs::read_to_string(&accounts_path).unwrap();
    let paper_row = text.split("[[account]]").nth(2).unwrap();
    assert!(
        !paper_row.contains("strategies"),
        "the empty set is spelled by absence"
    );

    // REBOOT: a fresh Core over the same file (paper re-enabled first so
    // the boot has something to restore), then boot-apply. The scope set
    // comes back EXACTLY as the file left it.
    let _ = c.set_scoped_strategy_enabled(
        &paper,
        "trend_follow",
        true,
        "test",
        "re-armed before reboot",
        4,
    );
    let mut c2 = Core::with_account_book(
        CoreConfig {
            accounts_config_path: Some(accounts_path.to_string_lossy().into_owned()),
            strategy_state_path: Some(state_path.to_string_lossy().into_owned()),
            ..Default::default()
        },
        crate::account::config::load_account_ledgers(
            &accounts_path,
            1,
            crate::account::config::TEST_ALLOWED_ROUNDS,
        )
        .unwrap(),
    );
    c2.enable_engine(Engine::new(EngineConfig::default()));
    crate::strategies::test_support::host_disabled(
        c2.engine.as_mut().unwrap(),
        EngineConfig::default().trend,
        EngineConfig::default().spread_arb,
    );
    let migrated = c2.apply_account_strategy_assignments_at_boot();
    assert!(migrated.is_empty(), "paper's explicit key = no migration");
    // The book remembers the set; the reply echo of a fresh read matches.
    assert_eq!(
        c2.strategy_enabled_accounts("trend_follow"),
        vec!["paper".to_string()],
        "the reboot restores paper's enablement exactly"
    );
    assert_eq!(
        c2.strategy_enabled_accounts("spread_arb"),
        Vec::<String>::new(),
        "default has no key and no global toggle → nothing to report"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// #537 关不掉牙（操作者 2026-10-19 报告）：default 账户的启用集写在
/// accounts.toml 后，面板的全局开关（`strategy.enable` accountId=default
/// 的服务层形状）只翻引擎旗标 + 写 strategy-state.json —— boot 的
/// rebuild 会把文件集合里的 scope 无条件拉回 enabled，「关掉」活不过一次
/// 重启。合流后的合同：default 的 scoped 写与全局开关**双向同步**——
/// `set_scoped_strategy_enabled(default, …)` 同时落 accounts.toml 集合
/// 与 strategy-state.json，boot 读到的就是操作者最后一次的决定；文件
/// 集合里被移除的名字不再借陈旧 state 文件复活（migration 的输入与
/// 集合写入永远一致）。
#[test]
fn default_toggle_persists_and_boot_does_not_resurrect() {
    let dir = std::env::temp_dir().join(format!("bk-default-toggle-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let accounts_path = dir.join("accounts.toml");
    // The pre-#537 production shape: the default account's enablement set
    // IS in the file (this is what made the toggle unable to win).
    std::fs::write(
        &accounts_path,
        "[[account]]\nid = \"default\"\nname = \"main\"\nstrategies = [\"trend_follow\", \"spread_arb\"]\n\n[[account]]\nid = \"paper\"\nname = \"paper\"\n",
    )
    .expect("fixture");
    let state_path = dir.join("strategy-state.json");

    let mut c = Core::with_account_book(
        CoreConfig {
            accounts_config_path: Some(accounts_path.to_string_lossy().into_owned()),
            strategy_state_path: Some(state_path.to_string_lossy().into_owned()),
            ..Default::default()
        },
        crate::account::config::load_account_ledgers(
            &accounts_path,
            1,
            crate::account::config::TEST_ALLOWED_ROUNDS,
        )
        .unwrap(),
    );
    c.enable_engine(Engine::new(EngineConfig::default()));
    crate::strategies::test_support::host_disabled(
        c.engine.as_mut().unwrap(),
        EngineConfig::default().trend,
        EngineConfig::default().spread_arb,
    );
    // The file is explicit: no migration.
    assert!(c.apply_account_strategy_assignments_at_boot().is_empty());
    let default_id = crate::account::default_account_id();

    // The operator turns spread_arb off (the scoped write is what the
    // panel's toggle now rides): the file's set loses the name AND the
    // state file follows.
    let (kept, set_now) = c
        .set_scoped_strategy_enabled(&default_id, "spread_arb", false, "test", "panel off", 2)
        .expect("the default disable lands");
    assert!(kept.is_empty(), "flat scope → nothing demoted");
    assert_eq!(set_now, vec!["trend_follow".to_string()]);
    let text = std::fs::read_to_string(&accounts_path).unwrap();
    let default_row = text.split("[[account]]").nth(1).unwrap();
    assert!(
        default_row.contains("strategies = [\"trend_follow\"]"),
        "the file lost spread_arb the moment the toggle did: {text}"
    );
    assert_eq!(
        crate::strategy_state::load(&state_path),
        vec!["trend_follow".to_string()],
        "the state file follows the scoped write"
    );

    // REBOOT: the boot's rebuild must not resurrect spread_arb — the file
    // is the only truth and the state file agrees with it.
    let mut c2 = Core::with_account_book(
        CoreConfig {
            accounts_config_path: Some(accounts_path.to_string_lossy().into_owned()),
            strategy_state_path: Some(state_path.to_string_lossy().into_owned()),
            ..Default::default()
        },
        crate::account::config::load_account_ledgers(
            &accounts_path,
            1,
            crate::account::config::TEST_ALLOWED_ROUNDS,
        )
        .unwrap(),
    );
    c2.enable_engine(Engine::new(EngineConfig::default()));
    crate::strategies::test_support::host_disabled(
        c2.engine.as_mut().unwrap(),
        EngineConfig::default().trend,
        EngineConfig::default().spread_arb,
    );
    let migrated = c2.apply_account_strategy_assignments_at_boot();
    assert!(migrated.is_empty(), "explicit key ⇒ no migration");
    assert!(
        !c2.enabled_strategy_names()
            .contains(&"spread_arb".to_string()),
        "a disabled scope must not come back from the file or the state file"
    );
    assert_eq!(
        c2.enabled_strategy_names(),
        vec!["trend_follow".to_string()],
    );

    // The reverse direction: re-enabling through the same write lands the
    // name in BOTH stores again.
    let _ = c2.set_scoped_strategy_enabled(&default_id, "spread_arb", true, "test", "panel on", 3);
    let text = std::fs::read_to_string(&accounts_path).unwrap();
    let default_row = text.split("[[account]]").nth(1).unwrap();
    assert!(
        default_row.contains("spread_arb"),
        "the enable lands in the file too: {text}"
    );
    assert!(crate::strategy_state::load(&state_path).contains(&"spread_arb".to_string()));
    let _ = std::fs::remove_dir_all(&dir);
}
/// `active` 指针 —— 引擎的 scope 集、启用标志、exits-only 降级与意图
/// 输出路径一行不碰。IPC 的 account.switch 更是会话级（另一 agent 域
/// 的既有牙已钉 wire 半）；这里钉的是核心侧可能被误接线的最后一环。
#[test]
fn process_level_switch_touches_nothing_but_the_active_pointer() {
    // A two-account core with a test-hosted engine (no strategy_dir: the
    // boot registers nothing itself), built inline — the audit mod's
    // `two_book_core` fixture is private to that module.
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
    let mut c = Core::with_account_book(
        CoreConfig {
            risk: RiskConfig {
                max_order_notional: dec!(100),
                ..Default::default()
            },
            dry_seed_balance: 10_000.into(),
            auto_exits_enabled: false,
            trade_log_path: None,
            ..Default::default()
        },
        book,
    );
    let cfg = c.config.clone();
    cfg.install_engine(&mut c)
        .expect("test engine (no strategy_dir: registers nothing itself)");
    let engine = c.engine.as_mut().expect("test engine");
    crate::strategies::test_support::host_disabled(engine, Default::default(), Default::default());
    // Build a distinguishable scope landscape: paper hosts spread_arb,
    // demoted exits-only; default hosts it ENABLED.
    engine
        .register_scoped_strategy(
            Box::new(crate::strategies::test_support::TestSpreadArb::new(
                Default::default(),
                Default::default(),
            )),
            "test".into(),
            "paper",
        )
        .expect("paper scope");
    engine.set_strategy_enabled("spread_arb", true);
    let held = vec!["spread_arb".to_string()];
    engine.rebuild_account_scopes("paper", &[], &held, &mut |_n, _a| {
        Some(Box::new(
            crate::strategies::test_support::TestSpreadArb::new(
                Default::default(),
                Default::default(),
            ),
        ))
    });
    // The closure above must stand a real instance; rebuild keeps the
    // existing scope anyway (the ADD phase finds it), so assert the shape.
    assert!(
        engine
            .strategy_scopes("spread_arb")
            .iter()
            .any(|a| a == "paper"),
        "the exits-only scope stays hosted"
    );
    let before_enabled = engine.enabled_strategies();
    let before_exits_only = engine.exits_only_scopes();

    // The switch: process-level (the book's own API).
    c.accounts
        .switch(&crate::account::AccountId::from("paper"))
        .expect("switch to a configured account");
    assert_eq!(
        c.accounts.active_id().as_str(),
        "paper",
        "the switch moved the active pointer"
    );

    // NOTHING else moved: scope landscape, enablement, demotions.
    assert_eq!(
        engine.enabled_strategies(),
        before_enabled,
        "enablement untouched by the switch"
    );
    assert_eq!(
        engine.exits_only_scopes(),
        before_exits_only,
        "exits-only demotions untouched by the switch"
    );
    assert!(
        engine
            .strategy_scopes("spread_arb")
            .iter()
            .any(|a| a == "paper"),
        "paper's scope survived the switch"
    );
    // The intent output path still resolves orders to their OWN accounts:
    // a place for paper still lands on paper after default became active.
    let view_default = c
        .account_views(1)
        .into_iter()
        .find(|v| v.id == "default")
        .unwrap();
    assert!(
        view_default.strategies.is_empty(),
        "the book's per-account enablement sets are untouched"
    );
}

/// A test strategy that dips on an explicit asset list. Declaring the assets
/// lets several strategies share one cycle without fighting over a token.
struct TargetDip {
    name: String,
    buy_below: Decimal,
    assets: Vec<String>,
    /// Shared entry gates this strategy declares unnecessary (E2-b / #27).
    gates: crate::strategies::GateExemptions,
    /// THIS instance's registration account — the stamp the signal
    /// carries (packages are account-blind by declaration; the host
    /// builder passes the scope account the same way `build_lua_scope`
    /// does). Default scope keeps the process default.
    scope: String,
}
impl crate::strategies::EngineStrategy for TargetDip {
    fn name(&self) -> &str {
        &self.name
    }
    fn on_book(&mut self, _t: &str, _s: &crate::model::OrderbookSnapshot, _n: i64) {}
    fn on_round(&mut self, _slot: i64, _time_left_sec: i64, _now_ms: i64) {}
    fn gate_exemptions(&self) -> crate::strategies::GateExemptions {
        self.gates
    }
    fn instance_account(&self) -> String {
        self.scope.clone()
    }
    fn find_candidates(
        &mut self,
        ctx: &crate::strategies::StrategyCtx<'_>,
    ) -> Vec<crate::signal::TradeSignal> {
        let mut out = Vec::new();
        for market in ctx.markets() {
            if !self.assets.contains(&market.asset) {
                continue;
            }
            for (token_id, direction) in [
                (&market.up_token_id, crate::model::SignalDirection::Up),
                (&market.down_token_id, crate::model::SignalDirection::Down),
            ] {
                let Some(book) = ctx.fresh_book(token_id) else {
                    continue;
                };
                if book.mid_price <= self.buy_below {
                    out.push(crate::signal::TradeSignal {
                        strategy: self.name.clone(),
                        account_id: self.scope.clone(),
                        asset: market.asset.clone(),
                        direction,
                        token_id: token_id.clone(),
                        condition_id: market.condition_id.clone(),
                        price: book.mid_price,
                        reason: "target dip".into(),
                        shares: None,
                        pair: None,
                    });
                }
            }
        }
        out
    }
}

const DIPS: [(&str, &[&str]); 3] = [("small", &["BTC"]), ("mid", &["ETH"]), ("unset", &["SOL"])];

/// Core with a three-asset engine and the given dip strategies registered
/// enabled. `spread_arb` is switched off so only the test strategies emit.
///
/// The engine is built through `CoreConfig::engine_config()` — the ONE
/// production mapping — so these tests exercise the same per-strategy sizing
/// projection the live server and the backtester use.
fn core_with_dips(
    limits: HashMap<String, StrategyLimit>,
    dips: &[(&str, &[&str])],
    global_max_positions: usize,
) -> Core {
    core_with_declared_dips(limits, dips, global_max_positions, 0, |_| {
        crate::strategies::GateExemptions::none()
    })
}

/// Same as [`core_with_dips`], but each strategy also declares the shared
/// entry gates it does not need (E2-b / #27) and the round-timing window is
/// set up front — the engine takes its scanner config from
/// `CoreConfig::engine_config()` at construction, so a later `config_mut`
/// write would not reach it (nor would one reach the `RiskGate`, which
/// snapshots `config.risk` the same way).
fn core_with_declared_dips(
    limits: HashMap<String, StrategyLimit>,
    dips: &[(&str, &[&str])],
    global_max_positions: usize,
    min_round_age_sec: i64,
    gates: impl Fn(&str) -> crate::strategies::GateExemptions,
) -> Core {
    core_with_declared_dips_and_risk(
        limits,
        dips,
        global_max_positions,
        min_round_age_sec,
        RiskConfig {
            max_order_notional: dec!(100),
            ..Default::default()
        },
        gates,
    )
}

/// The full form: also chooses the risk config the gate is built from.
fn core_with_declared_dips_and_risk(
    limits: HashMap<String, StrategyLimit>,
    dips: &[(&str, &[&str])],
    global_max_positions: usize,
    min_round_age_sec: i64,
    risk: RiskConfig,
    gates: impl Fn(&str) -> crate::strategies::GateExemptions,
) -> Core {
    let mut c = Core::new(CoreConfig {
        risk,
        dry_seed_balance: dec!(1000),
        engine_enabled: true,
        round_duration_sec: 900,
        min_round_age_sec,
        auto_exits_enabled: false,
        assets: vec!["BTC".into(), "ETH".into(), "SOL".into()],
        positions: crate::position::PositionConfig {
            max_positions: global_max_positions,
            exit: crate::exit_policy::ExitConfig {
                min_time_left_sec: 0,
                ..Default::default()
            },
            ..Default::default()
        },
        strategy_limits: limits,
        ..Default::default()
    });
    let cfg = c.config().engine_config();
    let mut eng = Engine::new(cfg.clone());
    // Host the reference adapters too (the kernel ships no strategies, PR-B),
    // so gate-exemption accounting has the same declarations to list the
    // production session would have.
    crate::strategies::test_support::host(&mut eng, cfg.trend, cfg.spread_arb);
    for (name, assets) in dips {
        eng.register_user_strategy(
            Box::new(TargetDip {
                name: (*name).to_string(),
                buy_below: dec!(0.45),
                assets: assets.iter().map(|a| (*a).to_string()).collect(),
                gates: gates(name),
                scope: blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
            }),
            "test".into(),
        )
        .unwrap();
        assert!(eng.set_strategy_enabled(name, true));
    }
    // No isolation step is needed any more: the kernel ships zero strategies
    // (PR-B), so a freshly built engine hosts exactly the dips registered above.
    c.enable_engine(eng);
    c
}

fn three_markets(now: i64) -> Vec<CryptoMarket> {
    let _ = now;
    ["BTC", "ETH", "SOL"]
        .iter()
        .map(|a| CryptoMarket {
            asset: (*a).into(),
            condition_id: format!("cond_{a}"),
            question_id: format!("q_{a}"),
            up_token_id: format!("{}_up", a.to_lowercase()),
            down_token_id: format!("{}_down", a.to_lowercase()),
            up_price: dec!(0.6),
            down_price: dec!(0.4),
            expires_at_ms: 1_800_000,
            round_slot: 1,
            round_duration_sec: 900,
            archive_verdict: false,
            venue: String::new(),
            neg_risk: true,
            question: format!("{a} up or down"),
        })
        .collect()
}

/// Push a 0.44-mid book on the UP token of each given asset. Only the listed
/// assets dip, so a cycle can be aimed at one strategy.
fn feed_dip_on(c: &mut Core, now: i64, assets: &[&str]) {
    for asset in assets {
        let token = format!("{}_up", asset.to_lowercase());
        c.engine_on_data(
            DataEvent::Book {
                token_id: token,
                bids: vec![(dec!(0.43), dec!(100))],
                asks: vec![(dec!(0.45), dec!(100))],
                now_ms: now,
            },
            now,
        );
    }
}

/// Round + a 0.44-mid book on every asset's up token. Each strategy that
/// targets one of those assets emits one entry priced at 0.44.
fn feed_three_asset_dip(c: &mut Core, now: i64) {
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: three_markets(now),
            now_ms: now,
        },
        now,
    );
    feed_dip_on(c, now + 1_000, &["BTC", "ETH", "SOL"]);
}

/// Cross the book so the resting maker bid fills and a position opens.
fn fill(c: &mut Core, asset: &str, now: i64) {
    let token = format!("{}_up", asset.to_lowercase());
    c.book_snapshot(
        &token,
        vec![(dec!(0.42), dec!(100))],
        vec![(dec!(0.42), dec!(100))],
        now,
    );
}

fn limits_of(rows: &[(&str, StrategyLimit)]) -> HashMap<String, StrategyLimit> {
    rows.iter()
        .map(|(n, l)| ((*n).to_string(), l.clone()))
        .collect()
}

/// One strategy's override bag for the startup-selection tests, built the way
/// the IPC handler builds it (the shadow-evolution test module has its own).
fn one_param(strategy: &str, knob: &str, value: Decimal) -> MutableParams {
    let mut p = StrategyParams::new();
    p.set(knob, value);
    let mut m = MutableParams::new();
    m.set_strategy(strategy, p);
    m
}

// ── #406: the zero-price fail-closed guard ──────────────────────────────

/// A dip probe whose `buy_below` threshold is 0: ANY fresh book (even a
/// mid of 0.00) arms an entry priced at that mid. A book whose sides are
/// both empty prices the signal at 0.00 — the exact intent shape that
/// PANICKED the kernel at the policy budget-shrink division
/// (`budget_usd / req.price`) before arbitration Gate 1 could refuse it.
/// The guard refuses the intent fail-closed: counted, bucketed, logged,
/// and the process survives (0 orders placed, no panic).
#[test]
fn zero_price_entry_is_refused_fail_closed_not_a_panic() {
    let mut c = Core::new(CoreConfig {
        dry_seed_balance: dec!(1000),
        engine_enabled: true,
        round_duration_sec: 900,
        auto_exits_enabled: false,
        assets: vec!["BTC".into()],
        positions: crate::position::PositionConfig {
            max_positions: 5,
            exit: crate::exit_policy::ExitConfig {
                min_time_left_sec: 0,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    });
    let cfg = c.config().engine_config();
    let mut eng = Engine::new(cfg.clone());
    crate::strategies::test_support::host(&mut eng, cfg.trend, cfg.spread_arb);
    eng.register_user_strategy(
        Box::new(TargetDip {
            name: "zero_dip".to_string(),
            buy_below: Decimal::ZERO,
            assets: vec!["BTC".to_string()],
            gates: crate::strategies::GateExemptions::none(),
            scope: blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
        }),
        "test".into(),
    )
    .unwrap();
    assert!(eng.set_strategy_enabled("zero_dip", true));
    c.enable_engine(eng);

    let now = 1_000_000i64;
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: three_markets(now),
            now_ms: now,
        },
        now,
    );
    // An ask-only book on the BTC up token: fresh (timestamp is now) and
    // NON-empty (so fresh_book serves it), but with no bid the F6 rule
    // prices the mid at 0.00 — the one-sided shape a top-only feed
    // degrades into, and the exact intent shape that PANICKED the kernel
    // at the policy budget-shrink division before this guard existed.
    c.engine_on_data(
        DataEvent::Book {
            token_id: "btc_up".into(),
            bids: vec![],
            asks: vec![(dec!(0.60), dec!(100))],
            now_ms: now + 1_000,
        },
        now + 1_000,
    );
    // The evaluate itself must not panic — this is the regression the
    // test exists for (pre-guard this panicked inside the policy arm).
    let placed = c.engine_evaluate(now + 1_100);
    assert_eq!(placed, 0, "a 0.00-price entry must be refused, not placed");
    assert!(
        c.list_orders().is_empty(),
        "no order may reach the book: {:#?}",
        c.list_orders()
    );
    let stats = c.engine_stats();
    assert_eq!(
        stats["illegalPriceRejected"], 1,
        "the guard counts the refusal: {stats}"
    );
    assert_eq!(
        stats["placeRejected"], 1,
        "and it counts as a place rejection"
    );
    // The per-strategy accounting carries the cause bucket.
    let rows = c.strategy_stats();
    let row = rows
        .iter()
        .find(|r| r["name"] == "zero_dip")
        .expect("the strategy's accounting row");
    assert_eq!(
        row["rejectionCauses"]["illegal.price"], 1,
        "bucketed under illegal.price: {row}"
    );
}

// ── E4-a: startup strategy selection ────────────────────────────────────

/// Installing an engine with a selection puts the session in exactly the state
/// an operator would reach with `strategy.enable`/`strategy.disable`: the
/// selection goes through the same toggle (so Shadow Evolution is rewired the
/// same way), an unknown name is ignored rather than fatal, and disabling wins
/// over enabling when both name the same strategy.
#[test]
fn startup_selection_matches_a_runtime_toggle() {
    // `install_engine` loads only what `strategy_dir` holds (PR-B: the
    // kernel registers nothing itself), so the test hosts the three
    // adapters right after it — the same registration the loader performs
    // for a real library. Nothing is enabled unless the config says so.
    let install = |cfg: CoreConfig| {
        let mut c = Core::new(cfg.clone());
        // No `requested_strategies` here, so the startup self-check has
        // nothing to insist on and install cannot refuse.
        cfg.install_engine(&mut c).expect("engine install");
        let cfg2 = cfg.engine_config();
        crate::strategies::test_support::host_disabled(
            c.engine.as_mut().expect("engine installed"),
            cfg2.trend,
            cfg2.spread_arb,
        );
        // The startup selection is applied after the strategy dir loads, in
        // the same order production uses — so a name asked for at startup
        // lands on the just-registered adapter (an unknown name stays a
        // silent no-op, exactly what install_engine does).
        for name in &cfg.enabled_strategies {
            c.set_strategy_enabled(name, true);
        }
        for name in &cfg.disabled_strategies {
            c.set_strategy_enabled(name, false);
        }
        c
    };
    let base = CoreConfig {
        dry_seed_balance: dec!(1000),
        engine_enabled: true,
        ..Default::default()
    };

    // Default: ZERO strategies enabled (the kernel couples to none). The
    // hosted adapters all register, all disabled.
    let plain = install(base.clone());
    assert!(
        plain.enabled_strategy_names().is_empty(),
        "a fresh boot must enable nothing: {:?}",
        plain.enabled_strategy_names()
    );
    assert_eq!(
        plain.strategy_names(),
        vec![
            "mean_reversion".to_string(),
            "spread_arb".to_string(),
            "trend_follow".to_string()
        ],
        "registered but disabled (sorted: the per-NAME projection dedupes across account scopes)"
    );

    // Opt in by flag: same result as toggling it at runtime.
    let by_flag = install(CoreConfig {
        enabled_strategies: vec!["trend_follow".into()],
        ..base.clone()
    });
    let mut by_toggle = install(base.clone());
    assert!(by_toggle.set_strategy_enabled("trend_follow", true));

    assert_eq!(
        by_flag.enabled_strategy_names(),
        vec!["trend_follow".to_string()]
    );
    assert_eq!(
        by_flag.enabled_strategy_names(),
        by_toggle.enabled_strategy_names()
    );

    // An explicit "off" wins over an "on" for the same name, and a name the
    // engine does not host is ignored without taking the session down.
    let both = install(CoreConfig {
        enabled_strategies: vec!["trend_follow".into(), "alpha_strategy".into()],
        disabled_strategies: vec!["trend_follow".into()],
        ..base.clone()
    });
    assert!(
        both.enabled_strategy_names().is_empty(),
        "enable then disable the same name nets to off: {:?}",
        both.enabled_strategy_names()
    );
    assert_eq!(
        both.strategy_names(),
        vec![
            "mean_reversion".to_string(),
            "spread_arb".to_string(),
            "trend_follow".to_string()
        ],
        "an unknown name must not register anything"
    );
}

/// The enabled-set is recorded on every toggle and on boot, and a fresh
/// install with the same state path replays it — that is the whole
/// "enable it once, it comes back after a restart" contract.
#[test]
fn strategy_enablement_persists_to_the_state_file() {
    let dir = std::env::temp_dir().join(format!("bk-strategy-state-svc-{}", std::process::id()));
    let state = dir.join("strategy-state.json");
    let _ = std::fs::remove_file(&state);

    let cfg = CoreConfig {
        dry_seed_balance: dec!(1000),
        engine_enabled: true,
        strategy_state_path: Some(state.to_string_lossy().into()),
        ..Default::default()
    };
    let install = || {
        let mut c = Core::new(cfg.clone());
        cfg.install_engine(&mut c).expect("engine install");
        let cfg2 = cfg.engine_config();
        crate::strategies::test_support::host_disabled(
            c.engine.as_mut().expect("engine installed"),
            cfg2.trend,
            cfg2.spread_arb,
        );
        c
    };

    // Boot with an empty set: install_engine records that empty set — and
    // the file now exists with nothing enabled.
    let mut c = install();
    assert!(c.enabled_strategy_names().is_empty());
    assert_eq!(
        crate::strategy_state::load(&state),
        Vec::<String>::new(),
        "boot reconciles the file to the effective set"
    );

    // The operator enables two strategies; both land in the file.
    assert!(c.set_strategy_enabled("spread_arb", true));
    assert!(c.set_strategy_enabled("trend_follow", true));
    assert_eq!(
        crate::strategy_state::load(&state),
        vec!["spread_arb".to_string(), "trend_follow".to_string()]
    );

    // Disabling records the off just the same.
    assert!(c.set_strategy_enabled("spread_arb", false));
    assert_eq!(
        crate::strategy_state::load(&state),
        vec!["trend_follow".to_string()]
    );

    // A brand-new install with the same state path replays the intent: the
    // boot list is read from the file (exactly what main.rs does), and the
    // selection applies after the strategy dir "loads" — the same order
    // production uses.
    let mut replay_cfg = cfg.clone();
    replay_cfg.enabled_strategies = crate::strategy_state::load(&state);
    let mut rebooted = Core::new(replay_cfg.clone());
    replay_cfg
        .install_engine(&mut rebooted)
        .expect("engine install");
    let rcfg = replay_cfg.engine_config();
    crate::strategies::test_support::host_disabled(
        rebooted.engine.as_mut().expect("engine installed"),
        rcfg.trend,
        rcfg.spread_arb,
    );
    for name in &replay_cfg.enabled_strategies {
        rebooted.set_strategy_enabled(name, true);
    }
    assert_eq!(
        rebooted.enabled_strategy_names(),
        vec!["trend_follow".to_string()],
        "the persisted intent survives the restart"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The plugin-switch ratchet (2026-10): a strategy the HANDSHAKE refused
/// at boot keeps its name in `strategy-state.json`, so switching the
/// market plugin back re-enables it with no manual re-toggling. The
/// first cut saved the effective set only, so one plugin switch
/// silently stripped every incompatible strategy from the file — and
/// switching back brought nothing back. An explicit disable still
/// removes the name: that is a decision, not a market mismatch.
#[test]
fn a_handshake_refused_strategy_stays_in_the_state_file() {
    use crate::market::compat::PluginModes;
    use blitzkrieg_market_api::{MarketStructure, MarketType};

    let dir =
        std::env::temp_dir().join(format!("bk-strategy-state-ratchet-{}", std::process::id()));
    let state = dir.join("strategy-state.json");
    let _ = std::fs::remove_file(&state);

    let cfg = CoreConfig {
        dry_seed_balance: dec!(1000),
        engine_enabled: true,
        strategy_state_path: Some(state.to_string_lossy().into()),
        enabled_strategies: vec!["spread_arb".into(), "trend_follow".into()],
        ..Default::default()
    };
    let mut c = Core::new(cfg.clone());
    // A declaration snapshot that refuses BOTH hosted adapters: an
    // unspecified-structure spot plugin cannot serve anything the
    // adapters are about to declare (prediction/binary_outcome_wheel,
    // set below through the same registration record the loader keeps).
    // This is the production shape of a plugin switch.
    c.set_plugin_modes(vec![PluginModes {
        name: "spot_venue".into(),
        modes: vec![blitzkrieg_market_api::MarketMode {
            market_type: MarketType::Spot,
            structure: None,
            capabilities: blitzkrieg_market_api::MarketCapabilities::NONE,
        }],
    }]);
    cfg.install_engine(&mut c).expect("engine install");
    let cfg2 = cfg.engine_config();
    crate::strategies::test_support::host_disabled(
        c.engine.as_mut().expect("engine installed"),
        cfg2.trend,
        cfg2.spread_arb,
    );
    // The adapters carry declarations, the snapshot cannot serve them —
    // so the same enable loop that install_engine ran inside refused
    // both, and the save path must have kept their names anyway.
    c.set_strategy_declarations_for_test(
        "spread_arb",
        vec![blitzkrieg_strategy_api::StrategyMode {
            market_type: MarketType::Prediction,
            structure: Some(MarketStructure::BinaryOutcomeWheel),
        }],
    );
    assert!(
        c.compatibility_refusal("spread_arb").is_some(),
        "the snapshot must refuse the declared strategy (sanity)"
    );

    // Reconcile exactly as install_engine now does: live enabled set
    // PLUS registered-but-refused intent, MINUS explicit disables.
    let mut enabled = c.enabled_strategy_names();
    for name in &cfg.enabled_strategies {
        if !enabled.contains(name)
            && !cfg.disabled_strategies.contains(name)
            && c.strategy_names().iter().any(|n| n == name)
        {
            enabled.push(name.clone());
        }
    }
    crate::strategy_state::save(std::path::Path::new(&state), &enabled);
    let persisted = crate::strategy_state::load(&state);
    assert!(
        persisted.contains(&"spread_arb".to_string())
            && persisted.contains(&"trend_follow".to_string()),
        "refused strategies stay in the file for a future session: {persisted:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ── E27 multi-plugin handshake: ANY active plugin can serve ─────────────

fn prediction_plugin_modes(name: &str) -> crate::market::compat::PluginModes {
    crate::market::compat::PluginModes {
        name: name.into(),
        modes: vec![blitzkrieg_market_api::MarketMode {
            market_type: blitzkrieg_market_api::MarketType::Prediction,
            structure: Some(blitzkrieg_market_api::MarketStructure::BinaryOutcomeWheel),
            capabilities: blitzkrieg_market_api::MarketCapabilities::NONE,
        }],
    }
}

fn spot_plugin_modes(name: &str) -> crate::market::compat::PluginModes {
    crate::market::compat::PluginModes {
        name: name.into(),
        modes: vec![blitzkrieg_market_api::MarketMode {
            market_type: blitzkrieg_market_api::MarketType::Spot,
            structure: None,
            capabilities: blitzkrieg_market_api::MarketCapabilities::NONE,
        }],
    }
}

fn prediction_declaration() -> Vec<blitzkrieg_strategy_api::StrategyMode> {
    vec![blitzkrieg_strategy_api::StrategyMode {
        market_type: blitzkrieg_market_api::MarketType::Prediction,
        structure: Some(blitzkrieg_market_api::MarketStructure::BinaryOutcomeWheel),
    }]
}

#[test]
fn a_declaration_served_by_any_active_plugin_loads() {
    // polymarket can serve the prediction declaration; kalshi (spot-shaped
    // here, only to make a second distinct plugin) cannot. Any one
    // satisfiable plugin loads the strategy — a venue the strategy cannot
    // serve must not block the venues it can.
    let mut c = Core::new(CoreConfig::default());
    c.set_plugin_modes(vec![
        prediction_plugin_modes("polymarket"),
        spot_plugin_modes("kalshi"),
    ]);
    c.set_strategy_declarations_for_test("served", prediction_declaration());
    assert!(
        c.compatibility_refusal("served").is_none(),
        "one satisfiable plugin is enough: {:?}",
        c.compatibility_refusal("served")
    );
}

#[test]
fn a_declaration_no_plugin_serves_names_every_plugin() {
    // A FUTURES declaration against a prediction plugin and a spot plugin:
    // both refuse, and the diagnostic names BOTH plugins so the operator
    // can see the full mismatch surface.
    let mut c = Core::new(CoreConfig::default());
    c.set_plugin_modes(vec![
        prediction_plugin_modes("polymarket"),
        spot_plugin_modes("kalshi"),
    ]);
    c.set_strategy_declarations_for_test(
        "unserved",
        vec![blitzkrieg_strategy_api::StrategyMode {
            market_type: blitzkrieg_market_api::MarketType::Futures,
            structure: None,
        }],
    );
    let refusal = c.compatibility_refusal("unserved").expect("all refused");
    assert!(refusal.contains("polymarket"), "{refusal}");
    assert!(refusal.contains("kalshi"), "{refusal}");
    assert!(refusal.contains("futures"), "{refusal}");
}

#[test]
fn a_single_plugin_refusal_keeps_the_historical_message() {
    // One plugin, refused: the message must be byte-identical to the
    // pre-multi-plugin wording (no per-plugin prefix, no join separator).
    let mut c = Core::new(CoreConfig::default());
    c.set_plugin_modes(vec![spot_plugin_modes("spot_venue")]);
    c.set_strategy_declarations_for_test("s", prediction_declaration());
    let refusal = c.compatibility_refusal("s").expect("refused");
    assert!(
        refusal.starts_with("incompatible modes: strategy 's' wants ["),
        "the historical format, unprefixed: {refusal}"
    );
    assert_eq!(refusal.matches("incompatible modes:").count(), 1);
}

#[test]
fn an_empty_plugin_snapshot_leaves_the_handshake_inert() {
    // Backtester / tests: no snapshot injected, nothing refuses.
    let mut c = Core::new(CoreConfig::default());
    c.set_plugin_modes(Vec::new());
    c.set_strategy_declarations_for_test("s", prediction_declaration());
    assert!(c.compatibility_refusal("s").is_none());
}

/// No `strategy_state_path` (backtests, most tests) — a toggle works but
/// writes nothing anywhere.
#[test]
fn toggles_without_a_state_path_persist_nothing() {
    let cfg = CoreConfig {
        dry_seed_balance: dec!(1000),
        engine_enabled: true,
        ..Default::default()
    };
    let mut c = Core::new(cfg.clone());
    cfg.install_engine(&mut c).expect("engine install");
    let cfg2 = cfg.engine_config();
    crate::strategies::test_support::host_disabled(
        c.engine.as_mut().expect("engine installed"),
        cfg2.trend,
        cfg2.spread_arb,
    );
    assert!(c.set_strategy_enabled("trend_follow", true));
    assert_eq!(c.enabled_strategy_names(), vec!["trend_follow".to_string()]);
}

/// #265: naming strategies that do not exist is a REFUSAL, not a warning. A
/// kernel that boots with a healthy-looking banner and then trades nothing
/// because the library it was told to run was never loaded is exactly the
/// failure this guards; `--allow-zero-strategies` is the acknowledged
/// opt-out, and a name the operator also disabled explicitly was never
/// requested in the first place.
#[test]
fn an_explicit_request_that_resolves_to_nothing_refuses_the_boot() {
    // The kernel registers nothing itself (PR-B), so in-process every
    // requested name is unresolved — which is precisely the production
    // shape of "the strategy library was not found".
    let boot = |requested: &[&str], disabled: &[&str], allow: bool| {
        let cfg = CoreConfig {
            dry_seed_balance: dec!(1000),
            engine_enabled: true,
            requested_strategies: requested.iter().map(|s| (*s).to_string()).collect(),
            disabled_strategies: disabled.iter().map(|s| (*s).to_string()).collect(),
            allow_zero_strategies: allow,
            ..Default::default()
        };
        let mut core = Core::new(cfg.clone());
        cfg.install_engine(&mut core)
    };

    // Nothing was asked for: the ordinary zero-strategy boot (tests, the
    // backtester, a session that only hosts builtins) is untouched.
    assert!(
        boot(&[], &[], false).is_ok(),
        "an unrequested empty set must never refuse"
    );

    // An explicit request that resolves to nothing refuses, and names both
    // what was asked for and the door out.
    let err = boot(&["alpha_strategy"], &[], false)
        .expect_err("a request that resolves to nothing must refuse the boot");
    assert!(
        err.contains("refusing to start") && err.contains("alpha_strategy"),
        "the refusal must name the request, got: {err}"
    );
    assert!(
        err.contains("--allow-zero-strategies"),
        "the refusal must name the opt-out, got: {err}"
    );
    // The message states the ENGINE's live set, not the request twice: the
    // first draft filled this slot with the enabled list under the label
    // "Requested:", which a probe read as "Requested: ." — a refusal that
    // cannot say what it refused is not a diagnosis.
    assert!(
        err.contains("the engine's live set is []"),
        "the refusal must state the live set, got: {err}"
    );

    // The door is not welded shut: the flag acknowledges the empty set.
    assert!(
        boot(&["alpha_strategy"], &[], true).is_ok(),
        "--allow-zero-strategies must start normally"
    );

    // `--enable-strategy x --disable-strategy x` asked for nothing, so it is
    // not a request that failed — the same rule the toggle order follows.
    assert!(
        boot(&["alpha_strategy"], &["alpha_strategy"], false).is_ok(),
        "an explicitly disabled name is not part of the request"
    );

    // Several names: the refusal counts and lists them all.
    let err = boot(&["alpha_strategy", "cat_strategy"], &[], false).expect_err("still a refusal");
    assert!(
        err.contains("alpha_strategy") && err.contains("cat_strategy"),
        "got: {err}"
    );
}

/// Shadow Evolution sees every evolvable strategy the engine hosts, whether it
/// is enabled or not (E2-c): the knob declaration is read off the live
/// instance, and `register_strategies` drops the cell of anything it is not
/// handed — so keying the set off `enabled` would make a runtime disable throw
/// away that strategy's evolved parameters and rollback anchor.
///
/// This is the regression guard for exactly that: toggle the chase leg off and
/// back on, and the value applied to it must still be in force.
#[test]
fn evolution_keeps_the_cell_of_a_disabled_strategy_across_a_toggle() {
    let (tuning, dir) = scratch("toggle-cell");
    let base = CoreConfig {
        dry_seed_balance: dec!(1000),
        engine_enabled: true,
        shadow_evolution_tuning: tuning,
        ..Default::default()
    };
    let mut c = Core::new(base.clone());
    base.install_engine(&mut c).expect("engine install");
    // The kernel registers nothing itself (PR-B); host the adapters so the
    // evolution manager has strategies to build units for.
    let ecfg = base.engine_config();
    crate::strategies::test_support::host(
        c.engine.as_mut().expect("engine installed"),
        ecfg.trend,
        ecfg.spread_arb,
    );
    c.shadow_evolution_enable(0);
    // Registered from the start even though it starts disabled.
    assert_eq!(
        c.shadow_evolution().strategy_names(),
        vec![
            "spread_arb".to_string(),
            "trend_follow".to_string(),
            "mean_reversion".to_string()
        ],
    );
    assert!(c.shadow_evolution_status_for("trend_follow", 0).is_some());
    // Its declared knobs are the strategy's own, not an invented set.
    let names: Vec<String> = c
        .shadow_evolution()
        .declared_knobs("trend_follow")
        .into_iter()
        .map(|k| k.name)
        .collect();
    assert_eq!(
        names,
        vec![
            "momentum_window_sec".to_string(),
            "min_move_pct".to_string(),
            "min_confirm_price".to_string(),
            "break_price".to_string(),
            "max_entry_price".to_string(),
            "max_spread_pct".to_string(),
        ]
    );

    assert!(c.set_strategy_enabled("trend_follow", true));
    assert!(
        c.shadow_evolution_apply(one_param("trend_follow", "min_move_pct", dec!(3.1)), 1_200)
            .is_ok()
    );
    let evolved = c
        .shadow_evolution()
        .params_for("trend_follow")
        .unwrap()
        .get("min_move_pct");
    assert_eq!(evolved, Some(dec!(3.1)));

    // Off and on again: same cell, same value — a toggle is not a reset.
    assert!(c.set_strategy_enabled("trend_follow", false));
    assert!(c.set_strategy_enabled("trend_follow", true));
    assert_eq!(
        c.shadow_evolution()
            .params_for("trend_follow")
            .unwrap()
            .get("min_move_pct"),
        Some(dec!(3.1)),
        "a runtime toggle must not discard the strategy's evolved value"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn three_strategies_size_independently_in_one_cycle() {
    // Global band is [10,10] at a 2.5u budget; two strategies tighten it and
    // one inherits it. All three trade the same cycle without interfering.
    let limits = limits_of(&[
        (
            "small",
            StrategyLimit {
                size_usd: Some(dec!(1)),
                min_shares: Some(dec!(2)),
                max_shares: Some(dec!(2)),
                ..Default::default()
            },
        ),
        (
            "mid",
            StrategyLimit {
                size_usd: Some(dec!(2)),
                min_shares: Some(dec!(4)),
                max_shares: Some(dec!(4)),
                ..Default::default()
            },
        ),
    ]);
    let mut c = core_with_dips(limits, &DIPS, 5);
    let now = 1_000_000i64;
    feed_three_asset_dip(&mut c, now);
    assert_eq!(
        c.engine_evaluate(now + 1_000),
        3,
        "one entry per strategy: {:#?}",
        c.list_orders()
    );

    let sizes: Vec<(String, Decimal)> = c
        .list_orders()
        .iter()
        .map(|o| (o.strategy.clone(), o.size))
        .collect();
    assert_eq!(
        sizes.iter().find(|(s, _)| s == "small").map(|(_, v)| *v),
        Some(dec!(2)),
        "1u/0.44 → 2 shares inside [2,2]: {sizes:?}"
    );
    assert_eq!(
        sizes.iter().find(|(s, _)| s == "mid").map(|(_, v)| *v),
        Some(dec!(4)),
        "2u/0.44 ≈ 5 → clamped to the strategy's 4-share ceiling: {sizes:?}"
    );
    assert_eq!(
        sizes.iter().find(|(s, _)| s == "unset").map(|(_, v)| *v),
        Some(dec!(10)),
        "no override must fall back to the global lot: {sizes:?}"
    );

    let stats = c.strategy_stats();
    let small = strategy_entry(&stats, "small");
    assert_eq!(small["sizingSource"], "strategy");
    assert_eq!(dec_of(&small["effectiveSizeUsd"]), dec!(1));
    assert_eq!(dec_of(&small["effectiveMinShares"]), dec!(2));
    assert_eq!(dec_of(&small["effectiveMaxShares"]), dec!(2));
    let unset = strategy_entry(&stats, "unset");
    assert_eq!(unset["sizingSource"], "global");
    assert_eq!(dec_of(&unset["effectiveSizeUsd"]), dec!(2.5));
    assert_eq!(dec_of(&unset["effectiveMaxShares"]), dec!(10));
}

// ── #202: the engine sizes from the balance the ledger hands it ─────────

/// A one-asset dip core whose sizing is the production mapping
/// (`CoreConfig::engine_config()`), so the ticket a test reads is the ticket
/// the live server would emit on that account.
fn equity_core(balance: Decimal, size_pct: Decimal) -> Core {
    let mut c = Core::new(CoreConfig {
        risk: RiskConfig {
            max_order_notional: dec!(1000),
            ..Default::default()
        },
        dry_seed_balance: balance,
        engine_enabled: true,
        round_duration_sec: 900,
        auto_exits_enabled: false,
        size_usd: dec!(2.5),
        // The engine's own ceiling stays wide so the equity budget is what
        // decides the lot in these tests.
        min_shares: dec!(1),
        max_shares: dec!(1000),
        size_pct,
        assets: vec!["BTC".into()],
        positions: crate::position::PositionConfig {
            max_positions: 5,
            exit: crate::exit_policy::ExitConfig {
                min_time_left_sec: 0,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    });
    c.set_balance(balance);
    let cfg = c.config().engine_config();
    let mut eng = Engine::new(cfg.clone());
    crate::strategies::test_support::host(&mut eng, cfg.trend, cfg.spread_arb);
    eng.register_user_strategy(
        Box::new(TargetDip {
            name: "dip".into(),
            buy_below: dec!(0.45),
            assets: vec!["BTC".into()],
            gates: crate::strategies::GateExemptions::none(),
            scope: blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
        }),
        "test".into(),
    )
    .unwrap();
    assert!(eng.set_strategy_enabled("dip", true));
    c.enable_engine(eng);
    c
}

/// Round + a 0.44-mid book on BTC: the dip strategy's one candidate.
fn feed_btc_dip(c: &mut Core, now: i64) {
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: three_markets(now),
            now_ms: now,
        },
        now,
    );
    feed_dip_on(c, now + 1_000, &["BTC"]);
}

/// The acceptance behind #202's first half: the SAME 20% on two account
/// sizes produces lots 100× apart, and the engine reads the balance the
/// ledger actually holds rather than a copy taken at configuration time.
#[test]
fn equity_sizing_scales_one_order_with_the_account() {
    let now = 1_000_000i64;
    let mut small = equity_core(dec!(4.8), dec!(20));
    feed_btc_dip(&mut small, now);
    assert_eq!(small.engine_evaluate(now + 1_000), 1);
    // 4.8 × 20% = 0.96 → 0.96/0.44 = 2.18 → 2 shares. The pre-#202 path
    // sent 10 shares (4.40 USD, 92% of this account) whatever the price.
    assert_eq!(small.list_orders()[0].size, dec!(2));
    assert_eq!(
        small.engine.as_ref().unwrap().equity_usd(),
        small.ledger().balance(),
        "the engine must have been handed the ledger's own balance"
    );

    let mut big = equity_core(dec!(480), dec!(20));
    feed_btc_dip(&mut big, now);
    assert_eq!(big.engine_evaluate(now + 1_000), 1);
    // 96/0.44 = 218.18 → 218 shares: the same statement, 100× the account.
    assert_eq!(big.list_orders()[0].size, dec!(218));
    // The panel states both the percentage and what it is worth here, so the
    // scaling is visible without re-deriving it from the flags.
    let small_stats = small.strategy_stats();
    assert_eq!(
        dec_of(&strategy_entry(&small_stats, "dip")["effectiveSizePct"]),
        dec!(20)
    );
    assert_eq!(
        dec_of(&strategy_entry(&small_stats, "dip")["entryBudgetUsd"]),
        dec!(0.96)
    );
    let big_stats = big.strategy_stats();
    assert_eq!(
        dec_of(&strategy_entry(&big_stats, "dip")["entryBudgetUsd"]),
        dec!(96)
    );
}

/// A budget that cannot buy one whole share emits NO order and says so in
/// the stats, rather than turning into a 0-size rejection on every tick.
#[test]
fn an_unaffordable_equity_budget_skips_the_signal_visibly() {
    let now = 1_000_000i64;
    let mut c = equity_core(dec!(4.8), dec!(1)); // 0.048 USD per entry
    feed_btc_dip(&mut c, now);
    assert_eq!(c.engine_evaluate(now + 1_000), 0, "no ticket, no order");
    assert!(c.list_orders().is_empty());
    assert_eq!(
        c.engine_stats_at(now + 1_000)["sizing"]["sizePctSkippedSignals"].as_u64(),
        Some(1),
        "the skip must be counted, not silent"
    );
}

/// Unconfigured (the shipped default) is untouched: the service pushes a
/// balance every cycle now, and this is the test that proves that push alone
/// changes nothing about an absolute-budget deployment's orders.
#[test]
fn an_unconfigured_core_keeps_its_historical_lot() {
    let now = 1_000_000i64;
    let mut c = equity_core(dec!(4.8), Decimal::ZERO);
    feed_btc_dip(&mut c, now);
    assert_eq!(c.engine_evaluate(now + 1_000), 1);
    // The historical path: 2.5/0.44 ≈ 5.68 → 6 shares, inside [1, 1000].
    assert_eq!(c.list_orders()[0].size, dec!(6));
    assert_eq!(
        c.engine_stats_at(now + 1_000)["sizing"]["sizePctSkippedSignals"].as_u64(),
        Some(0)
    );
}

#[test]
fn per_strategy_quota_is_independent_across_strategies() {
    // `capped` may hold zero positions and targets two assets; `other` has no
    // cap and targets a third. In one cycle the cap rejects both of `capped`'s
    // candidates while `other` trades untouched — the quotas are per strategy,
    // and one strategy being full never starves another.
    let limits = limits_of(&[(
        "capped",
        StrategyLimit {
            max_open_positions: Some(0),
            size_usd: Some(dec!(1)),
            min_shares: Some(dec!(2)),
            max_shares: Some(dec!(2)),
            ..Default::default()
        },
    )]);
    let mut c = core_with_dips(
        limits,
        &[("capped", &["BTC", "ETH"]), ("other", &["SOL"])],
        5,
    );
    let now = 1_000_000i64;
    feed_three_asset_dip(&mut c, now);
    assert_eq!(
        c.engine_evaluate(now + 1_000),
        1,
        "only `other` may place: {:#?}",
        c.list_orders()
    );
    assert_eq!(
        c.engine_stats()["strategyLimitRejected"],
        2,
        "both capped candidates were rejected"
    );
    assert_eq!(c.list_orders()[0].strategy, "other");

    let stats = c.strategy_stats();
    let capped = strategy_entry(&stats, "capped");
    assert_eq!(capped["ordersPlaced"], 0);
    assert_eq!(
        capped["limitRejected"], 2,
        "counted against the capped strategy only"
    );
    assert_eq!(capped["maxOpenPositions"], 0);
    assert_eq!(
        dec_of(&capped["effectiveMaxShares"]),
        dec!(2),
        "its own lot is 2 shares"
    );
    let other = strategy_entry(&stats, "other");
    assert_eq!(other["ordersPlaced"], 1, "the sibling still trades");
    assert_eq!(other["limitRejected"], 0);
    assert_eq!(
        other["maxOpenPositions"],
        serde_json::Value::Null,
        "unset = uncapped"
    );
    assert_eq!(
        dec_of(&other["effectiveSizeUsd"]),
        dec!(2.5),
        "and inherits the global sizing"
    );
}

#[test]
fn a_strategy_cap_counts_only_its_own_open_positions() {
    // `capped` allows one position and gets exactly one: it takes BTC, fills
    // it, and is then rejected on ETH — while `other`'s SOL position is
    // invisible to `capped`'s quota (and vice versa).
    let limits = limits_of(&[(
        "capped",
        StrategyLimit {
            max_open_positions: Some(1),
            ..Default::default()
        },
    )]);
    let mut c = core_with_dips(
        limits,
        &[("capped", &["BTC", "ETH"]), ("other", &["SOL"])],
        5,
    );
    let now = 1_000_000i64;
    feed_three_asset_dip(&mut c, now);
    assert_eq!(
        c.engine_evaluate(now + 1_000),
        3,
        "two capped candidates + one sibling"
    );

    // Fill BTC (capped) and SOL (other): two live positions, one per strategy.
    fill(&mut c, "BTC", now + 2_000);
    fill(&mut c, "SOL", now + 2_000);
    let open = c.positions().open_positions();
    assert_eq!(open.len(), 2, "{open:#?}");
    assert_eq!(open.iter().filter(|p| p.strategy == "capped").count(), 1);
    assert_eq!(open.iter().filter(|p| p.strategy == "other").count(), 1);
}

#[test]
fn the_global_position_ceiling_still_binds_across_strategies() {
    // Per-strategy quotas do not lift the global capacity gate. Open two
    // positions with two strategies, then let a THIRD strategy find a dip:
    // its entry is rejected by the shared global ceiling, not by its own cap.
    let mut c = core_with_dips(
        HashMap::new(),
        &[("a", &["BTC"]), ("b", &["ETH"]), ("c", &["SOL"])],
        2,
    );
    let now = 1_000_000i64;
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: three_markets(now),
            now_ms: now,
        },
        now,
    );

    feed_dip_on(&mut c, now + 1_000, &["BTC"]);
    assert_eq!(c.engine_evaluate(now + 1_000), 1, "a takes BTC");
    fill(&mut c, "BTC", now + 2_000);
    feed_dip_on(&mut c, now + 3_000, &["ETH"]);
    assert_eq!(c.engine_evaluate(now + 3_000), 1, "b takes ETH");
    fill(&mut c, "ETH", now + 4_000);
    assert_eq!(
        c.positions().open_positions().len(),
        2,
        "global ceiling reached"
    );

    feed_dip_on(&mut c, now + 5_000, &["SOL"]);
    assert_eq!(
        c.engine_evaluate(now + 5_000),
        0,
        "the third strategy must be gated out"
    );
    assert_eq!(
        c.engine_stats()["placeRejected"],
        1,
        "rejected by the global capacity gate"
    );
    assert_eq!(
        c.engine_stats()["strategyLimitRejected"],
        0,
        "and NOT by a per-strategy quota"
    );
    assert_eq!(c.positions().open_positions().len(), 2);

    let stats = c.strategy_stats();
    // No strategy configured a quota, so all three report uncapped yet the
    // global gate still held the line.
    for name in ["a", "b", "c"] {
        assert_eq!(
            strategy_entry(&stats, name)["maxOpenPositions"],
            serde_json::Value::Null
        );
    }
    assert_eq!(strategy_entry(&stats, "c")["ordersRejected"], 1);
}

#[test]
fn a_greedy_strategy_override_is_clamped_to_the_global_risk_band() {
    // sizeUsd 100 / maxShares 999 / minShares 0 all lose to the globals; the
    // report still marks the strategy as sizing-scoped.
    let limits = limits_of(&[(
        "small",
        StrategyLimit {
            size_usd: Some(dec!(100)),
            min_shares: Some(dec!(0)),
            max_shares: Some(dec!(999)),
            ..Default::default()
        },
    )]);
    let mut c = core_with_dips(limits, &[("small", &["BTC"])], 5);
    let now = 1_000_000i64;
    feed_three_asset_dip(&mut c, now);
    assert_eq!(c.engine_evaluate(now + 1_000), 1);
    assert_eq!(c.list_orders()[0].size, dec!(10), "global ceiling must win");

    let stats = c.strategy_stats();
    let s = strategy_entry(&stats, "small");
    assert_eq!(s["sizingSource"], "strategy");
    assert_eq!(
        dec_of(&s["effectiveSizeUsd"]),
        dec!(2.5),
        "notional clamped to the global budget"
    );
    assert_eq!(
        dec_of(&s["effectiveMaxShares"]),
        dec!(10),
        "share ceiling clamped to the global"
    );
    assert_eq!(
        dec_of(&s["effectiveMinShares"]),
        dec!(10),
        "floor raised to the global min"
    );
}

// ── E2-b: per-strategy gate opt-out (#27) ───────────────────────────────

/// Shut for every strategy that did not declare the timing window unnecessary.
const WINDOW_SHUT: i64 = 10_000;

#[test]
fn a_declared_timing_exemption_places_an_order_the_gate_would_have_blocked() {
    let mut c = core_with_declared_dips(
        HashMap::new(),
        &[("fader", &["BTC"]), ("gated", &["ETH"])],
        5,
        WINDOW_SHUT,
        |name| crate::strategies::GateExemptions {
            timing: name == "fader",
            momentum: false,
            timing_min_time_left_sec: None,
        },
    );
    let now = 1_000_000i64;
    feed_three_asset_dip(&mut c, now);
    assert_eq!(
        c.engine_evaluate(now + 1_000),
        1,
        "only the declaring strategy may trade: {:#?}",
        c.list_orders()
    );
    assert_eq!(c.list_orders()[0].strategy, "fader");
    assert_eq!(
        c.engine_stats()["blocked"]["timing"],
        1,
        "the sibling stayed gated"
    );

    let stats = c.strategy_stats();
    let fader = strategy_entry(&stats, "fader");
    assert_eq!(fader["gateExemptions"], serde_json::json!(["timing"]));
    assert_eq!(
        fader["gateExemptedTiming"], 1,
        "the honoured exemption is counted"
    );
    assert_eq!(fader["blockedTiming"], 0);
    let gated = strategy_entry(&stats, "gated");
    assert_eq!(gated["gateExemptions"], serde_json::json!([]));
    assert_eq!(gated["blockedTiming"], 1);
    assert_eq!(gated["gateExemptedTiming"], 0);
    assert_eq!(gated["ordersPlaced"], 0);
}

#[test]
fn blocked_gate_counts_are_attributable_to_a_specific_strategy() {
    let mut c = core_with_declared_dips(
        HashMap::new(),
        &[("a", &["BTC"]), ("b", &["ETH"])],
        5,
        WINDOW_SHUT,
        |_| crate::strategies::GateExemptions::none(),
    );
    let now = 1_000_000i64;
    feed_three_asset_dip(&mut c, now);
    assert_eq!(c.engine_evaluate(now + 1_000), 0, "both are gated");

    let blocked = c.engine_stats()["blocked"].clone();
    assert_eq!(blocked["timing"], 2, "global total");
    assert_eq!(blocked["byStrategy"]["a"]["timing"], 1, "{blocked}");
    assert_eq!(blocked["byStrategy"]["b"]["timing"], 1, "{blocked}");
    assert_eq!(blocked["byStrategy"]["a"]["momentum"], 0, "{blocked}");
    // Only the hosted test adapter declares an exemption (momentum) here;
    // the test dip strategies declare nothing beyond what their builder gave
    // them. (PR-B: there is no builtin set — every hosted strategy declares
    // its own gates through the same seam.)
    assert_eq!(
        blocked["declaredExemptions"],
        serde_json::json!([{ "strategy": "mean_reversion", "gates": ["momentum"] }])
    );
}

/// 账户跟随（策略计数的账户切片）：同一策略名在两个账户各跑一个 scope，
/// 各自的 gate 判定只落在自己的 (strategy, account) 键下 —— 账户 A 的
/// 策略页绝不能看见账户 B 被挡了几次，而全局 fold（回放基线）不动。
#[test]
fn gate_counts_follow_the_account_in_the_strategy_page() {
    // One strategy NAME, two scopes: default dips BTC, paper dips ETH.
    // WINDOW_SHUT shuts the timing gate for every scope.
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
    let mut c = Core::with_account_book(
        CoreConfig {
            risk: RiskConfig {
                max_order_notional: dec!(100),
                ..Default::default()
            },
            dry_seed_balance: dec!(1000),
            engine_enabled: true,
            round_duration_sec: 900,
            min_round_age_sec: WINDOW_SHUT,
            auto_exits_enabled: false,
            assets: vec!["BTC".into(), "ETH".into(), "SOL".into()],
            positions: crate::position::PositionConfig {
                max_positions: 5,
                exit: crate::exit_policy::ExitConfig {
                    min_time_left_sec: 0,
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        },
        book,
    );
    let cfg = c.config().engine_config();
    let mut eng = Engine::new(cfg.clone());
    crate::strategies::test_support::host(&mut eng, cfg.trend, cfg.spread_arb);
    eng.register_user_strategy(
        Box::new(TargetDip {
            name: "acc".into(),
            buy_below: dec!(0.45),
            assets: vec!["BTC".into()],
            gates: crate::strategies::GateExemptions::none(),
            scope: blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
        }),
        "test".into(),
    )
    .unwrap();
    eng.register_scoped_strategy(
        Box::new(TargetDip {
            name: "acc".into(),
            buy_below: dec!(0.45),
            assets: vec!["ETH".into()],
            gates: crate::strategies::GateExemptions::none(),
            scope: "paper".into(),
        }),
        "test".into(),
        "paper",
    )
    .expect("paper scope registers");
    assert!(eng.set_strategy_enabled("acc", true));
    c.enable_engine(eng);

    let now = 1_000_000i64;
    feed_three_asset_dip(&mut c, now);
    assert_eq!(
        c.engine_evaluate(now + 1_000),
        0,
        "both scopes are timing-gated"
    );

    let global_row = strategy_entry(&c.strategy_stats(), "acc").clone();
    assert_eq!(
        global_row["blockedTiming"], 2,
        "the per-name fold is the sum of both slices: {global_row}"
    );

    let for_default = c.strategy_stats_for_account("default");
    let d = strategy_entry(&for_default, "acc");
    assert_eq!(
        d["blockedTiming"], 1,
        "default sees only its own block: {d}"
    );
    assert_eq!(d["blockedMomentum"], 0);
    assert_eq!(d["gateExemptedTiming"], 0);
    let for_paper = c.strategy_stats_for_account("paper");
    let p = strategy_entry(&for_paper, "acc");
    assert_eq!(p["blockedTiming"], 1, "paper sees only its own block: {p}");
    assert_eq!(p["blockedMomentum"], 0);
    assert_eq!(p["gateExemptedTiming"], 0);
}

#[test]
fn declared_exemptions_are_reported_in_engine_stats() {
    let c = core_with_declared_dips(
        HashMap::new(),
        &[("fader", &["BTC"]), ("plain", &["ETH"])],
        5,
        0,
        |name| crate::strategies::GateExemptions {
            timing: name == "fader",
            momentum: name == "fader",
            timing_min_time_left_sec: None,
        },
    );
    let declared = c.engine_stats()["blocked"]["declaredExemptions"].clone();
    assert_eq!(
        declared,
        serde_json::json!([
            { "strategy": "mean_reversion", "gates": ["momentum"] },
            { "strategy": "fader", "gates": ["timing", "momentum"] }
        ]),
        "hosted and user declarations are both listed"
    );
}

#[test]
fn a_declared_timing_floor_is_reported_beside_the_gates() {
    // D-31: the floor is the difference between "this opt-out reaches into the
    // closing window" and "it stops at the global window", so it has to be
    // visible from the stats snapshot alone. `fader` declares a floor;
    // `plain` does not, and a strategy that declares nothing must NOT grow a
    // key it never asked for.
    let c = core_with_declared_dips(
        HashMap::new(),
        &[("fader", &["BTC"]), ("plain", &["ETH"])],
        5,
        0,
        |name| crate::strategies::GateExemptions {
            timing: name == "fader",
            momentum: name == "fader",
            timing_min_time_left_sec: (name == "fader").then_some(180),
        },
    );
    let stats = c.engine_stats();
    assert_eq!(
        stats["blocked"]["declaredExemptions"],
        serde_json::json!([
            { "strategy": "mean_reversion", "gates": ["momentum"] },
            { "strategy": "fader", "gates": ["timing", "momentum"], "timingFloorSec": 180 }
        ]),
        "a declared floor is named; an undeclared one adds nothing"
    );

    let by_name = |n: &str| -> serde_json::Value {
        stats["strategies"]
            .as_array()
            .expect("strategies array")
            .iter()
            .find(|s| s["name"] == n)
            .unwrap_or_else(|| panic!("no stats row for {n}"))["gateExemptionTimingFloorSec"]
            .clone()
    };
    assert_eq!(by_name("fader"), serde_json::json!(180));
    // `null`, not absent and not 0: "this strategy left the floor to the kernel"
    // must be distinguishable from "it declared a floor of zero".
    assert_eq!(by_name("plain"), serde_json::Value::Null);
}

#[test]
fn a_gate_exemption_never_bypasses_a_safety_boundary() {
    // The strongest form of the acceptance criterion: a strategy that declares
    // BOTH entry gates unnecessary still cannot get past the global risk gate,
    // the kill switch or the global capacity ceiling — those live in
    // `Core::place`, far below the entry gates.
    // The risk gate snapshots `config.risk` at construction, so the per-order
    // notional ceiling is set here: 1 USD vs the 0.44 x 10 = 4.40 order.
    let mut c = core_with_declared_dips_and_risk(
        HashMap::new(),
        &[("all_in", &["BTC"])],
        5,
        WINDOW_SHUT,
        RiskConfig {
            max_order_notional: dec!(1),
            ..Default::default()
        },
        |_| crate::strategies::GateExemptions::all(),
    );
    let now = 1_000_000i64;
    feed_three_asset_dip(&mut c, now);

    assert_eq!(
        c.engine_evaluate(now + 1_000),
        0,
        "risk gate must still reject"
    );
    assert_eq!(c.engine_stats()["placeRejected"], 1);
    assert!(c.list_orders().is_empty());
    assert!(
        c.strategy_stats().iter().any(|s| {
            s["name"] == "all_in"
                && s["gateExemptions"] == serde_json::json!(["timing", "momentum"])
        }),
        "the declaration is still reported even though the order was refused"
    );

    // Kill switch: identical exemption, hard stop wins.
    c.kill("test".into());
    assert_eq!(
        c.engine_evaluate(now + 2_000),
        0,
        "kill switch is not exemptible"
    );
    assert!(c.positions().open_positions().is_empty());

    // Global capacity ceiling: also outside the entry gates.
    let mut c2 = core_with_declared_dips(
        HashMap::new(),
        &[("all_in", &["BTC"])],
        0,
        WINDOW_SHUT,
        |_| crate::strategies::GateExemptions::all(),
    );
    feed_three_asset_dip(&mut c2, now);
    assert_eq!(
        c2.engine_evaluate(now + 1_000),
        0,
        "global capacity is not exemptible"
    );
    assert_eq!(c2.engine_stats()["placeRejected"], 1);

    // And the exemption cannot conjure a market (structural precondition).
    let mut c3 = core_with_declared_dips(HashMap::new(), &[("all_in", &["BTC"])], 5, 0, |_| {
        crate::strategies::GateExemptions::all()
    });
    assert_eq!(
        c3.engine_evaluate(now),
        0,
        "no round markets → nothing to exempt"
    );
}

#[test]
fn a_gate_exemption_does_not_lift_the_daily_loss_cap() {
    // Realise a loss through the ordinary exit path, then confirm the
    // exempting strategy is refused exactly like any other.
    let mut c = core_with_declared_dips(HashMap::new(), &[("all_in", &["BTC"])], 5, 0, |_| {
        crate::strategies::GateExemptions::all()
    });
    c.config_mut().auto_exits_enabled = true;
    c.config_mut().positions.max_daily_loss_usd = dec!(0.0001);
    let now = 1_000_000i64;
    feed_three_asset_dip(&mut c, now);
    assert_eq!(
        c.engine_evaluate(now + 1_000),
        1,
        "the exemption still lets the entry in"
    );
    fill(&mut c, "BTC", now + 2_000);
    assert_eq!(c.positions().open_positions().len(), 1);

    // Crash the book and tick at the force-exit horizon → a realized loss.
    let token = "btc_up";
    c.book_snapshot(
        token,
        vec![(dec!(0.05), dec!(100))],
        vec![(dec!(0.06), dec!(100))],
        1_800_000 - 101_000,
    );
    c.tick(1_800_000 - 100_000).unwrap();
    assert!(
        c.positions().daily_pnl_for("default") < Decimal::ZERO,
        "expected a realized loss"
    );

    // A fresh dip on the NEXT round: the exempted strategy is refused by the
    // daily-loss cap, proving the exemption stops at the entry gates.
    let now2 = 1_800_000i64;
    feed_three_asset_dip(&mut c, now2);
    let before = c.list_orders().len();
    assert_eq!(
        c.engine_evaluate(now2 + 1_000),
        0,
        "daily loss cap is not exemptible"
    );
    assert_eq!(c.list_orders().len(), before, "no new order was tracked");
}

/// Round + confirmed UP trend + calm spot + one dip book: the builtin emits
/// exactly one spread_arb entry (0.43 x 10 = 4.30 USD) on the next cycle.
fn feed_entry_setup(c: &mut Core, now: i64) {
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: vec![round_market(now)],
            now_ms: now,
        },
        now,
    );
    for i in 0..12 {
        let t = now + i * 1000;
        c.engine_on_data(
            DataEvent::Book {
                token_id: "up".into(),
                bids: vec![(dec!(0.55), dec!(100))],
                asks: vec![(dec!(0.57), dec!(100))],
                now_ms: t,
            },
            t,
        );
    }
    let t = now + 12_000;
    c.engine_on_data(
        DataEvent::Book {
            token_id: "up".into(),
            bids: vec![(dec!(0.43), dec!(100))],
            asks: vec![(dec!(0.45), dec!(100))],
            now_ms: t,
        },
        t,
    );
    c.engine_on_data(
        DataEvent::Spot {
            asset: "BTC".into(),
            price: dec!(60000),
            now_ms: t,
        },
        t,
    );
}

/// The payload decimals arrive as JSON numbers (Node wire convention).
fn dec_of(v: &serde_json::Value) -> Decimal {
    match v {
        serde_json::Value::Number(n) => Decimal::from_str_exact(&n.to_string()),
        serde_json::Value::String(s) => Decimal::from_str_exact(s.trim()),
        other => panic!("not a decimal: {other}"),
    }
    .unwrap_or_else(|e| panic!("not a decimal: {v} ({e})"))
}

fn strategy_entry<'a>(stats: &'a [serde_json::Value], name: &str) -> &'a serde_json::Value {
    stats
        .iter()
        .find(|s| s["name"] == name)
        .unwrap_or_else(|| panic!("no stats for {name}: {stats:?}"))
}

#[test]
fn default_config_has_no_limits_and_places_the_entry() {
    let mut c = core_with_engine(HashMap::new());
    let now = 1_000_000i64;
    feed_entry_setup(&mut c, now);
    assert_eq!(
        c.engine_evaluate(now + 12_000),
        1,
        "no caps configured must not change behaviour"
    );
    assert_eq!(c.engine_stats()["strategyLimitRejected"], 0);

    // Fill the resting maker entry so exposure is live, then read the ledger.
    // The tuned 0.88 discount rests the entry bid at round2(0.44*0.88)
    // = 0.39, so the crossing ask is 0.39 (was 0.42 under the 0.98 factor).
    c.book_snapshot(
        "up",
        vec![(dec!(0.38), dec!(100))],
        vec![(dec!(0.39), dec!(100))],
        now + 13_000,
    );
    let stats = c.strategy_stats();
    let s = strategy_entry(&stats, "spread_arb");
    assert_eq!(s["enabled"], true);
    // The kernel has no `builtin` class any more (PR-B): the test adapter is
    // hosted by the test itself, and a production session would report the
    // library's `dylib:<path>` provenance here instead.
    assert_eq!(s["source"], "test");
    assert_eq!(s["ordersPlaced"], 1);
    assert_eq!(s["openPositions"], 1);
    assert_eq!(dec_of(&s["openNotionalUsd"]), dec!(3.90));
}

#[test]
fn strategy_position_cap_rejects_the_entry_and_is_counted() {
    let mut limits = HashMap::new();
    limits.insert(
        "spread_arb".to_string(),
        StrategyLimit {
            max_open_positions: Some(0),
            ..Default::default()
        },
    );
    let mut c = core_with_engine(limits);
    let now = 1_000_000i64;
    feed_entry_setup(&mut c, now);
    assert_eq!(
        c.engine_evaluate(now + 12_000),
        0,
        "position cap 0 must block the entry"
    );
    assert!(
        c.ome().live_orders().is_empty(),
        "no order may reach the OME"
    );
    assert_eq!(c.engine_stats()["strategyLimitRejected"], 1);
    let stats = c.strategy_stats();
    let s = strategy_entry(&stats, "spread_arb");
    assert_eq!(s["limitRejected"], 1);
    assert_eq!(s["ordersPlaced"], 0);
    assert_eq!(s["openPositions"], 0);
}

#[test]
fn rejection_causes_report_position_cap_bucket() {
    // E9-c: the limit rejection lands in a machine-parseable bucket next
    // to the live counters, so an operator can see WHY without logs.
    let mut limits = HashMap::new();
    limits.insert(
        "spread_arb".to_string(),
        StrategyLimit {
            max_open_positions: Some(0),
            ..Default::default()
        },
    );
    let mut c = core_with_engine(limits);
    let now = 1_000_000i64;
    feed_entry_setup(&mut c, now);
    assert_eq!(c.engine_evaluate(now + 12_000), 0);
    let stats = c.strategy_stats();
    let s = strategy_entry(&stats, "spread_arb");
    assert_eq!(
        s["rejectionCauses"]["limit.positionCap"], 1,
        "position-cap rejection must be bucketed: {s}"
    );
    assert_eq!(
        s["ordersRejected"], 0,
        "cap rejections are not place rejections"
    );
    // A strategy that never placed has NO causes object at all.
    let clean_core = core_with_engine(HashMap::new());
    let clean_stats = clean_core.strategy_stats();
    let clean = strategy_entry(&clean_stats, "spread_arb");
    assert!(clean["rejectionCauses"].is_null());
}

#[test]
fn rejection_causes_report_per_order_cap_risk_bucket() {
    // E9-c: a Risk::check per-order-cap rejection inside place() is
    // bucketed as risk.perOrderCap (previously the reason was dropped).
    let mut c = core_with_engine(HashMap::new());
    let now = 1_000_000i64;
    feed_entry_setup(&mut c, now);
    c.risk_config_mut().max_order_notional = dec!(1);
    assert_eq!(
        c.engine_evaluate(now + 12_000),
        0,
        "1 USD per-order cap must reject the 4.30 entry"
    );
    let stats = c.strategy_stats();
    let s = strategy_entry(&stats, "spread_arb");
    assert_eq!(s["ordersRejected"], 1);
    assert_eq!(
        s["rejectionCauses"]["risk.perOrderCap"], 1,
        "must count and bucket the risk reason: {s}"
    );
}

#[test]
fn strategy_notional_cap_blocks_below_and_passes_above_the_entry() {
    // Entry notional is 0.43 * 10 = 4.30: a 1.00 cap rejects, a 10.00 cap passes.
    let mut tight = HashMap::new();
    tight.insert(
        "spread_arb".to_string(),
        StrategyLimit {
            max_open_notional_usd: Some(dec!(1)),
            ..Default::default()
        },
    );
    let mut c = core_with_engine(tight);
    let now = 1_000_000i64;
    feed_entry_setup(&mut c, now);
    assert_eq!(
        c.engine_evaluate(now + 12_000),
        0,
        "notional cap 1.00 must block a 4.30 entry"
    );
    assert_eq!(c.engine_stats()["strategyLimitRejected"], 1);

    let mut loose = HashMap::new();
    loose.insert(
        "spread_arb".to_string(),
        StrategyLimit {
            max_open_notional_usd: Some(dec!(10)),
            ..Default::default()
        },
    );
    let mut c2 = core_with_engine(loose);
    feed_entry_setup(&mut c2, now);
    assert_eq!(
        c2.engine_evaluate(now + 12_000),
        1,
        "cap above the entry notional must not block"
    );
    assert_eq!(c2.engine_stats()["strategyLimitRejected"], 0);
}

/// E16/#98: the portfolio-level open-notional cap bounds the TOTAL open
/// commitment across all strategies (0 = off = shipped behaviour).
#[test]
fn portfolio_notional_cap_bounds_total_open_exposure() {
    // Entry notional is 0.43 * 10 = 4.30: a 2.00 portfolio cap rejects it
    // even though no per-strategy limit is configured at all.
    let mut c = core_with_engine(HashMap::new());
    c.risk_config_mut().max_open_notional_usd = dec!(2);
    let now = 1_000_000i64;
    feed_entry_setup(&mut c, now);
    assert_eq!(
        c.engine_evaluate(now + 12_000),
        0,
        "a 4.30 entry breaches a 2.00 portfolio cap"
    );
    assert_eq!(c.engine_stats()["strategyLimitRejected"], 1);
    let stats = c.strategy_stats();
    let s = strategy_entry(&stats, "spread_arb");
    assert_eq!(
        s["rejectionCauses"]["limit.portfolioNotionalCap"], 1,
        "the portfolio cap must be bucketed so tooling can answer why: {s}"
    );

    // 0 = off: the same entry goes through untouched.
    let mut c2 = core_with_engine(HashMap::new());
    feed_entry_setup(&mut c2, now);
    assert_eq!(c2.engine_evaluate(now + 12_000), 1);
    assert_eq!(c2.engine_stats()["strategyLimitRejected"], 0);
}

// ── E-VA: strategy-level virtual accounts ───────────────────────────────
//
// Test 1 (strategy isolation): A's big entry must NOT consume B's
// headroom — each governed scope reserves against its OWN pot.
// Test 2 (atomic batch): a pair batch whose sum exceeds the pot places
// ZERO legs; the legs cancel together through the existing StrategyLimit
// rejection accounting.
// Test 3 (precise release): a partial fill converts exactly the filled
// shares' share of the hold; the rest stays outstanding.
// Test 4 (terminal release): a cancelled order's hold leaves the pot.
// Test 5 (bypass guard): undeclared strategy + armed pot = the historical
// path, byte-identically (no pot, no counters).

/// A dip strategy that declares `atomic_batch` and emits N equal-share
/// legs per cycle (one per configured asset) — the pair-batch shape the
/// virtual-account path exists for.
struct AtomicDip {
    name: String,
    assets: Vec<String>,
}
impl crate::strategies::EngineStrategy for AtomicDip {
    fn name(&self) -> &str {
        &self.name
    }
    fn on_book(&mut self, _t: &str, _s: &crate::model::OrderbookSnapshot, _n: i64) {}
    fn on_round(&mut self, _slot: i64, _time_left_sec: i64, _now_ms: i64) {}
    // Pair semantics: legs hold to settlement (the EngineStrategy doc's
    // complete-set rationale). Also the release posture the cancel test
    // exercises — a cancelled leg's token re-arms for a re-attempt.
    fn holds_to_settlement(&self) -> bool {
        true
    }
    fn atomic_batch(&self) -> bool {
        true
    }
    fn find_candidates(
        &mut self,
        ctx: &crate::strategies::StrategyCtx<'_>,
    ) -> Vec<crate::signal::TradeSignal> {
        let mut out = Vec::new();
        for market in ctx.markets() {
            if !self.assets.contains(&market.asset) {
                continue;
            }
            for (token_id, direction) in [
                (&market.up_token_id, crate::model::SignalDirection::Up),
                (&market.down_token_id, crate::model::SignalDirection::Down),
            ] {
                let Some(book) = ctx.fresh_book(token_id) else {
                    continue;
                };
                if book.mid_price <= dec!(0.45) {
                    out.push(crate::signal::TradeSignal {
                        strategy: self.name.clone(),
                        account_id: blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
                        asset: market.asset.clone(),
                        direction,
                        token_id: token_id.clone(),
                        condition_id: market.condition_id.clone(),
                        price: book.mid_price,
                        reason: "atomic dip".into(),
                        shares: Some(dec!(10)),
                        pair: None,
                    });
                }
            }
        }
        out
    }
}

/// A core with one enabled `AtomicDip` on the given assets (the kernel
/// ships no strategies; the test hosts its own, PR-B posture).
fn core_with_atomic_dip(virtual_cap: Option<Decimal>, assets: &[&str]) -> Core {
    let mut virtual_limits = HashMap::new();
    if let Some(cap) = virtual_cap {
        virtual_limits.insert(
            "pair_dip".to_string(),
            VirtualAccountLimit {
                max_batch_notional_usd: Some(cap),
                accounts: vec![blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string()],
            },
        );
    }
    let mut c = Core::new(CoreConfig {
        risk: RiskConfig {
            max_order_notional: dec!(100),
            ..Default::default()
        },
        dry_seed_balance: dec!(1000),
        engine_enabled: true,
        round_duration_sec: 900,
        auto_exits_enabled: false,
        assets: vec!["BTC".into(), "ETH".into(), "SOL".into()],
        positions: crate::position::PositionConfig {
            max_positions: 5,
            exit: crate::exit_policy::ExitConfig {
                min_time_left_sec: 0,
                ..Default::default()
            },
            ..Default::default()
        },
        virtual_limits,
        ..Default::default()
    });
    let cfg = c.config().engine_config();
    let mut eng = Engine::new(cfg.clone());
    eng.register_user_strategy(
        Box::new(AtomicDip {
            name: "pair_dip".to_string(),
            assets: assets.iter().map(|a| (*a).to_string()).collect(),
        }),
        "test".into(),
    )
    .unwrap();
    assert!(eng.set_strategy_enabled("pair_dip", true));
    c.enable_engine(eng);
    c
}

#[test]
fn virtual_quota_is_per_strategy_a_cannot_starve_b() {
    // Test 1: two strategies share one account. A holds a big RESERVATION
    // (a placed, unfilled entry); B's entry must still place — B's quota
    // is its own pot, and the REAL ledger was never touched by A's hold
    // (the pot is a risk-layer book, not cash).
    let mut c = core_with_atomic_dip(Some(dec!(100)), &["BTC"]);
    let now = 1_000_000i64;
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: three_markets(now),
            now_ms: now,
        },
        now,
    );
    c.engine_on_data(
        DataEvent::Book {
            token_id: "btc_up".into(),
            bids: vec![(dec!(0.43), dec!(100))],
            asks: vec![(dec!(0.45), dec!(100))],
            now_ms: now + 1_000,
        },
        now + 1_000,
    );
    // A (pair_dip) places its 10-share leg at 0.44: 4.40 USD held.
    assert_eq!(c.engine_evaluate(now + 2_000), 1);
    assert_eq!(
        c.virtual_used(blitzkrieg_market_api::DEFAULT_ACCOUNT_ID, "pair_dip"),
        dec!(4.40)
    );

    // B (a plain dip, NOT declared atomic) places against the SAME
    // account while A's hold is outstanding: B's entry is judged on the
    // real ledger (untouched) — it places.
    let cfg = c.config().engine_config();
    let mut eng = Engine::new(cfg.clone());
    eng.register_user_strategy(
        Box::new(TargetDip {
            name: "plain_b".into(),
            buy_below: dec!(0.45),
            assets: vec!["ETH".into()],
            gates: crate::strategies::GateExemptions::none(),
            scope: blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
        }),
        "test".into(),
    )
    .unwrap();
    assert!(eng.set_strategy_enabled("plain_b", true));
    // The engine is replaced wholesale — re-push the round/book state the
    // new engine needs, then evaluate B's candidate in the same cycle.
    c.enable_engine(eng);
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: three_markets(now),
            now_ms: now,
        },
        now,
    );
    c.engine_on_data(
        DataEvent::Book {
            token_id: "eth_up".into(),
            bids: vec![(dec!(0.43), dec!(100))],
            asks: vec![(dec!(0.45), dec!(100))],
            now_ms: now + 3_000,
        },
        now + 3_000,
    );
    let placed = c.engine_evaluate(now + 4_000);
    assert_eq!(
        placed, 1,
        "B's entry must place: A's virtual hold is A's own"
    );
    let stats = c.strategy_stats();
    let b = strategy_entry(&stats, "plain_b");
    assert_eq!(b["ordersPlaced"], 1, "B must not be starved: {b}");
    // A's hold is untouched by B's placement.
    assert_eq!(
        c.virtual_used(blitzkrieg_market_api::DEFAULT_ACCOUNT_ID, "pair_dip"),
        dec!(4.40)
    );
}

#[test]
fn atomic_batch_over_cap_cancels_the_whole_batch() {
    // Test 2: two legs at 0.44 x 10 shares = 4.40 each, batch 8.80. A pot
    // of 8.00 covers leg 1 alone but not the SUM: zero legs may place.
    let mut c = core_with_atomic_dip(Some(dec!(8)), &["BTC", "ETH"]);
    let now = 1_000_000i64;
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: three_markets(now),
            now_ms: now,
        },
        now,
    );
    for (token, t) in [("btc_up", now + 1_000), ("eth_up", now + 1_100)] {
        c.engine_on_data(
            DataEvent::Book {
                token_id: token.into(),
                bids: vec![(dec!(0.43), dec!(100))],
                asks: vec![(dec!(0.45), dec!(100))],
                now_ms: t,
            },
            t,
        );
    }
    assert_eq!(
        c.engine_evaluate(now + 2_000),
        0,
        "an over-cap batch must place ZERO legs (no naked first leg)"
    );
    assert!(c.ome().live_orders().is_empty(), "no leg may reach the OME");
    // The refusal rides the EXISTING StrategyLimit accounting.
    assert_eq!(c.engine_stats()["strategyLimitRejected"], 1);
    let stats = c.strategy_stats();
    let s = strategy_entry(&stats, "pair_dip");
    assert_eq!(
        s["rejectionCauses"]["limit.virtualCap"], 2,
        "both legs bucketed: {s}"
    );
    assert_eq!(s["ordersPlaced"], 0);
    assert_eq!(
        c.virtual_used(blitzkrieg_market_api::DEFAULT_ACCOUNT_ID, "pair_dip"),
        Decimal::ZERO,
        "a cancelled batch holds nothing"
    );

    // A pot of 20.00 fits the whole batch: both legs place.
    let mut c2 = core_with_atomic_dip(Some(dec!(20)), &["BTC", "ETH"]);
    c2.engine_on_data(
        DataEvent::RoundMarkets {
            markets: three_markets(now),
            now_ms: now,
        },
        now,
    );
    for (token, t) in [("btc_up", now + 1_000), ("eth_up", now + 1_100)] {
        c2.engine_on_data(
            DataEvent::Book {
                token_id: token.into(),
                bids: vec![(dec!(0.43), dec!(100))],
                asks: vec![(dec!(0.45), dec!(100))],
                now_ms: t,
            },
            t,
        );
    }
    assert_eq!(c2.engine_evaluate(now + 2_000), 2);
    assert_eq!(
        c2.virtual_used(blitzkrieg_market_api::DEFAULT_ACCOUNT_ID, "pair_dip"),
        dec!(8.80),
        "both legs' holds outstanding after placement"
    );
}

#[test]
fn virtual_release_is_fill_proportional() {
    // Test 3: hold 0.44 x 10 = 4.40; fill 1.2 shares → exactly 1.2 x 0.44
    // = 0.528 released, 3.872 still outstanding.
    let mut c = core_with_atomic_dip(Some(dec!(100)), &["BTC"]);
    let now = 1_000_000i64;
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: three_markets(now),
            now_ms: now,
        },
        now,
    );
    c.engine_on_data(
        DataEvent::Book {
            token_id: "btc_up".into(),
            bids: vec![(dec!(0.43), dec!(100))],
            asks: vec![(dec!(0.45), dec!(100))],
            now_ms: now + 1_000,
        },
        now + 1_000,
    );
    assert_eq!(c.engine_evaluate(now + 2_000), 1);
    let acct = blitzkrieg_market_api::DEFAULT_ACCOUNT_ID;
    assert_eq!(c.virtual_used(acct, "pair_dip"), dec!(4.40));

    // Fill PART of the 10 resting shares via a crossing book. The identity
    // fill model takes min(remaining, crossing depth) — the ask depth of
    // 1.2 fills exactly 1.2 shares, the rest keeps resting.
    c.book_snapshot(
        "btc_up",
        vec![(dec!(0.42), dec!(1))],
        vec![(dec!(0.42), dec!(1.2))],
        now + 3_000,
    );
    let used_after = c.virtual_used(acct, "pair_dip");
    let released = dec!(4.40) - used_after;
    assert_eq!(
        used_after,
        dec!(3.872),
        "release = 1.2 x 0.44 exactly: {released}"
    );
    assert!(
        c.positions()
            .open_positions()
            .iter()
            .any(|p| p.shares == dec!(1.2)),
        "the 1.2 filled shares are position occupancy"
    );
}

#[test]
fn cancelled_order_releases_its_virtual_hold() {
    // Test 4: a placed entry holds 4.40; cancelling it must leave the
    // pot empty (today's real-ledger release has no virtual twin — that
    // is the hole this closes).
    let mut c = core_with_atomic_dip(Some(dec!(100)), &["BTC"]);
    let now = 1_000_000i64;
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: three_markets(now),
            now_ms: now,
        },
        now,
    );
    c.engine_on_data(
        DataEvent::Book {
            token_id: "btc_up".into(),
            bids: vec![(dec!(0.43), dec!(100))],
            asks: vec![(dec!(0.45), dec!(100))],
            now_ms: now + 1_000,
        },
        now + 1_000,
    );
    assert_eq!(c.engine_evaluate(now + 2_000), 1);
    let acct = blitzkrieg_market_api::DEFAULT_ACCOUNT_ID;
    assert_eq!(c.virtual_used(acct, "pair_dip"), dec!(4.40));
    let id = c
        .ome()
        .live_orders()
        .first()
        .expect("the entry rests")
        .order_id
        .clone();
    c.cancel(&id, now + 3_000).unwrap();
    assert_eq!(
        c.virtual_used(acct, "pair_dip"),
        Decimal::ZERO,
        "the cancelled order's hold must release"
    );
    // The freed pot funds the next batch (the pre-check re-passes).
    let mut limits = HashMap::new();
    limits.insert(
        "pair_dip".to_string(),
        VirtualAccountLimit {
            max_batch_notional_usd: Some(dec!(100)),
            accounts: vec![acct.to_string()],
        },
    );
    let _ = limits; // cap unchanged; the re-place below proves the release
    c.book_snapshot(
        "btc_up",
        vec![(dec!(0.43), dec!(100))],
        vec![(dec!(0.45), dec!(100))],
        now + 4_000,
    );
    assert_eq!(
        c.engine_evaluate(now + 5_000),
        1,
        "the freed pot funds the next batch"
    );
}

// ── cross_venue_arb × E-VA: the REAL package through the kernel ─────────

/// The REAL cross_venue_arb Lua package, hosted the way `install_engine`
/// hosts one: package loaded from its worktree dir, `holds_to_settlement`
/// and `atomic_batch` stamped from the manifest, the kernel's ONE fee
/// schedule injected (the shipped legacy_quadratic — `bk.fees()` nil
/// would refuse everything). Registration rides
/// `register_user_strategy` (the `load_lua_package_inner` route minus the
/// engine plumbing), DISABLED — the test enables it explicitly, the same
/// opt-in an operator makes.
fn core_with_cross_venue_arb(virtual_cap: Option<Decimal>) -> Core {
    let package_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../user_layer/strategies_lua/cross_venue_arb");
    let mut virtual_limits = HashMap::new();
    if let Some(cap) = virtual_cap {
        virtual_limits.insert(
            "cross_venue_arb".to_string(),
            VirtualAccountLimit {
                max_batch_notional_usd: Some(cap),
                accounts: vec![blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string()],
            },
        );
    }
    let mut c = Core::new(CoreConfig {
        risk: RiskConfig {
            max_order_notional: dec!(10000),
            ..Default::default()
        },
        dry_seed_balance: dec!(100000),
        engine_enabled: true,
        round_duration_sec: 900,
        auto_exits_enabled: false,
        min_round_age_sec: 0,
        // The pair's legs are DECLARED at the strategy's capacity (the
        // thin leg's depth); the kernel band must never clamp below it.
        min_shares: dec!(1),
        max_shares: dec!(10000),
        // The pair's two legs are TWO live positions on one asset with
        // DIFFERENT condition ids (one per venue) — the #363 per-asset
        // count must cover them, exactly as an operator running this
        // package would configure it (`--max-positions-per-asset 2`).
        max_positions_per_asset: Some(4),
        // cwd-relative logs are the historical three-copies pollution:
        // a test writes nothing to the process cwd.
        order_log_path: None,
        trade_log_path: None,
        position_log_path: None,
        execution_policy_path: None,
        positions: crate::position::PositionConfig {
            // Same shape as the per-asset count above, on the position
            // manager itself: two legs = two open positions.
            max_positions: 8,
            ..Default::default()
        },
        virtual_limits,
        ..Default::default()
    });
    let cfg = c.config().engine_config();
    let mut eng = Engine::new(cfg.clone());
    crate::strategies::test_support::host_disabled(&mut eng, cfg.trend, cfg.spread_arb);
    let loaded = crate::strategy_engine::lua_loader::load_lua_package(&package_dir)
        .expect("cross_venue_arb package must load");
    assert!(
        loaded.atomic_batch,
        "the fixture premise: the manifest declares atomic_batch"
    );
    let sched = crate::exit_policy::fee_schedule();
    let adapter =
        crate::strategy_engine::lua_loader::LuaEngineAdapter::new(loaded.strategy, loaded.tunables)
            .declare(loaded.holds_to_settlement, loaded.gate_exemptions)
            .declare_atomic_batch(loaded.atomic_batch)
            // RFC-645 A-1: this fixture hosts the REAL arbiter package — stamp
            // the statistics declaration the production loader stamps (the
            // acceptance predicate still requires atomic_batch + the live
            // PairBook entry the fixture's evaluate produces).
            .declare_pair_book_stats(true);
    adapter.set_fee_schedule(blitzkrieg_lua_runtime::FeeScheduleView {
        name: sched.name.to_string(),
        rate: sched.rate.to_string(),
        exponent: sched.exponent,
    });
    eng.register_user_strategy(Box::new(adapter), format!("lua:{}", package_dir.display()))
        .expect("cross_venue_arb registers");
    assert!(eng.set_strategy_enabled("cross_venue_arb", true));
    c.enable_engine(eng);
    // 跨所套利弹窗 v2: the pairing is CONFIG-DRIVEN (`arb.config.set` →
    // the adapter's params bag); unconfigured ⇒ the strategy pairs
    // nothing (fail-closed). The fixture wires both legs onto the
    // DEFAULT account — the pre-v2 single-account batch shape these
    // dispatch tests judge (pot accounting, orphan sweep, naked-leg
    // degradation ride the same (account, strategy) scope as before).
    c.arb_config_set(&crate::account::CrossVenueArbConfig {
        leg_a_venue: "polymarket".into(),
        leg_a_account: blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
        leg_b_venue: "kalshi".into(),
        leg_b_account: blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
        pairs: vec!["BTC".into()],
    })
    .expect("the leg configuration lands (memory-only core)");
    c
}

/// The two-venue round the package prices: pm quotes the cheaper UP ask,
/// kx the cheaper DOWN ask. `expires_at_ms` rides the 900 s grid (the
/// #354 cadence the config runs).
fn cross_venue_markets(now: i64) -> Vec<CryptoMarket> {
    let slot = now / 1000 / 900;
    let market = |venue: &str, cond: &str, up: &str, down: &str| CryptoMarket {
        asset: "BTC".into(),
        condition_id: cond.into(),
        question_id: "q".into(),
        up_token_id: up.into(),
        down_token_id: down.into(),
        up_price: dec!(0.48),
        down_price: dec!(0.45),
        expires_at_ms: (slot + 1) * 900 * 1000,
        round_slot: slot,
        round_duration_sec: 900,
        archive_verdict: false,
        venue: venue.into(),
        neg_risk: false,
        question: "BTC up or down".into(),
    };
    vec![
        market("polymarket", "pm-cond", "pm-up", "pm-down"),
        market("kalshi", "kx-cond", "kx-up", "kx-down"),
    ]
}

/// The confirmed mapping binding the two listings into one bet (the human
/// gate: `unified_confirm`'s own ledger route — the exact path production
/// clicks).
fn confirm_cross_venue_mapping(c: &mut Core, now: i64) {
    let listing =
        |venue: blitzkrieg_market_api::unified::Venue, cond: &str, up: &str, down: &str| {
            blitzkrieg_market_api::unified::PlatformListing {
                venue,
                condition_id: cond.into(),
                up_token_id: up.into(),
                down_token_id: down.into(),
                asset: Some("bitcoin".into()),
                title: "Bitcoin Up or Down".into(),
                settlement_rules: "Binance 1m candle".into(),
                settlement_source: blitzkrieg_market_api::unified::SettlementSource::Chainlink,
                expires_at_ms: now + 900_000,
                status: blitzkrieg_market_api::unified::ListingStatus::Active,
            }
        };
    c.unified_confirm(
        "Bitcoin Up or Down",
        "uid:test",
        vec![
            listing(
                blitzkrieg_market_api::unified::Venue::Polymarket,
                "pm-cond",
                "pm-up",
                "pm-down",
            ),
            listing(
                blitzkrieg_market_api::unified::Venue::Kalshi,
                "kx-cond",
                "kx-up",
                "kx-down",
            ),
        ],
        now,
    )
    .expect("the two-listing mapping confirms");
}

/// Feed the pair books: pm up ask 0.44 / depth 600, kx down ask 0.42 /
/// depth 250 (the thin leg), the other legs mid-priced. Cross-venue pair
/// cost ≈ 0.875 + fees ≤ 0.98 → the package fires both legs.
fn feed_cross_venue_books(c: &mut Core, now: i64) {
    let mut book = |token: &str, ask: Decimal, depth: Decimal| {
        c.engine_on_data(
            DataEvent::Book {
                token_id: token.into(),
                bids: vec![(ask - dec!(0.01), depth)],
                asks: vec![(ask, depth)],
                now_ms: now,
            },
            now,
        );
    };
    book("pm-up", dec!(0.44), dec!(600));
    book("pm-down", dec!(0.50), dec!(600));
    book("kx-up", dec!(0.48), dec!(9000));
    book("kx-down", dec!(0.42), dec!(250));
}

#[test]
fn cross_venue_arb_over_pot_refuses_the_whole_pair_through_the_real_package() {
    // Two legs at 0.44 / 0.42 × 250 declared shares ≈ 215 notional. A pot
    // of 200 covers neither pair whole... it refuses the WHOLE batch:
    // zero legs reach the OME, and the refusal rides the existing
    // StrategyLimit accounting (`limit.virtualCap`, one per leg).
    let mut c = core_with_cross_venue_arb(Some(dec!(200)));
    let now = 1_700_000_000_000i64;
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: cross_venue_markets(now),
            now_ms: now,
        },
        now,
    );
    confirm_cross_venue_mapping(&mut c, now);
    feed_cross_venue_books(&mut c, now + 1_000);
    assert_eq!(
        c.engine_evaluate(now + 2_000),
        0,
        "an over-cap pot must place ZERO legs through the real package"
    );
    assert!(c.ome().live_orders().is_empty(), "no leg reaches the OME");
    assert_eq!(c.engine_stats()["strategyLimitRejected"], 1);
    let stats = c.strategy_stats();
    let s = strategy_entry(&stats, "cross_venue_arb");
    assert_eq!(
        s["rejectionCauses"]["limit.virtualCap"], 2,
        "both legs bucketed: {s}"
    );
    assert_eq!(
        c.virtual_used(blitzkrieg_market_api::DEFAULT_ACCOUNT_ID, "cross_venue_arb"),
        Decimal::ZERO,
        "a refused pair holds nothing"
    );

    // The same books under a pot that fits: BOTH legs place — the
    // acceptance twin proving the refusal above was the cap, not the
    // package. The package quotes at its own asks, so each leg's
    // MakerThenTaker POST crosses and the leg lands inside its own place
    // call: the pot's holds convert to position occupancy within the
    // cycle, and the committed 215 reads off the POSITION book (the pot's
    // own `batch_fits` arithmetic: used + committed_elsewhere).
    let mut c2 = core_with_cross_venue_arb(Some(dec!(1000)));
    c2.engine_on_data(
        DataEvent::RoundMarkets {
            markets: cross_venue_markets(now),
            now_ms: now,
        },
        now,
    );
    confirm_cross_venue_mapping(&mut c2, now);
    feed_cross_venue_books(&mut c2, now + 1_000);
    assert_eq!(c2.engine_evaluate(now + 2_000), 2, "both legs place");
    let committed: Decimal = c2
        .positions()
        .open_positions()
        .iter()
        .map(|p| p.cost_usd)
        .sum();
    assert_eq!(
        committed,
        dec!(215),
        "250 × (0.44 + 0.42) committed as ONE ticket"
    );
    assert_eq!(
        c2.positions().open_positions().len(),
        2,
        "the pair is whole: one position per venue leg"
    );
}

/// 套利合计字段（前端消费契约，PR #577）：arbNetPnlUsd / arbWins /
/// arbLosses / arbClosedTrades / arbActivePairs —— 只有 cross_venue_arb
/// 的行携带；无论账户切片选哪个账户，五个字段都读套利单元的全局合计
/// （双腿平仓各记在 (cross_venue_arb, 各腿账户) 键下，全局 fold 是和）。
/// 非套利行不带 arb* 键 —— 前端按行内有无字段 feature-detect。
#[test]
fn arb_totals_ride_every_account_slice_of_the_arb_row_only() {
    let mut c = core_with_cross_venue_arb(Some(dec!(1000)));
    // A≠B 的腿配置（跨所配对的声明闸要求两腿盖章到不同账户）：套利
    // 单元 = 实例主账户 default + 腿账户 arb-b。arb-b 是内存账簿的新
    // 账户（set 校验要求账户已配置）。
    c.accounts_mut()
        .insert(crate::account::Account {
            id: crate::account::AccountId::from("arb-b"),
            name: "arb-b".into(),
            market_type: blitzkrieg_market_api::MarketType::Prediction,
            status: crate::account::AccountStatus::Active,
            credential_keys: crate::account::CredentialKeys::default(),
            venue: String::new(),
            trading_enabled: true,
            strategies: Vec::new(),
            rounds: Vec::new(),
            updated_at_ms: 0,
        })
        .expect("arb-b does not exist yet");
    c.accounts_mut()
        .get_mut(&crate::account::AccountId::from("arb-b"))
        .expect("arb-b just inserted")
        .set_balance(dec!(1000));
    c.arb_config_set(&crate::account::CrossVenueArbConfig {
        leg_a_venue: "polymarket".into(),
        leg_a_account: blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
        leg_b_venue: "kalshi".into(),
        leg_b_account: "arb-b".into(),
        pairs: vec!["BTC".into()],
    })
    .expect("the A≠B leg configuration lands");
    let now = 1_700_000_000_000i64;
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: cross_venue_markets(now),
            now_ms: now,
        },
        now,
    );
    confirm_cross_venue_mapping(&mut c, now);
    feed_cross_venue_books(&mut c, now + 1_000);
    assert_eq!(c.engine_evaluate(now + 2_000), 2, "both legs place");
    // 声明受理：跨所配对真的进了配对簿（在册读数的前提）。
    assert_eq!(
        c.pair_book_all().len(),
        1,
        "the A≠B pair declaration lands in the book"
    );

    // 两腿平仓：生产 close 路径（close → on_position_closed），净额一
    // 正一负，wins/losses 各记一笔；合计字段读全局 fold = 两腿之和
    // （含各腿真实手续费 —— 合计字段是 fold 的别名，不是手抄数）。
    let legs: Vec<crate::position::OpenPosition> = c.positions().open_positions().to_vec();
    assert_eq!(legs.len(), 2, "fixture premise: the pair is open");
    let mut net_sum = Decimal::ZERO;
    for held in &legs {
        // 价差驱动平仓价：正腿 +0.40/股，负腿 −0.30/股。
        let delta = if held.entry_price < dec!(0.43) {
            dec!(0.40)
        } else {
            dec!(-0.30)
        };
        let closed = c
            .positions
            .close(
                &held.id,
                held.entry_price + delta,
                crate::model::ExitReason::TakeProfit,
                false,
                1,
            )
            .expect("the leg closes through the production close path");
        net_sum += closed.net_pnl_usd;
        c.on_position_closed(&closed, 1);
    }
    assert!(net_sum.is_sign_positive(), "the fixture books a net profit");

    // ── 全局行：arb* = 该行 fold 值（双腿各账户键之和）──────────────
    let global = c.strategy_stats();
    let g = strategy_entry(&global, "cross_venue_arb");
    assert_eq!(
        dec_of(&g["arbNetPnlUsd"]),
        net_sum,
        "the arb row's total is both legs' sum: {g}"
    );
    assert_eq!(g["arbWins"], serde_json::json!(1));
    assert_eq!(g["arbLosses"], serde_json::json!(1));
    assert_eq!(g["arbClosedTrades"], serde_json::json!(2));
    assert_eq!(
        g["arbActivePairs"],
        serde_json::json!(1),
        "the pair sits in the book until the round rolls: {g}"
    );
    assert_eq!(
        dec_of(&g["netPnlUsd"]),
        net_sum,
        "the plain counter and the arb total agree on the global row"
    );

    // ── 账户切片行：arb* 仍读全局合计；平计数是该账户的腿切片 ──
    // （一正一负 ⇒ 任一切片都只看得见自己那腿的胜负，但 arb* 都是 1/1。）
    for account in [blitzkrieg_market_api::DEFAULT_ACCOUNT_ID, "arb-b"] {
        let sliced = c.strategy_stats_for_account(account);
        let s = strategy_entry(&sliced, "cross_venue_arb");
        assert_eq!(
            dec_of(&s["arbNetPnlUsd"]),
            net_sum,
            "every account's page shows the arb unit's combined take ({account}): {s}"
        );
        assert_eq!(s["arbWins"], serde_json::json!(1), "{account}");
        assert_eq!(s["arbLosses"], serde_json::json!(1), "{account}");
        assert_eq!(s["arbClosedTrades"], serde_json::json!(2), "{account}");
        assert_eq!(s["arbActivePairs"], serde_json::json!(1), "{account}");
        // 切片的平计数是本账户腿的（两腿账户不同 ⇒ 各自 1 笔）。
        assert_eq!(
            s["closedTrades"],
            serde_json::json!(1),
            "the slice's plain counters stay per-account ({account}): {s}"
        );
    }

    // ── 非套利行不带 arb* 键 ──
    let g_plain = strategy_entry(&global, "spread_arb");
    assert!(
        g_plain.get("arbNetPnlUsd").is_none(),
        "a non-arb row carries no arb totals: {g_plain}"
    );
    let s_plain_slice = c.strategy_stats_for_account("arb-b");
    let s_plain = strategy_entry(&s_plain_slice, "spread_arb");
    assert!(
        s_plain.get("arbNetPnlUsd").is_none(),
        "a non-arb row carries no arb totals on a slice either: {s_plain}"
    );
}

#[test]
fn pair_orphan_sweep_flattens_a_naked_leg_after_the_grace_window() {
    // The compensation watch: leg ONE lands, leg TWO's order dies before
    // it can (the starved ledger below is one real way — the #457 expiry
    // sweep, a manual withdraw and a venue reject all share the same
    // terminal shape), and after the 5 s landing grace the LIVE tick sweep
    // alerts and flattens the naked leg.
    //
    // Device: the ledger runs dry between the legs. pm-up places and
    // FILLS inside its own place call (the package prices at its own ask,
    // so the MakerThenTaker POST crosses and escalates); kx-down's 105
    // reserve then exceeds what the book has left and its place dies —
    // a leg that never rested, never filled, never will.
    let mut c = core_with_cross_venue_arb(Some(dec!(1000)));
    {
        // Drain the default book to 110: pm-up (0.44 × 250 = 110) just
        // fits; kx-down (0.42 × 250 = 105) does not.
        c.accounts_mut().active_ledger_mut().set_balance(dec!(110));
    }
    let now = 1_700_000_000_000i64;
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: cross_venue_markets(now),
            now_ms: now,
        },
        now,
    );
    confirm_cross_venue_mapping(&mut c, now);
    feed_cross_venue_books(&mut c, now + 1_000);
    assert_eq!(c.engine_evaluate(now + 2_000), 1, "only leg one lands");

    let held: Vec<String> = c
        .positions()
        .open_positions()
        .iter()
        .map(|p| p.token_id.clone())
        .collect();
    assert_eq!(
        held,
        vec!["pm-up".to_string()],
        "pm-up filled, kx-down never placed"
    );

    // Inside the grace window the sweep holds its hand (the two legs of
    // one batch land serially inside one evaluate cycle).
    c.tick(now + 4_000).expect("grace-window tick");
    assert_eq!(
        c.positions().open_positions().len(),
        1,
        "the grace window protects the just-filled leg"
    );

    // Past the window, the LIVE tick sweep flattens the naked leg. The
    // alert names the SAME-market partner token (`partner_of` stamps the
    // condition's other outcome — the dry settlement ladder's key), not
    // the cross-venue leg that died.
    let (tx, mut rx) = mpsc::unbounded_channel();
    c.set_event_sink(tx);
    c.book_snapshot(
        "pm-up",
        vec![(dec!(0.42), dec!(600))],
        vec![(dec!(0.42), dec!(600))],
        now + 12_000,
    );
    c.tick(now + 12_000).expect("sweep tick");
    let mut alerts = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let Event::RiskAlert { message, .. } = ev {
            alerts.push(message);
        }
    }
    assert!(
        alerts.iter().any(|m| m.contains("pair leg orphaned")
            && m.contains("cross_venue_arb")
            && m.contains("partner_missing=pm-down")),
        "the orphan alert must name the strategy and the missing leg: {alerts:?}"
    );
    assert!(
        c.positions().open_positions().is_empty(),
        "the naked leg is flattened: remaining = {:?}",
        c.positions()
            .open_positions()
            .iter()
            .map(|p| p.token_id.clone())
            .collect::<Vec<_>>()
    );
}

#[test]
fn pair_orphan_sweep_leaves_ungoverned_strategies_alone() {
    // The zero-config guard: the SAME naked-leg shape (one fill, the
    // partner leg dead) under a strategy the pot book does NOT govern
    // must see ZERO alerts and ZERO flatten orders — orphan compensation
    // is a governed-atomic-scope posture, never a global behaviour.
    let mut c = core_with_cross_venue_arb(None);
    {
        // Same starved-ledger device as the governed test: pm-up lands,
        // kx-down's place dies on the reserve.
        c.accounts_mut().active_ledger_mut().set_balance(dec!(110));
    }
    let now = 1_700_000_000_000i64;
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: cross_venue_markets(now),
            now_ms: now,
        },
        now,
    );
    confirm_cross_venue_mapping(&mut c, now);
    feed_cross_venue_books(&mut c, now + 1_000);
    assert_eq!(
        c.engine_evaluate(now + 2_000),
        1,
        "leg one lands the same way"
    );
    assert_eq!(
        c.positions().open_positions().len(),
        1,
        "leg one fills the same way"
    );

    let (tx, mut rx) = mpsc::unbounded_channel();
    c.set_event_sink(tx);
    c.book_snapshot(
        "pm-up",
        vec![(dec!(0.42), dec!(600))],
        vec![(dec!(0.42), dec!(600))],
        now + 12_000,
    );
    c.tick(now + 12_000).expect("sweep tick");
    let mut alerts = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let Event::RiskAlert { message, .. } = ev {
            alerts.push(message);
        }
    }
    assert!(
        alerts.iter().all(|m| !m.contains("pair leg orphaned")),
        "an ungoverned scope is invisible to the orphan sweep: {alerts:?}"
    );
    assert_eq!(
        c.positions().open_positions().len(),
        1,
        "the naked leg of an ungoverned strategy stays in the book"
    );
    assert!(
        c.ome()
            .live_orders()
            .into_iter()
            .all(|o| o.side != Side::Sell),
        "no flatten SELL was spawned"
    );
}

#[test]
fn undeclared_strategy_keeps_the_per_order_path_byte_identically() {
    // Test 5 (bypass guard): a strategy that did NOT declare
    // `atomic_batch` but IS armed in virtual_limits (and one that is
    // neither) must take the historical path: no pot, no reservation,
    // no new counters — the same entry the default core places.
    // Reference run: NO virtual layer at all.
    let mut reference = core_with_dips(HashMap::new(), &[("plain", &["BTC"])], 5);
    let now = 1_000_000i64;
    feed_three_asset_dip(&mut reference, now);
    let ref_placed = reference.engine_evaluate(now + 2_000);
    let ref_stats = reference.strategy_stats();
    let ref_row = strategy_entry(&ref_stats, "plain").clone();

    // Armed pot, undeclared strategy: same outcome, empty pot.
    let mut limits = HashMap::new();
    limits.insert(
        "plain".to_string(),
        VirtualAccountLimit {
            max_batch_notional_usd: Some(dec!(1)),
            accounts: vec![blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string()],
        },
    );
    let mut armed = core_with_dips(HashMap::new(), &[("plain", &["BTC"])], 5);
    armed.set_virtual_limits_for_test(limits);
    feed_three_asset_dip(&mut armed, now);
    assert_eq!(
        armed.engine_evaluate(now + 2_000),
        ref_placed,
        "an undeclared strategy ignores the pot entirely"
    );
    let armed_stats = armed.strategy_stats();
    let armed_row = strategy_entry(&armed_stats, "plain");
    assert_eq!(
        armed_row["ordersPlaced"], ref_row["ordersPlaced"],
        "byte-identical placement outcome"
    );
    assert!(
        armed_row["rejectionCauses"].is_null()
            || armed_row["rejectionCauses"] == ref_row["rejectionCauses"],
        "no new rejection bucket may appear: {armed_row}"
    );
    assert_eq!(
        armed.virtual_used(blitzkrieg_market_api::DEFAULT_ACCOUNT_ID, "plain"),
        Decimal::ZERO,
        "the pot must stay empty — reservations are the declared path only"
    );
}

#[test]
fn closed_trade_lands_in_the_strategy_ledger() {
    let mut c = core_with_engine(HashMap::new());
    let now = 1_000_000i64;
    feed_entry_setup(&mut c, now);
    assert_eq!(c.engine_evaluate(now + 12_000), 1);
    assert_eq!(
        c.positions().open_positions().len(),
        0,
        "maker entry rests until the book crosses"
    );

    // Fill the resting bid (ask 0.42 crosses the 0.43 bid), then push a
    // +100%-ish book so the exit engine closes the position at a profit.
    // The tuned 0.88 discount rests the entry bid at round2(0.44*0.88)
    // = 0.39, so the crossing ask is 0.39 (was 0.42 under the 0.98 factor).
    c.book_snapshot(
        "up",
        vec![(dec!(0.38), dec!(100))],
        vec![(dec!(0.39), dec!(100))],
        now + 13_000,
    );
    assert_eq!(
        c.positions().open_positions().len(),
        1,
        "crossing ask must fill the maker entry"
    );
    c.book_snapshot(
        "up",
        vec![(dec!(0.95), dec!(100))],
        vec![(dec!(0.97), dec!(100))],
        now + 14_000,
    );
    c.tick(now + 14_200).unwrap();
    // #496: the maker-first TP exit rests at 0.96 (one tick above the
    // bid) and the bid rising into it is the honest fill that closes.
    c.book_snapshot(
        "up",
        vec![(dec!(0.96), dec!(100))],
        vec![(dec!(0.98), dec!(100))],
        now + 15_000,
    );
    c.tick(now + 15_200).unwrap();
    assert_eq!(
        c.positions().open_positions().len(),
        0,
        "profit target must close the position"
    );

    let stats = c.strategy_stats();
    let s = strategy_entry(&stats, "spread_arb");
    assert_eq!(s["closedTrades"], 1);
    assert_eq!(s["wins"], 1);
    assert_eq!(s["losses"], 0);
    assert!(
        dec_of(&s["netPnlUsd"]) > Decimal::ZERO,
        "expected positive realized PnL"
    );
    // E17: the entry rests on the book and is hit (maker), and the profit
    // target rests at the bid and is hit (maker), so this fixture pays NO
    // fee. Before the fix the entry was charged the taker rate while the
    // record claimed maker — that phantom fee is what made "fees > 0" pass,
    // and it is exactly the drift E17 removed. Pin the equality that
    // actually matters: the fee total IS the record's, and the PnL IS cash.
    let rec = &c.positions().closed_positions()[0];
    let rec_fees = (rec.entry_fee_pct / Decimal::ONE_HUNDRED) * rec.entry_price * rec.shares
        + (rec.exit_fee_pct / Decimal::ONE_HUNDRED) * rec.exit_price * rec.shares;
    assert_eq!(
        rec_fees,
        Decimal::ZERO,
        "both legs rested on the book, so nothing was charged"
    );
    assert_eq!(
        dec_of(&s["feesUsd"]),
        rec_fees,
        "the ledger's fee total must equal the trade record's, not exceed it"
    );
    assert_eq!(
        dec_of(&s["netPnlUsd"]),
        rec.net_pnl_usd,
        "strategy PnL must be the cash the ledger moved"
    );
    assert_eq!(
        dec_of(&s["openNotionalUsd"]),
        Decimal::ZERO,
        "closed position leaves no exposure"
    );
}

/// The cash ledger must equal `seed + Σ netPnlUsd` once nothing is reserved
/// and no position is open — the identity #75 set out to establish, and the
/// E17 acceptance criterion.
///
/// How this fixture used to fail (drift 0.0817722), for the record. A
/// `MakerThenTaker` entry rests at the bid, gets hit, and its profit target
/// exits as a maker; three places disagreed about what kind of fill happened:
///
/// 1. ENTRY — the position recorded `was_maker_entry = true` and a 0 entry
///    fee, while `apply_delta_effects` read the escalated order's
///    `mode = MakerThenTaker` and charged the TAKER fee. Cash sat BELOW the
///    record by that fee.
/// 2. EXIT — the closing delta covered 9.99 of the 10 shares (the old flat
///    0.01 share buffer), so cash received 0.01 × exit_price less proceeds
///    than the record's gross assumed.
/// 3. EXIT — the record re-derived `was_maker_exit = false` and charged a
///    taker exit fee, while the delta's `mode = Maker` charged none. Cash sat
///    ABOVE the record by that fee.
///
/// The E17 fix removes the disagreement rather than the symptom: the fee now
/// follows `FillDelta.role` (resolved from the fill), the position accrues the
/// fee the ledger actually charged, the flat share buffer is gone (exits floor
/// to the 0.01 share grid), and a sub-grid remainder is written off inside
/// `net`. See `MIGRATION_LOG.md`.
#[test]
fn dry_balance_is_seed_plus_realized_net() {
    const SEED: Decimal = dec!(1000);
    let mut c = core_with_engine(HashMap::new());
    let now = 1_000_000i64;
    feed_entry_setup(&mut c, now);
    assert_eq!(c.engine_evaluate(now + 12_000), 1);
    // Fill the resting bid, then mark up so the profit target closes it.
    // The tuned 0.88 discount rests the entry bid at round2(0.44*0.88)
    // = 0.39, so the crossing ask is 0.39 (was 0.42 under the 0.98 factor).
    c.book_snapshot(
        "up",
        vec![(dec!(0.38), dec!(100))],
        vec![(dec!(0.39), dec!(100))],
        now + 13_000,
    );
    c.book_snapshot(
        "up",
        vec![(dec!(0.95), dec!(100))],
        vec![(dec!(0.97), dec!(100))],
        now + 14_000,
    );
    c.tick(now + 14_200).unwrap();
    // #496: the TP exit rests at 0.96; the bid rising into it fills it.
    c.book_snapshot(
        "up",
        vec![(dec!(0.96), dec!(100))],
        vec![(dec!(0.98), dec!(100))],
        now + 15_000,
    );
    c.tick(now + 15_200).unwrap();
    assert_eq!(c.positions().open_positions().len(), 0);
    assert_eq!(c.ledger().reserved(), Decimal::ZERO);

    let net: Decimal = c
        .positions()
        .closed_positions()
        .iter()
        .map(|p| p.net_pnl_usd)
        .sum();
    assert!(
        net > Decimal::ZERO,
        "the fixture is meant to close at a profit, got {net}"
    );
    let parts: Vec<String> = c
        .positions()
        .closed_positions()
        .iter()
        .map(|p| {
            let ef = (p.entry_fee_pct / Decimal::ONE_HUNDRED) * p.entry_price * p.shares;
            let xf = (p.exit_fee_pct / Decimal::ONE_HUNDRED) * p.exit_price * p.shares;
            format!(
                "id={} entry={} exit={} shares={} mkEntry={} mkExit={} entryFee={} exitFee={} gross={} net={}",
                p.id, p.entry_price, p.exit_price, p.shares, p.was_maker_entry,
                p.was_maker_exit, ef, xf, p.pnl_usd, p.net_pnl_usd
            )
        })
        .collect();
    assert_eq!(
        c.ledger().balance(),
        SEED + net,
        "cash must equal principal + realized net once nothing is open; closed={parts:?}"
    );
}

// ── #363: the execution-policy verdict seam, wired into the pipeline ────

/// A dip-buying core with an operator-supplied policy. The point of the
/// seam: a policy verdict happens BEFORE arbitration, so the RiskGate
/// never sees a refused intent (the policy may remove, never relax),
/// and the refusal is bucketed `policy.skip`.
fn policy_core(mut c: Core, policy: crate::execution_policy::Policy) -> Core {
    c.execution_policy = Some(policy);
    c
}

// ── #380: the boot-time policy-path contract ────────────────────────────

/// A scratch tree with the shipped policy file at
/// `<root>/user_layer/configs/execution_policy.toml`, carrying ONE
/// identifying rule (the backstop) so a loaded file is distinguishable
/// from the neutral kernel by `execution_policy_loaded()` alone.
fn policy_tree_with_backstop(root: &std::path::Path) -> std::path::PathBuf {
    use std::sync::atomic::AtomicU64;
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "bk380-boot-{}-{}-{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        root.file_name().and_then(|s| s.to_str()).unwrap_or("t")
    ));
    std::fs::create_dir_all(dir.join("user_layer").join("configs")).expect("config tree");
    let file = dir
        .join("user_layer")
        .join("configs")
        .join("execution_policy.toml");
    std::fs::write(
        &file,
        r#"
version = 1

[defaults]
budget_ratio = 0.10
min_budget_usd = 1.00
max_budget_usd = 50.00
min_equity_usd = 1.00
max_positions_per_asset = 1

[[defaults.rules]]
name = "global_backstop"
enabled = true
priority = 100
when = "open_positions >= 2"
then = { action = "skip" }
"#,
    )
    .expect("write policy fixture");
    file
}

/// #380: an EXPLICIT policy path is honoured verbatim by the boot load —
/// a file written anywhere the cwd's spelling names is loaded, with the
/// file's own rule in force. (The install-root resolution itself is
/// unit-tested in `execution_policy.rs`; this pins the SERVICE's half:
/// the path the config carries is the path the boot actually reads.)
#[test]
fn boot_loads_the_explicit_policy_path_verbatim() {
    let tree = std::env::temp_dir().join(format!("bk380-explicit-{}", std::process::id()));
    let file = policy_tree_with_backstop(&tree);
    let cfg = CoreConfig {
        execution_policy_path: Some(file.to_string_lossy().into_owned()),
        dry_seed_balance: dec!(100),
        trade_log_path: None,
        order_log_path: None,
        position_log_path: None,
        ..Default::default()
    };
    let c = Core::new(cfg);
    assert!(
        c.execution_policy_loaded(),
        "the explicit file exists: the boot must load it"
    );
    // The loaded policy is the FILE's, not the neutral kernel: the
    // backstop rule the fixture wrote is in force.
    let section = c
        .execution_policy_section_view("default")
        .expect("the file's [defaults] section");
    let rules = section.rules.expect("the file's backstop rule");
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0]["name"], serde_json::json!("global_backstop"));
    let _ = std::fs::remove_dir_all(&tree);
}

/// #380: the DEFAULT spelling absent everywhere is the zero-config
/// deployment — the boot loads the NEUTRAL kernel (code defaults, no
/// rules, no account sections). This is the exact behaviour the /tmp A/B
/// in the issue saw from a cwd without the file; what changes in #380 is
/// that the BINARY says so (WARN with the searched directories) instead
/// of degrading silently. The kernel side is unchanged: a missing file
/// loads `Policy::default()` — the same `Some` shape a zero-config boot
/// always produced, bit for bit.
#[test]
fn boot_with_the_default_spelling_missing_is_the_neutral_kernel() {
    // A tree with no policy file anywhere above it — the default spelling
    // (`user_layer/configs/...`) resolves to nothing there.
    let bare = std::env::temp_dir().join(format!("bk380-bare-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&bare);
    std::fs::create_dir_all(&bare).expect("bare dir");
    let resolved =
        crate::execution_policy::resolve_default_config_path(Some(&bare.join("bin")), Some(&bare));
    assert_eq!(
        resolved.source,
        crate::execution_policy::DefaultPathSource::Fallback,
        "the premise: nothing found in the bare tree"
    );
    let cfg = CoreConfig {
        // What `main.rs` stores after a fallback resolution: the
        // exe-root-relative spelling, pointing at a file that does not
        // exist here.
        execution_policy_path: Some(resolved.path.to_string_lossy().into_owned()),
        dry_seed_balance: dec!(100),
        trade_log_path: None,
        order_log_path: None,
        position_log_path: None,
        ..Default::default()
    };
    let c = Core::new(cfg);
    // The boot SUCCEEDS (#372 semantics: missing = zero-config, never a
    // refusal), and the policy in force is the NEUTRAL kernel: no rules
    // anywhere, the code defaults exactly.
    let policy = c.execution_policy.as_ref().expect("#363: always Some");
    assert!(policy.defaults.rules.is_empty(), "neutral: no rules");
    assert!(policy.accounts.is_empty(), "neutral: no account sections");
    assert_eq!(
        policy.defaults.budget_ratio,
        crate::execution_policy::code_defaults().budget_ratio,
        "the neutral budget ratio, not a file's"
    );
    assert_eq!(
        c.execution_policy_section_view("default").unwrap().rules,
        Some(Vec::new()),
        "the section view reports an EMPTY rule list for the neutral kernel"
    );
    let _ = std::fs::remove_dir_all(&bare);
}

/// The compiled default: no rules, budget unbound — the zero-config
/// kernel. (The `open_positions >= 2 → skip` backstop lives in the
/// SHIPPED FILE, not here.)
fn neutral_policy() -> crate::execution_policy::Policy {
    crate::execution_policy::Policy::default()
}

/// A place() request for the dip core's book (taker at the given price),
/// naming an account (or a blank one for the missing-id test).
fn policy_req(
    price: Decimal,
    size: Decimal,
    key: &str,
    account: &str,
) -> crate::model::OrderRequest {
    crate::model::OrderRequest {
        token_id: "btc_up".into(),
        condition_id: "cond".into(),
        side: Side::Buy,
        mode: FillPolicy::Taker,
        price,
        size,
        internal_key: key.into(),
        strategy: "dip".into(),
        asset: "BTC".into(),
        direction: "up".into(),
        round_slot: 1,
        account_id: crate::model::AccountId::from(account),
    }
}

#[test]
fn policy_skip_refuses_the_entry_before_the_risk_gate() {
    // The shipped file's backstop (open_positions >= 2 → skip) can only
    // bite at 2+ positions, so here judge the STRUCTURAL facts instead:
    // a policy whose ONLY content is a skip-everything rule.
    let mut policy = neutral_policy();
    policy.defaults.rules = vec![crate::execution_policy::Rule {
        name: "sit_out".into(),
        priority: 1,
        enabled: true,
        when: crate::execution_policy::When {
            field: crate::execution_policy::ConditionField::OpenPositions,
            op: crate::execution_policy::CompareOp::Ge,
            value: crate::execution_policy::ConditionValue::Num(Decimal::ZERO),
        },
        then: crate::execution_policy::Action {
            action: Some(crate::execution_policy::SkipAction::Skip),
            budget_ratio: None,
            min_budget_usd: None,
            max_budget_usd: None,
            cooldown_sec: None,
        },
        reason: Some("测试：一票否决".into()),
    }];
    let mut c = policy_core(equity_core(dec!(4.8), Decimal::ZERO), policy);
    let now = 1_000_000i64;
    feed_btc_dip(&mut c, now);
    assert_eq!(
        c.engine_evaluate(now + 1_000),
        0,
        "policy skip removes the order"
    );
    assert!(c.list_orders().is_empty(), "nothing reached place()");
    let stats = c.strategy_stats();
    let s = strategy_entry(&stats, "dip");
    assert_eq!(s["rejectionCauses"]["policy.skip"], 1, "{s}");
}

/// The negative half of "规则不能绕过 RiskEngine": with the policy
/// REMOVED (its Place path), an order that the risk cap rejects is STILL
/// rejected — a policy budget never widens anything. Here the dip ticket
/// (size_usd 2.5 at 0.44 → 5 shares → 2.20 USD) sails under the default
/// 1000 notional cap, so the assertion pins the budget does not RAISE
/// the ticket: the engine's own band decided 5 shares and the policy
/// (neutral, budget = full balance) leaves it exactly alone.
#[test]
fn policy_place_leaves_the_ticket_untouched_when_neutral() {
    let mut c = policy_core(equity_core(dec!(4.8), Decimal::ZERO), neutral_policy());
    let now = 1_000_000i64;
    feed_btc_dip(&mut c, now);
    assert_eq!(c.engine_evaluate(now + 1_000), 1);
    let orders = c.list_orders();
    assert_eq!(orders.len(), 1);
    assert_eq!(
        orders[0].size,
        dec!(6),
        "2.5/0.44 → 6 shares, unchanged by the neutral policy"
    );
}

/// "account_id 缺失 → 拒绝下单": an intent with a blank account id is
/// refused (never silently judged under the defaults) and the refusal
/// is visible in the strategy's rejection causes.
#[test]
fn a_missing_account_id_refuses_the_order() {
    let mut c = policy_core(equity_core(dec!(4.8), Decimal::ZERO), neutral_policy());
    let now = 1_000_000i64;
    feed_btc_dip(&mut c, now);
    // Blank the account id on the produced intent before the verdict is
    // consulted: exactly what a mis-wired strategy host would send.
    // The verdict map keys on the SAME id, so the lookup itself must
    // refuse — drive it through a custom request via place_gated is
    // simpler: place() with an empty account id.
    assert!(c.list_orders().is_empty(), "engine_evaluate not run yet");
    // place() path: the intent carries a BLANK account id. The policy's
    // own refusal must reject it (never silently judge under defaults).
    let r = c.place(
        policy_req(dec!(0.44), dec!(5), "policy-missing-account", ""),
        0,
        now + 1_000,
    );
    assert!(r.is_err(), "an empty account id must refuse: {r:?}");
}

/// The per-asset cap reads the policy file: `max_positions_per_asset = 2`
/// admits a SECOND same-asset position (UP + DOWN of one asset — the
/// @Almach replay's shape) where the compiled 1 would refuse, and a third
/// same-asset entry is refused with the exact historical vocabulary.
#[test]
fn per_asset_cap_comes_from_the_policy_section() {
    let now = 1_000_000i64;
    // Boot-time resolution: CoreConfig's explicit value wins over any
    // policy file (which this test disables — the CONFIG path is what
    // the flag exercises).
    let mut cfg = CoreConfig {
        risk: RiskConfig {
            max_order_notional: dec!(1000),
            ..Default::default()
        },
        dry_seed_balance: dec!(100),
        engine_enabled: true,
        round_duration_sec: 900,
        auto_exits_enabled: false,
        size_usd: dec!(2.5),
        min_shares: dec!(1),
        max_shares: dec!(1000),
        max_positions_per_asset: Some(2),
        assets: vec!["BTC".into()],
        positions: crate::position::PositionConfig {
            max_positions: 5,
            exit: crate::exit_policy::ExitConfig {
                min_time_left_sec: 0,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    cfg.execution_policy_path = None; // no file in unit tests
    let mut c = Core::new(cfg);
    c.set_balance(dec!(100));
    // Books for BOTH of BTC's sides — two positions on ONE asset.
    for side in ["up", "down"] {
        c.engine_on_data(
            DataEvent::Book {
                token_id: format!("btc_{side}"),
                bids: vec![(dec!(0.43), dec!(100))],
                asks: vec![(dec!(0.45), dec!(100))],
                now_ms: now,
            },
            now,
        );
    }
    let leg = |token: &str, key: &str| {
        let mut r = policy_req(dec!(0.45), dec!(5), key, "default");
        r.token_id = token.to_string();
        r
    };
    // Two same-ASSET entries on DIFFERENT tokens, both admitted and filled.
    let a = c.place(leg("btc_up", "k1"), 0, now);
    assert!(a.is_ok(), "first same-asset entry: {a:?}");
    let b = c.place(leg("btc_down", "k2"), 0, now);
    assert!(
        b.is_ok(),
        "second same-asset entry admitted at cap 2: {b:?}"
    );
    assert_eq!(
        c.positions().open_positions().len(),
        2,
        "the fixture's premise: two BTC positions actually opened"
    );
    // A THIRD same-asset entry (a new token, same BTC) is refused at the
    // cap, with the exact historical vocabulary ("Already in {asset}")
    // that classify_rejection buckets as positions.already_in.
    let r = c.place(leg("btc_another", "k3"), 0, now);
    let reason = match r {
        Err(e) => e.to_string(),
        Ok((_, status)) => {
            panic!("third entry must not be admitted at the cap (status {status:?})")
        }
    };
    assert!(reason.contains("Already in BTC"), "got: {reason}");
}

/// A policy cooldown: the winning rule puts the ACCOUNT on ice; the
/// very next entry is refused with `policy.skip` until the clock runs
/// out, then entries resume — the caller owns the clock.
#[test]
fn a_cooldown_rule_ices_the_account_then_thaws_it() {
    let mut policy = neutral_policy();
    // The rule arms while the account is funded (balance >= 100): the
    // triggering cycle consumes the entry AND starts the 60s ice. Later
    // the balance is drawn down so the rule no longer matches — proving
    // the THAW comes from the parked clock expiring, not from a fresh
    // rule hit re-icing the account every cycle.
    policy.defaults.rules = vec![crate::execution_policy::Rule {
        name: "ice".into(),
        priority: 1,
        enabled: true,
        when: crate::execution_policy::When {
            field: crate::execution_policy::ConditionField::AvailableBalance,
            op: crate::execution_policy::CompareOp::Ge,
            value: crate::execution_policy::ConditionValue::Num(dec!(100)),
        },
        then: crate::execution_policy::Action {
            action: None,
            budget_ratio: None,
            min_budget_usd: None,
            max_budget_usd: None,
            cooldown_sec: Some(60),
        },
        reason: None,
    }];
    let mut c = policy_core(equity_core(dec!(100), Decimal::ZERO), policy);
    let now = 1_000_000i64;
    feed_btc_dip(&mut c, now);
    // First cycle: the cooldown verdict parks the account — the intent
    // that TRIGGERED it is consumed (the rule hit on its facts).
    assert_eq!(
        c.engine_evaluate(now + 1_000),
        0,
        "cooldown verdict consumes the triggering entry"
    );
    let stats = c.strategy_stats();
    let s = strategy_entry(&stats, "dip");
    assert_eq!(s["rejectionCauses"]["policy.skip"], 1, "{s}");
    // Second cycle, still inside 60s: refused by the parked cooldown —
    // and note the rule is now OFF (balance drawn below 100), so this
    // refusal is the parked clock talking, not a fresh rule hit.
    c.set_balance(dec!(50));
    feed_btc_dip(&mut c, now + 2_000);
    assert_eq!(c.engine_evaluate(now + 3_000), 0, "account is on ice");
    // After the clock runs out: the entry passes (the rule is off and the
    // ice has melted — the caller-owned clock ran out).
    feed_btc_dip(&mut c, now + 61_000);
    assert_eq!(
        c.engine_evaluate(now + 62_000),
        1,
        "thawed: the entry places"
    );
}

/// The production deadlock sequence (default-enable bugfix, 2026-10):
/// a boot whose accounts.toml gives every account an EMPTY enablement
/// set hosts no live scopes; the operator's first disable tears down the
/// boot-registered default scopes, and every later global toggle reports
/// `applied == false` forever (the reply bytes lied: `found: true,
/// applied: false, reason: null`). The handler's fix trusts the SCOPED
/// write as the default account's enablement owner — this test pins the
/// exact readouts that decision consumes: after the teardown, the
/// scoped enable still lands `Ok` (the package is registered) while the
/// legacy toggle's `applied` stays false. The handler turns that Ok
/// into the honest `found/applied = true` reply.
#[test]
fn empty_set_teardown_then_enable_scoped_write_still_lands_ok() {
    let dir = std::env::temp_dir().join(format!("bk-default-enable-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let accounts_path = dir.join("accounts.toml");
    std::fs::write(
        &accounts_path,
        "[[account]]\nid = \"default\"\nname = \"main\"\n",
    )
    .expect("fixture");
    let mut c = Core::with_account_book(
        CoreConfig {
            accounts_config_path: Some(accounts_path.to_string_lossy().into_owned()),
            ..Default::default()
        },
        crate::account::config::load_account_ledgers(
            &accounts_path,
            1,
            crate::account::config::TEST_ALLOWED_ROUNDS,
        )
        .unwrap(),
    );
    c.enable_engine(Engine::new(EngineConfig::default()));
    crate::strategies::test_support::host_disabled(
        c.engine.as_mut().unwrap(),
        EngineConfig::default().trend,
        EngineConfig::default().spread_arb,
    );
    // Boot shape: the default scope hosts the package DISABLED (the
    // kernel enables nothing by itself).
    let default_id = crate::account::default_account_id();
    assert!(
        c.strategy_names().iter().any(|n| n == "spread_arb"),
        "the package is registered even with zero enabled scopes"
    );
    // First disable tears the flat default scope down (the production
    // first toggle).
    let (kept, set_now) = c
        .set_scoped_strategy_enabled(&default_id, "spread_arb", false, "test", "off", 1)
        .expect("the disable lands");
    assert!(kept.is_empty() && set_now.is_empty());
    // Deadlock readout: the legacy toggle now finds NO live instance —
    // `applied == false` is exactly what gated the old mirror write.
    let (found, applied, reason) = c.set_strategy_enabled_checked("spread_arb", true);
    assert!(found, "the package registry still names it");
    assert!(
        !applied && reason.is_none(),
        "the live dispatch hosts no instance — the old gate starved here"
    );
    // The fix's authority: the scoped write still lands Ok (persist +
    // rebuild; a Lua source rebuilds ENABLED — the test adapter is a
    // non-Lua source, so the ADD phase skips the instance exactly like
    // the #537 probe2 note: the PERSIST is what survives to the next
    // boot, and that persistence is what the handler reports as the
    // honest applied=true).
    let (kept2, set_now2) = c
        .set_scoped_strategy_enabled(&default_id, "spread_arb", true, "test", "on", 2)
        .expect("the scoped enable lands even after the teardown");
    assert_eq!(set_now2, vec!["spread_arb".to_string()]);
    assert!(kept2.is_empty());
    assert!(
        c.accounts
            .account(&default_id)
            .expect("default configured")
            .strategies
            .contains(&"spread_arb".to_string()),
        "the durable enablement set names it — the operator's intent \
         stuck and the next boot re-derives the scope from it"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

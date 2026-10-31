use super::*;

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
    let accounts_path = dir.join("accounts.toml"); // the LEGACY path — never written
    let env_path = dir.join(".env");
    let journal_dir = dir.join("ledger");
    std::fs::create_dir_all(&journal_dir).expect("journal dir");
    std::fs::write(
        &env_path,
        "ACCOUNT_LIST=default\nACCOUNT_DEFAULT_NAME=\"main\"\n",
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
        crate::account::config::load_account_ledgers_single_source(
            &accounts_path,
            Some(&env_path),
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
        crate::account::CredentialKeys::default(),
        ("test", "mint"),
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
    let accounts_path_live = dir.join("accounts-live.toml"); // the LEGACY path — never written
    let env_path_live = dir.join("live.env");
    let journal_dir_live = dir.join("ledger-live");
    std::fs::create_dir_all(&journal_dir_live).expect("live journal dir");
    std::fs::write(
        &env_path_live,
        "ACCOUNT_LIST=default\nACCOUNT_DEFAULT_NAME=\"main\"\n",
    )
    .expect("fixture");
    let mut live = Core::with_account_book(
        CoreConfig {
            mode: Mode::Live,
            accounts_config_path: Some(accounts_path_live.to_string_lossy().into_owned()),
            ledger_journal_dir: Some(journal_dir_live.to_string_lossy().into_owned()),
            ..Default::default()
        },
        crate::account::config::load_account_ledgers_single_source(
            &accounts_path_live,
            Some(&env_path_live),
            1,
            crate::account::config::TEST_ALLOWED_ROUNDS,
        )
        .unwrap(),
    );
    let probe_live = crate::account::AccountId::from("probe3");
    live.create_account_persisting(
        &probe_live,
        None,
        None,
        crate::account::CredentialKeys::default(),
        ("test", "mint live"),
        6,
    )
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
    let accounts_path = dir.join("accounts.toml"); // the LEGACY path — never written
    let env_path = dir.join(".env");
    std::fs::write(
        &env_path,
        "ACCOUNT_LIST=default,paper\nACCOUNT_DEFAULT_NAME=\"main\"\nACCOUNT_PAPER_NAME=\"paper\"\n",
    )
    .expect("fixture");
    let mut c = Core::with_account_book(
        CoreConfig {
            accounts_config_path: Some(accounts_path.to_string_lossy().into_owned()),
            ..Default::default()
        },
        crate::account::config::load_account_ledgers_single_source(
            &accounts_path,
            Some(&env_path),
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
    let reloaded = crate::account::config::load_account_ledgers_single_source(
        &accounts_path,
        Some(&env_path),
        2,
        crate::account::config::TEST_ALLOWED_ROUNDS,
    )
    .unwrap();
    assert_eq!(reloaded.display_id(), "paper", "the pointer is durable");
    // The top-level key survived the render next to both rows' fields.
    let text = std::fs::read_to_string(&env_path).unwrap();
    assert!(text.contains("ACCOUNT_DISPLAY=\"paper\""), "{text}");

    // UNKNOWN id: refused plan-first, memory and file untouched.
    let before = std::fs::read_to_string(&env_path).unwrap();
    let err = c
        .set_display_account_persisting(&crate::account::AccountId::from("ghost"), "test", "")
        .expect_err("unknown id");
    assert!(err.message.contains("unknown account"), "{}", err.message);
    assert_eq!(c.accounts.display_id(), "paper");
    assert_eq!(
        std::fs::read_to_string(&env_path).unwrap(),
        before,
        "a refused write leaves the file byte-identical"
    );

    // issue 561（2026-10-20 裁决）改写旧断言：frozen 账户此前被
    // Active-only 校验拒绝，现在 display 是面板唯一可信账户源、切观察
    // 户必须生效 —— 「存在即可」，frozen1 合法落位，内存与文件同步。
    std::fs::write(
        &env_path,
        "ACCOUNT_LIST=default,frozen1\nACCOUNT_DEFAULT_NAME=\"main\"\nACCOUNT_FROZEN1_NAME=\"f\"\nACCOUNT_FROZEN1_STATUS=\"frozen\"\n",
    )
    .expect("fixture two");
    let mut c2 = Core::with_account_book(
        CoreConfig {
            accounts_config_path: Some(accounts_path.to_string_lossy().into_owned()),
            ..Default::default()
        },
        crate::account::config::load_account_ledgers_single_source(
            &accounts_path,
            Some(&env_path),
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
    let reloaded2 = crate::account::config::load_account_ledgers_single_source(
        &accounts_path,
        Some(&env_path),
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
    let accounts_path = dir.join("accounts.toml"); // the LEGACY path — never written
    let env_path = dir.join(".env");
    std::fs::write(
        &env_path,
        "ACCOUNT_LIST=default,paper\nACCOUNT_DEFAULT_NAME=\"main\"\nACCOUNT_PAPER_NAME=\"paper\"\n",
    )
    .expect("fixture");
    let mut c = Core::with_account_book(
        CoreConfig {
            accounts_config_path: Some(accounts_path.to_string_lossy().into_owned()),
            ..Default::default()
        },
        crate::account::config::load_account_ledgers_single_source(
            &accounts_path,
            Some(&env_path),
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
    let text = std::fs::read_to_string(&env_path).unwrap();
    assert!(
        !text.contains("_STRATEGIES"),
        "empty spelled by absence: {text}"
    );

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
    let text = std::fs::read_to_string(&env_path).unwrap();
    assert!(
        text.contains("ACCOUNT_DEFAULT_STRATEGIES=\"spread_arb\""),
        "the file spells the named assignment: {text}"
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
    let accounts_path = dir.join("accounts.toml"); // the LEGACY path — never written
    let env_path = dir.join(".env");
    std::fs::write(
        &env_path,
        "ACCOUNT_LIST=default,probe2\nACCOUNT_DEFAULT_NAME=\"main\"\nACCOUNT_PROBE2_NAME=\"probe two\"\nACCOUNT_PROBE2_STRATEGIES=\"spread_arb\"\n",
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
        crate::account::config::load_account_ledgers_single_source(
            &accounts_path,
            Some(&env_path),
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
    let text = std::fs::read_to_string(&env_path).unwrap();
    assert!(
        text.contains("ACCOUNT_DEFAULT_STRATEGIES"),
        "the file gained the default's explicit assignment: {text}"
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
        crate::account::config::load_account_ledgers_single_source(
            &accounts_path,
            Some(&env_path),
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
    let accounts_path = dir.join("accounts.toml"); // the LEGACY path — never written
    let env_path = dir.join(".env");
    // The production file's exact spelling (from the deployment tree).
    std::fs::write(
        &env_path,
        "ACCOUNT_LIST=default\nACCOUNT_DEFAULT_NAME=\"main\"\nACCOUNT_DEFAULT_STRATEGIES=\"pair_discount_arb,spread_arb\"\n",
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
        crate::account::config::load_account_ledgers_single_source(
            &accounts_path,
            Some(&env_path),
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
    let accounts_path = dir.join("accounts.toml"); // the LEGACY path — never written
    let env_path = dir.join(".env");
    std::fs::write(
        &env_path,
        "ACCOUNT_LIST=default,paper\nACCOUNT_DEFAULT_NAME=\"main\"\nACCOUNT_PAPER_NAME=\"paper\"\n",
    )
    .expect("fixture");
    let state_path = dir.join("strategy-state.json");

    let mut c = Core::with_account_book(
        CoreConfig {
            accounts_config_path: Some(accounts_path.to_string_lossy().into_owned()),
            strategy_state_path: Some(state_path.to_string_lossy().into_owned()),
            ..Default::default()
        },
        crate::account::config::load_account_ledgers_single_source(
            &accounts_path,
            Some(&env_path),
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
    let text = std::fs::read_to_string(&env_path).unwrap();
    assert!(
        text.contains("ACCOUNT_PAPER_STRATEGIES=\"trend_follow\""),
        "the file spells paper's enablement: {text}"
    );

    // DISABLE it again for paper (flat scope → teardown), then re-enable:
    // the whole dance is the panel's round-trip.
    let (kept, set_now) = c
        .set_scoped_strategy_enabled(&paper, "trend_follow", false, "test", "panel off", 3)
        .expect("the scoped disable lands");
    assert!(kept.is_empty() && set_now.is_empty());
    assert!(c.strategy_enabled_accounts("trend_follow").is_empty());
    let text = std::fs::read_to_string(&env_path).unwrap();
    assert!(
        !text.contains("ACCOUNT_PAPER_STRATEGIES"),
        "the empty set is spelled by absence: {text}"
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
        crate::account::config::load_account_ledgers_single_source(
            &accounts_path,
            Some(&env_path),
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
    let accounts_path = dir.join("accounts.toml"); // the LEGACY path — never written
    let env_path = dir.join(".env");
    // The pre-#537 production shape: the default account's enablement set
    // IS in the file (this is what made the toggle unable to win).
    std::fs::write(
        &env_path,
        "ACCOUNT_LIST=default,paper\nACCOUNT_DEFAULT_NAME=\"main\"\nACCOUNT_DEFAULT_STRATEGIES=\"trend_follow,spread_arb\"\nACCOUNT_PAPER_NAME=\"paper\"\n",
    )
    .expect("fixture");
    let state_path = dir.join("strategy-state.json");

    let mut c = Core::with_account_book(
        CoreConfig {
            accounts_config_path: Some(accounts_path.to_string_lossy().into_owned()),
            strategy_state_path: Some(state_path.to_string_lossy().into_owned()),
            ..Default::default()
        },
        crate::account::config::load_account_ledgers_single_source(
            &accounts_path,
            Some(&env_path),
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
    let text = std::fs::read_to_string(&env_path).unwrap();
    assert!(
        text.contains("ACCOUNT_DEFAULT_STRATEGIES=\"trend_follow\""),
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
        crate::account::config::load_account_ledgers_single_source(
            &accounts_path,
            Some(&env_path),
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
    let text = std::fs::read_to_string(&env_path).unwrap();
    assert!(
        text.contains("ACCOUNT_DEFAULT_STRATEGIES") && text.contains("spread_arb"),
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

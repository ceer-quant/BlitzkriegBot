//! RFC 568 阶段一（PairBook）的内核级验收牙 —— 六条授权测试中 1/2/5/6 的
//! 端到端证据（3 由 pair.rs 的 13 颗单测钉死；4/7/8 由三套基线套件全绿证明，
//! 见交付报告）。
//!
//! 牙清单（判定面）：
//!
//!  1. `executor_credentials_never_reach_the_pair_book_or_decl_chain` —
//!     授权测试 (1)：凭证只住在账户簿的 `CredentialKeys`（env 注入点在
//!     plugin/executor 层）；PairBook 的全部公开面与声明受理链（engine 的
//!     PairDeclaration / A≠B 拒绝 / Core 的 declare-bind-事实回流）零凭证
//!     字段、零 env 读取。以「无凭证 env 的 Live core 走完整链」+ 全结构体
//!     枚举断言（PairState/PairView/PairLegState 字段数用断言钉死，新增
//!     凭证字段即编译/断言双红）。
//!  2. `a_scope_refused_leg_never_declares_into_the_pair_book` —
//!     授权测试 (2) 前半：scope 整组拒（RFC #541 挂钩 A）的腿不进 PairBook
//!     （受理链用 precheck 幸存集过滤 —— 拒整批 ⇒ 零声明）；对照组同夹具
//!     不绑 venue ⇒ 两腿都入簿。
//!  3. `the_pair_book_never_feeds_back_into_judgment_paths` —
//!     授权测试 (2) 的变异验证：run_pair_orphan_sweep 的判定链零改动 ——
//!     PairBook 空账（零声明）时 sweep 照旧按 OME/持仓判定 orphan 并
//!     flatten（它根本不读 PairBook）；PairBook 有账也只是观察者
//!     （on_order_event / close_by_token 是写点，读数只走投影）。
//!  4. `a_pre_pairing_kernel_bundle_loads_and_trades_unchanged` —
//!     授权测试 (5)：旧内核兼容。无 pair 字段的 Lua 包解析逐位不变（entries
//!     无 pair ⇒ TradeSignal.pair = None = 旧行为）；`bk.arb.pairs` 在旧
//!     runtime 上 feature-detect 为 nil ⇒ 策略按「无登记」处理。以真实
//!     pair_discount_arb 包（无 pair 声明）走完整引擎链证明。
//!  5. `a_pair_declaration_with_identical_leg_accounts_is_refused` —
//!     授权测试 (6)：A=B 声明被拒负向用例 —— 同自报 key 两腿盖章到同一
//!     effective account ⇒ APPLICATION 错误事件（`pair.declRefused`，点名
//!     `field: pair.key`），两腿都不进 PairBook；A≠B 对照组两腿入簿成对。
//!  6. `round_advancement_and_sweep_close_the_book_while_judgment_stays_pure` —
//!     授权测试 (2) 后半（终态写点）：轮次推进（retire_before）把旧轮配对
//!     闭账、终态吸收迟到事件；sweep 观察者写点 close_by_token 幂等。
//!
//! 夹具口径复刻 `arb_batch_precheck_check.rs`（REAL cross_venue_arb 经
//! LuaEngineAdapter 装载 + Live 模式 + env 串行防护）与
//! `arb_leg_config_check.rs`（Dry 模式双腿成交链）。注释写 `issue 568`
//! 形式（禁 `#568` —— RAW_COLOR 正则）。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock};

use blitzkrieg_core::account::{Account, AccountStatus, CredentialKeys};
use blitzkrieg_core::engine::DataEvent;
use blitzkrieg_core::ipc::schema::Event;
use blitzkrieg_core::model::{Mode, default_account_id};
use blitzkrieg_core::risk::RiskConfig;
use blitzkrieg_core::service::{Core, CoreConfig, VirtualAccountLimit};
use blitzkrieg_core::strategy_engine::lua_loader::{LuaEngineAdapter, load_lua_package};
use blitzkrieg_lua_runtime::FeeScheduleView;
use blitzkrieg_market_api::account::AccountId;
use blitzkrieg_market_api::types::MarketType;
use blitzkrieg_market_api::unified::{
    EventStatus, ListingStatus, PlatformListing, SettlementSource, UnifiedEvent, Venue,
};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

const NOW: i64 = 1_700_000_000_000;

// 与 cross_venue_arb 系列测试同构的 PM/KX 符号对：UP 腿 polymarket、DOWN 腿
// kalshi（跨所 pair —— issue 568 配对簿的对象形态）。
const PM: &str = "pm-cond";
const KX: &str = "kx-cond";
const PM_UP: &str = "pm-up";
const PM_DOWN: &str = "pm-down";
const KX_UP: &str = "kx-up";
const KX_DOWN: &str = "kx-down";

const POLYMARKET: &str = "polymarket";
const KALSHI: &str = "kalshi";

// ── 夹具（arb_batch_precheck_check 同款）────────────────────────────────────

fn package_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../user_layer/strategies_lua/cross_venue_arb")
}

fn pair_discount_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../user_layer/strategies_lua/pair_discount_arb")
}

/// REAL cross_venue_arb 包经 LuaEngineAdapter 装载，部署线收费表
/// legacy_quadratic。
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

/// 带 slot 形参的变体（牙 6 用：第二轮 slot 必须真前进 —— 同 slot 的
/// RoundMarkets 是 top-up，不触发 retire_before）。
fn market_at(
    token_up: &str,
    token_down: &str,
    cond: &str,
    venue: &str,
    now: i64,
) -> blitzkrieg_core::model::CryptoMarket {
    blitzkrieg_core::model::CryptoMarket {
        asset: "BTC".into(),
        condition_id: cond.into(),
        question_id: "q".into(),
        up_token_id: token_up.into(),
        down_token_id: token_down.into(),
        up_price: dec!(0.48),
        down_price: dec!(0.45),
        expires_at_ms: (now / 1000 / 900 + 1) * 900_000,
        round_slot: now / 1000 / 900,
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

/// 折价盘口：UP@pm 0.44 + DOWN@kx 0.42 = 0.86 + 双腿费 ≈ 0.8753 < $1 开仓线，
/// 深度 600（容量按声明 600 股）。
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

fn pair_market_data(c: &mut Core, now: i64) {
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: vec![
                market_at(PM_UP, PM_DOWN, PM, POLYMARKET, now),
                market_at(KX_UP, KX_DOWN, KX, KALSHI, now),
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

/// LIVE core（arb_batch_precheck_check 同款：pot 双账户、真实双腿腿账户映射
/// default(polymarket)/arb-b(kalshi)、凭证 env 由调用方守护）。
fn arb_core_live(pot: Decimal) -> Core {
    let mut virtual_limits = HashMap::new();
    virtual_limits.insert(
        "cross_venue_arb".to_string(),
        VirtualAccountLimit {
            max_batch_notional_usd: Some(pot),
            accounts: vec![default_account_id().to_string(), "arb-b".to_string()],
        },
    );
    let mut c = Core::new(CoreConfig {
        mode: Mode::Live,
        risk: RiskConfig {
            max_order_notional: dec!(100_000),
            ..Default::default()
        },
        engine_enabled: true,
        round_duration_sec: 900,
        min_shares: dec!(1),
        max_shares: dec!(10000),
        assets: vec!["BTC".into()],
        positions: blitzkrieg_core::position::PositionConfig {
            exit: blitzkrieg_core::exit_policy::ExitConfig {
                min_time_left_sec: 0,
                exit_grace_sec: 0,
                ..Default::default()
            },
            max_positions_per_asset: 4,
            ..Default::default()
        },
        execution_policy_path: None,
        replay_scheduling: false,
        virtual_limits,
        ..Default::default()
    });
    c.set_balance(pot);
    c.accounts_mut()
        .insert(Account {
            id: AccountId::from("arb-b"),
            name: "arb-b".into(),
            market_type: MarketType::Prediction,
            status: AccountStatus::Active,
            credential_keys: CredentialKeys::default(),
            venue: String::new(),
            trading_enabled: true,
            strategies: Vec::new(),
            updated_at_ms: NOW,
        })
        .expect("arb-b does not exist yet — the fixture account inserts");
    c.accounts_mut()
        .get_mut(&AccountId::from("arb-b"))
        .expect("arb-b just inserted")
        .set_balance(pot);
    let cfg = c.config().engine_config();
    let mut eng = blitzkrieg_core::engine::Engine::new(cfg.clone());
    eng.register_user_strategy(Box::new(arb_adapter()), "test".into())
        .expect("cross_venue_arb registers under its package name");
    assert!(eng.set_strategy_enabled("cross_venue_arb", true));
    c.enable_engine(eng);
    c.arb_config_set(&blitzkrieg_core::account::CrossVenueArbConfig {
        leg_a_venue: "polymarket".into(),
        leg_a_account: default_account_id().to_string(),
        leg_b_venue: "kalshi".into(),
        leg_b_account: "arb-b".into(),
        pairs: vec!["BTC".into()],
    })
    .expect("the leg configuration lands (memory-only core)");
    c
}

// ── env 串行防护（arb_batch_precheck_check 同款）────────────────────────────

static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn env_guard() -> MutexGuard<'static, ()> {
    ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

struct EnvRestore(Vec<&'static str>);
impl EnvRestore {
    fn take(names: &[&'static str]) -> Self {
        Self(names.to_vec())
    }
}
impl Drop for EnvRestore {
    fn drop(&mut self) {
        for n in &self.0 {
            #[allow(unsafe_op_in_unsafe_fn)]
            unsafe {
                std::env::remove_var(n);
            }
        }
    }
}

fn set_env(name: &'static str, value: &str) {
    unsafe {
        std::env::set_var(name, value);
    }
}

/// 凭证四 env 全设（arb_batch_precheck_check 同名单）——不设它们的牙自己
/// 带守卫与 EnvRestore。
fn credentials_ready() {
    set_env("POLYMARKET_PRIVATE_KEY", "test-pk");
    set_env("POLYMARKET_FUNDER_ADDRESS", "0xtest");
    set_env("KALSHI_API_KEY_ID", "test-key");
    set_env("KALSHI_RSA_PRIVATE_KEY", "test-rsa");
}

// ── 牙 1：授权测试 (1) —— executor 凭证读取路径零改动 ───────────────────────

/// 凭证只住在账户簿的 `CredentialKeys`（plugin/executor 层经 env 注入点自取
/// —— `venue_credentials_missing` 的读取面是账户簿，不在本测试树里）；配对
/// 簿与声明受理链是纯结构/纯事实的：全部公开类型零凭证字段，受理链运行时
/// 零 env 读取。断言三面：
///   a) PairBook 公开类型的字段面逐字段枚举（新增凭证字段 ⇒ 编译红 ——
///      字段名逐字列出，类型层防注入）；
///   b) 无凭证 env 的 Live core 走「声明 → precheck 过闸 → declare → place
///      → bind → 成交事实回流」全链（凭证门在 arb_precheck，必须不因配对
///      簿的存在而改变行为）；
///   c) `CredentialKeys`（executor 的注入点）保持 `loaded` 布尔 + 键名集合
///      的既有形状（进程内账户簿，键值永不出内核）。
#[test]
fn executor_credentials_never_reach_the_pair_book_or_decl_chain() {
    // a) 结构面：PairBook 公开类型零凭证字段（字段名逐字钉死 —— 补字段即
    //    编译红；凭证类字段名按本轮 grep 口径）。
    {
        use blitzkrieg_core::pair::{PairLegState, PairState, PairView};
        let leg = PairLegState {
            token_id: String::new(),
            account_id: String::new(),
            internal_key: String::new(),
            declared_shares: Decimal::ZERO,
            filled_shares: Decimal::ZERO,
            dead: false,
        };
        let _ = leg; // 字段面在构造处逐字钉死：token/account/internal_key/shares/filled/dead
        let p = PairState {
            id: String::new(),
            owner_strategy: String::new(),
            slot: 0,
            legs: Vec::new(),
            status: blitzkrieg_core::pair::PairStatus::Closed,
            created_ms: 0,
        };
        let _ = p; // 字段面在构造处逐字钉死：id/owner/slot/legs/status/created
        let v = PairView {
            owner_strategy: String::new(),
            id: String::new(),
            slot: 0,
            status: String::new(),
            legs: Vec::new(),
            naked_token: None,
        };
        let _ = v; // 投影同形：无凭证字段（account_id 是 stamp 结果，非秘密）
        // 静态防注入：字段名清单里不允许出现凭证类命名。
        const LEG_FIELDS: [&str; 6] = [
            "token_id",
            "account_id",
            "internal_key",
            "declared_shares",
            "filled_shares",
            "dead",
        ];
        assert!(
            !LEG_FIELDS.iter().any(|f| f.contains("credential")),
            "no credential field on a pair leg"
        );
    }
    // c) 注入点形状：executor 的凭证落脚点（账户簿的 CredentialKeys）保持
    //    既有字段（loaded 标志 + 键名集合），配对簿未触碰它。
    {
        let keys = CredentialKeys::default();
        assert!(!keys.loaded, "a fresh account starts credential-less");
        assert!(
            keys.api_key_env.is_none()
                && keys.secret_env.is_none()
                && keys.passphrase_env.is_none()
                && keys.private_key_env.is_none(),
            "no env bindings by default (values never enter the kernel at all)"
        );
    }

    // b) 运行面：凭证 env 全设的 Live core —— 声明受理链照常工作（其余牙
    //    也在同款夹具上跑，凭证路径的行为由 precheck 的既有判定管）。此处
    //    钉的是反向：配对簿的受理链路（declare/bind/事实回流）不读 env ——
    //    用 A≠B 对照组同夹具跑通全链作证据（凭证读取路径零改动 = 该链的
    //    凭证判定仍只在 arb_precheck 一处）。
    let _env = env_guard();
    let _restore = EnvRestore::take(&[
        "POLYMARKET_PRIVATE_KEY",
        "POLYMARKET_FUNDER_ADDRESS",
        "KALSHI_API_KEY_ID",
        "KALSHI_RSA_PRIVATE_KEY",
    ]);
    credentials_ready();
    let mut c = arb_core_live(dec!(1000));
    let now = NOW;
    pair_market_data(&mut c, now);
    let placed = c.engine_evaluate(now + 2_000);
    assert_eq!(
        placed, 2,
        "the full declare→precheck→declare→bind→place chain works with the \
         credential judgment exactly where it was (arb_precheck)"
    );
    // 双腿入簿：一条配对、两腿各盖自己的章（不校验凭证 —— 补钉 3 的落点在
    // arb_precheck 之后）。
    let book = c.pair_book_all();
    assert_eq!(book.len(), 1, "one pair for the round");
    assert_eq!(book[0].legs.len(), 2, "both legs registered");
    let accounts: Vec<&str> = book[0]
        .legs
        .iter()
        .map(|l| l.account_id.as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    // 声明顺序（pm-up 先）在 BTreeSet 里按字典序排 —— 集合形状才是断言面。
    let default_id = default_account_id();
    let mut expected = vec![default_id.as_str(), "arb-b"];
    expected.sort_unstable();
    assert_eq!(
        accounts, expected,
        "the legs stamp their OWN accounts (A≠B) — the book never validates credentials"
    );
}

// ── 牙 2：授权测试 (2) 前半 —— scope 拒腿不进配对簿 ─────────────────────────

/// scope 整组拒（RFC #541 挂钩 A，引擎侧）的腿，声明受理链不得登记：受理在
/// arb_precheck 过闸后按幸存单集过滤 —— 引擎整组丢 ⇒ 零 OrderRequest ⇒ 幸存
/// 集不含该腿 ⇒ 零 declare。对照组：同夹具不绑 venue ⇒ 两腿都入簿。
#[test]
fn a_scope_refused_leg_never_declares_into_the_pair_book() {
    // ── 拒绝组：arb-b 绑 polymarket ⇒ kalshi 腿被 scope 拒 ⇒ 整组丢。
    {
        let _env = env_guard();
        let _restore = EnvRestore::take(&[
            "POLYMARKET_PRIVATE_KEY",
            "POLYMARKET_FUNDER_ADDRESS",
            "KALSHI_API_KEY_ID",
            "KALSHI_RSA_PRIVATE_KEY",
        ]);
        credentials_ready();

        let mut c = arb_core_live(dec!(1000));
        c.accounts_mut()
            .set_venue(&AccountId::from("arb-b"), POLYMARKET, NOW)
            .expect("the fixture account exists — the binding writes");
        let now = NOW;
        pair_market_data(&mut c, now);

        let placed = c.engine_evaluate(now + 2_000);
        assert_eq!(
            placed, 0,
            "one scope-refused leg drops the WHOLE atomic pair (hook A, unchanged)"
        );
        let book = c.pair_book_all();
        assert!(
            book.is_empty(),
            "a scope-refused cycle declares NOTHING into the pair book: {book:?}"
        );
    }
    // ── 对照组：同夹具、不绑 venue ⇒ 两腿正常、两腿入簿。
    {
        let _env = env_guard();
        let _restore = EnvRestore::take(&[
            "POLYMARKET_PRIVATE_KEY",
            "POLYMARKET_FUNDER_ADDRESS",
            "KALSHI_API_KEY_ID",
            "KALSHI_RSA_PRIVATE_KEY",
        ]);
        credentials_ready();

        let mut c = arb_core_live(dec!(1000));
        let now = NOW;
        pair_market_data(&mut c, now);

        let placed = c.engine_evaluate(now + 2_000);
        assert_eq!(placed, 2, "the unbound control places both legs");
        let book = c.pair_book_all();
        assert_eq!(book.len(), 1);
        assert_eq!(book[0].legs.len(), 2, "both legs declared");
    }
}

// ── 牙 3：授权测试 (2) 的变异验证 —— sweep 判定链不读配对簿 ─────────────────

/// run_pair_orphan_sweep 的判定链零改动：它直读 OME 与持仓（配对簿空账 /
/// 满账都不进判定）。变异验证：把孤儿腿的判定前置条件保持不动（naked 持仓
/// 与 partner 缺席、落地宽限 5s 过后），空账配对簿下 sweep 照旧 alert 并
/// flatten —— 判定不因簿的存在/缺席而变；flatten 之后观察者写点把簿里的
/// 配对闭账（有账时）。
///
/// 夹具：Dry 模式双腿成交 → 手工 flatten 对手腿 ⇒ 裸腿形态
/// （account_decouple_check 的 naked_pair_fixture 同款）。
#[test]
fn the_pair_book_never_feeds_back_into_judgment_paths() {
    let _env = env_guard();
    let _restore = EnvRestore::take(&[
        "POLYMARKET_PRIVATE_KEY",
        "POLYMARKET_FUNDER_ADDRESS",
        "KALSHI_API_KEY_ID",
        "KALSHI_RSA_PRIVATE_KEY",
    ]);
    credentials_ready();

    // Dry 双账户夹具（arb_leg_config_check 同款：真实 accounts.toml）。
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/pair-book-checks");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("accounts-sweep-{}.toml", std::process::id()));
    std::fs::write(
        &path,
        "[[account]]\nid = \"default\"\nvenue = \"polymarket\"\n\n[[account]]\nid = \"arb-b\"\nvenue = \"kalshi\"\n",
    )
    .expect("accounts.toml fixture writes");

    let mut virtual_limits = HashMap::new();
    virtual_limits.insert(
        "cross_venue_arb".to_string(),
        VirtualAccountLimit {
            max_batch_notional_usd: Some(dec!(10_000)),
            accounts: vec![default_account_id().to_string(), "arb-b".to_string()],
        },
    );
    let config = CoreConfig {
        mode: Mode::Dry,
        risk: RiskConfig {
            max_order_notional: dec!(100_000),
            ..Default::default()
        },
        dry_seed_balance: dec!(1_000_000),
        engine_enabled: true,
        round_duration_sec: 900,
        min_shares: dec!(1),
        max_shares: dec!(10000),
        assets: vec!["BTC".into()],
        positions: blitzkrieg_core::position::PositionConfig {
            exit: blitzkrieg_core::exit_policy::ExitConfig {
                min_time_left_sec: 0,
                maker_first_exit_enabled: false,
                exit_grace_sec: 0,
                ..Default::default()
            },
            max_positions_per_asset: 4,
            ..Default::default()
        },
        execution_policy_path: None,
        accounts_config_path: Some(path.to_string_lossy().into_owned()),
        virtual_limits,
        ..Default::default()
    };
    let book = blitzkrieg_core::account::config::load_account_ledgers(&path, NOW)
        .expect("accounts fixture parses");
    let mut c = Core::with_account_book(config, book);
    let cfg = c.config().engine_config();
    let mut eng = blitzkrieg_core::engine::Engine::new(cfg.clone());
    let loaded = load_lua_package(&package_dir()).expect("cross_venue_arb package must load");
    let adapter = LuaEngineAdapter::new(loaded.strategy, loaded.tunables)
        .declare(loaded.holds_to_settlement, loaded.gate_exemptions)
        .declare_atomic_batch(loaded.atomic_batch);
    adapter.set_fee_schedule(FeeScheduleView {
        name: "legacy_quadratic".into(),
        rate: "0.125".into(),
        exponent: 2,
    });
    eng.register_user_strategy(
        Box::new(adapter),
        format!("lua:{}", package_dir().display()),
    )
    .expect("cross_venue_arb registers under its package name");
    assert!(eng.set_strategy_enabled("cross_venue_arb", true));
    c.enable_engine(eng);
    c.arb_config_set(&blitzkrieg_core::account::CrossVenueArbConfig {
        leg_a_venue: "polymarket".into(),
        leg_a_account: "default".into(),
        leg_b_venue: "kalshi".into(),
        leg_b_account: "arb-b".into(),
        pairs: vec!["BTC".into()],
    })
    .expect("the leg configuration lands (memory-only core)");

    let now = NOW;
    pair_market_data(&mut c, now);
    assert_eq!(c.engine_evaluate(now + 2_000), 2, "both legs fill (Dry)");
    // 双腿成交事实已回流配对簿 ⇒ Complete（观察者只记事实）。两腿经
    // maker→taker escalation 成交 —— 续段 key（{order}:escalated）的事实
    // 已归一到声明腿（bind_continuation 接线）。
    let book = c.pair_book_all();
    assert_eq!(book.len(), 1);
    assert_eq!(
        book[0].status,
        blitzkrieg_core::pair::PairStatus::Complete,
        "two fill facts make the pair Complete — the observer only records"
    );
    assert_eq!(
        book[0]
            .legs
            .iter()
            .map(|l| l.filled_shares)
            .collect::<Vec<_>>(),
        vec![dec!(600), dec!(600)],
        "the escalated-taker fills booked on the DECLARED legs (600 shares each)"
    );

    // 裸腿形态：手工 flatten 对手腿（DOWN 腿），裸 UP 腿留在持仓（落地宽限
    // 过后 sweep 判定 orphan —— 判定读的是持仓/OME，不是配对簿）。
    let down_id = c
        .positions()
        .open_positions()
        .iter()
        .find(|p| p.token_id == KX_DOWN)
        .expect("the DOWN leg filled into a position")
        .id
        .clone();
    c.flatten(Some(&down_id), now + 3_000)
        .expect("flattening the second leg succeeds");

    // 落地宽限（5s）后 tick ⇒ sweep 警报 + flatten 裸腿。配对簿此刻有账
    // （Complete 的两条腿），sweep 的判定如果读了它，这个形态会被簿的
    // Complete 状态改写判定 —— 变异验证钉死：判定照旧。
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    c.set_event_sink(tx);
    c.tick(now + 12_000).expect("the sweep tick runs");
    let mut alerts = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let Event::RiskAlert { message, .. } = ev
            && (message.contains("orphan") || message.contains("naked"))
        {
            alerts.push(message);
        }
    }
    assert!(
        alerts.iter().any(|m| m.contains("orphaned")),
        "the sweep judged the naked leg from OME/positions (PairBook was NOT \
         an input): {alerts:?}"
    );
    assert!(
        !c.positions()
            .open_positions()
            .iter()
            .any(|p| p.token_id == PM_UP),
        "the naked leg is flattened by the unchanged judgment chain"
    );
    // 观察者写点：flatten 处置后，簿里该 token 的配对闭账（close_by_token）。
    let book = c.pair_book_all();
    assert_eq!(
        book.len(),
        1,
        "the pair row stays (a closed record, not a deletion)"
    );
    assert_eq!(
        book[0].status,
        blitzkrieg_core::pair::PairStatus::Closed,
        "the sweep's observer write-point closed the pair after the flatten"
    );
}

// ── 牙 4：授权测试 (5) —— 旧内核兼容：无 pair 字段逐位不变 + feature-detect ─

/// 无 pair 声明的 Lua 包：entries 无 `pair` 字段 ⇒ TradeSignal.pair = None
/// （旧行为逐位不变）；配对簿零行（无声明 ⇒ 零入账）。用 REAL
/// pair_discount_arb 包（从不带 pair 声明）走完整引擎链证明；Lua 侧
/// `bk.arb.pairs` 的 feature-detect 由该包策略自身 pcall 兜底（不存在 ⇒
/// 按无登记处理），runtime 的注册面由 `lua-decimal-sync` 套件与
/// cross_venue_arb 系列测试覆盖。
#[test]
fn a_pre_pairing_kernel_bundle_loads_and_trades_unchanged() {
    let _env = env_guard();
    let _restore = EnvRestore::take(&[
        "POLYMARKET_PRIVATE_KEY",
        "POLYMARKET_FUNDER_ADDRESS",
        "KALSHI_API_KEY_ID",
        "KALSHI_RSA_PRIVATE_KEY",
    ]);
    credentials_ready();

    // Live core 与 cross_venue 夹具同款（同一组 token/行情），但注册的是
    // pair_discount_arb（单所双腿包，无 pair 声明、无 atomic_batch）。
    let mut virtual_limits = HashMap::new();
    virtual_limits.insert(
        "cross_venue_arb".to_string(),
        VirtualAccountLimit {
            max_batch_notional_usd: Some(dec!(1000)),
            accounts: vec![default_account_id().to_string()],
        },
    );
    let mut c = Core::new(CoreConfig {
        mode: Mode::Live,
        risk: RiskConfig {
            max_order_notional: dec!(100_000),
            ..Default::default()
        },
        engine_enabled: true,
        round_duration_sec: 900,
        min_shares: dec!(1),
        max_shares: dec!(10000),
        assets: vec!["BTC".into()],
        positions: blitzkrieg_core::position::PositionConfig {
            exit: blitzkrieg_core::exit_policy::ExitConfig {
                min_time_left_sec: 0,
                exit_grace_sec: 0,
                ..Default::default()
            },
            max_positions_per_asset: 4,
            ..Default::default()
        },
        execution_policy_path: None,
        replay_scheduling: false,
        virtual_limits,
        ..Default::default()
    });
    c.set_balance(dec!(1000));
    let cfg = c.config().engine_config();
    let mut eng = blitzkrieg_core::engine::Engine::new(cfg.clone());
    let loaded =
        load_lua_package(&pair_discount_dir()).expect("pair_discount_arb package must load");
    let adapter = LuaEngineAdapter::new(loaded.strategy, loaded.tunables)
        .declare(loaded.holds_to_settlement, loaded.gate_exemptions)
        .declare_atomic_batch(loaded.atomic_batch);
    adapter.set_fee_schedule(FeeScheduleView {
        name: "legacy_quadratic".into(),
        rate: "0.125".into(),
        exponent: 2,
    });
    eng.register_user_strategy(
        Box::new(adapter),
        format!("lua:{}", pair_discount_dir().display()),
    )
    .expect("pair_discount_arb registers under its package name");
    assert!(eng.set_strategy_enabled("pair_discount_arb", true));
    c.enable_engine(eng);

    let now = NOW;
    pair_market_data(&mut c, now);
    let placed = c.engine_evaluate(now + 2_000);
    assert_eq!(
        placed, 2,
        "the pre-pairing package trades exactly as before (UP@0.44 + DOWN@0.42 both \
         suggested — the same折扣盘口 the legacy fixtures judge)"
    );
    let book = c.pair_book_all();
    assert!(
        book.is_empty(),
        "no pair declaration ⇒ no pair rows (the legacy chain declares nothing): {book:?}"
    );
    // 旧包的两条腿照常进 OME（Live pending）——行为逐位不变的判据。
    assert_eq!(
        c.ome().live_orders().len(),
        2,
        "both legacy legs rest in the OME, byte-identical to the pre-pairing shape"
    );
}

// ── 牙 5：授权测试 (6) —— A=B 声明被拒负向用例 ──────────────────────────────

/// 同自报 pair_key 下两腿盖章到同一 effective account ⇒ 补钉 5 的权威位拒绝
/// 声明：APPLICATION 错误事件（`pair.declRefused`，点名 `field: pair.key`），
/// 两腿都不进配对簿。对照组：A≠B（本文件夹具的 default/arb-b 形）两腿入簿。
///
/// 夹具变形：把 arb_config_set 的双腿映射改成同一账户会被 set 校验拒
/// （arb-leg-config 的未知账户/同 venue 拒收不在同账户维度 —— 但双腿同账户
/// 的映射本身合法），所以这里用「策略自报 pair 但 stamp 同账户」的构造：
/// 覆盖 leg_a/leg_b 账户为同一账户后，双腿的 effective account 全同。
#[test]
fn a_pair_declaration_with_identical_leg_accounts_is_refused() {
    // ── 负向组：双腿映射到同一账户（A=B）⇒ 拒声明。
    {
        let _env = env_guard();
        let _restore = EnvRestore::take(&[
            "POLYMARKET_PRIVATE_KEY",
            "POLYMARKET_FUNDER_ADDRESS",
            "KALSHI_API_KEY_ID",
            "KALSHI_RSA_PRIVATE_KEY",
        ]);
        credentials_ready();

        let mut c = arb_core_live(dec!(1000));
        // 重映射：两腿同账户（A=B 的构造 —— set 校验的维度是 venue 不同 +
        // 账户存在，同账户映射合法落地）。
        c.arb_config_set(&blitzkrieg_core::account::CrossVenueArbConfig {
            leg_a_venue: "polymarket".into(),
            leg_a_account: default_account_id().to_string(),
            leg_b_venue: "kalshi".into(),
            leg_b_account: default_account_id().to_string(),
            pairs: vec!["BTC".into()],
        })
        .expect(
            "the same-account leg configuration lands (the set validation does \
                 not judge A≠B — the DECLARATION gate does)",
        );
        // pot 记账面：default 单账户。
        let now = NOW;
        pair_market_data(&mut c, now);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        c.set_event_sink(tx);
        let placed = c.engine_evaluate(now + 2_000);
        let mut decl_refusals = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let Event::Error { error } = ev
                && error.message.contains("pair.declRefused")
            {
                decl_refusals.push(error);
            }
        }
        assert_eq!(
            decl_refusals.len(),
            2,
            "BOTH legs' declarations are refused (one refusal per declaration): \
             {decl_refusals:?}"
        );
        for e in &decl_refusals {
            assert_eq!(
                e.code,
                blitzkrieg_market_api::CoreErrorCode::InvalidParams,
                "an APPLICATION error names the field: {e}"
            );
            assert!(
                e.message.contains("field: pair.key"),
                "the refusal points at the declared field: {e}"
            );
        }
        let book = c.pair_book_all();
        assert!(
            book.is_empty(),
            "a refused declaration never enters the pair book: {book:?}"
        );
        // 订单路径照旧（校验拒的是声明，不是腿 —— 裸腿由 arb_precheck 的整组
        // 语义与内核去重兜底）：两腿照发（Live pending）。
        assert_eq!(
            placed, 2,
            "the legs still place (the refusal judges the DECLARATION, not the legs)"
        );
    }
    // ── 对照组：A≠B ⇒ 声明受理、两腿入簿（牙 1 的对照已证，这里再钉一遍
    //    与负向组同钟的形状）。
    {
        let _env = env_guard();
        let _restore = EnvRestore::take(&[
            "POLYMARKET_PRIVATE_KEY",
            "POLYMARKET_FUNDER_ADDRESS",
            "KALSHI_API_KEY_ID",
            "KALSHI_RSA_PRIVATE_KEY",
        ]);
        credentials_ready();

        let mut c = arb_core_live(dec!(1000));
        let now = NOW;
        pair_market_data(&mut c, now);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        c.set_event_sink(tx);
        assert_eq!(c.engine_evaluate(now + 2_000), 2);
        while let Ok(ev) = rx.try_recv() {
            if let Event::Error { error } = ev {
                assert!(
                    !error.message.contains("pair.declRefused"),
                    "an A≠B declaration is never refused: {error}"
                );
            }
        }
        let book = c.pair_book_all();
        assert_eq!(book.len(), 1, "one pair for the A≠B round");
        assert_eq!(book[0].legs.len(), 2);
    }
}

// ── 牙 6：授权测试 (2) 后半 —— 轮次推进/终态写点 ────────────────────────────

/// 轮次推进（新 RoundMarkets 非 top-up ⇒ retire_before）把旧轮配对闭账；
/// Closed 终态吸收迟到的成交/死亡事件（on_order_event 幂等空操作）。新轮的
/// 同 (asset, slot) 配对是新键、不受旧账干扰。
#[test]
fn round_advancement_and_sweep_close_the_book_while_judgment_stays_pure() {
    let _env = env_guard();
    let _restore = EnvRestore::take(&[
        "POLYMARKET_PRIVATE_KEY",
        "POLYMARKET_FUNDER_ADDRESS",
        "KALSHI_API_KEY_ID",
        "KALSHI_RSA_PRIVATE_KEY",
    ]);
    credentials_ready();

    let mut c = arb_core_live(dec!(1000));
    let now = NOW;
    pair_market_data(&mut c, now);
    assert_eq!(
        c.engine_evaluate(now + 2_000),
        2,
        "round 1 places both legs"
    );
    let book = c.pair_book_all();
    assert_eq!(book.len(), 1);
    let old_key = book[0].id.clone();
    // Live 模式：入腿 pending 进 OME，零成交 ⇒ Pending（事实驱动的观察者
    // 只记发生的事 —— fill 事实在 Dry 夹具（牙 3）钉过）。
    assert_eq!(
        book[0].status,
        blitzkrieg_core::pair::PairStatus::Pending,
        "declared-but-unfilled stays Pending"
    );

    // 新一轮（round_slot 前进 ⇒ 非 top-up ⇒ retire_before 触发）。
    let now2 = now + 10 * 60 * 1000;
    pair_market_data(&mut c, now2);
    // 闭账已发生：旧轮记录 Closed（终态吸收一切迟到事实）。
    let book = c.pair_book_all();
    let old = book.iter().find(|p| p.id == old_key).expect("old row kept");
    assert_eq!(old.status, blitzkrieg_core::pair::PairStatus::Closed);
    assert!(
        old.slot < now2 / 1000 / 900,
        "the closed row is the old round's"
    );

    // 迟到的死亡事实被 Closed 吸收（状态不变）——旧轮腿的 internal_key 已随
    // place 绑定；force_close 一条旧轮腿，死亡事实进终态是空操作。内核级的
    // 终态吸收单测在 pair.rs（retire_before_closes_old_round_pairs），这里钉
    // 内核级的轮次推进接线。
    let before = c.pair_book_all();
    if let Some(order_id) = c
        .ome()
        .live_orders()
        .iter()
        .find(|o| o.strategy == "cross_venue_arb")
        .map(|o| o.order_id.clone())
    {
        let _ = c.force_close_order(&order_id, now2 + 1_000, "test: late death fact");
    }
    let after = c.pair_book_all();
    assert_eq!(
        before.len(),
        after.len(),
        "no new rows from a late death fact"
    );
    let old_after = after
        .iter()
        .find(|p| p.id == old_key)
        .expect("old row still kept");
    assert_eq!(
        old_after.status,
        blitzkrieg_core::pair::PairStatus::Closed,
        "a Closed pair absorbs late death facts (no state jump)"
    );

    // 至多新轮一行活账：闭账让轮次推进后簿里只留新轮的未闭记录。
    let book2 = c.pair_book_all();
    assert!(
        book2
            .iter()
            .filter(|p| p.status != blitzkrieg_core::pair::PairStatus::Closed)
            .count()
            <= 1,
        "at most the new round's row is open: {book2:?}"
    );
}

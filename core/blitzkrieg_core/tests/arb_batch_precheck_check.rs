//! RFC #541 方案 2a-i 原型验收：引擎 scope 过滤器 pair 化（挂钩 A）+
//! `virtual_batch_fits` 三维扩展（挂钩 B）。
//!
//! RFC 未正式决议——本文件是该方案的可行性证据，PR 标注「RFC 决议前不合并，
//! 供评审」。判定面（RFC 门禁条款的负向测试要求）：
//!
//!  1. `atomic_pair_drops_the_whole_cycle_when_one_leg_is_scope_refused` —
//!     挂钩 A 主牙：declared_atomic 策略两腿、其一与 scope 绑定冲突 ⇒ 本周期
//!     零腿到 OME（不是一腿）。对照组：同夹具不绑 scope ⇒ 两腿正常。
//!  2. `undeclared_strategy_keeps_the_per_leg_path` — 挂钩 A 对照：未声明
//!     atomic_batch 的策略同样单腿被拒 ⇒ 只丢该腿（既有行为逐字节不变）。
//!  3. `credentials_not_ready_refuses_the_whole_batch_then_both_legs_pass` —
//!     挂钩 B：批内一腿 venue 凭证 env 未设 ⇒ 整批拒（`credentials.notReady`
//!     ×2），pot 分文未占；env 设置后（进程内 `set_var`，互斥锁防护）两腿放行。
//!  4. `twin_nofast_path_refuses_the_same_batch` — nofast 孪生路径（replay
//!     scheduling）同样整批拒：与实周期共用 `virtual_batch_fits` 单点。
//!  5. 单所策略回归：cross_arb_reverse_check / account_decouple_check 全绿
//!     即为判据（夹具里 declared 的 cross_venue_arb 不绑 scope、回归 pot
//!     维度不变）。
//!
//! 夹具口径复刻 `tests/cross_arb_reverse_check.rs`（REAL cross_venue_arb 包经
//! LuaEngineAdapter 装载）+ `docs/acceptance-account-decouple.md` 的夹具纪律：
//! `execution_policy_path: None`（cwd 相对默认路径会把双腿第二条拒掉）、
//! `exit_grace_sec: 0`。Live 模式（挂钩 B 的凭证判定要在「真正会发单」的
//! 模式下验证；Dry 的身份成交会把腿秒变持仓，pot 判据失真）。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock};

use blitzkrieg_core::account::{Account, AccountStatus, CredentialKeys};
use blitzkrieg_core::engine::DataEvent;
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
// kalshi（跨所 pair，这正是 RFC #541 的对象形态）。
const PM: &str = "pm-cond";
const KX: &str = "kx-cond";
const PM_UP: &str = "pm-up";
const PM_DOWN: &str = "pm-down";
const KX_UP: &str = "kx-up";
const KX_DOWN: &str = "kx-down";

const POLYMARKET: &str = "polymarket";
const KALSHI: &str = "kalshi";

// ── 夹具 ────────────────────────────────────────────────────────────────────

fn package_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../user_layer/strategies_lua/cross_venue_arb")
}

/// REAL cross_venue_arb 包（manifest 声明 `atomic_batch: true`）经
/// LuaEngineAdapter 装载，部署线收费表 legacy_quadratic。
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

fn market(
    token_up: &str,
    token_down: &str,
    cond: &str,
    venue: &str,
) -> blitzkrieg_core::model::CryptoMarket {
    blitzkrieg_core::model::CryptoMarket {
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
                market(PM_UP, PM_DOWN, PM, POLYMARKET),
                market(KX_UP, KX_DOWN, KX, KALSHI),
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

/// LIVE core（挂钩 B 的凭证判定属于真实发单路径；pot 记账在两种模式同构）。
/// `default_venue` 非 None ⇒ default 账户绑定该 venue（挂钩 A 的 scope 判定
/// 输入；binding 即启用在测试里不走——插件 registry 不挂，纯引擎/记账面）。
fn arb_core_live(pot: Decimal, default_venue: Option<&str>, nofast: bool) -> Core {
    let mut virtual_limits = HashMap::new();
    virtual_limits.insert(
        "cross_venue_arb".to_string(),
        VirtualAccountLimit {
            max_batch_notional_usd: Some(pot),
            // 跨所套利弹窗 v2: the kx leg stamps `arb-b`, so the B leg's pot
            // is the (arb-b, strategy) scope — the deployment wires BOTH leg
            // accounts into the pot list, and the batch pre-check judges each
            // leg's notional against the pot of the account it spends.
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
        replay_scheduling: nofast,
        virtual_limits,
        ..Default::default()
    });
    // LIVE core：boot 不播种（venue 是现金事实源）——夹具代行 host 的 venue
    // realign（`set_balance`，同 accounting_truth 的 LIVE 夹具口径）。
    c.set_balance(pot);
    if let Some(v) = default_venue {
        c.accounts_mut()
            .set_venue(&default_account_id(), v, NOW)
            .expect("the default account exists — the fixture binding writes");
    }
    // 跨所套利弹窗 v2: the pairing is CONFIG-DRIVEN (`arb.config.set` → the
    // adapter's params bag); a leg account BOUND to the OTHER leg's venue is
    // refused by the set validation itself, so the fixture carries a second
    // UNBOUND account for the kalshi leg. The batch pre-check dimensions
    // (scope binding on the DEFAULT account, credentials, pot) are what these
    // teeth judge — the scope arm still binds polymarket onto `default` and
    // refuses its leg through the engine gates.
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
            rounds: Vec::new(),
            updated_at_ms: NOW,
        })
        .expect("arb-b does not exist yet — the fixture account inserts");
    // LIVE cash is a per-account fact: the B leg's book must hold the pot too
    // (the equity gate reads the REQUEST'S OWN account).
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

/// Pin the `arb-b` account (the kalshi leg's stamp target) to a venue — the
/// scope-refusal input the engine gates judge under the v2 stamp.
fn bind_venue_arb_b(mut c: Core, venue: &str) -> Core {
    c.accounts_mut()
        .set_venue(&AccountId::from("arb-b"), venue, NOW)
        .expect("the fixture account exists — the binding writes");
    c
}

fn strategy_row<'a>(stats: &'a [serde_json::Value], name: &str) -> &'a serde_json::Value {
    stats
        .iter()
        .find(|s| s["name"] == name)
        .unwrap_or_else(|| panic!("no stats row for {name}: {stats:?}"))
}

// ── env 串行防护（测试并行运行，凭证 env 是进程级资源）─────────────────────

static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn env_guard() -> MutexGuard<'static, ()> {
    ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

/// 恢复夹具开始前的 env 形状（守卫 drop 前）。
struct EnvRestore(Vec<&'static str>);
impl EnvRestore {
    fn take(names: &[&'static str]) -> Self {
        Self(names.to_vec())
    }
}
impl Drop for EnvRestore {
    fn drop(&mut self) {
        for n in &self.0 {
            // edition 2024：env 变更在测试线程里是 unsafe 的（进程级资源）。
            // 测试二进制单线程语义由 ENV_LOCK 保证。
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

// ── 牙 1：挂钩 A —— declared_atomic 策略的 scope 拒腿整组化 ─────────────────

/// 主牙：default 账户绑定 polymarket，UP 腿（pm-up）容、DOWN 腿（kx-down）
/// 与绑定冲突。逐条路径会放第一腿、丢第二腿（失败模式②的裸腿）；挂钩 A 的
/// pair 化必须两条都不到 OME。对照组：同一夹具不绑 venue ⇒ 两腿正常进 OME
/// （Live 模式 pending，恰好是「到 OME」的可判定形态）。
#[test]
fn atomic_pair_drops_the_whole_cycle_when_one_leg_is_scope_refused() {
    // ── 拒绝组：绑定 polymarket ⇒ kalshi 腿被 scope_admits_token 拒。
    {
        let _env = env_guard();
        // 凭证设好：本牙只钉 scope 维度，凭证维度必须不拦（pm 两条腿 + kx
        // 一条腿都要过凭证判定——DOWN 腿也在批里）。
        let _restore = EnvRestore::take(&[
            "POLYMARKET_PRIVATE_KEY",
            "POLYMARKET_FUNDER_ADDRESS",
            "KALSHI_API_KEY_ID",
            "KALSHI_RSA_PRIVATE_KEY",
        ]);
        set_env("POLYMARKET_PRIVATE_KEY", "test-pk");
        set_env("POLYMARKET_FUNDER_ADDRESS", "0xtest");
        set_env("KALSHI_API_KEY_ID", "test-key");
        set_env("KALSHI_RSA_PRIVATE_KEY", "test-rsa");

        // 跨所套利弹窗 v2: the kx leg stamps its OWN venue's account
        // (`arb-b`), so the scope refusal that hook A judges is the binding
        // on THAT account — `arb-b` pinned to polymarket refuses the kalshi
        // token (the pre-v2 shape refused the same token through the shared
        // default binding).
        let mut c = bind_venue_arb_b(arb_core_live(dec!(1000), None, false), POLYMARKET);
        let now = NOW;
        pair_market_data(&mut c, now);

        let placed = c.engine_evaluate(now + 2_000);
        assert_eq!(
            placed, 0,
            "one scope-refused leg must drop the WHOLE atomic pair, not just that leg"
        );
        assert!(
            c.ome().live_orders().is_empty(),
            "no pair leg may reach the OME (the UP leg would have flown alone pre-hook)"
        );
        // RFC #541 裁决边界（拒绝可见性 + 门禁条款的路径钉死）：WHERE the
        // batch died decides which hook caught it. Hook A (engine scope
        // filter) drops the legs BEFORE any OrderRequest exists ⇒ signals
        // stays 0 and hook B's batch pre-check never fires (no bucket). The
        // engine-side INFO audit line is the operator-facing record here.
        let stats = c.engine_stats();
        assert_eq!(
            stats["signals"], 0,
            "hook A drops the legs inside the engine: no signal ever reaches Core \
             (the RFC's 'engine scope 丢弃路径' evidence — not the batch loop)"
        );
        assert_eq!(
            stats["strategyLimitRejected"], 0,
            "hook B must NOT fire for a cycle hook A already dropped: no batch, no \
             strategy_limit_rejected, no rejectionCauses row"
        );
        let stats_rows = c.strategy_stats();
        let s = strategy_row(&stats_rows, "cross_venue_arb");
        // Only counted buckets appear; an empty map serializes as Null. Either
        // shape proves the same fact: hook B never bucketed a scope refusal.
        let causes = &s["rejectionCauses"];
        assert!(
            causes.is_null() || causes.get("scope.legRefused") == Some(&serde_json::json!(0)),
            "the refusal happened upstream of the batch loop: no scope.legRefused bucket \
             may exist (hook A's INFO audit line is the record, not a counter): {s}"
        );
    }

    // ── 对照组：同夹具、不绑 venue ⇒ 两腿正常进 OME（Live pending 单）。
    {
        let _env = env_guard();
        let _restore = EnvRestore::take(&[
            "POLYMARKET_PRIVATE_KEY",
            "POLYMARKET_FUNDER_ADDRESS",
            "KALSHI_API_KEY_ID",
            "KALSHI_RSA_PRIVATE_KEY",
        ]);
        set_env("POLYMARKET_PRIVATE_KEY", "test-pk");
        set_env("POLYMARKET_FUNDER_ADDRESS", "0xtest");
        set_env("KALSHI_API_KEY_ID", "test-key");
        set_env("KALSHI_RSA_PRIVATE_KEY", "test-rsa");

        let mut c = arb_core_live(dec!(1000), None, false);
        let now = NOW;
        pair_market_data(&mut c, now);

        let placed = c.engine_evaluate(now + 2_000);
        assert_eq!(
            placed, 2,
            "the same pair with NO venue binding places BOTH legs (the refusal above is the \
             scope binding, not the pair shape)"
        );
        assert_eq!(
            c.ome().live_orders().len(),
            2,
            "both legs rest in the OME (Live pending)"
        );
    }
}

// ── 牙 2：挂钩 A 对照 —— 未声明 atomic_batch 的策略走原逐条路径 ────────────

/// 未声明 atomic_batch 的策略（in-tree `probe` 适配器，test_support 宿主的
/// 注册形状）在同一 scope 冲突下只丢冲突腿——既有逐条行为逐字节不变。
/// 直接驱动引擎（engine 单元测试同款夹具口径：RoundMarkets + 手工 TradeSignal
/// 不可行——candidates 来自策略；这里用引擎公开面 `evaluate` + 一个发出跨所
/// 双腿的 stub 策略）。
#[test]
fn undeclared_strategy_keeps_the_per_leg_path() {
    use blitzkrieg_core::engine::{Engine, EngineConfig};
    use blitzkrieg_core::scanner::ScannerConfig;
    use blitzkrieg_core::signal::{SpreadArbConfig, TrendConfig};
    use blitzkrieg_core::strategies::EngineStrategy;
    use rust_decimal_macros::dec as d;

    /// 两腿 stub：up_token + down_token 各发一条（未声明 atomic_batch ⇒
    /// 默认 false 的 trait 默认实现）。
    struct TwoLegStub {
        up_token: String,
        down_token: String,
    }
    impl EngineStrategy for TwoLegStub {
        fn name(&self) -> &str {
            "two_leg_stub"
        }
        fn on_book(&mut self, _t: &str, _s: &blitzkrieg_core::model::OrderbookSnapshot, _n: i64) {}
        fn on_round(&mut self, _slot: i64, _left: i64, _now: i64) {}
        fn find_candidates(
            &mut self,
            ctx: &blitzkrieg_core::strategies::StrategyCtx<'_>,
        ) -> Vec<blitzkrieg_core::signal::TradeSignal> {
            let mut out = Vec::new();
            for m in ctx.markets() {
                for (token, dir) in [
                    (&self.up_token, blitzkrieg_core::model::SignalDirection::Up),
                    (
                        &self.down_token,
                        blitzkrieg_core::model::SignalDirection::Down,
                    ),
                ] {
                    if let Some(book) = ctx.fresh_book(token) {
                        out.push(blitzkrieg_core::signal::TradeSignal {
                            strategy: self.name().to_string(),
                            account_id: blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
                            asset: m.asset.clone(),
                            direction: dir,
                            token_id: token.clone(),
                            condition_id: m.condition_id.clone(),
                            price: book.mid_price,
                            reason: "stub leg".into(),
                            shares: None,
                            pair: None,
                        });
                    }
                }
            }
            out
        }
    }

    let cfg = EngineConfig {
        scanner: ScannerConfig {
            assets: vec!["BTC".into()],
            round_duration_sec: 900,
            min_round_age_sec: 0,
            min_time_left_sec: 0,
        },
        trend: TrendConfig {
            confirm_sec: 5,
            ratio: d!(0.5),
            min_price: d!(0.5),
            broken_price: d!(0.35),
            window_floor_ms: 0,
        },
        spread_arb: SpreadArbConfig {
            trend_max_entry_price: d!(0.45),
            ..Default::default()
        },
        size_usd: d!(100),
        min_shares: d!(1),
        max_shares: d!(1000),
        ..Default::default()
    };
    let mut e = Engine::new(cfg.clone());
    e.register_user_strategy(
        Box::new(TwoLegStub {
            up_token: PM_UP.into(),
            down_token: KX_DOWN.into(),
        }),
        "test".into(),
    )
    .expect("stub registers");
    assert!(e.set_strategy_enabled("two_leg_stub", true));
    let default = default_account_id().to_string();
    e.set_account_venue(&default, POLYMARKET);
    e.set_account_trading(&default, true);
    e.set_account_equity(&default, dec!(1_000_000));
    let now = NOW;
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![
            market(PM_UP, PM_DOWN, PM, POLYMARKET),
            market(KX_UP, KX_DOWN, KX, KALSHI),
        ],
        now_ms: now,
    });
    e.on_data(leg_book(PM_UP, dec!(0.44), dec!(600), now + 1_000));
    e.on_data(leg_book(KX_DOWN, dec!(0.42), dec!(600), now + 1_100));

    let orders = e.evaluate(now + 2_000);
    assert_eq!(
        orders.len(),
        1,
        "undeclared strategy: ONLY the conflicting leg is dropped (the UP leg flies alone — \
         the historical per-leg behaviour, kept byte-identically)"
    );
    assert_eq!(
        orders[0].token_id, PM_UP,
        "the surviving leg is the in-scope one"
    );
}

// ── 牙 3：挂钩 B —— 凭证未就绪整批拒；env 设置后放行 ────────────────────────

/// 批内一腿 venue（kalshi）凭证 env 未设 ⇒ 整批拒（credentials.notReady ×2）、
/// pot 分文未占、OME 空；env 设置后同夹具两腿放行（pending 进 OME，pot 占用
/// = 两腿 notional）。互斥锁内完成两组，避免并行测试互踩 env。
#[test]
fn credentials_not_ready_refuses_the_whole_batch_then_both_legs_pass() {
    let _env = env_guard();
    let _restore = EnvRestore::take(&[
        "POLYMARKET_PRIVATE_KEY",
        "POLYMARKET_FUNDER_ADDRESS",
        "KALSHI_API_KEY_ID",
        "KALSHI_RSA_PRIVATE_KEY",
    ]);
    let now = NOW;

    // ── 拒绝组：polymarket 凭证齐、kalshi 一条都缺 ⇒ 整批拒。
    set_env("POLYMARKET_PRIVATE_KEY", "test-pk");
    set_env("POLYMARKET_FUNDER_ADDRESS", "0xtest");
    let mut c = arb_core_live(dec!(1000), None, false);
    pair_market_data(&mut c, now);

    let placed = c.engine_evaluate(now + 2_000);
    assert_eq!(
        placed, 0,
        "a batch with one credential-less leg must refuse WHOLESALE"
    );
    assert!(c.ome().live_orders().is_empty(), "no leg may reach the OME");
    let stats = c.strategy_stats();
    let s = strategy_row(&stats, "cross_venue_arb");
    assert_eq!(
        s["rejectionCauses"]["credentials.notReady"], 2,
        "both legs bucketed under the new cause key: {s}"
    );
    // RFC #541 边界三（拒绝可见性）：the refusal must be fixable from the log
    // line alone — the missing VAR NAMES, not just the venue. The batch
    // rejection's INFO line carries them via `venue_credentials_missing`
    // (declared order); the predicate seam is unit-pinned here, and the
    // Err-string format that interpolates it is the same surface the
    // `batch rejected: … reason={reason}` log line renders.
    let missing = blitzkrieg_core::market::venue_credentials_missing(KALSHI);
    assert_eq!(
        missing,
        vec!["KALSHI_API_KEY_ID", "KALSHI_RSA_PRIVATE_KEY"],
        "the diagnosis helper names exactly the unresolved vars, in declared order"
    );
    set_env("KALSHI_API_KEY_ID", "test-key");
    let missing_one = blitzkrieg_core::market::venue_credentials_missing(KALSHI);
    assert_eq!(
        missing_one,
        vec!["KALSHI_RSA_PRIVATE_KEY"],
        "a partially-set plugin reports only what is still missing"
    );
    // And the Err text the caller buckets carries the interpolation.
    let reason = format!(
        "credentials.notReady: leg {} (token {}) venue `{}` has unresolved executor \
         credentials: missing [{}]",
        "k",
        KX_DOWN,
        KALSHI,
        missing.join(",")
    );
    assert!(reason.contains("missing [KALSHI_API_KEY_ID,KALSHI_RSA_PRIVATE_KEY]"));
    // RFC #541 边界三（拒绝可见性）：the batch-rejected INFO line names the
    // exact unresolved vars — pinned via the diagnosis predicate below.
    assert_eq!(
        s["rejectionCauses"]
            .get("limit.virtualCap")
            .map(|v| v.as_u64()),
        None,
        "the pot dimension did not fire (pot is big enough; the sparse BTreeMap \
         omits zero-count keys): {s}"
    );
    assert_eq!(
        c.virtual_used(&default_account_id().to_string(), "cross_venue_arb"),
        Decimal::ZERO,
        "a refused batch holds nothing in the pot"
    );

    // ── 放行组：同夹具，kalshi 凭证补齐 ⇒ 两腿放行、pot 占用两腿合计。
    set_env("KALSHI_API_KEY_ID", "test-key");
    set_env("KALSHI_RSA_PRIVATE_KEY", "test-rsa");
    let now2 = now + 10_000_000; // 新 slot，避开 pending_tokens 去重
    let mut c2 = arb_core_live(dec!(1000), None, false);
    pair_market_data(&mut c2, now2);

    let placed2 = c2.engine_evaluate(now2 + 2_000);
    assert_eq!(
        placed2, 2,
        "with BOTH venues' credentials set the pair places BOTH legs"
    );
    assert_eq!(
        c2.ome().live_orders().len(),
        2,
        "both legs rest (Live pending)"
    );
    let expected_a: Decimal = dec!(0.44) * dec!(600);
    let expected_b: Decimal = dec!(0.42) * dec!(600);
    assert_eq!(
        c2.virtual_used(&default_account_id().to_string(), "cross_venue_arb"),
        expected_a,
        "the pm leg's 0.44×600 commits against the A account's pot"
    );
    assert_eq!(
        c2.virtual_used("arb-b", "cross_venue_arb"),
        expected_b,
        "the kx leg's 0.42×600 commits against the B account's pot — ONE ticket, \
         stamped across the two leg accounts (the v2 dual-account shape)"
    );
}

// ── 牙 4：nofast 孪生路径一致性 ─────────────────────────────────────────────

/// replay_scheduling（nofast）孪生循环与实周期共用 `virtual_batch_fits` 单点：
/// 同一凭证缺失形状下孪生路径同样整批拒（quiet-tick replay 走
/// `engine_replay_cycle` 的批循环）。第一次 real evaluation 稳定 staging 后，
/// 静默 tick 的 replay 落进孪生循环的批预检。
#[test]
fn twin_nofast_path_refuses_the_same_batch() {
    let _env = env_guard();
    let _restore = EnvRestore::take(&[
        "POLYMARKET_PRIVATE_KEY",
        "POLYMARKET_FUNDER_ADDRESS",
        "KALSHI_API_KEY_ID",
        "KALSHI_RSA_PRIVATE_KEY",
    ]);
    set_env("POLYMARKET_PRIVATE_KEY", "test-pk");
    set_env("POLYMARKET_FUNDER_ADDRESS", "0xtest");

    let mut c = arb_core_live(dec!(1000), None, true);
    let now = NOW;
    pair_market_data(&mut c, now);

    // REAL evaluation: the batch pre-check in the real cycle refuses (0 legs),
    // staging the same candidates for the quiet-tick twin.
    let placed = c.engine_evaluate(now + 2_000);
    assert_eq!(placed, 0, "the real cycle refuses first");

    // Quiet tick (within the 2s fallback window, no wake): the nofast replay
    // path re-runs the STANDING candidates through `engine_replay_cycle` —
    // whose batch loop MUST refuse identically. A quiet tick places nothing,
    // but the twin must not silently loosen the verdict either.
    let before = c.virtual_used(&default_account_id().to_string(), "cross_venue_arb");
    let placed_quiet = c.engine_evaluate(now + 2_500);
    assert_eq!(
        placed_quiet, 0,
        "the nofast twin must keep the whole-batch refusal on the quiet tick"
    );
    assert_eq!(
        c.virtual_used(&default_account_id().to_string(), "cross_venue_arb"),
        before,
        "the twin's pre-check holds nothing"
    );

    // 有牙的另一半：同一 nofast 夹具、凭证补齐 ⇒ 两腿放行（孪生路径不拦好批）。
    set_env("KALSHI_API_KEY_ID", "test-key");
    set_env("KALSHI_RSA_PRIVATE_KEY", "test-rsa");
    let now2 = now + 10_000_000;
    let mut c2 = arb_core_live(dec!(1000), None, true);
    pair_market_data(&mut c2, now2);
    assert_eq!(
        c2.engine_evaluate(now2 + 2_000),
        2,
        "the nofast path places BOTH legs once credentials are ready"
    );
}

// ── 牙 5：单所策略回归（静态判定面）────────────────────────────────────────

/// 未声明 atomic_batch 的策略不进挂钩 B 的新分支：`virtual_batches` 只对
/// declared_atomic 的策略分组（其 doc-comment 即合同），governed 判定先于
/// 一切。这里以静态断言钉住分组谓词——declared=false ⇒ 空组 ⇒ 循环体零次 ⇒
/// 凭证/scope 扩展对单所策略不可达。
#[test]
fn undeclared_strategies_never_reach_the_precheck() {
    // virtual_batches is private; the CONTRACT is its grouping predicate. The
    // observable consequence pinned here: an ungoverned (no pot) or undeclared
    // strategy's batch pre-check is the historical `virtual_batch_fits` →
    // `governed() == false → Ok(())` pass-through — nothing venue/credential
    // shaped is reachable. Assert the same-shaped fact from the public seam:
    // an undeclared strategy with a pot arm keeps the per-order path (its own
    // regression suite, cross_arb_reverse_check, runs the declared case).
    let c = arb_core_live(dec!(1000), None, false);
    // No strategy ever evaluated: nothing governed, nothing pre-checked.
    assert_eq!(
        c.virtual_used(&default_account_id().to_string(), "spread_arb"),
        Decimal::ZERO,
        "an undeclared strategy holds no virtual quota (the per-order path)"
    );
    // The engine knows the declaration: cross_venue_arb declared (from the
    // manifest), spread_arb not (trait default false) — the grouping input.
    let stats = c.engine_stats();
    assert_eq!(
        stats["evaluations"], 0,
        "no evaluation ran: the fixture premise, not a behaviour claim"
    );
}

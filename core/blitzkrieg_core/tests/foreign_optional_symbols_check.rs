//! #642 的反向验收：cdylib 的 4 项新可选符号经真实 loader（`load_foreign` →
//! dlopen → 符号探测 → `ForeignStrategy`）驱动，行为与 Lua 侧逐位对齐：
//!
//!  1. `bk_strategy_on_kline` —— 引擎只把 **闭合** bar 推给策略（§10.4 同一
//!     分派点）；fixture 把收到的每根 bar 经 vtable 既有 `diagnostics` 槽
//!     报回来（dlopen 边界上唯一可用的读数通道 —— rlib 链接会让测试二进制
//!     拿到自己那一份 static，读到的永远是空表），测试用引擎自己的 K 线
//!     路径重导出同一根 bar 逐字段比对（decimal-STRING 线格式，无浮点漂移）。
//!  2. `bk_strategy_entry_pair` —— 装载期 pair 缺省 + evaluate JSON 的
//!     per-entry `pair` 字段双通道，经引擎真实链路落到
//!     `take_pair_declarations()`：per-entry 赢过缺省；A≠B 校验对双腿同
//!     DEFAULT 账户会拒 —— 本牙的两腿声明 key 不同（缺省 / per-entry），
//!     不进同一 pair_key 桶，照常落 declare。
//!  3. `bk_strategy_trailing_exit` —— 三态：fixture 声明 1 ⇒
//!     `Some(true)`（spread_arb manifest `trailing_exit: true` 同型）；无
//!     符号库 ⇒ `None`（随内核 `risk.setExit`）。
//!  4. `bk_strategy_atomic_batch` —— fixture 声明 1 ⇒ `true`；无符号库
//!     ⇒ `false`（E-VA 逐单路径不变）。
//!
//! fixture（`user_layer/optional_capability_fixture`）是手写 ABI（同
//! e24_modes_fixture 的理由：证明**符号契约**，不是证明宏）；对照库是既有
//! `parity_strategy`（未导出 4 个新符号 ⇒ 「无符号=缺省」语义的活证据）。
//! 信任面：fixture 不在 `APPROVED_ROOTS`，测试用 loader 自身的 operator
//! 逃生口（`BLITZKRIEG_STRATEGY_ALLOW_DIRS`，声明检查脚本同款机制）放行
//! —— 走生产路径，不改 `APPROVED_ROOTS`。构建先于测试（CI 两步）；
//! `BK_REQUIRE_DYLIB=1` 把缺工件从 skip 变硬红（foreign_parity 同款）。
#![cfg(feature = "strategy-loading")]

use blitzkrieg_core::engine::{DataEvent, Engine, EngineConfig};
use blitzkrieg_core::model::CryptoMarket;
use blitzkrieg_core::signal::{SpreadArbConfig, TrendConfig};
use blitzkrieg_core::strategies::EngineStrategy as _;
use blitzkrieg_core::strategy_engine::loader::{ENV_ALLOW_DIRS, load_foreign};
use blitzkrieg_market_api::KlineInterval;
use rust_decimal_macros::dec;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock};

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("user_layer")
        .join("optional_capability_fixture")
}

fn fixture_lib() -> PathBuf {
    let base = fixture_dir().join("target").join("release");
    for name in [
        "liboptional_capability_fixture.dylib",
        "liboptional_capability_fixture.so",
        "optional_capability_fixture.dll",
    ] {
        let p = base.join(name);
        if p.exists() {
            return p;
        }
    }
    let msg = concat!(
        "build the fixture first: ",
        "(cd user_layer/optional_capability_fixture && cargo build --release)"
    );
    if std::env::var_os("BK_REQUIRE_DYLIB").is_some() {
        panic!("{msg} (looked in {})", base.display());
    }
    eprintln!("skipping: {msg}");
    std::process::exit(0);
}

fn parity_lib() -> PathBuf {
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("user_layer")
        .join("parity_strategy")
        .join("target")
        .join("release");
    for name in [
        "libparity_strategy.dylib",
        "libparity_strategy.so",
        "parity_strategy.dll",
    ] {
        let p = base.join(name);
        if p.exists() {
            return p;
        }
    }
    let msg =
        "build the parity cdylib first: (cd user_layer/parity_strategy && cargo build --release)";
    if std::env::var_os("BK_REQUIRE_DYLIB").is_some() {
        panic!("{msg} (looked in {})", base.display());
    }
    eprintln!("skipping: {msg}");
    std::process::exit(0);
}

// ── env 串行防护（pair_book_check / arb_batch_precheck_check 同款）──────────
// 信任面是进程级 env：改 `BLITZKRIEG_STRATEGY_ALLOW_DIRS` 的牙必须独占进程
// 且离开时还原，别的库（foreign_parity 的 parity 加载）不得看见这把伞。

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

/// 放行 fixture 目录（operator 逃生口，生产路径）并返回 RAII 还原句柄。
fn allow_fixture_dir() -> EnvRestore {
    unsafe {
        std::env::set_var(
            ENV_ALLOW_DIRS,
            fixture_dir()
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        );
    }
    EnvRestore::take(&[ENV_ALLOW_DIRS])
}

/// 加载 fixture（带信任面放行）—— 三个牙共用的唯一装载路径。
fn load_fixture() -> blitzkrieg_core::strategy_engine::loader::LoadedForeign {
    let _restore = allow_fixture_dir();
    load_foreign(&fixture_lib()).unwrap_or_else(|e| panic!("fixture load failed: {e:?}"))
}

fn engine_cfg() -> EngineConfig {
    EngineConfig {
        scanner: blitzkrieg_core::scanner::ScannerConfig {
            assets: vec!["BTC".into()],
            round_duration_sec: 900,
            min_round_age_sec: 0,
            min_time_left_sec: 0,
        },
        trend: TrendConfig {
            confirm_sec: 5,
            ratio: dec!(0.5),
            min_price: dec!(0.5),
            broken_price: dec!(0.35),
            window_floor_ms: 0,
        },
        spread_arb: SpreadArbConfig::default(),
        max_orderbook_stale_ms: 8000,
        momentum_window_sec: 30,
        momentum_tol_pct: dec!(0.03),
        size_usd: dec!(2.5),
        min_shares: dec!(10),
        max_shares: dec!(10),
        size_pct: dec!(0),
        strategy_sizes: std::collections::HashMap::new(),
    }
}

fn market(end_ms: i64, slot: i64) -> CryptoMarket {
    CryptoMarket {
        asset: "BTC".into(),
        condition_id: "cond".into(),
        question_id: "q".into(),
        up_token_id: "up".into(),
        down_token_id: "down".into(),
        up_price: dec!(0.6),
        down_price: dec!(0.4),
        expires_at_ms: end_ms,
        round_slot: slot,
        round_duration_sec: 900,
        archive_verdict: false,
        venue: String::new(),
        neg_risk: true,
        question: "BTC up or down".into(),
    }
}

fn book(
    token: &str,
    bids: &[(rust_decimal::Decimal, rust_decimal::Decimal)],
    now_ms: i64,
) -> DataEvent {
    DataEvent::Book {
        token_id: token.into(),
        bids: bids.to_vec(),
        asks: vec![(dec!(0.52), dec!(100)), (dec!(0.55), dec!(100))],
        now_ms,
    }
}

/// fixture 的 `diagnostics` 读数：经引擎的宿主面（`confirmed_diagnostics`
/// —— service `engine_stats` 走的同一读数）回来的 bar 记录数组。只挑
/// `kind:"kline"` 行（将来其他读数若也走这一通道不会混进来）。
fn recorded_klines(e: &Engine) -> Vec<serde_json::Value> {
    e.confirmed_diagnostics(0)
        .into_iter()
        .filter(|v| v.get("kind").and_then(|k| k.as_str()) == Some("kline"))
        .collect()
}

/// 牙 3+4+2（声明面）：装载回执与 trait 面一次断言（fixture 全声明，
/// parity 全缺省 —— 「无符号=缺省」的两侧各一枚活证据）。
#[test]
fn declarations_survive_the_real_loader_and_default_when_absent() {
    let _env = env_guard();
    let loaded = load_fixture();
    assert_eq!(loaded.name, "optional_capability_fixture");
    // 3. trailing 三态：符号返回 1 ⇒ Some(true)（spread_arb manifest 同型）。
    assert_eq!(
        loaded.wants_trailing_exit,
        Some(true),
        "symbol trailing_exit:1 must land as Some(true)"
    );
    // 4. atomic_batch：符号返回 1 ⇒ true。
    assert!(
        loaded.atomic_batch,
        "symbol atomic_batch:1 must land as true"
    );
    // 2. pair 缺省：符号 JSON {key, leg} ⇒ TradePairDecl（字段逐字透传）。
    let pair = loaded
        .entry_pair
        .as_ref()
        .expect("the load-time pair default must be declared");
    assert_eq!(pair.key, "fixture-pair-default");
    assert_eq!(pair.leg, "up");
    // trait 面同答（`strategy.list`/回执走的同一读数）。
    assert_eq!(loaded.strategy.wants_trailing_exit(), Some(true));
    assert!(loaded.strategy.atomic_batch());

    // 对照组：parity_strategy 未导出任何新符号 ⇒ 全缺省（Lua 无声明包的
    // cdylib 镜像；「无符号=缺省」不是解析失败的巧合，是两库的差分证据）。
    let parity =
        load_foreign(&parity_lib()).unwrap_or_else(|e| panic!("parity load failed: {e:?}"));
    assert_eq!(parity.wants_trailing_exit, None);
    assert!(!parity.atomic_batch);
    assert_eq!(parity.entry_pair, None);
    assert_eq!(parity.strategy.wants_trailing_exit(), None);
    assert!(!parity.strategy.atomic_batch());
    // parity 也未导出 on_kline/diagnostics ⇒ 两个新面都静默缺省（不崩、
    // 不投递）：内核推 bar 时直接跳过，宿主读数为空数组。
    assert!(
        !parity.strategy.wants_trailing_exit().is_some()
            && std::mem::discriminant::<Option<bool>>(&None)
                == std::mem::discriminant(&parity.strategy.wants_trailing_exit()),
        "the parity library declares no trailing verdict"
    );
}

/// 牙 1：闭合 bar 经真实 `load_foreign` 实例流入 `bk_strategy_on_kline`。
/// fixture 把每根 bar 的线格式字段原样记录，经 vtable 既有 `diagnostics`
/// 槽报回来；测试用引擎自己的 K 线路径（Book mid → Sec1 桶 → 跨桶闭合）
/// 重导出同样的序列逐字段比对——只允许闭合 bar 进来（生长中的 bar 从不
/// 出现在记录里），且十进制是字符串原样。
#[test]
fn closed_bars_reach_the_optional_symbol_field_identically() {
    let _env = env_guard();
    let loaded = load_fixture();
    let mut e = Engine::new(engine_cfg());
    e.register_user_strategy(Box::new(loaded.strategy), "fixture".into())
        .unwrap();
    assert!(e.set_strategy_enabled("optional_capability_fixture", true));

    let t0: i64 = 1_700_000_000_000;
    // 桶对齐：t0 落在 [..t0+999] 的 sec1 桶内；t0+1000 的 tick 关闭它。
    for (i, price) in [dec!(0.40), dec!(0.42)].iter().enumerate() {
        e.on_data(book("up", &[(*price, dec!(100))], t0 + i as i64 * 300));
    }
    // 第三本落在下一个桶 ⇒ 关闭第一根（open=0.40, close=0.42, high/low 如实）。
    e.on_data(book("up", &[(dec!(0.45), dec!(100))], t0 + 1000));

    // 内核侧同一序列（同一聚合器路径的宿主读数）。
    let closed = e.take_closed_klines();
    assert_eq!(closed.len(), 1, "exactly one sec1 bar closes: {closed:?}");
    let host_bar = &closed[0];
    assert!(host_bar.is_closed);
    assert_eq!(host_bar.interval, KlineInterval::Sec1);

    // 策略侧读数：经真实 vtable `diagnostics` 槽回来的记录（引擎宿主面）。
    let recorded = recorded_klines(&e);
    assert_eq!(
        recorded.len(),
        1,
        "the symbol must have received exactly the CLOSED bar (no growing bars): {recorded:?}"
    );
    let k = &recorded[0];
    // 字段逐字：decimal-STRING 线格式与共享类型串形一致（无 f64 漂移）。
    assert_eq!(k["symbol"], host_bar.symbol);
    assert_eq!(k["interval"], "sec1");
    assert_eq!(k["openTimeMs"], host_bar.open_time_ms);
    assert_eq!(k["closeTimeMs"], host_bar.close_time_ms);
    assert_eq!(k["open"], host_bar.open.to_string());
    assert_eq!(k["high"], host_bar.high.to_string());
    assert_eq!(k["low"], host_bar.low.to_string());
    assert_eq!(k["close"], host_bar.close.to_string());
    assert_eq!(k["volume"], host_bar.volume.to_string());
    assert_eq!(k["tradeCount"], host_bar.trade_count);
    assert_eq!(k["isClosed"], true);
}

/// 牙 2（行为面）：evaluate 输出经 `adopt_eval_json` 采纳入候选，走引擎
/// 真实链路（A≠B 校验 → `PairDeclaration`）落到 `take_pair_declarations`
/// —— UP 腿无 per-entry `pair` ⇒ 落装载期缺省 `fixture-pair-default/up`；
/// DOWN 腿自带 per-entry `pair` ⇒ 赢过缺省落 `fixture-pair-per-entry/down`
/// 。与 Lua 的 `pair={key=,leg=}` 透传语义逐位同构（缺省 None = 旧行为
/// 不变）。两腿 key 不同，不进同一 pair_key 桶 ⇒ A≠B 校验（双腿同账户
/// 整组拒）不触发，两份声明照常落账。
#[test]
fn pair_channels_compose_per_entry_wins_over_load_default() {
    let _env = env_guard();
    let loaded = load_fixture();
    let mut e = Engine::new(engine_cfg());
    e.register_user_strategy(Box::new(loaded.strategy), "fixture".into())
        .unwrap();
    assert!(e.set_strategy_enabled("optional_capability_fixture", true));

    let t0: i64 = 1_700_000_000_000;
    e.on_data(DataEvent::RoundMarkets {
        markets: vec![market(t0 + 900_000, 1)],
        now_ms: t0,
    });
    // 策略定价 0.10（fixture 定值）；两本盘口让两腿过闸（0.10 是 maker
    // 挂单价，盘口覆盖它即可，方向无谓——本牙看 pair，不看成交）。
    e.on_data(book(
        "up",
        &[(dec!(0.09), dec!(100)), (dec!(0.08), dec!(100))],
        t0 + 10,
    ));
    e.on_data(book(
        "down",
        &[(dec!(0.09), dec!(100)), (dec!(0.08), dec!(100))],
        t0 + 10,
    ));
    // 驱动一个评估周期（TopOfBook 也喂 K 线聚合器，无妨——本牙看 pair）。
    e.on_data(DataEvent::TopOfBook {
        token_id: "up".into(),
        best_bid: Some(dec!(0.09)),
        best_ask: Some(dec!(0.10)),
        now_ms: t0 + 20,
    });
    e.on_data(DataEvent::TopOfBook {
        token_id: "down".into(),
        best_bid: Some(dec!(0.09)),
        best_ask: Some(dec!(0.10)),
        now_ms: t0 + 20,
    });

    let _orders = e.evaluate(t0 + 20);
    let decls = e.take_pair_declarations();
    assert_eq!(
        decls.len(),
        2,
        "both legs must carry a declaration through the real A!=B path: {decls:?}"
    );
    let by_token = |token: &str| {
        decls
            .iter()
            .find(|d| d.token_id == token)
            .unwrap_or_else(|| panic!("no declaration for {token}: {decls:?}"))
    };
    // UP：无 per-entry ⇒ 装载期缺省。
    let up = by_token("up");
    assert_eq!(up.decl.key, "fixture-pair-default");
    assert_eq!(up.decl.leg, "up");
    // DOWN：per-entry 赢过缺省（Lua per-entry `pair` 表同权）。
    let down = by_token("down");
    assert_eq!(down.decl.key, "fixture-pair-per-entry");
    assert_eq!(down.decl.leg, "down");
    // 伴生事实同型（engine → Core 的提交伴生载荷）：owner 是实例主账户，
    // 腿账户是候选盖章结果；声明股数透传（fixture 未报 shares ⇒ None）。
    assert_eq!(up.owner_strategy, "optional_capability_fixture");
    assert_eq!(up.owner_account, blitzkrieg_market_api::DEFAULT_ACCOUNT_ID);
    assert_eq!(up.leg_account, blitzkrieg_market_api::DEFAULT_ACCOUNT_ID);
    assert_eq!(up.shares, None);
    assert_eq!(down.owner_strategy, "optional_capability_fixture");
    assert_eq!(down.shares, None);
    // A≠B 校验未拒任何声明（两 key 不同桶）——拒绝回执为空是它的读数。
    assert!(e.take_pair_refusals().is_empty());
}

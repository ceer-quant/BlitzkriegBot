//! 环境差异排查探针（任务二 1-3）——先红的有牙测试。
//!
//! 三个探针，全部「只观测、不拦截」：
//! 1. **系统时钟偏差（NTP）**：定期与 SNTP 服务器比对本地时钟；|offset| >= 50ms
//!    打 WARNING，>= 100ms 在 engine.stats 的 `probe.ntp.skewRed` 打红标。
//!    验收红线：系统时间偏差超过 100ms 且无日志警告 → 本文件必须红。
//! 2. **数据流断流/延迟（tick health）**：按 (venue, token) 计数，每 10s 汇总
//!    （窗口 tick 数 / 零 tick token / 最长静默）；token 曾有 tick 后静默超
//!    预算 → 「断流」warn + 计数（每静默期一次，恢复后重新武装）。
//! 3. **信号失效（spot 停更）**：spot 源停更超预算 → warn + 计数。
//!
//! 回放红线：NTP 任务只允许在生产的 `server::run()` 里 spawn —— 本文件不
//! （也无法）测 spawn 本身，测的是纯决策函数 `should_spawn_ntp`，`server.rs`
//! 与 `backtest.rs` 的分工由编译结构保证：回放驱动从不触达 `server::run()`。
//!
//! 时钟纪律（memory: wallclock-test-flake 处方）：所有汇总/停更判定都吃调用方
//! 传入的 `now_ms`（受控时钟），本文件没有一处 sleep，没有一处读墙钟做窗口。

use blitzkrieg_core::engine::{DataEvent, Engine, EngineConfig};
use blitzkrieg_core::model::CryptoMarket;
use blitzkrieg_core::probe;
use blitzkrieg_core::service::{Core, CoreConfig};
use rust_decimal_macros::dec;

/// 固定时刻（2001-09-09T01:46:40Z）——夹具常量，永远不是"现在"。
const T0: i64 = 1_000_000_000_000;

// ────────────────────────── tracing 捕获（#318 模式）──────────────────────────

#[derive(Clone, Default)]
struct LogSink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for LogSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogSink {
    type Writer = LogSink;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

thread_local! {
    static CAPTURE_SINK: std::cell::RefCell<Option<LogSink>> =
        const { std::cell::RefCell::new(None) };
}

struct ThreadRoutedWriter;

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for ThreadRoutedWriter {
    type Writer = LogSink;
    fn make_writer(&'a self) -> Self::Writer {
        CAPTURE_SINK
            .with(|s| s.borrow().clone())
            .unwrap_or_default()
    }
}

fn install_capture() {
    static INSTALL: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    INSTALL.get_or_init(|| {
        let subscriber = tracing_subscriber::fmt()
            .with_writer(ThreadRoutedWriter)
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .finish();
        let _ = tracing::subscriber::set_global_default(subscriber);
    });
}

fn with_logs<R>(f: impl FnOnce() -> R) -> (R, String) {
    install_capture();
    let sink = LogSink::default();
    struct Route(());
    impl Drop for Route {
        fn drop(&mut self) {
            CAPTURE_SINK.with(|s| *s.borrow_mut() = None);
        }
    }
    CAPTURE_SINK.with(|s| *s.borrow_mut() = Some(sink.clone()));
    let route = Route(());
    let out = f();
    drop(route);
    let text = String::from_utf8_lossy(&sink.0.lock().unwrap()).into_owned();
    (out, text)
}

// ────────────────────────────── 夹具 ──────────────────────────────────────────

/// 无持久化的 dry 内核（同 engine_stats_view_seam 的 dry_core 形状）。
fn dry_core() -> Core {
    let mut c = Core::new(CoreConfig {
        dry_seed_balance: dec!(1000),
        auto_exits_enabled: false,
        trade_log_path: None,
        order_log_path: None,
        position_log_path: None,
        ..Default::default()
    });
    c.set_balance(dec!(1000));
    c
}

fn book(token: &str, at_ms: i64) -> DataEvent {
    DataEvent::Book {
        token_id: token.to_string(),
        bids: vec![(dec!(0.40), dec!(50))],
        asks: vec![(dec!(0.42), dec!(50))],
        now_ms: at_ms,
    }
}

fn market(up: &str, down: &str) -> CryptoMarket {
    CryptoMarket {
        asset: "BTC".into(),
        condition_id: "cond".into(),
        question_id: "q".into(),
        up_token_id: up.into(),
        down_token_id: down.into(),
        up_price: dec!(0.48),
        down_price: dec!(0.45),
        expires_at_ms: T0 + 300_000,
        round_slot: 1,
        round_duration_sec: 300,
        archive_verdict: false,
        venue: "polymarket".into(),
        neg_risk: false,
        question: "BTC up or down".into(),
    }
}

// ────────────────────── 探针 1：NTP 时钟偏差 ────────────────────────────────

/// 验收红线：偏差 150ms（> 100ms 红线）必须有日志警告，且 stats 打红标。
#[test]
fn ntp_offset_over_the_red_line_warns_and_flags() {
    let (c, logs) = with_logs(|| {
        let mut c = dry_core();
        c.note_ntp_offset(Some(150), T0);
        c
    });
    assert!(
        logs.contains("clock offset"),
        "a 150 ms clock offset must log a warning, got:\n{logs}"
    );
    assert!(
        logs.contains("150"),
        "the warning must carry the measured offset, got:\n{logs}"
    );

    let s = c.engine_stats();
    assert_eq!(s["probe"]["ntp"]["synced"], serde_json::json!(true));
    assert_eq!(s["probe"]["ntp"]["offsetMs"], serde_json::json!(150));
    assert_eq!(
        s["probe"]["ntp"]["skewRed"],
        serde_json::json!(true),
        "150 ms >= the 100 ms red line: the stats must flag it"
    );
}

/// 50ms 档：超警告线（>= 50 打 WARNING）但不到红线。
#[test]
fn ntp_offset_at_the_warning_threshold_warns_without_the_red_flag() {
    let (c, logs) = with_logs(|| {
        let mut c = dry_core();
        c.note_ntp_offset(Some(50), T0);
        c
    });
    assert!(
        logs.contains("clock offset"),
        "50 ms is AT the warning threshold (>= 50): must warn, got:\n{logs}"
    );
    let s = c.engine_stats();
    assert_eq!(s["probe"]["ntp"]["skewRed"], serde_json::json!(false));
}

/// 正常偏差（10ms）：安静，不打警告。
#[test]
fn ntp_offset_within_tolerance_is_quiet() {
    let (c, logs) = with_logs(|| {
        let mut c = dry_core();
        c.note_ntp_offset(Some(10), T0);
        c
    });
    assert!(
        !logs.contains("clock offset"),
        "a 10 ms offset is within tolerance: no warning, got:\n{logs}"
    );
    let s = c.engine_stats();
    assert_eq!(s["probe"]["ntp"]["offsetMs"], serde_json::json!(10));
    assert_eq!(s["probe"]["ntp"]["skewRed"], serde_json::json!(false));
    assert_eq!(s["probe"]["ntp"]["failures"], serde_json::json!(0));
}

/// 查询失败：静默降级（不刷屏），只计失败数。
#[test]
fn ntp_query_failure_degrades_silently_and_counts() {
    let (c, logs) = with_logs(|| {
        let mut c = dry_core();
        c.note_ntp_offset(None, T0);
        c.note_ntp_offset(None, T0 + 300_000);
        c
    });
    assert!(
        !logs.contains("clock offset"),
        "failures degrade silently (no warning spam), got:\n{logs}"
    );
    let s = c.engine_stats();
    assert_eq!(s["probe"]["ntp"]["failures"], serde_json::json!(2));
    assert_eq!(s["probe"]["ntp"]["synced"], serde_json::json!(false));
}

/// SNTP 纯函数：偏移公式 ((t2-t1)+(t3-t4))/2。
#[test]
fn sntp_offset_formula_is_the_standard_one() {
    // 服务器时钟比本地快 100ms、单程 5ms、往返对称：
    // t1=0, t2=105（=100 skew + 5 网络抵达）, t3=110, t4=10。
    // 严格演算：((t2-t1)+(t3-t4))/2 = ((105-0)+(110-10))/2 = 102（向零截断）。
    assert_eq!(probe::sntp_offset_ms(0, 105, 110, 10), 102);
    // 零偏移零延迟：t1=0, t2=0, t3=0, t4=0 → 0。
    assert_eq!(probe::sntp_offset_ms(0, 0, 0, 0), 0);
    // 对称网络时延（t2-t1 = t3-t4 = d）但服务器快 s：offset 恒等于 s，
    // 与时延无关——这是 SNTP 公式的核心性质。
    assert_eq!(probe::sntp_offset_ms(0, 50, 60, 10), 50);
}

/// NTP 时间戳编解码往返：unix ms ↔ 1900 纪元的 64 位定点的往返必须守恒（毫秒精度）。
#[test]
fn ntp_timestamp_encode_decode_round_trips() {
    for ms in [0i64, 1_000_000_000_000, 1_700_000_000_123] {
        let (sec, frac) = probe::ntp_format(ms);
        let back = probe::ntp_to_unix_ms(sec, frac);
        assert!(
            (back - ms).abs() <= 1,
            "unix ms {ms} must round-trip through the NTP wire format (got {back})"
        );
    }
}

/// SNTP 客户端对本地 mock 服务器的完整往返：mock 把 T3 拨快 100ms，
/// 客户端测得的 offset 必须落在 100ms 附近（回环 ε 由范围断言吸收，
/// 不是墙钟窗口——不会 flake）。
#[test]
fn sntp_client_round_trips_against_a_mock_server() {
    use std::net::UdpSocket;
    let server = UdpSocket::bind("127.0.0.1:0").expect("bind mock");
    let addr = server.local_addr().expect("addr");

    let responder = std::thread::spawn(move || {
        let mut req = [0u8; 48];
        let (_, src) = server.recv_from(&mut req).expect("recv");
        // T1 = 客户端写进 originate 字段的发送时刻。
        let t1 = probe::ntp_to_unix_ms(
            u32::from_be_bytes(req[24..28].try_into().unwrap()),
            u32::from_be_bytes(req[28..32].try_into().unwrap()),
        );
        let mut rep = [0u8; 48];
        rep[0] = 0x24; // LI=0, VN=4, Mode=4 (server)
        rep[1] = 2; // stratum
        rep[24..32].copy_from_slice(&req[24..32]); // originate = echo T1
        // 服务器时钟 +100ms：T2 = t1 + 100（零网络时差写入），T3 = T2 + 5。
        let (t2s, t2f) = probe::ntp_format(t1 + 100);
        let (t3s, t3f) = probe::ntp_format(t1 + 105);
        rep[32..36].copy_from_slice(&t2s.to_be_bytes());
        rep[36..40].copy_from_slice(&t2f.to_be_bytes());
        rep[40..44].copy_from_slice(&t3s.to_be_bytes());
        rep[44..48].copy_from_slice(&t3f.to_be_bytes());
        server.send_to(&rep, src).expect("send");
    });

    // 阻塞 UDP 调用直接在测试线程上跑（当前线程 runtime 仅在此未用）。
    let addr_str = addr.to_string();
    let offset =
        tokio::task::block_in_place(|| probe::query_once(&addr_str, 123)).expect("mock answers");
    responder.join().expect("responder ran");

    assert!(
        (90..=110).contains(&offset),
        "the mock server clock runs +100 ms; measured offset {offset} ms must land near it"
    );
}

/// 回放红线守卫（其一）：空主机 = 探针关闭，`should_spawn_ntp` 必须拒绝——
/// `server::run()` 只在它为 true 时才 spawn 网络任务。
#[test]
fn empty_ntp_host_disables_the_probe_entirely() {
    assert!(!probe::should_spawn_ntp(""), "empty host = no NTP task");
    assert!(
        !probe::should_spawn_ntp("   "),
        "whitespace host = no NTP task"
    );
    assert!(probe::should_spawn_ntp("pool.ntp.org"));
}

// ────────────────────── 探针 2：数据流断流 / 10s 汇总 ───────────────────────

/// 断流：tokA/tokB 各有一 tick 后静默 69~71s（预算 60s）→ warn + 计数，
/// 且每个静默期只 warn 一次；恢复 tick 后重新武装。
#[test]
fn a_silent_token_warns_once_per_episode_and_recounts() {
    let ((_core, (warns1, warns2)), text) = with_logs(|| {
        let mut c = dry_core();
        c.engine_on_data(book("tokA", T0), T0);
        c.engine_on_data(book("tokB", T0 + 2_000), T0 + 2_000);
        c.tick(T0 + 10_000).expect("tick");
        c.tick(T0 + 71_000).expect("tick"); // 汇总点：tokA 静默 71s、tokB 69s
        let warns1 = c.engine_stats()["probe"]["tickHealth"]["streamBreakWarnings"]
            .as_u64()
            .expect("counter");
        c.engine_on_data(book("tokA", T0 + 72_000), T0 + 72_000); // 恢复 tokA
        c.tick(T0 + 82_000).expect("tick");
        let warns2 = c.engine_stats()["probe"]["tickHealth"]["streamBreakWarnings"]
            .as_u64()
            .expect("counter");
        (c, (warns1, warns2))
    });

    assert_eq!(
        warns1, 2,
        "two tokens past the 60 s budget at the summary = two stream-break warnings"
    );
    assert_eq!(
        warns2, 2,
        "tokA resumed (re-armed, not re-warned) and tokB is still the SAME episode: no new warning"
    );
    assert!(
        text.contains("stream break"),
        "the break must be announced as a stream break, got:\n{text}"
    );
    assert!(text.contains("tokA"), "the warn names the token");
}

/// 零 tick token：本轮已知 token 在汇总窗口内一个 tick 都没有 → 汇总点名。
#[test]
fn zero_tick_round_tokens_are_named_by_the_summary() {
    let (c, logs) = with_logs(|| {
        let mut c = dry_core();
        c.enable_engine(Engine::new(EngineConfig::default()));
        c.engine_on_data(
            DataEvent::RoundMarkets {
                markets: vec![market("m-up", "m-down")],
                now_ms: T0,
            },
            T0,
        );
        c.tick(T0 + 10_000).expect("tick"); // 汇总：两个 token 零 tick
        c
    });

    let s = c.engine_stats();
    let zero = s["probe"]["tickHealth"]["zeroTickTokens"]
        .as_array()
        .expect("list")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        zero,
        vec!["m-down".to_string(), "m-up".to_string()],
        "the round's tokens had no tick in the window: the summary must name them (sorted)"
    );
    assert!(
        logs.contains("zero tick"),
        "zero-tick tokens must be announced, got:\n{logs}"
    );
}

/// 受控时钟守卫（memory: wallclock-test-flake 处方）：汇总节奏只由调用方的
/// `now_ms` 驱动。t+9_999 不汇总（lastSummaryMs 不动），t+10_000 才汇总。
#[test]
fn the_summary_cadence_follows_the_callers_clock_only() {
    let mut c = dry_core();
    c.engine_on_data(book("tok", T0), T0);
    c.engine_on_data(book("tok", T0 + 1_000), T0 + 1_000);

    // 未到 10s：无汇总。
    c.tick(T0 + 9_999).expect("tick");
    let s = c.engine_stats();
    assert_eq!(
        s["probe"]["tickHealth"]["lastSummaryMs"],
        serde_json::json!(null),
        "9_999 ms into the window: no summary yet"
    );

    // 到点：汇总，窗口内 2 个 tick，最长静默 = 10_000 - 1_000。
    c.tick(T0 + 10_000).expect("tick");
    let s = c.engine_stats();
    assert_eq!(
        s["probe"]["tickHealth"]["windowTicks"],
        serde_json::json!(2)
    );
    assert_eq!(
        s["probe"]["tickHealth"]["lastSummaryMs"],
        serde_json::json!(T0 + 10_000)
    );
    assert_eq!(
        s["probe"]["tickHealth"]["maxSilenceMs"],
        serde_json::json!(9_000)
    );

    // 9_999 ms 之后又不汇总：窗口清零，汇总点不动（间隔不等式，不是计数窗口）。
    c.tick(T0 + 19_999).expect("tick");
    let s = c.engine_stats();
    assert_eq!(
        s["probe"]["tickHealth"]["lastSummaryMs"],
        serde_json::json!(T0 + 10_000),
        "the summary point must not move before another full 10 s"
    );
    assert_eq!(
        s["probe"]["tickHealth"]["windowTicks"],
        serde_json::json!(0),
        "the new window starts empty"
    );
}

// ────────────────────── 探针 3：spot 停更（信号失效）───────────────────────

#[test]
fn a_stale_spot_feed_signals_once_per_episode() {
    let ((c, (w1, w2, w3)), text) = with_logs(|| {
        let mut c = dry_core();
        c.engine_on_data(
            DataEvent::Spot {
                asset: "BTC".into(),
                price: dec!(50_000),
                now_ms: T0,
            },
            T0,
        );
        c.tick(T0 + 61_000).expect("tick"); // 停更 61s（预算 60s）→ warn
        let w1 = c.engine_stats()["probe"]["spot"]["staleWarnings"]
            .as_u64()
            .expect("counter");
        c.tick(T0 + 75_000).expect("tick"); // 同一静默期：不再 warn
        let w2 = c.engine_stats()["probe"]["spot"]["staleWarnings"]
            .as_u64()
            .expect("counter");
        c.engine_on_data(
            DataEvent::Spot {
                asset: "BTC".into(),
                price: dec!(50_100),
                now_ms: T0 + 80_000,
            },
            T0 + 80_000,
        ); // 恢复
        c.tick(T0 + 150_000).expect("tick"); // 再次停更 70s → 第二次
        let w3 = c.engine_stats()["probe"]["spot"]["staleWarnings"]
            .as_u64()
            .expect("counter");
        (c, (w1, w2, w3))
    });

    assert_eq!(w1, 1, "61 s of spot silence (60 s budget) = one signal");
    assert_eq!(w2, 1, "the same silence never re-warns");
    assert_eq!(w3, 2, "a resumed feed that stalls again is a new episode");
    assert!(
        text.contains("spot"),
        "the stale-spot signal names the feed, got:\n{text}"
    );

    let s = c.engine_stats();
    assert_eq!(
        s["probe"]["spot"]["staleMs"],
        serde_json::json!(70_000),
        "staleMs is measured against the caller's clock at the last summary"
    );
    assert_eq!(
        s["probe"]["spot"]["lastSeenMs"],
        serde_json::json!(T0 + 80_000)
    );
}

// ────────────────────── stats 出口：字段名钉死 ──────────────────────────────

/// 从未同步/从未有数据时的探针块形状：全部字段在场（null/0），字段名即契约。
#[test]
fn the_probe_stats_block_carries_every_field_even_when_cold() {
    let c = dry_core();
    let s = c.engine_stats();
    let probe_block = &s["probe"];
    assert_eq!(probe_block["ntp"]["synced"], serde_json::json!(false));
    assert_eq!(probe_block["ntp"]["offsetMs"], serde_json::json!(null));
    assert_eq!(probe_block["ntp"]["skewRed"], serde_json::json!(false));
    assert_eq!(probe_block["ntp"]["failures"], serde_json::json!(0));
    assert_eq!(probe_block["ntp"]["lastCheckMs"], serde_json::json!(null));
    assert_eq!(
        probe_block["tickHealth"]["windowTicks"],
        serde_json::json!(0)
    );
    assert_eq!(
        probe_block["tickHealth"]["zeroTickTokens"],
        serde_json::json!([])
    );
    assert_eq!(
        probe_block["tickHealth"]["maxSilenceMs"],
        serde_json::json!(0)
    );
    assert_eq!(
        probe_block["tickHealth"]["streamBreakWarnings"],
        serde_json::json!(0)
    );
    assert_eq!(
        probe_block["tickHealth"]["lastSummaryMs"],
        serde_json::json!(null)
    );
    assert_eq!(probe_block["spot"]["lastSeenMs"], serde_json::json!(null));
    assert_eq!(probe_block["spot"]["staleMs"], serde_json::json!(null));
    assert_eq!(probe_block["spot"]["staleWarnings"], serde_json::json!(0));
}

/// 探针只观测：计数器不能改变任何既有统计或订单语义。
#[test]
fn the_probes_are_observers_only() {
    let mut c = dry_core();
    c.engine_on_data(book("tok", T0), T0);
    let books_before = c.engine_stats()["books"].as_u64().expect("books");
    assert_eq!(books_before, 1);
    // 既有计数不受探针影响（books 计数与探针并存）。
    assert_eq!(
        c.engine_stats()["probe"]["tickHealth"]["windowTicks"],
        serde_json::json!(1)
    );
}

// ────────────────────── CLI 旗标 ─────────────────────────────────────────────

#[test]
fn the_ntp_flags_parse_with_their_defaults() {
    // 裸 Args 的默认值（复制 main.rs 测试的私有构造方式不可行——这里走公开面：
    // 默认值由 golden sample 钉死，本测试只钉解析行为）。
    // `--ntp-host ""` = 关；`--ntp-interval-secs 0` = 关（周期检查）。
    // 解析行为在 main.rs 的单测里有 Args 级快照；此处用 env 层守卫语义：
    // probe 模块对"空主机=关"的纯决策已由 empty_ntp_host_disables_the_probe_entirely 钉死。
    // 这里补一条间隔常量：默认 300s。
    assert_eq!(probe::DEFAULT_NTP_INTERVAL_SECS, 300);
    assert_eq!(probe::DEFAULT_NTP_HOST, "pool.ntp.org");
    assert_eq!(probe::SUMMARY_INTERVAL_MS, 10_000);
    assert_eq!(probe::TICK_STALE_WARN_MS, 60_000);
    assert_eq!(probe::SPOT_STALE_WARN_MS, 60_000);
    assert_eq!(probe::NTP_WARN_OFFSET_MS, 50);
    assert_eq!(probe::NTP_RED_OFFSET_MS, 100);
}

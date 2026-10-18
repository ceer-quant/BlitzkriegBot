//! 环境差异排查探针（任务二 1-3）：时钟偏差 / 数据流断流 / spot 失效。
//!
//! 三个探针的分工：
//! * **NTP（时钟偏差）**：[`query_once`] 发一次 SNTP 请求并算出本机相对
//!   服务器的时钟偏移（ms）。周期循环 [`spawn_loop`] 只在生产的
//!   `server::run()` 里由 [`should_spawn_ntp`] 的判定启动——回放/基线路径
//!   （`backtest.rs`、`install_engine`）永不触达网络。测得的偏移喂给
//!   `Core::note_ntp_offset`：|offset| >= [`NTP_WARN_OFFSET_MS`] 打 WARNING，
//!   >= [`NTP_RED_OFFSET_MS`] 在 `engine.stats.probe.ntp.skewRed` 打红标。
//! * **tick health（数据流断流）**：`Core::engine_on_data`（一切数据事件的
//!   必经之路）按 (venue, token) 记账；`Core::tick` 用**调用方的时钟**每
//!   [`SUMMARY_INTERVAL_MS`] 汇总一次——窗口 tick 数、零 tick token、最长
//!   静默。曾收到 tick 的 token 静默超过 [`TICK_STALE_WARN_MS`] 视为断流：
//!   每个静默期 warn 一次并计数，恢复 tick 重新武装。
//! * **spot 失效（信号失效）**：spot 源停更超过 [`SPOT_STALE_WARN_MS`] →
//!   每个静默期 warn 一次并计数；恢复更新重新武装。
//!
//! 观测纪律：探针只写日志和计数器，不拦截任何数据/订单路径；所有时间判定
//! 都吃调用方传入的 `now_ms`（回放里是虚拟时钟），受控时钟可测、无墙钟
//! 窗口、无 sleep（memory: wallclock-test-flake 处方）。

use std::collections::HashMap;
use std::net::UdpSocket;
use std::time::Duration;

// 测试面（tests/env_probes.rs）用的平铺别名：`probe::sntp_offset_ms` 等。
pub use sntp::{format as ntp_format, offset_ms as sntp_offset_ms, to_unix_ms as ntp_to_unix_ms};

/// SNTP 常量与消息布局。
pub mod sntp {
    /// 客户端请求包长（SNTP v4，48 字节）。
    pub const PACKET_LEN: usize = 48;
    /// NTP 纪元（1900-01-01）到 Unix 纪元的秒数。
    const NTP_UNIX_DELTA_SECS: i64 = 2_208_988_800;

    /// Unix 毫秒 → (秒, 1/2^32 分数)，NTP 64 位时间戳的两个 32 位半字。
    pub fn format(unix_ms: i64) -> (u32, u32) {
        const FRAC_PER_MS: i128 = 4_294_967_296; // 2^32
        let secs = unix_ms.div_euclid(1000) + NTP_UNIX_DELTA_SECS;
        let frac = (unix_ms.rem_euclid(1000) as i128 * FRAC_PER_MS / 1000) as u32;
        (secs as u32, frac)
    }

    /// (秒, 分数) → Unix 毫秒（毫秒精度，±1ms 内）。
    pub fn to_unix_ms(secs: u32, frac: u32) -> i64 {
        (secs as i64 - NTP_UNIX_DELTA_SECS) * 1000 + ((frac as i64) * 1000 / 4_294_967_296)
    }

    /// 标准偏移公式：offset = ((t2 - t1) + (t3 - t4)) / 2（四时间戳，ms）。
    /// 整数除法向零截断——探针观测 1ms 粒度足够。
    pub fn offset_ms(t1: i64, t2: i64, t3: i64, t4: i64) -> i64 {
        ((t2 - t1) + (t3 - t4)) / 2
    }

    /// 解析一个 SNTP 响应：校验包长、LI（闰秒警报）/VN/Mode、stratum 非零，
    /// 返回 (t2, t3) 两个服务器时间戳（Unix ms）。非零 originate 时间戳的
    /// 一致性由调用方对照自己发出的 t1 校验（防无关应答）。
    pub fn parse_response(packet: &[u8], t1: i64) -> Option<(i64, i64)> {
        if packet.len() < PACKET_LEN {
            return None;
        }
        let li_vn_mode = packet[0];
        let li = li_vn_mode >> 6;
        let version = (li_vn_mode >> 3) & 0b111;
        let mode = li_vn_mode & 0b111;
        if li == 3 || version < 3 || mode != 4 {
            return None; // 闰秒未同步 / 版本过低 / 不是服务器应答
        }
        if packet[1] == 0 {
            return None; // stratum 0 = kiss-o'-death
        }
        // originate timestamp 必须回显客户端的 t1（防乱序应答错配）。
        let o_secs = u32::from_be_bytes(packet[24..28].try_into().ok()?);
        let o_frac = u32::from_be_bytes(packet[28..32].try_into().ok()?);
        if o_secs != 0 || o_frac != 0 {
            let (e_secs, e_frac) = format(t1);
            if o_secs != e_secs || o_frac != e_frac {
                return None;
            }
        }
        let t2 = to_unix_ms(
            u32::from_be_bytes(packet[32..36].try_into().ok()?),
            u32::from_be_bytes(packet[36..40].try_into().ok()?),
        );
        let t3 = to_unix_ms(
            u32::from_be_bytes(packet[40..44].try_into().ok()?),
            u32::from_be_bytes(packet[44..48].try_into().ok()?),
        );
        Some((t2, t3))
    }
}

/// 发起一次 SNTP 查询，返回测得的时钟偏移（本机 − 服务器，ms）；
/// 任何失败（解析/IO/超时）返回 `None`——探针静默降级，由调用方计数。
///
/// 单次往返、阻塞调用（`recv` 带超时）：调用方是专用线程上的循环任务，
/// 绝不在内核锁内运行。
pub fn query_once(host: &str, timeout_ms: u64) -> Option<i64> {
    // host 已带端口（`host:port`，mock 测试/自定义端口）就用它；否则补
    // SNTP 的著名端口 123。
    let addr = if host
        .rsplit(':')
        .next()
        .is_some_and(|p| p.parse::<u16>().is_ok())
        && host.contains(':')
    {
        host.to_string()
    } else {
        format!("{host}:123")
    };
    let sock = UdpSocket::bind(if addr.starts_with('[') {
        // IPv6 literal host: bind the wildcard of the same family.
        "[::]:0"
    } else {
        "0.0.0.0:0"
    })
    .ok()?;
    sock.connect(&addr).ok()?;
    sock.set_read_timeout(Some(Duration::from_millis(timeout_ms)))
        .ok()?;

    let mut req = [0u8; sntp::PACKET_LEN];
    req[0] = 0x23; // LI=0, VN=4, Mode=3 (client)
    let t1 = blitzkrieg_market_api::net::now_ms();
    let (t1s, t1f) = sntp::format(t1);
    req[24..28].copy_from_slice(&t1s.to_be_bytes());
    req[28..32].copy_from_slice(&t1f.to_be_bytes());
    sock.send(&req).ok()?;

    let mut buf = [0u8; sntp::PACKET_LEN];
    let n = sock.recv(&mut buf).ok()?;
    let (t2, t3) = sntp::parse_response(&buf[..n], t1)?;
    let t4 = blitzkrieg_market_api::net::now_ms();
    Some(sntp::offset_ms(t1, t2, t3, t4))
}

// ─────────────────────────── 参数（CLI 钉过）────────────────────────────────

pub const DEFAULT_NTP_HOST: &str = "pool.ntp.org";
pub const DEFAULT_NTP_INTERVAL_SECS: i64 = 300;
/// 单次查询的等待上限：一次丢包最多占住循环任务 1.5s，不是内核锁。
pub const QUERY_TIMEOUT_MS: u64 = 1_500;
pub const NTP_WARN_OFFSET_MS: i64 = 50;
pub const NTP_RED_OFFSET_MS: i64 = 100;
pub const SUMMARY_INTERVAL_MS: i64 = 10_000;
/// 曾有 tick 的 token 静默超过该预算 → 断流 warn（一个静默期一次）。
pub const TICK_STALE_WARN_MS: i64 = 60_000;
/// spot 源停更超过该预算 → 信号失效 warn（一个静默期一次）。
pub const SPOT_STALE_WARN_MS: i64 = 60_000;

/// 生产判定：主机非空才允许 spawn NTP 任务。`server::run()` 在此为 false 时
/// 一个网络任务都不建——回放安全的一半（另一半：回放根本不走 `server::run`）。
pub fn should_spawn_ntp(host: &str) -> bool {
    !host.trim().is_empty()
}

/// 启动 NTP 周期任务（生产专用；见 [`should_spawn_ntp`]）。每个周期查询一次，
/// 结果交给 `note`（通常是 `Core::note_ntp_offset`）。查询失败计数、不刷屏。
/// 循环持有自己的线程面（tokio spawn_blocking）——SNTP 是阻塞 UDP，绝不碰
/// 内核锁。
pub fn spawn_loop(
    host: String,
    interval_secs: i64,
    note: std::sync::Arc<dyn Fn(Option<i64>) + Send + Sync>,
) {
    if !should_spawn_ntp(&host) {
        return;
    }
    let interval = interval_secs.max(1);
    std::thread::Builder::new()
        .name("ntp-probe".into())
        .spawn(move || {
            let mut next = blitzkrieg_market_api::net::now_ms();
            loop {
                let now = blitzkrieg_market_api::net::now_ms();
                if now >= next {
                    next = now + interval * 1000;
                    let offset = query_once(&host, QUERY_TIMEOUT_MS);
                    note(offset);
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        })
        .expect("spawn ntp-probe thread");
}

// ───────────────────────── 共享状态（内核侧记账）─────────────────────────────

/// 一次 tick 记账：token → (venue, 最近 tick 时刻)。
#[derive(Default)]
pub struct TickHealth {
    /// 汇总窗口内的 tick 计数（全部 token 合计）。
    pub window_ticks: u64,
    /// 上次汇总时刻（`None` = 还没有汇总过——首个汇总要等"第一批数据"
    /// 之后满 [`SUMMARY_INTERVAL_MS`]，用 [`Self::first_data_ms`] 判定，
    /// 一个没有任何数据的 10s 不产生"零 tick"假警告）。
    pub last_summary_ms: Option<i64>,
    /// 见过的第一批数据（tick/spot/round 皆可）的时刻——首个汇总窗口的
    /// 起点。只看 tick 会让"round 开了但一个 tick 都没有"永远沉默，而
    /// 那正是零 tick 探针要抓的场景。
    pub first_data_ms: Option<i64>,
    /// 每个 token 的最近 tick（venue 标签 + 时刻）。回放里 token 集有限，
    /// 10s 汇总时全量扫描；跨轮 token 由 `round_token_ids` 的覆盖自然淘汰
    /// （汇总只报告"已知 token"，不会无限增长——token 键随轮替换保持有界）。
    pub per_token: HashMap<String, (String, i64)>,
    /// 断流 warn 次数（一个静默期一次；恢复后重新武装）。
    pub stream_break_warnings: u64,
    /// 当前处于断流状态的 token（该集合内的 token 不再重复 warn）。
    pub broken: std::collections::HashSet<String>,
    /// 汇总读过窗口后等待清零（见 [`Self::consume_window`]）。
    pub window_consumed: bool,
}

impl TickHealth {
    /// `engine_on_data` 的 Book/TopOfBook 臂调用：一次行情 tick。
    pub fn note_tick(&mut self, token: &str, venue: &str, now_ms: i64) {
        self.window_ticks += 1;
        self.note_any_data(now_ms);
        self.per_token
            .insert(token.to_string(), (venue.to_string(), now_ms));
        self.broken.remove(token); // 恢复 → 重新武装
    }

    /// 任一数据事件（spot/round 也算）到场的时刻——首个汇总窗口起点。
    pub fn note_any_data(&mut self, now_ms: i64) {
        if self.first_data_ms.is_none() {
            self.first_data_ms = Some(now_ms);
        }
    }

    /// 汇总刚刚读过窗口：进入"窗口已读"状态。下一个**非汇总** tick 把
    /// 计数清零（不是汇总瞬间清零——汇总后立刻读 stats 必须还能看到
    /// 刚结束的窗口）。
    pub fn consume_window(&mut self, now_ms: i64) {
        self.last_summary_ms = Some(now_ms);
        self.window_consumed = true;
    }

    /// 一个常规（非汇总）tick：若上一轮汇总已读过窗口，从现在起重新计数。
    pub fn clear_window_if_consumed(&mut self) {
        if self.window_consumed {
            self.window_ticks = 0;
            self.window_consumed = false;
        }
    }

    /// `tick(now_ms)` 每 10s 调用一次（受控时钟）。返回值：
    /// (本窗口 tick 数, 零 tick token（排序）, 最长静默 ms, 新断流 token 名单)。
    /// `last_summary_ms == None` 时只打基线，不做判定（第一个窗口没有"之前"）。
    pub fn summarize(
        &mut self,
        now_ms: i64,
        round_tokens: &[String],
        warn_budget_ms: i64,
    ) -> (u64, Vec<String>, i64, Vec<String>) {
        // 零 tick：本轮已声明的 token 里，窗口内没有任何行情的。
        let mut zero: Vec<String> = round_tokens
            .iter()
            .filter(|t| !self.per_token.contains_key(t.as_str()))
            .cloned()
            .collect();
        zero.sort();

        let mut max_silence = 0i64;
        let mut newly_broken = Vec::new();
        for (token, (_venue, at)) in self.per_token.iter() {
            let silence = now_ms - *at;
            if silence > max_silence {
                max_silence = silence;
            }
            if silence >= warn_budget_ms && self.broken.insert(token.clone()) {
                newly_broken.push(token.clone());
            }
        }
        let ticks = self.window_ticks;
        // 窗口的"清零"延迟到下一个非汇总 tick（clear_window_if_consumed）：
        // 汇总瞬间读 stats 必须还能看到刚结束的窗口。
        self.consume_window(now_ms);
        self.stream_break_warnings += newly_broken.len() as u64;
        (ticks, zero, max_silence, newly_broken)
    }
}

/// spot 停更跟踪。
#[derive(Default)]
pub struct SpotWatch {
    pub last_seen_ms: Option<i64>,
    pub stale_warnings: u64,
    pub stale_signaled: bool,
}

impl SpotWatch {
    pub fn note(&mut self, now_ms: i64) {
        self.last_seen_ms = Some(now_ms);
        self.stale_signaled = false; // 恢复 → 重新武装
    }

    /// 每 10s 随 tick 汇总调用：停更超预算 → 每个静默期一次。
    /// 返回 (是否本次发出 warn, 停更时长 ms)。
    pub fn check(&mut self, now_ms: i64, warn_budget_ms: i64) -> (bool, Option<i64>) {
        let Some(last) = self.last_seen_ms else {
            return (false, None); // 从未见过 spot：无从判定停更
        };
        let stale = now_ms - last;
        if stale >= warn_budget_ms && !self.stale_signaled {
            self.stale_signaled = true;
            self.stale_warnings += 1;
            (true, Some(stale))
        } else {
            (false, Some(stale))
        }
    }
}

/// NTP 偏移记录（由生产循环喂，见 `note_ntp_offset`）。
#[derive(Default)]
pub struct NtpWatch {
    pub last_offset_ms: Option<i64>,
    pub last_check_ms: Option<i64>,
    pub failures: u64,
}

impl NtpWatch {
    /// 应用一次测量结果。返回 true 表示本次应该打 WARNING（|offset| >= 预算）。
    pub fn apply(&mut self, offset: Option<i64>, now_ms: i64) -> bool {
        self.last_check_ms = Some(now_ms);
        match offset {
            Some(ms) => {
                self.last_offset_ms = Some(ms);
                ms.abs() >= NTP_WARN_OFFSET_MS
            }
            None => {
                self.failures += 1;
                false
            }
        }
    }

    pub fn skew_red(&self) -> bool {
        self.last_offset_ms
            .is_some_and(|ms| ms.abs() >= NTP_RED_OFFSET_MS)
    }
}

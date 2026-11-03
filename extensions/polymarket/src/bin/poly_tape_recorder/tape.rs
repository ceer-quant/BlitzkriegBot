//! 落盘通道（单 jsonl + 每来源 seq）与每来源会话统计（原样迁出）。

use blitzkrieg_market_api::net::now_ms;
use serde_json::{Value, json};
use std::io::Write as IoWrite;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// 统一行形状（纯函数，便于测试）：每行都带
/// `channel/symbol/recv_ts/server_ts/seq/payload`。
pub(crate) fn build_row(
    channel: &str,
    symbol: Option<&str>,
    recv_ts: i64,
    server_ts: Option<i64>,
    seq: u64,
    payload: Value,
) -> Value {
    json!({
        "channel": channel,
        "symbol": symbol,
        "recv_ts": recv_ts,
        "server_ts": server_ts,
        "seq": seq,
        "payload": payload,
    })
}

/// 落盘通道：单 jsonl 文件 + 每来源连续 seq。写线程逐行 flush（「立即落盘」
/// 的 P0 语义：录制器被 kill 最多丢正在写的那一行）。
#[derive(Clone)]
pub(crate) struct Tape {
    tx: std::sync::mpsc::Sender<String>,
    seq_poly: Arc<AtomicU64>,
    seq_spot: Arc<AtomicU64>,
    seq_other: Arc<AtomicU64>,
}

impl Tape {
    pub(crate) fn start(path: PathBuf) -> Result<(Self, std::thread::JoinHandle<()>), String> {
        let file = std::fs::File::create(&path)
            .map_err(|e| format!("创建 {} 失败: {e}", path.display()))?;
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let handle = std::thread::Builder::new()
            .name("tape-writer".into())
            .spawn(move || {
                let mut w = std::io::BufWriter::new(file);
                for line in rx {
                    if writeln!(w, "{line}").is_err() {
                        break; // 磁盘没了：丢行好过静默吞线程
                    }
                    let _ = w.flush();
                }
                let _ = w.flush();
            })
            .map_err(|e| format!("起写线程失败: {e}"))?;
        Ok((
            Self {
                tx,
                seq_poly: Arc::new(AtomicU64::new(0)),
                seq_spot: Arc::new(AtomicU64::new(0)),
                seq_other: Arc::new(AtomicU64::new(0)),
            },
            handle,
        ))
    }

    fn next_seq(&self, channel: &str) -> u64 {
        let counter = match channel {
            "poly_market" => &self.seq_poly,
            "spot" => &self.seq_spot,
            _ => &self.seq_other,
        };
        counter.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// 组行并落盘。`recv_ts` 由调用方在解析前打点（延迟口径用它）。
    pub(crate) fn emit(
        &self,
        channel: &str,
        symbol: Option<&str>,
        recv_ts: i64,
        server_ts: Option<i64>,
        payload: Value,
    ) {
        let row = build_row(
            channel,
            symbol,
            recv_ts,
            server_ts,
            self.next_seq(channel),
            payload,
        );
        let _ = self.tx.send(row.to_string());
    }

    /// 生命周期/发现/汇总行（无场地时间戳，recv 当场打点）。
    pub(crate) fn emit_event(&self, channel: &str, symbol: &str, payload: Value) {
        self.emit(channel, Some(symbol), now_ms(), None, payload);
    }
}

// ── 统计与断线记账 ──────────────────────────────────────────────────────────

/// 每来源（poly_market / spot）的会话统计。锁粒度=每来源一把，热路径只在
/// 每消息一次加锁，量级（几十条/秒）完全无压力。
#[derive(Default)]
pub(crate) struct SourceStat {
    pub(crate) msgs: u64,
    pub(crate) bytes: u64,
    pub(crate) latencies: Vec<i64>,
    pub(crate) first_msg_recv_ts: Option<i64>,
    pub(crate) last_msg_recv_ts: Option<i64>,
    pub(crate) connect_failures: u64,
    pub(crate) unplanned_disconnects: u64,
    pub(crate) planned_disconnects: u64,
    pub(crate) unplanned_downtime_ms: u64,
    pub(crate) planned_downtime_ms: u64,
    pub(crate) stalls: u64,
}

impl SourceStat {
    pub(crate) fn note_msg(&mut self, bytes: u64, recv: i64, server_ts: Option<i64>) {
        self.msgs += 1;
        self.bytes += bytes;
        if self.first_msg_recv_ts.is_none() {
            self.first_msg_recv_ts = Some(recv);
        }
        self.last_msg_recv_ts = Some(recv);
        if let Some(ts) = server_ts {
            self.latencies.push(ts - recv);
        }
    }
}

pub(crate) type StatHandle = Arc<Mutex<SourceStat>>;

pub(crate) fn lock(stat: &StatHandle) -> std::sync::MutexGuard<'_, SourceStat> {
    // 中毒锁 = 某处 panic 了，但计数仍是能拿到的最好证据：不放大失败。
    stat.lock().unwrap_or_else(|p| p.into_inner())
}

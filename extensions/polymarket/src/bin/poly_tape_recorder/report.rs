//! 会话汇总报告与判定装配（原样迁出）。

use serde_json::{Value, json};

use crate::tape::SourceStat;
use crate::verdict::{MAX_DISCONNECT_RATE_PCT, MAX_P99_LATENCY_MS, percentile, verdict_for_source};

pub(crate) fn source_report(stat: &SourceStat, duration_ms: u64) -> Value {
    let rate = if duration_ms > 0 {
        stat.unplanned_downtime_ms as f64 / duration_ms as f64 * 100.0
    } else {
        0.0
    };
    json!({
        "msgs": stat.msgs,
        "bytes": stat.bytes,
        "first_msg_recv_ts": stat.first_msg_recv_ts,
        "last_msg_recv_ts": stat.last_msg_recv_ts,
        "latency": {
            "n": stat.latencies.len(),
            "p50_ms": percentile(stat.latencies.clone(), 50.0),
            "p99_ms": percentile(stat.latencies.clone(), 99.0),
            "max_ms": stat.latencies.iter().max().copied(),
            "口径": "server_ts − recv_ts，毫秒，含场地时钟偏差（工单口径）",
        },
        "unplanned_disconnects": stat.unplanned_disconnects,
        "planned_disconnects": stat.planned_disconnects,
        "connect_failures": stat.connect_failures,
        "unplanned_downtime_ms": stat.unplanned_downtime_ms,
        "planned_downtime_ms": stat.planned_downtime_ms,
        "stalls": stat.stalls,
        "disconnect_rate_pct": (rate * 1000.0).round() / 1000.0,
        "口径_断线率": "unplanned 断线时长/会话时长×100（轮滚动 planned 重连不计入）",
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_report(
    started_at_ms: i64,
    ended_at_ms: i64,
    duration_ms: u64,
    tape_path: &std::path::Path,
    assets: &[String],
    rounds: &[i64],
    poly: std::sync::MutexGuard<'_, SourceStat>,
    spot: std::sync::MutexGuard<'_, SourceStat>,
    no_spot: bool,
    twap_rows: u64,
    tokens_final: &[String],
) -> Value {
    let poly_rep = source_report(&poly, duration_ms);
    let poly_p99 = poly_rep["latency"]["p99_ms"].as_i64();
    let poly_rate = poly_rep["disconnect_rate_pct"].as_f64().unwrap_or(0.0);
    let mut failures = verdict_for_source("poly_market", poly_rate, poly_p99);
    if !no_spot {
        let spot_rep = source_report(&spot, duration_ms);
        let spot_p99 = spot_rep["latency"]["p99_ms"].as_i64();
        let spot_rate = spot_rep["disconnect_rate_pct"].as_f64().unwrap_or(0.0);
        failures.extend(verdict_for_source("spot", spot_rate, spot_p99));
    }
    json!({
        "tool": "poly-tape-recorder",
        "issue": 860,
        "session": {
            "started_at_ms": started_at_ms,
            "ended_at_ms": ended_at_ms,
            "duration_ms": duration_ms,
            "duration_min": (duration_ms as f64 / 60_000.0 * 100.0).round() / 100.0,
            "assets": assets,
            "rounds": rounds,
            "tape": tape_path.display().to_string(),
            "pid": std::process::id(),
        },
        "sources": {
            "poly_market": poly_rep,
            "spot": if no_spot { Value::Null } else { source_report(&spot, duration_ms) },
        },
        "twap_rows": twap_rows,
        "tokens_final": tokens_final,
        "verdict": {
            "pass": failures.is_empty(),
            "failures": failures,
            "阈值": {"disconnect_rate_pct_per_h": MAX_DISCONNECT_RATE_PCT, "p99_latency_ms": MAX_P99_LATENCY_MS},
            "退出码映射": {"PASS": 0, "FAIL": 2},
        },
    })
}

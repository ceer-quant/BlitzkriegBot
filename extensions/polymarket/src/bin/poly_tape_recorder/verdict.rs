//! 断线/延迟失败判定：工单阈值 + 判定函数（原样迁出）。

/// 工单失败阈值：断线 >1%/h、p99 >1s。
pub(crate) const MAX_DISCONNECT_RATE_PCT: f64 = 1.0;
pub(crate) const MAX_P99_LATENCY_MS: i64 = 1_000;

/// 断线原因分类。Planned = 录制器自己安排的重连（轮滚动），Unplanned =
/// 链路故障（EOF/错误/stall），Stop = 会话结束。
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum BreakReason {
    Planned(String),
    Unplanned(String),
    Stop,
}

/// nearest-rank 分位数（线性数组，p∈[0,100]）。空集 → None。
pub(crate) fn percentile(mut v: Vec<i64>, p: f64) -> Option<i64> {
    if v.is_empty() {
        return None;
    }
    v.sort_unstable();
    let idx = ((p / 100.0) * (v.len() as f64 - 1.0)).round() as usize;
    Some(v[idx.min(v.len() - 1)])
}

/// 工单失败判定：断线率 >1%/h 或 p99 >1000ms → 返回失败原因列表（空=PASS）。
pub(crate) fn verdict_for_source(name: &str, rate_pct: f64, p99_ms: Option<i64>) -> Vec<String> {
    let mut failures = Vec::new();
    if rate_pct > MAX_DISCONNECT_RATE_PCT {
        failures.push(format!(
            "{name} 断线率 {rate_pct:.3}% > {MAX_DISCONNECT_RATE_PCT}%/h"
        ));
    }
    if let Some(p99) = p99_ms.filter(|v| *v > MAX_P99_LATENCY_MS) {
        failures.push(format!("{name} p99 延迟 {p99}ms > {MAX_P99_LATENCY_MS}ms"));
    }
    failures
}

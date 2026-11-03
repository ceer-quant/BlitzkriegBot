//! 尾随窗口 TWAP 跟踪器（现货最后价阶梯的时间加权均值，原样迁出）。

use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

/// TWAP 推导窗的保留余量（缓冲裁剪边界）。
pub(crate) const TWAP_WINDOW_MAX_MS: i64 = 900_000;

/// 尾随窗口 TWAP：维护 `(t_ms, price)` 段阶梯（价格自 t 起生效直到下一段）。
/// 覆盖不满窗时如实返回 covered_ms（代理诚实口径，不脑补窗外价格）。
#[derive(Default)]
pub(crate) struct TwapTracker {
    pub(crate) samples: VecDeque<(i64, f64)>,
}

impl TwapTracker {
    pub(crate) fn push(&mut self, now_ms: i64, t_ms: i64, price: f64) {
        if !price.is_finite() {
            return;
        }
        // 乱序/重复：跳过（tape 本身保真，这里只做推导）。
        if self
            .samples
            .back()
            .is_some_and(|&(lt, lp)| t_ms < lt || (t_ms == lt && price == lp))
        {
            return;
        }
        self.samples.push_back((t_ms, price));
        // 裁剪：保留能跨住 `now - 15m - 余量` 的最老一段。
        let cutoff = now_ms - TWAP_WINDOW_MAX_MS - 30_000;
        while self.samples.len() >= 2 && self.samples[1].0 < cutoff {
            self.samples.pop_front();
        }
    }

    /// `window` 毫秒尾随 TWAP。None = 窗内无价格。
    pub(crate) fn twap(&self, now_ms: i64, window_ms: i64) -> Option<(f64, i64, usize, bool)> {
        let start = now_ms - window_ms;
        let v = &self.samples;
        if v.is_empty() {
            return None;
        }
        // 跳过整段落在窗外之前的段（保留跨 start 的那一段）。
        let mut idx = 0;
        while idx + 1 < v.len() && v[idx + 1].0 <= start {
            idx += 1;
        }
        let first_t = v[idx].0.max(start);
        if first_t >= now_ms {
            return None; // 全是未来时间戳（时钟漂移），不脑补
        }
        let mut sum = 0.0;
        let mut i = idx;
        while i < v.len() {
            let (t, p) = v[i];
            let seg_start = t.max(start);
            let seg_end = if i + 1 < v.len() {
                v[i + 1].0.min(now_ms)
            } else {
                now_ms
            };
            if seg_end > seg_start {
                sum += p * (seg_end - seg_start) as f64;
            }
            i += 1;
        }
        let covered = now_ms - first_t;
        if covered <= 0 {
            return None;
        }
        let full = v[idx].0 <= start;
        Some((sum / covered as f64, covered, v.len() - idx, full))
    }

    pub(crate) fn last_price(&self) -> Option<f64> {
        self.samples.back().map(|&(_, p)| p)
    }

    pub(crate) fn snapshot(&self, now_ms: i64) -> Value {
        fn fmt(t: Option<(f64, i64, usize, bool)>) -> Value {
            match t {
                None => Value::Null,
                Some((v, covered, n, full)) => json!({
                    "value": v, "covered_ms": covered, "n": n, "full_window": full,
                }),
            }
        }
        json!({
            "spot_last": self.last_price(),
            "twap_60s": fmt(self.twap(now_ms, 60_000)),
            "twap_300s": fmt(self.twap(now_ms, 300_000)),
            "twap_900s": fmt(self.twap(now_ms, 900_000)),
        })
    }
}

pub(crate) type Trackers = Arc<Mutex<HashMap<String, TwapTracker>>>;

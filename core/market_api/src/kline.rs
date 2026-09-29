//! K-line (candlestick) domain type — DEV_V0_3 §10.2.
//!
//! Lives here, not in `blitzkrieg_core`, for the same reason
//! `MarketStructure`/`MarketCapabilities` do (§11.1 / A.1.6): the strategy API
//! needs the type for `SafeStrategy::on_kline` (`&Kline`), and the strategy API
//! may NOT depend on the core. `blitzkrieg_market_api` is the one crate every
//! side may link (it has no internal dependencies), so the shared type lives
//! here and each side re-exports it — `core::kline` and
//! `blitzkrieg_strategy_api::Kline` are the same type, not two mirrors.
//!
//! Decimals are exact: `low`/`high` feed stop-touch decisions, so f64 drift
//! here is a money problem, not a display problem.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// Bar width. Wire = `snake_case` (`sec1` … `day1`), matching the existing
/// enum convention (`MarketType`, `Side`, `OrderType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KlineInterval {
    Sec1,
    Sec5,
    Sec15,
    Min1,
    Min5,
    Min15,
    Hour1,
    Hour4,
    Day1,
}

impl KlineInterval {
    pub fn secs(self) -> i64 {
        match self {
            Self::Sec1 => 1,
            Self::Sec5 => 5,
            Self::Sec15 => 15,
            Self::Min1 => 60,
            Self::Min5 => 300,
            Self::Min15 => 900,
            Self::Hour1 => 3600,
            Self::Hour4 => 14400,
            Self::Day1 => 86400,
        }
    }

    /// Bucket start: UTC-aligned floor. `Day1` is a **UTC day**, not a local
    /// day — stated explicitly because "which midnight does a daily bar use" is
    /// the most common silent disagreement in K-line aggregation.
    ///
    /// `div_euclid` (not `/`): it floors toward negative infinity, so a
    /// pre-epoch timestamp buckets the same way a positive one does.
    pub fn bucket_open_ms(self, ts_ms: i64) -> i64 {
        let s = self.secs() * 1000;
        ts_ms.div_euclid(s) * s
    }

    /// Last millisecond of the bar that starts at `bucket_open_ms`. The ONE
    /// derivation of `Kline::close_time_ms`, so the invariant
    /// `close_time_ms == open_time_ms + secs*1000 - 1` cannot be spelled
    /// differently in two places (§10.2).
    pub fn bucket_close_ms(self, bucket_open_ms: i64) -> i64 {
        bucket_open_ms + self.secs() * 1000 - 1
    }
}

/// One OHLCV bar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Kline {
    pub symbol: String,
    pub interval: KlineInterval,
    pub open_time_ms: i64,
    /// Always `open_time_ms + interval_secs * 1000 - 1`: derived by the
    /// constructor, never passed in — see [`Kline::new`].
    pub close_time_ms: i64,
    #[serde(with = "crate::decimal")]
    pub open: Decimal,
    #[serde(with = "crate::decimal")]
    pub high: Decimal,
    #[serde(with = "crate::decimal")]
    pub low: Decimal,
    #[serde(with = "crate::decimal")]
    pub close: Decimal,
    #[serde(with = "crate::decimal")]
    pub volume: Decimal,
    pub trade_count: u64,
    /// True exactly once: when the first input past `close_time_ms` arrives.
    pub is_closed: bool,
}

impl Kline {
    /// Open a bar on its first trade. `close_time_ms` is derived from the
    /// interval, so a caller cannot create an inverted or mismatched bar.
    pub fn new(
        symbol: impl Into<String>,
        interval: KlineInterval,
        open_time_ms: i64,
        price: Decimal,
        size: Decimal,
    ) -> Self {
        Self {
            symbol: symbol.into(),
            interval,
            open_time_ms,
            close_time_ms: interval.bucket_close_ms(open_time_ms),
            open: price,
            high: price,
            low: price,
            close: price,
            volume: size,
            trade_count: 1,
            is_closed: false,
        }
    }

    /// Fold one trade into the bar. Caller must only pass trades inside the
    /// bar's window; the aggregator owns the bucketing decision (§10.3).
    pub fn apply_trade(&mut self, price: Decimal, size: Decimal) {
        if price > self.high {
            self.high = price;
        }
        if price < self.low {
            self.low = price;
        }
        self.close = price;
        self.volume += size;
        self.trade_count += 1;
    }
}

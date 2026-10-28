//! Read-only view structs the diagnostics panels consume, moved verbatim
//! from `service.rs` (pure code motion, no logic edits). `service.rs`
//! re-exports everything (`pub use views::*;`), so `crate::service::RoundView`
//! and friends resolve exactly as before the split.

use rust_decimal::Decimal;

/// Compact round info for the UI.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RoundView {
    pub slot: i64,
    pub age_sec: i64,
    pub time_left_sec: i64,
    pub markets: usize,
    pub can_trade: bool,
    /// Per-asset live prices (from the local book mid, falling back to the
    /// scanner's last price). For the UI Prices section.
    pub market_prices: Vec<MarketPriceView>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketPriceView {
    pub asset: String,
    pub up: Decimal,
    pub down: Decimal,
}

/// 盘口深度 view (E8-c): one round asset with both token books, as the UI depth
/// chart renders them. Levels are best-first and capped per side by the caller.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetBooksView {
    /// 行自身的 venue 标签（镜像盘显示簿合成行，2026-10-25）：server 端
    /// 按它打标签，不再按 token 反查 —— 镜像行与被镜像所共享 token，
    /// `venue_of` 反查会首中被镜像所，标签就错了。
    #[serde(rename = "venue")]
    pub venue: String,
    pub asset: String,
    /// E29 (§12.2): the aggregator keys prediction-token bars by TOKEN id, so
    /// the panel needs the same key `feed_klines` was fed to ask
    /// `kline.history` for the right series.
    pub up_token_id: String,
    pub down_token_id: String,
    pub up: BookSideView,
    pub down: BookSideView,
}

/// One token's live book. Every metric is `null` when no book has arrived for
/// the token — never a zero stand-in, which the panel must not read as a real
/// quote (from_levels' empty-book sentinels stay internal to the strategy
/// layer, they are not observables).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BookSideView {
    pub bids: Vec<BookLevelView>,
    pub asks: Vec<BookLevelView>,
    pub best_bid: Option<Decimal>,
    pub best_ask: Option<Decimal>,
    pub mid_price: Option<Decimal>,
    /// Order-book imbalance (bid_depth − ask_depth) / (bid_depth + ask_depth).
    pub obi: Option<Decimal>,
    pub spread: Option<Decimal>,
    pub spread_pct: Option<Decimal>,
}

impl BookSideView {
    pub(super) fn empty() -> Self {
        Self {
            bids: Vec::new(),
            asks: Vec::new(),
            best_bid: None,
            best_ask: None,
            mid_price: None,
            obi: None,
            spread: None,
            spread_pct: None,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BookLevelView {
    pub price: Decimal,
    pub size: Decimal,
}

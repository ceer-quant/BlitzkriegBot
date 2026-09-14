//! Round discovery — Rust-native (P4/P5).
//!
//! Discovers the current round's UP/DOWN markets for each configured asset by
//! querying Gamma with the slug pattern `${asset}-updown-${label}-${slotStart}`,
//! then feeds the markets to the engine and subscribes their tokens on the
//! orderbook feed. On rollover it re-discovers and re-subscribes.
//!
//! With this running, Node is out of the market-data path entirely: the core
//! finds its own markets, pulls its own books/spot, decides and trades.

use blitzkrieg_market_api::{MarketDescriptor, MarketHost};
use crate::gamma::{duration_label, slug_for};
use polymarket_client_sdk_v2::gamma::types::request::MarketsRequest;
use polymarket_client_sdk_v2::gamma::Client as GammaClient;
use std::sync::Arc;
use std::time::Duration;

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Spawn the discovery loop. `assets` are e.g. ["BTC","ETH"]; `round_sec` is the
/// round length (300 = 5m). Returns immediately.
pub fn spawn(
    host: Arc<dyn MarketHost>,
    assets: Vec<String>,
    round_sec: i64,
    poll_sec: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let client = match GammaClient::new("https://gamma-api.polymarket.com") {
            Ok(c) => c,
            Err(e) => {
                eprintln!("polymarket-extension: gamma client init failed: {e}");
                return;
            }
        };
        if duration_label(round_sec).is_none() {
            eprintln!("polymarket-extension: unsupported round duration {round_sec}s; discovery disabled");
            return;
        }

        let mut last_slot = i64::MIN;
        let mut tick = tokio::time::interval(Duration::from_secs(poll_sec.max(3)));
        loop {
            tick.tick().await;
            let now = now_ms();
            let slot = now / 1000 / round_sec;
            if slot == last_slot {
                continue; // same round already fed
            }

            let mut markets: Vec<MarketDescriptor> = Vec::new();
            for asset in &assets {
                let Some(slug) = slug_for(asset, round_sec, now / 1000) else { continue };
                let req = MarketsRequest::builder().slug(vec![slug.clone()]).build();
                let found = match client.markets(&req).await {
                    Ok(m) => m,
                    Err(e) => {
                        eprintln!("polymarket-extension: gamma query failed for {slug}: {e}");
                        continue;
                    }
                };
                let Some(m) = found.into_iter().next() else { continue };
                if m.closed.unwrap_or(false) || !m.active.unwrap_or(false) {
                    continue;
                }
                let Some(condition_id) = m.condition_id.map(|c| c.to_string()) else { continue };
                let outcomes = m.outcomes.clone().unwrap_or_default();
                let tokens = m.clob_token_ids.clone().unwrap_or_default();
                let prices = m.outcome_prices.clone().unwrap_or_default();
                if outcomes.len() < 2 || tokens.len() < 2 {
                    continue;
                }
                let up_idx = outcomes.iter().position(|o| {
                    let o = o.to_lowercase();
                    o == "up" || o == "yes"
                });
                let down_idx = outcomes.iter().position(|o| {
                    let o = o.to_lowercase();
                    o == "down" || o == "no"
                });
                let (Some(ui), Some(di)) = (up_idx, down_idx) else { continue };

                let end_ms = m.end_date.map(|d| d.timestamp_millis()).unwrap_or(0);
                if end_ms <= now {
                    continue; // already expired
                }
                markets.push(MarketDescriptor {
                    asset: asset.to_uppercase(),
                    condition_id,
                    question_id: m.question_id.map(|q| q.to_string()).unwrap_or_default(),
                    up_token_id: tokens[ui].to_string(),
                    down_token_id: tokens[di].to_string(),
                    up_price: prices.get(ui).copied().unwrap_or_else(|| rust_decimal::Decimal::new(5, 1)),
                    down_price: prices.get(di).copied().unwrap_or_else(|| rust_decimal::Decimal::new(5, 1)),
                    expires_at_ms: end_ms,
                    round_slot: end_ms / 1000 / round_sec,
                    neg_risk: m.neg_risk.unwrap_or(true),
                    question: m.question.clone().unwrap_or_default(),
                });
            }

            if markets.is_empty() {
                continue; // retry next tick (market may not be live yet)
            }
            last_slot = slot;

            let count = markets.len();
            let label_summary = markets.iter().map(|m| m.asset.clone()).collect::<Vec<_>>().join(",");

            // Registers the round with the engine AND subscribes its tokens on the
            // running data feed (both inside the host).
            host.on_round_markets(markets).await;
            eprintln!("polymarket-extension: discovered round slot={slot} ({count} markets: {label_summary})");
        }
    })
}

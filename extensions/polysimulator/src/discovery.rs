//! PolySimulator round discovery (#454).
//!
//! Unlike kalshi, this venue HAS a first-class round concept: `GET
//! /v1/markets/updown` returns the active up/down interval markets with their
//! own `start_date`/`end_date`. Discovery polls it every `poll_sec`, keeps the
//! rows that are open and not yet expired, and feeds them to the engine as one
//! round's UP/DOWN descriptors.
//!
//! Mapping decisions (all made HERE, the plugin boundary):
//!   * UP/DOWN tokens — `token_ids` inside the nested `markets[0]` is a
//!     JSON-encoded string (`"[\"71321…\", \"82432…\"]"`, the PM Gamma
//!     convention) needing a second parse; `[0]` is UP, `[1]` is DOWN. Unlike
//!     the kalshi feed there is NO book split: each token has its own
//!     `GET /v1/clob/book/{token_id}` on this venue, so the feed polls both
//!     books directly.
//!   * prices — `outcome_prices` (same double-parse) gives (up, down) as
//!     0..1 decimals straight through.
//!   * asset — the row carries its own `asset` field (BTC/ETH/SOL/…); a row
//!     matching no configured asset is skipped: the engine can only price
//!     what its strategies have a spot reference for.
//!   * round_slot — derived from the row's own `end_date` (`end_ms / 1000 /
//!     round_sec`), so a market that closes mid-slot is never filed under a
//!     slot it does not belong to.
//!   * only rows LIVE right now pass (`start <= now < end`, both halves
//!     enforced) AND rows whose own window length equals the configured
//!     round length (the 15M family when round_sec=900). The venue lists
//!     every interval × asset × future round — feeding all of it once
//!     subscribed 500+ tokens the feed's request budget could not rotate
//!     inside the scanner's freshness window (the 2026-10-25 round-clock
//!     freeze).

use crate::rest::PolySimRest;
use blitzkrieg_market_api::net::now_ms;
use blitzkrieg_market_api::{MarketDescriptor, MarketHost};
use rust_decimal::Decimal;
use std::sync::Arc;
use std::time::Duration;

/// Parse an ISO-8601 timestamp into epoch millis. The venue sends both
/// `2026-03-15T13:00:00+00:00` and `2026-03-15T14:00:00Z` shapes; the parse
/// is deliberately narrow (a wrong shape is a skipped market + a log line,
/// never a panic).
pub(crate) fn iso_to_ms(raw: &str) -> Option<i64> {
    // Hand-rolled to keep the dependency surface flat: YYYY-MM-DDTHH:MM:SS
    // with an optional fractional part and an optional offset suffix.
    let b = raw.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || (b[10] != b'T' && b[10] != b' ') {
        return None;
    }
    let num =
        |from: usize, to: usize| -> Option<i64> { raw.get(from..to)?.trim().parse::<i64>().ok() };
    let year = num(0, 4)?;
    let month = num(5, 7)?;
    let day = num(8, 10)?;
    let hour = num(11, 13)?;
    let min = num(14, 16)?;
    let sec = num(17, 19)?;
    // Days since civil epoch (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some((((days * 24 + hour) * 60 + min) * 60 + sec) * 1000)
}

/// Is this row's asset one of the configured ones? Exact symbol match
/// (uppercase both sides) — unlike kalshi, the venue gives a structured
/// field, so no prose needle-matching is needed.
pub(crate) fn asset_matches(row_asset: &str, assets: &[String]) -> Option<String> {
    let upper = row_asset.to_ascii_uppercase();
    assets
        .iter()
        .any(|a| a.to_ascii_uppercase() == upper)
        .then_some(upper)
}

/// Spawn the discovery loop. Returns immediately; the loop runs until the
/// process ends.
pub(crate) fn spawn(
    host: Arc<dyn MarketHost>,
    client: Arc<PolySimRest>,
    assets: Vec<String>,
    round_sec: i64,
    poll_sec: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // The feed-once guard compares the CONTENT (condition-id set) of the
        // last feed, NOT a wall-clock slot. The old wall-slot guard burned the
        // whole round on a single poll that fed only long-interval rows (or
        // raced the venue's own list roll): the wall slot advanced, the guard
        // said "same round already fed", and the 15M row the venue published
        // seconds later was never re-fed — the round clock froze at 00:00 for
        // a quarter hour while prices kept streaming.
        let mut last_fed: Option<std::collections::BTreeSet<String>> = None;
        let mut tick = tokio::time::interval(Duration::from_secs(poll_sec.max(3)));
        loop {
            tick.tick().await;
            let now = now_ms();

            let rows = match client.updown_markets(None, None).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(error = %e, "polysimulator discovery: updown query failed");
                    continue;
                }
            };

            let mut markets: Vec<MarketDescriptor> = Vec::new();
            for row in &rows {
                if !row.is_open() {
                    continue;
                }
                // Stock/equity daily rows carry `"asset": null` on the wire —
                // no crypto symbol, never priceable by the engine's spot
                // references. Skipped, not an error.
                let Some(row_asset) = row.asset.as_deref() else {
                    continue;
                };
                let Some(asset) = asset_matches(row_asset, &assets) else {
                    continue;
                };
                let Some(end_ms) = row.end_ms() else {
                    tracing::warn!(
                        condition = %row.condition_id,
                        "polysimulator discovery: unreadable end_date"
                    );
                    continue;
                };
                if end_ms <= now {
                    continue; // the window has already closed
                }
                // The doc always promised `start <= now < end`; the start half
                // was missing from the code. Without it every FUTURE round's
                // rows (5M/15M/1H/4H/daily × 8 assets) were subscribed at
                // once — the feed's token rotation outran the venue's request
                // budget, each book's turn fell far outside the scanner's
                // freshness window, and the round clock froze.
                let Some(start_ms) = row.start_ms() else {
                    tracing::warn!(
                        condition = %row.condition_id,
                        "polysimulator discovery: unreadable start_date"
                    );
                    continue;
                };
                if start_ms > now {
                    continue; // a future round — feed it when its window opens
                }
                // The configured round FAMILY: a row's own window length is
                // its interval. 5M/1H/4H/daily rows carry windows of other
                // lengths; feeding them subscribes books the scanner's
                // (venue, round_sec) family never judges on.
                if end_ms - start_ms != round_sec * 1000 {
                    continue; // another interval's round
                }
                let Some((up_token, down_token)) = row.parse_token_ids() else {
                    tracing::warn!(
                        condition = %row.condition_id,
                        "polysimulator discovery: unreadable token_ids"
                    );
                    continue;
                };
                let (up_price, down_price) = match row.parse_outcome_prices() {
                    Some((u, d)) => (u, d),
                    None => (
                        Decimal::ONE / Decimal::from(2),
                        Decimal::ONE / Decimal::from(2),
                    ),
                };
                let question = row.title.clone().unwrap_or_else(|| {
                    format!(
                        "{} {} Up/Down",
                        row.asset.as_deref().unwrap_or("?"),
                        row.interval
                    )
                });
                markets.push(MarketDescriptor {
                    asset: asset.clone(),
                    condition_id: row.condition_id.clone(),
                    question_id: row.event_id.clone(),
                    up_token_id: up_token,
                    down_token_id: down_token,
                    up_price,
                    down_price,
                    expires_at_ms: end_ms,
                    round_slot: end_ms / 1000 / round_sec,
                    // #427: this plugin's own identity, declared as data.
                    venue: "polysimulator".to_string(),
                    neg_risk: false,
                    question,
                });
            }

            if markets.is_empty() {
                continue; // retry next tick (nothing open for the configured assets)
            }
            if !should_feed(&last_fed, &markets) {
                continue; // same round already fed (nothing in the row set changed)
            }
            last_fed = Some(markets.iter().map(|m| m.condition_id.clone()).collect());

            let count = markets.len();
            // Registers the round with the engine AND subscribes the tokens
            // on the running data feed (both inside the host).
            host.on_round_markets(markets).await;
            eprintln!("polysimulator-extension: discovered round ({count} markets)");
        }
    })
}

/// The feed-once decision: feed when the row SET changed (a new round's rows
/// published, one expired out, an interval added) — not when a wall-clock slot
/// turned over. Idempotent on price drift: the signature is the condition-id
/// set only.
fn should_feed(
    last_fed: &Option<std::collections::BTreeSet<String>>,
    markets: &[MarketDescriptor],
) -> bool {
    let signature: std::collections::BTreeSet<String> =
        markets.iter().map(|m| m.condition_id.clone()).collect();
    last_fed.as_ref() != Some(&signature)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 喂一次守卫按内容（condition-id 集）判，不按墙钟 slot 判——真线事故：
    /// 一拍只喂到长周期行就把墙钟守卫烧掉 15 分钟，场地晚发布的 15M 行永远
    /// 补不上（轮钟冻在 00:00 而行情还在走）。
    #[test]
    fn feed_decision_follows_the_row_set_not_the_wall_clock() {
        let mk = |cond: &str| MarketDescriptor {
            asset: "BTC".into(),
            condition_id: cond.into(),
            question_id: String::new(),
            up_token_id: "up".into(),
            down_token_id: "down".into(),
            up_price: Decimal::ONE / Decimal::from(2),
            down_price: Decimal::ONE / Decimal::from(2),
            expires_at_ms: 0,
            round_slot: 0,
            venue: "polysimulator".into(),
            neg_risk: false,
            question: String::new(),
        };
        let first = vec![mk("a"), mk("b")];
        // 首拍：没喂过 → 喂。
        assert!(should_feed(&None, &first));
        let fed = Some(first.iter().map(|m| m.condition_id.clone()).collect());
        // 同一行集（价格漂移不换 id）：不重喂。
        assert!(!should_feed(&fed, &first));
        // 行集变了（新一轮的行出现/旧行过期）：喂——这正是墙钟守卫漏掉的场景。
        let second = vec![mk("a"), mk("c")];
        assert!(should_feed(&fed, &second));
        // 只喂到长周期行的拍（行集与已喂不同）：也喂——15M 行晚到必须补上。
        let long_only = vec![mk("long-1h")];
        assert!(should_feed(&fed, &long_only));
    }

    #[test]
    fn iso_timestamps_parse_across_the_shapes_the_venue_sends() {
        assert_eq!(
            iso_to_ms("2026-03-15T14:00:00Z"),
            Some(1_773_583_200_000),
            "verified against the civil-epoch algorithm (2026-03-15T14:00:00Z)"
        );
        assert_eq!(
            iso_to_ms("2026-03-15T13:00:00+00:00"),
            Some(1_773_579_600_000),
            "offset suffix is ignored (venue always sends UTC)"
        );
        assert_eq!(iso_to_ms("garbage"), None);
        assert_eq!(iso_to_ms("2026-03-15"), None);
    }

    #[test]
    fn asset_match_is_exact_symbol_not_prose() {
        let assets = vec![String::from("BTC"), String::from("ETH")];
        assert_eq!(asset_matches("btc", &assets), Some(String::from("BTC")));
        assert_eq!(asset_matches("BTC", &assets), Some(String::from("BTC")));
        assert_eq!(asset_matches("SOL", &assets), None);
        // Prose must NOT match: the venue gives a structured field.
        assert_eq!(asset_matches("Will Bitcoin go up", &assets), None);
    }
}

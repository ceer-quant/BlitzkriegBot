//! Kalshi round discovery (#424).
//!
//! Kalshi has no "round" concept of its own — the trading day's events are
//! just events. Discovery therefore mirrors the engine's round model onto
//! whatever the venue offers: every `poll_sec` it lists recent events with
//! their nested markets, keeps the ones whose close time is still ahead, and
//! feeds them to the engine as one round's UP/DOWN descriptors.
//!
//! Mapping decisions (all made HERE, the plugin boundary):
//!   * asset — Kalshi tickers carry no asset; the configured `assets` list is
//!     matched against the event/market title case-insensitively (an event
//!     titled "Will Bitcoin close above …" matches "BTC"). Markets matching
//!     no configured asset are skipped: the engine can only price what its
//!     strategies have a spot reference for.
//!   * UP/DOWN tokens — a Kalshi market is one binary with YES and NO sides.
//!     YES maps to UP (token = ticker), NO maps to DOWN (token =
//!     `ticker` + ":no"). The kernel's book updates are keyed by token id, so
//!     the feed polls the market's orderbook once and splits it: the YES side
//!     of the book becomes the UP token's book, the NO side becomes the DOWN
//!     token's. The feed's cent→probability mapping makes the two books
//!     consistent (NO price = 1 − YES price at the same level).
//!   * prices — `yes_bid`/`yes_ask` cents → probabilities; `down_price` gets
//!     the NO side's mid.
//!   * round_slot — derived from the market's own close time (`close_ms /
//!     1000 / round_sec`), so a market that closes mid-slot is never filed
//!     under a slot it does not belong to.

use crate::rest::KalshiRest;
use blitzkrieg_market_api::net::now_ms;
use blitzkrieg_market_api::{MarketDescriptor, MarketHost};
use rust_decimal::Decimal;
use std::sync::Arc;
use std::time::Duration;

/// The DOWN-side token id for a Kalshi market: the venue has one orderbook
/// per binary market; the feed splits it into a YES book and a NO book, and
/// this suffix is the agreed key for the NO half.
pub(crate) const NO_SUFFIX: &str = ":no";

/// One discovery row before it becomes a descriptor: the ticker plus which
/// asset bucket it matched, so the feed can split the book per token.
#[derive(Debug, Clone)]
pub(crate) struct DiscoveredMarket {
    pub ticker: String,
    pub asset: String,
}

/// Does this event/market title mention one of the configured assets?
/// Substring match, case-insensitive — Kalshi titles are prose
/// ("Will Bitcoin close above $100,000 on …"), not tickers.
pub(crate) fn title_matches_asset(title: &str, assets: &[String]) -> Option<String> {
    let lower = title.to_ascii_lowercase();
    // Longest needle first so the full asset name wins over a shorter alias
    // that happens to be a substring of it.
    let mut best: Option<(usize, String)> = None;
    for asset in assets {
        for needle in asset_needles(asset) {
            if lower.contains(&needle) {
                let len = needle.len();
                if best.as_ref().is_none_or(|(l, _)| len > *l) {
                    best = Some((len, asset.to_uppercase()));
                }
            }
        }
    }
    best.map(|(_, a)| a)
}

/// The strings that identify one configured asset inside a Kalshi title:
/// the ticker itself plus the prose full name, where one exists. Kalshi
/// titles are sentences ("Will Bitcoin close above …") and rarely carry the
/// ticker verbatim.
fn asset_needles(asset: &str) -> Vec<String> {
    let mut needles = vec![asset.to_ascii_lowercase()];
    let alias = match asset.to_ascii_uppercase().as_str() {
        "BTC" | "XBT" => Some("bitcoin"),
        "ETH" => Some("ethereum"),
        "SOL" => Some("solana"),
        "XRP" => Some("xrp"),
        "DOGE" => Some("dogecoin"),
        _ => None,
    };
    if let Some(a) = alias {
        needles.push(a.to_string());
    }
    needles
}

/// YES bid/ask cents → (up_price, down_price) probabilities. The NO mid is
/// 1 − YES mid by construction on Kalshi (the venue's own NO quotes mirror
/// it); deriving keeps the two sides arithmetically complementary.
pub(crate) fn prices_from_market(
    yes_bid: Option<Decimal>,
    yes_ask: Option<Decimal>,
) -> (Decimal, Decimal) {
    use std::str::FromStr;
    let half = Decimal::from(2);
    let mid = match (yes_bid, yes_ask) {
        (Some(b), Some(a)) if a >= b => (b + a) / half,
        (Some(b), Some(_)) => b, // crossed/zero book: trust the bid side
        (Some(b), None) => b,
        (None, Some(a)) => a,
        (None, None) => Decimal::from_str("0.5").unwrap_or(Decimal::from(50)),
    };
    let prob = crate::feed::cents_to_prob(mid).unwrap_or_else(|| Decimal::from_str("0.5").unwrap());
    let down = Decimal::ONE - prob;
    (prob, down)
}

/// Parse an ISO-8601 close time into epoch millis. Kalshi sends
/// `2026-10-09T14:30:00Z`; the parse is deliberately narrow (a wrong shape is
/// a skipped market + a log line, never a panic).
pub(crate) fn close_time_to_ms(raw: &str) -> Option<i64> {
    // Hand-rolled to keep the dependency surface flat: YYYY-MM-DDTHH:MM:SS(Z).
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
    // Days since civil epoch (Howard Hinnant's algorithm), then to millis.
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some((((days * 24 + hour) * 60 + min) * 60 + sec) * 1000)
}

/// Spawn the discovery loop. Returns immediately; the loop runs until the
/// process ends.
pub(crate) fn spawn(
    host: Arc<dyn MarketHost>,
    client: Arc<KalshiRest>,
    assets: Vec<String>,
    round_sec: i64,
    poll_sec: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut last_slot = i64::MIN;
        let mut tick = tokio::time::interval(Duration::from_secs(poll_sec.max(3)));
        loop {
            tick.tick().await;
            let now = now_ms();
            let slot = now / 1000 / round_sec;
            if slot == last_slot {
                continue; // same round already fed
            }

            // Recent events with their nested markets, newest first.
            let events = match client.events_with_markets(50).await {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(error = %e, "kalshi discovery: events query failed");
                    continue;
                }
            };

            let mut markets: Vec<MarketDescriptor> = Vec::new();
            let mut discovered: Vec<DiscoveredMarket> = Vec::new();
            for (_event_ticker, kalshi_markets) in &events {
                for m in kalshi_markets {
                    if !m.is_active() {
                        continue;
                    }
                    let Some(asset) =
                        title_matches_asset(&format!("{} {}", m.event_ticker, m.title), &assets)
                    else {
                        continue;
                    };
                    let Some(close_ms) = close_time_to_ms(&m.close_time) else {
                        tracing::warn!(ticker = %m.ticker, "kalshi discovery: unreadable close_time");
                        continue;
                    };
                    if close_ms <= now {
                        continue; // already closed
                    }
                    let (up_price, down_price) = prices_from_market(m.yes_bid, m.yes_ask);
                    discovered.push(DiscoveredMarket {
                        ticker: m.ticker.clone(),
                        asset: asset.clone(),
                    });
                    markets.push(MarketDescriptor {
                        asset: asset.clone(),
                        condition_id: m.event_ticker.clone(),
                        question_id: m.ticker.clone(),
                        up_token_id: m.ticker.clone(),
                        down_token_id: format!("{}{NO_SUFFIX}", m.ticker),
                        up_price,
                        down_price,
                        expires_at_ms: close_ms,
                        round_slot: close_ms / 1000 / round_sec,
                        neg_risk: false,
                        question: m.title.clone(),
                    });
                }
            }

            if markets.is_empty() {
                continue; // retry next tick (nothing open for the configured assets)
            }
            last_slot = slot;

            let count = markets.len();
            let summary = discovered
                .iter()
                .map(|d| format!("{}/{}", d.asset, d.ticker))
                .collect::<Vec<_>>()
                .join(",");

            // Registers the round with the engine AND subscribes the tickers
            // on the running data feed (both inside the host).
            host.on_round_markets(markets).await;
            eprintln!("kalshi-extension: discovered slot={slot} ({count} markets: {summary})");
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn titles_match_their_asset_case_insensitively() {
        let assets = vec![String::from("BTC"), String::from("ETH")];
        assert_eq!(
            title_matches_asset("Will Bitcoin close above $100k", &assets),
            Some(String::from("BTC"))
        );
        assert_eq!(
            title_matches_asset("KXETHD-26-OCT09 Will Ethereum be up?", &assets),
            Some(String::from("ETH"))
        );
        assert_eq!(
            title_matches_asset("Will Solana be up", &assets),
            None,
            "a market for an unconfigured asset must be skipped"
        );
    }

    #[test]
    fn prices_are_complementary_probabilities() {
        let (up, down) = prices_from_market(
            Some(Decimal::from_str("43").unwrap()),
            Some(Decimal::from_str("47").unwrap()),
        );
        assert_eq!(up, Decimal::from_str("0.45").unwrap());
        assert_eq!(down, Decimal::from_str("0.55").unwrap());
        assert_eq!(up + down, Decimal::ONE);
    }

    #[test]
    fn close_times_parse_across_the_shapes_kalshi_sends() {
        assert_eq!(
            close_time_to_ms("2026-10-09T14:30:00Z"),
            Some(1_791_556_200_000),
            "verified against chrono (datetime 2026-10-09T14:30:00Z)"
        );
        assert_eq!(close_time_to_ms("garbage"), None);
        assert_eq!(close_time_to_ms("2026-10-09"), None);
    }
}

//! predict.fun round discovery (#424).
//!
//! predict.fun has no "round" concept of its own — the venue is a list of
//! markets with an end date. Discovery therefore mirrors the engine's round
//! model onto whatever the venue offers: every `poll_sec` it lists recent
//! markets, keeps the ones whose end time is still ahead, and feeds them to
//! the engine as one round's UP/DOWN descriptors.
//!
//! Mapping decisions (all made HERE, the plugin boundary):
//!   * asset — predict.fun market rows carry no asset; the configured `assets`
//!     list is matched against the question/description text case-insensitively
//!     (a question "Will Bitcoin close above …" matches "BTC"). Markets
//!     matching no configured asset are skipped: the engine can only price
//!     what its strategies have a spot reference for.
//!   * UP/DOWN tokens — predict.fun is a CLOB over outcome tokens: one row
//!     per token, keyed by `token_id`, prices already 0..1. Unlike Kalshi
//!     there is nothing to split — the UP token of a binary pair is the row
//!     itself and its DOWN complement is the sibling row sharing the same
//!     `condition_id`. A row with no sibling (or a pair with more than two
//!     outcomes, which the binary wheel cannot price) is skipped and logged.
//!   * prices — the venue's own `best_bid`/`best_ask` mid per row; the pair's
//!     `down_price` comes from the sibling row's own mid (venue-reported on
//!     both sides, never derived by 1−x, so a stale sibling cannot masquerade
//!     as a fresh price).
//!   * round_slot — derived from the market's own end time (`end_ms / 1000 /
//!     round_sec`), so a market that ends mid-slot is never filed under a slot
//!     it does not belong to.

use crate::rest::PredictRest;
use blitzkrieg_market_api::net::now_ms;
use blitzkrieg_market_api::{MarketDescriptor, MarketHost};
use std::sync::Arc;
use std::time::Duration;

/// Does this question/description text mention one of the configured assets?
/// Substring match, case-insensitive — predict.fun questions are prose
/// ("Will Bitcoin close above $100,000 on …"), not tickers. Same needle table
/// as the Kalshi discovery (one asset vocabulary, two venues).
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

/// The strings that identify one configured asset inside a question: the
/// ticker itself plus the prose full name, where one exists.
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

/// Parse an ISO-8601 end time into epoch millis. predict.fun sends
/// `2026-10-31T23:59:00Z`; the parse is deliberately narrow (a wrong shape is
/// a skipped market + a log line, never a panic).
pub(crate) fn end_time_to_ms(raw: &str) -> Option<i64> {
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

/// One matched pair of outcome rows: the UP token row and its DOWN sibling
/// (same `condition_id`). The asset bucket is assigned in the spawn loop,
/// where the configured `assets` list lives.
#[derive(Debug, Clone)]
pub(crate) struct PairedMarket {
    pub condition_id: String,
    pub up: crate::types::PredictMarket,
    pub down: crate::types::PredictMarket,
}

/// Fold venue rows into UP/DOWN pairs keyed by `condition_id`. Groups whose
/// outcome count is not exactly two are skipped (a binary wheel cannot price
/// them; skipping beats inventing a mapping). Asset matching happens later in
/// the spawn loop, where the configured `assets` list lives.
pub(crate) fn pair_markets(rows: Vec<crate::types::PredictMarket>) -> Vec<PairedMarket> {
    let mut groups: std::collections::HashMap<String, Vec<crate::types::PredictMarket>> =
        std::collections::HashMap::new();
    for row in rows {
        let Some(cid) = row.condition_id.clone() else {
            continue;
        };
        groups.entry(cid).or_default().push(row);
    }
    let mut pairs = Vec::new();
    for (condition_id, mut members) in groups {
        if members.len() != 2 {
            continue;
        }
        members.sort_by(|a, b| a.token_id.cmp(&b.token_id));
        let [up, down] = members.as_slice() else {
            continue;
        };
        pairs.push(PairedMarket {
            condition_id,
            up: up.clone(),
            down: down.clone(),
        });
    }
    pairs
}

/// Spawn the discovery loop. Returns immediately; the loop runs until the
/// process ends.
pub(crate) fn spawn(
    host: Arc<dyn MarketHost>,
    client: Arc<PredictRest>,
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

            // One page of live markets (paging beyond that is a stage-4
            // concern; the engine trades one round's binaries, not the venue).
            let rows = match client.markets(200).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(error = %e, "predictfun discovery: markets query failed");
                    continue;
                }
            };

            let mut markets: Vec<MarketDescriptor> = Vec::new();
            let mut summary: Vec<String> = Vec::new();
            for pair in pair_markets(rows) {
                let text = format!(
                    "{} {} {} {}",
                    pair.up.question,
                    pair.up.description.clone().unwrap_or_default(),
                    pair.down.question,
                    pair.down.description.clone().unwrap_or_default(),
                );
                let Some(matched_asset) = title_matches_asset(&text, &assets) else {
                    continue;
                };
                let (up, down) = (&pair.up, &pair.down);
                let Some(end_ms) = up
                    .end_date
                    .as_deref()
                    .and_then(end_time_to_ms)
                    .or(down.end_date.as_deref().and_then(end_time_to_ms))
                else {
                    tracing::warn!(
                        condition_id = %pair.condition_id,
                        "predictfun discovery: unreadable end_date"
                    );
                    continue;
                };
                if end_ms <= now {
                    continue; // already ended
                }
                if !up.is_active() || !down.is_active() {
                    continue; // one halted side is not a tradeable pair
                }
                let (Some(up_price), Some(down_price)) = (up.mid(), down.mid()) else {
                    continue; // no two-sided quote on a side: not priceable yet
                };
                summary.push(up.token_id.clone());
                markets.push(MarketDescriptor {
                    asset: matched_asset.clone(),
                    condition_id: pair.condition_id.clone(),
                    question_id: pair.condition_id.clone(),
                    up_token_id: up.token_id.clone(),
                    down_token_id: down.token_id.clone(),
                    up_price,
                    down_price,
                    expires_at_ms: end_ms,
                    round_slot: end_ms / 1000 / round_sec,
                    // #427: this plugin's own identity, declared as data.
                    venue: "predictfun".to_string(),
                    neg_risk: up.is_neg_risk || down.is_neg_risk,
                    question: up.question.clone(),
                });
            }

            if markets.is_empty() {
                continue; // retry next tick (nothing open for the configured assets)
            }
            last_slot = slot;

            let count = markets.len();
            let joined = summary.join(",");

            // Registers the round with the engine AND subscribes the tokens
            // on the running data feed (both inside the host).
            host.on_round_markets(markets).await;
            eprintln!("predictfun-extension: discovered slot={slot} ({count} markets: {joined})");
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::PredictMarket;
    use rust_decimal::Decimal;
    use std::str::FromStr;

    fn row(token_id: &str, condition_id: Option<&str>, question: &str) -> PredictMarket {
        serde_json::from_value(serde_json::json!({
            "token_id": token_id,
            "question": question,
            "condition_id": condition_id,
        }))
        .unwrap()
    }

    #[test]
    fn questions_match_their_asset_case_insensitively() {
        let assets = vec![String::from("BTC"), String::from("ETH")];
        assert_eq!(
            title_matches_asset("Will Bitcoin close above $100k", &assets),
            Some(String::from("BTC"))
        );
        assert_eq!(
            title_matches_asset("Will Ethereum be up on Friday?", &assets),
            Some(String::from("ETH"))
        );
        assert_eq!(
            title_matches_asset("Will Solana be up", &assets),
            None,
            "a market for an unconfigured asset must be skipped"
        );
    }

    #[test]
    fn end_times_parse_across_the_shapes_the_venue_sends() {
        assert_eq!(
            end_time_to_ms("2026-10-09T14:30:00Z"),
            Some(1_791_556_200_000),
            "verified against chrono (datetime 2026-10-09T14:30:00Z)"
        );
        assert_eq!(end_time_to_ms("garbage"), None);
        assert_eq!(end_time_to_ms("2026-10-09"), None);
    }

    #[test]
    fn binary_pairs_fold_by_condition_id() {
        let mut up = row("0xup", Some("0xc9d1"), "Will BTC close above $100k?");
        up.best_bid = Some(Decimal::from_str("0.44").unwrap());
        up.best_ask = Some(Decimal::from_str("0.46").unwrap());
        let mut down = row("0xdown", Some("0xc9d1"), "Will BTC close above $100k?");
        down.best_bid = Some(Decimal::from_str("0.54").unwrap());
        down.best_ask = Some(Decimal::from_str("0.56").unwrap());
        let pairs = pair_markets(vec![up, down]);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].up.token_id, "0xdown");
        assert_eq!(pairs[0].down.token_id, "0xup");
    }

    #[test]
    fn groups_that_are_not_exactly_two_outcomes_are_skipped() {
        let a = row("0xa", Some("0xtri"), "Will it be up, mid, or down?");
        let b = row("0xb", Some("0xtri"), "…");
        let c = row("0xc", Some("0xtri"), "…");
        assert!(pair_markets(vec![a, b, c]).is_empty());
        // A row with no condition id cannot be paired at all.
        let orphan = row("0xsolo", None, "Will BTC close above $100k?");
        assert!(pair_markets(vec![orphan]).is_empty());
    }

    #[test]
    fn pairing_does_not_depend_on_row_order() {
        let mut first = row("0xup", Some("0xp"), "Will ETH be up?");
        first.best_bid = Some(Decimal::from_str("0.44").unwrap());
        first.best_ask = Some(Decimal::from_str("0.46").unwrap());
        let mut second = row("0xdown", Some("0xp"), "Will ETH be up?");
        second.best_bid = Some(Decimal::from_str("0.54").unwrap());
        second.best_ask = Some(Decimal::from_str("0.56").unwrap());
        let forward = pair_markets(vec![first.clone(), second.clone()]);
        let backward = pair_markets(vec![second, first]);
        assert_eq!(forward[0].up.token_id, backward[0].up.token_id);
        assert_eq!(forward[0].down.token_id, backward[0].down.token_id);
    }
}

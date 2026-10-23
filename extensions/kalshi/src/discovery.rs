//! Kalshi round discovery (#424, rebuilt against the live venue 2026-10-15).
//!
//! Kalshi's 15m crypto rounds live in the `KX{ASSET}15M` series (one series
//! per asset, one OPEN event per round, one market per event) — discovered
//! by querying `/events?series_ticker=KX{ASSET}15M&status=open`. The old
//! "list every event and substring-match titles" approach was blind on the
//! real venue: the default event listing ranks long-running events first, so
//! the 15m rounds never reached page 1, and `"resolved"` substring-matched
//! SOL (4× SOL rows for one asset).
//!
//! Mapping decisions (all made HERE, the plugin boundary):
//!   * series — `series_ticker_for(asset, round_sec)`: 15m rounds map to
//!     `KX{ASSET}15M`. Kalshi ships no 5m/1h/4h crypto-round series
//!     (verified), so those configurations simply find nothing rather than
//!     discovering wrong markets. The daily family (`KX{ASSET}D`) is also
//!     NOT routed — deliberately: it is a strike ladder, not an up/down
//!     binary (see `series_ticker_for` and the one-time warning in
//!     `discovery_loop`).
//!   * asset — the series name carries the asset; the title match is only a
//!     fallback sanity check (word-boundary, so "solved" is not SOL).
//!   * UP/DOWN tokens — a Kalshi market is one binary with YES and NO sides.
//!     YES maps to UP (token = ticker), NO maps to DOWN (token =
//!     `ticker` + ":no"). The feed polls the market's orderbook once and
//!     splits it into the two token books (duality: a NO bid at p IS a YES
//!     ask at 1−p).
//!   * prices — the `_dollars` quote fields are already 0..1 probabilities;
//!     the descriptor takes the YES mid (and 1 − mid for DOWN). A market with
//!     no quote at all is SKIPPED — the old `(None, None) => 0.5` fallback
//!     manufactured a fake probability out of an empty book.
//!   * round_slot — derived from the market's own close time
//!     (`close_ms / 1000 / round_sec`), and a close time further than two
//!     rounds out is REJECTED: the old code filed multi-market event ladders
//!     (strike ladder expiring in years) under the current slot.

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

/// The Kalshi round series for one configured asset at the engine's round
/// length. Only the 15m crypto-round family is routable: `KX{ASSET}15M`.
/// Anything else returns None — discovery finding nothing is honest;
/// discovering the WRONG market is not.
///
/// The daily family `KX{ASSET}D` is deliberately NOT routed, even though the
/// series exists on the venue. Live-venue audit (2026-10-23,
/// `target/kalshi-daily-audit-report.md`) settled it: `KX{ASSET}D` is a
/// STRIKE LADDER, not an up/down binary — each event carries 188–300
/// mutually exclusive `strike_type: greater` markets ("close ≥ $X", BRTI
/// 60s-average settlement), so consuming it as UP/DOWN pairs misfeeds the
/// engine three ways: every rung lands in the same round_slot, spread_arb
/// reads a near-ITM rung as trend strength, and cross_venue_arb's
/// UP+DOWN=$1 pair economics collapses (`pair_for_asset` silently nils
/// once multiple rungs are listed). No true daily up/down family exists on
/// the venue (all 278 crypto series enumerated). Re-enable only after a
/// venue-side daily up/down family actually ships.
pub(crate) fn series_ticker_for(asset: &str, round_sec: i64) -> Option<String> {
    let sym = match asset.to_ascii_uppercase().as_str() {
        "BTC" | "XBT" => "BTC",
        "ETH" => "ETH",
        "SOL" => "SOL",
        "XRP" => "XRP",
        "DOGE" => "DOGE",
        _ => return None,
    };
    match round_sec {
        900 => Some(format!("KX{sym}15M")),
        // 86_400 (KX{ASSET}D) intentionally unmapped: strike ladder, not an
        // up/down binary. `discovery_loop` warns once when this round length
        // is configured, so an operator is never left with a silent empty
        // market set.
        _ => None,
    }
}

/// One-time explicit warning that the configured round length is the daily
/// family the plugin refuses to route. Fires once per process on the first
/// discovery tick, before the loop settles into its normal cadence. Returns
/// whether THIS call emitted the warning (test hook: the alarm is
/// once-per-process, so tests probe the gate directly).
fn warn_daily_family_disabled_once(round_sec: i64) -> bool {
    use std::sync::atomic::{AtomicBool, Ordering};
    static DAILY_WARNED: AtomicBool = AtomicBool::new(false);
    if round_sec != 86_400 || DAILY_WARNED.swap(true, Ordering::Relaxed) {
        return false;
    }
    tracing::warn!(
        target: "kalshi",
        round_sec,
        "kalshi discovery: daily (86400s) round family DISABLED — Kalshi KX{{ASSET}}D \
         (KXBTCD/KXETHD/…) is a STRIKE LADDER, not an up/down binary: each event lists \
         188–300 mutually-exclusive 'close ≥ $X' rungs (strike_type=greater, BRTI 60s \
         average settlement), so there is no up/down pair to consume. Routing it as \
         UP/DOWN misfeeds every rung into one round_slot, makes spread_arb buy near-ITM \
         rungs as fake trend, and breaks cross_venue_arb's UP+DOWN=$1 economics (audit: \
         target/kalshi-daily-audit-report.md, verdict: 选项2 daily 族禁用+告警). \
         Switch round_duration to 900 (15m) — the only verified up/down round family."
    );
    true
}

/// Word-boundary asset match inside prose titles. `"Will SOL be up"` matches
/// SOL; `"The market was resolved"` does NOT (the old plain-`contains` filed
/// every resolved market under SOL).
pub(crate) fn title_matches_asset(title: &str, assets: &[String]) -> Option<String> {
    let lower = title.to_ascii_lowercase();
    let mut best: Option<(usize, String)> = None;
    for asset in assets {
        for needle in asset_needles(asset) {
            if matches_at_word_boundary(&lower, &needle) {
                let len = needle.len();
                if best.as_ref().is_none_or(|(l, _)| len > *l) {
                    best = Some((len, asset.to_uppercase()));
                }
            }
        }
    }
    best.map(|(_, a)| a)
}

/// `needle` occurs in `hay` as a whole word (non-alphanumeric on both sides,
/// or at the string edge). "solved" contains "sol" but not as a word.
fn matches_at_word_boundary(hay: &str, needle: &str) -> bool {
    let is_word = |c: char| c.is_ascii_alphanumeric();
    let mut start = 0;
    while let Some(pos) = hay[start..].find(needle) {
        let at = start + pos;
        let before_ok = hay[..at].chars().next_back().is_none_or(|c| !is_word(c));
        let after_at = at + needle.len();
        let after_ok = hay[after_at..].chars().next().is_none_or(|c| !is_word(c));
        if before_ok && after_ok {
            return true;
        }
        start = at + 1;
    }
    false
}

/// The strings that identify one configured asset inside a Kalshi title:
/// the ticker itself plus the prose full name, where one exists. Kalshi
/// titles are sentences ("BTC price up in next 15 mins?") and rarely carry
/// the ticker verbatim.
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
    discovery_loop(host, client, assets, round_sec, poll_sec)
}

/// Test-only entry for the #548 gate: drive the real discovery loop against a
/// mock server with an explicit cadence. Not part of the public surface —
/// it exists so the reverse-acceptance gate exercises the production loop,
/// not a copy of it (same pattern as `feed_tests_only_spawn`).
#[doc(hidden)]
pub fn discovery_tests_only_spawn(
    host: Arc<dyn MarketHost>,
    client: Arc<KalshiRest>,
    assets: Vec<String>,
    round_sec: i64,
    poll_sec: u64,
) -> tokio::task::JoinHandle<()> {
    discovery_loop(host, client, assets, round_sec, poll_sec)
}

fn discovery_loop(
    host: Arc<dyn MarketHost>,
    client: Arc<KalshiRest>,
    assets: Vec<String>,
    round_sec: i64,
    poll_sec: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // #548：最近一次实际喂入的 (slot, 批签名)。顶部逐 poll 重查 + 这个
        // 闸替代了旧「slot == last_slot → continue」守卫（它让 #498 轮内补
        // 查门永不可达）：同 slot 批未变跳过重喂；venue 补上架/换发 token
        // 改变签名 → 重喂（engine 同 slot merge 按 token 去重，只增不炸）。
        let mut last_fed_slot = i64::MIN;
        let mut last_fed_signature = String::new();
        // #audit-daily: a configured 86400s round now routes to NOTHING (the
        // daily family is a strike ladder, audit-settled). Warn once, loudly,
        // instead of letting every asset fall through the per-asset `continue`
        // silently — an operator must be able to tell "disabled on purpose"
        // from "venue has nothing listed".
        warn_daily_family_disabled_once(round_sec);
        let mut tick = tokio::time::interval(Duration::from_secs(poll_sec.max(3)));
        loop {
            tick.tick().await;
            let now = now_ms();
            let slot = now / 1000 / round_sec;

            // One series query per configured asset. A series that does not
            // exist for this round length (e.g. 5m) is skipped silently —
            // discovery finding nothing is the honest answer.
            let mut markets: Vec<MarketDescriptor> = Vec::new();
            let mut discovered: Vec<DiscoveredMarket> = Vec::new();
            for asset in &assets {
                let Some(series) = series_ticker_for(asset, round_sec) else {
                    continue;
                };
                let events = match client.open_events_for_series(&series).await {
                    Ok(e) => e,
                    Err(e) => {
                        tracing::warn!(series = %series, error = %e, "kalshi discovery: series query failed");
                        continue;
                    }
                };
                for (_event_ticker, kalshi_markets) in events {
                    for m in kalshi_markets {
                        if !m.is_active() {
                            continue;
                        }
                        // Sanity: the title must still speak the asset's
                        // language (word-boundary match — "resolved" is not
                        // SOL). A series row whose title disagrees is a venue
                        // surprise worth skipping loudly.
                        if title_matches_asset(&m.title, std::slice::from_ref(asset)).is_none() {
                            tracing::warn!(
                                ticker = %m.ticker,
                                title = %m.title,
                                "kalshi discovery: series row title does not match its asset; skipping"
                            );
                            continue;
                        }
                        let Some(close_ms) = close_time_to_ms(&m.close_time) else {
                            tracing::warn!(ticker = %m.ticker, "kalshi discovery: unreadable close_time");
                            continue;
                        };
                        if close_ms <= now {
                            continue; // already closed
                        }
                        // A real round closes within one round length. Anything
                        // further out (strike ladders, stale events) must not
                        // hijack the round clock — the old code fed a 2045
                        // expiry into `observe_end_time` and the panel counted
                        // down 19 years.
                        if close_ms - now > 2 * round_sec * 1000 {
                            tracing::warn!(
                                ticker = %m.ticker,
                                "kalshi discovery: close time too far out for a round; skipping"
                            );
                            continue;
                        }
                        // A market with no quote is skipped, never priced at a
                        // made-up 0.5 (the old fallback manufactured fake
                        // probabilities from empty books).
                        let Some(mid) = m.yes_mid_prob() else {
                            continue;
                        };
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
                            up_price: mid,
                            down_price: Decimal::ONE - mid,
                            expires_at_ms: close_ms,
                            round_slot: close_ms / 1000 / round_sec,
                            // #427: this plugin's own identity, declared as data.
                            venue: "kalshi".to_string(),
                            neg_risk: false,
                            question: m.title.clone(),
                        });
                    }
                }
            }

            if markets.is_empty() {
                continue; // retry next tick (nothing open for the configured assets)
            }
            // #498: the venue lists the round's asset events STAGGERED — the
            // same slot has been observed at 2, then 3, then 4 markets on
            // reboots minutes apart. Locking `last_slot` on the first
            // non-empty tick froze the round at whatever subset the first
            // poll caught, so the missing assets never traded that round
            // ("只剩一个币对"). Keep re-polling INSIDE the slot until every
            // configured asset has a market (or no asset is still missing a
            // series that exists — an asset the venue genuinely has nothing
            // for is already skipped silently above), and re-feed the FULL
            // set when it grows: the feed's subscription is replace-semantics
            // and the engine merges a same-slot feed additively.
            let found_assets: std::collections::HashSet<&str> =
                discovered.iter().map(|d| d.asset.as_str()).collect();
            let missing: Vec<&String> = assets
                .iter()
                .filter(|a| {
                    !found_assets.contains(a.as_str()) && series_ticker_for(a, round_sec).is_some()
                })
                .collect();

            // #548：批未增长且 slot 已喂过 → 跳过重喂（防 5s 一条的重复
            // RoundMarkets/日志）。partial 披露沿用 #498 的 note 通道。
            let signature = markets
                .iter()
                .map(|m| format!("{}:{}|{}", m.asset, m.up_token_id, m.down_token_id))
                .collect::<Vec<_>>()
                .join(",");
            if slot == last_fed_slot && signature == last_fed_signature {
                continue;
            }

            let count = markets.len();
            let summary = discovered
                .iter()
                .map(|d| format!("{}/{}", d.asset, d.ticker))
                .collect::<Vec<_>>()
                .join(",");

            // Registers the round with the engine AND subscribes the tickers
            // on the running data feed (both inside the host).
            host.on_round_markets(markets).await;
            let note = if missing.is_empty() {
                String::new()
            } else {
                format!(
                    " [partial: {} still unlisted, re-polling within the slot]",
                    missing
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(",")
                )
            };
            eprintln!(
                "kalshi-extension: discovered slot={slot} ({count} markets: {summary}){note}"
            );
            last_fed_slot = slot;
            last_fed_signature = signature;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn series_names_follow_the_venue_families() {
        assert_eq!(series_ticker_for("BTC", 900).as_deref(), Some("KXBTC15M"));
        assert_eq!(series_ticker_for("doge", 900).as_deref(), Some("KXDOGE15M"));
        assert_eq!(series_ticker_for("XBT", 900).as_deref(), Some("KXBTC15M"));
        // Round lengths the venue does not ship — and the deliberately
        // disabled daily family (its own regression test below) — must find
        // nothing.
        assert_eq!(series_ticker_for("BTC", 300), None);
        assert_eq!(series_ticker_for("BTC", 3_600), None);
        assert_eq!(series_ticker_for("BTC", 14_400), None);
        assert_eq!(series_ticker_for("APE", 900), None);
    }

    #[test]
    fn daily_family_is_disabled_strike_ladder_not_an_updown_binary() {
        // Regression gate for the audit-settled fix (2026-10-23,
        // `target/kalshi-daily-audit-report.md`): KX{ASSET}D is a STRIKE
        // LADDER, not an up/down binary — every event carries 188–300
        // mutually-exclusive `strike_type: greater` rungs ("close ≥ $X", BRTI
        // 60s-average settlement). Consuming it as UP/DOWN pairs put all 238
        // rungs in one round_slot, made spread_arb read near-ITM rungs as
        // trend, and voided cross_venue_arb's UP+DOWN=$1 economics
        // (pair_for_asset silently nils). No true daily up/down family
        // exists on the venue (278 crypto series enumerated), so 86400s
        // rounds must route to NOTHING (discovery_loop warns once instead).
        for asset in ["BTC", "XBT", "ETH", "SOL", "XRP", "DOGE", "btc"] {
            assert_eq!(
                series_ticker_for(asset, 86_400),
                None,
                "the daily strike-ladder family must never be routed as an up/down round"
            );
        }
    }

    #[test]
    fn daily_family_warning_fires_once_per_process() {
        // The explicit-alarm half of the fix: the one-time warning fires for
        // a daily configuration (first call returns true), never for other
        // round lengths, and repeats are suppressed (silent skip would hide
        // an operator-visible misconfiguration again).
        assert!(
            !warn_daily_family_disabled_once(900),
            "15m configurations must not trip the daily warning"
        );
        assert!(
            warn_daily_family_disabled_once(86_400),
            "the first daily tick must emit the explicit warning"
        );
        assert!(
            !warn_daily_family_disabled_once(86_400),
            "repeats are suppressed — it is a ONE-TIME alarm, not per-poll spam"
        );
    }

    #[test]
    fn titles_match_their_asset_case_insensitively() {
        let assets = vec![String::from("BTC"), String::from("ETH")];
        assert_eq!(
            title_matches_asset("BTC price up in next 15 mins?", &assets),
            Some(String::from("BTC"))
        );
        assert_eq!(
            title_matches_asset("Will Bitcoin close above $100k", &assets),
            Some(String::from("BTC"))
        );
        assert_eq!(
            title_matches_asset("Will Ethereum be up?", &assets),
            Some(String::from("ETH"))
        );
        assert_eq!(
            title_matches_asset("Will Solana be up", &assets),
            None,
            "a market for an unconfigured asset must be skipped"
        );
    }

    #[test]
    fn resolved_does_not_substring_match_sol() {
        // The live-venue bug: "The market was resolved" contains "sol" as a
        // plain substring, which filed every resolved market under SOL.
        let assets = vec![String::from("SOL")];
        assert_eq!(title_matches_asset("Market resolved", &assets), None);
        assert_eq!(title_matches_asset("Insolvency report", &assets), None);
        assert_eq!(
            title_matches_asset("Will SOL be up", &assets),
            Some(String::from("SOL"))
        );
        assert_eq!(
            title_matches_asset("Will Solana be up", &assets),
            Some(String::from("SOL"))
        );
    }

    #[test]
    fn market_quotes_parse_as_probabilities_and_empty_books_are_none() {
        use crate::types::KalshiMarket;
        let quoted = KalshiMarket {
            ticker: "T-1".into(),
            event_ticker: "T".into(),
            market_type: "binary".into(),
            title: "t".into(),
            subtitle: None,
            close_time: "2026-10-15T12:30:00Z".into(),
            expiration_time: None,
            status: Some("active".into()),
            yes_bid_dollars: Some(dec!(0.65)),
            yes_ask_dollars: Some(dec!(0.66)),
            no_bid_dollars: Some(dec!(0.34)),
            no_ask_dollars: Some(dec!(0.35)),
            volume_fp: None,
        };
        assert_eq!(quoted.yes_mid_prob(), Some(dec!(0.655)));

        let empty = KalshiMarket {
            yes_bid_dollars: Some(dec!(0.0000)),
            yes_ask_dollars: Some(dec!(1.0000)),
            no_bid_dollars: Some(dec!(0.0000)),
            no_ask_dollars: Some(dec!(1.0000)),
            ..quoted.clone()
        };
        assert_eq!(
            empty.yes_mid_prob(),
            None,
            "0/1 sentinels are the venue's no-quote markers, never prices"
        );

        let blank = KalshiMarket {
            yes_bid_dollars: None,
            yes_ask_dollars: None,
            no_bid_dollars: None,
            no_ask_dollars: None,
            ..quoted
        };
        assert_eq!(blank.yes_mid_prob(), None);
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

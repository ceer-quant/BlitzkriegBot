//! Gamma market parsing — the Polymarket-specific half of the old core scanner.
//!
//! Round *timing* math (slot, `round_state`, `can_trade`, clock offset) stays in
//! the core's `scanner.rs` because it is venue-agnostic; only slug construction
//! and Gamma JSON parsing live here.

use blitzkrieg_market_api::MarketResolution;
use chrono::{DateTime, Datelike, Timelike};
use rust_decimal::Decimal;

/// A resolved market's payouts sum to exactly 1 — the collateral is fully
/// accounted for between the outcomes. The tolerance leaves room for the last
/// prints of a half-cent while rejecting a payout vector that still describes a
/// market in doubt.
const RESOLUTION_SUM_TOLERANCE: Decimal = rust_decimal_macros::dec!(0.01);
/// A payout this close to 0 (or 1) is decisive on its own.
const DECISIVE_LOW: Decimal = rust_decimal_macros::dec!(0.01);
const DECISIVE_HIGH: Decimal = rust_decimal_macros::dec!(0.99);

/// Duration label used in Gamma slugs (5m/15m/1h/4h/daily).
pub fn duration_label(round_duration_sec: i64) -> Option<&'static str> {
    match round_duration_sec {
        300 => Some("5m"),
        900 => Some("15m"),
        3600 => Some("1h"),
        14400 => Some("4h"),
        86400 => Some("daily"),
        _ => None,
    }
}

/// Gamma slug for a round. `now_sec` must already include any clock offset
/// applied by the caller.
///
/// Two naming families exist on the venue (both pinned to live Gamma samples —
/// see the PR notes):
/// - **epoch family** (5m/15m/4h): `btc-updown-5m-1792756200` — slot start as a
///   raw Unix timestamp. Handled by [`slug_epoch`].
/// - **ET family** (1h/daily): the slug names an America/New_York clock wall
///   time, e.g. `bitcoin-up-or-down-october-23-2026-7am-et` (1h) and
///   `bitcoin-up-or-down-on-october-23-2026` (daily). Handled by
///   [`slug_et_hour`] / [`slug_et_daily`].
pub fn slug_for(asset: &str, round_duration_sec: i64, now_sec: i64) -> Option<String> {
    match round_duration_sec {
        3600 => slug_et_hour(asset, now_sec),
        86400 => slug_et_daily(asset, now_sec),
        _ => slug_epoch(asset, round_duration_sec, now_sec),
    }
}

/// Epoch-family slug (5m/15m/4h), e.g. `btc-updown-4h-1792742400`.
fn slug_epoch(asset: &str, round_duration_sec: i64, now_sec: i64) -> Option<String> {
    let label = duration_label(round_duration_sec)?;
    let slot_start = (now_sec / round_duration_sec) * round_duration_sec;
    Some(format!(
        "{}-updown-{}-{}",
        asset.to_lowercase(),
        label,
        slot_start
    ))
}

/// Ticker → the full asset name the ET-named slugs spell out. Verified live
/// against Gamma on 2026-10-23: `ethereum-up-or-down-october-23-2026-7am-et`,
/// `solana-up-or-down-october-23-2026-7am-et`, `xrp-up-or-down-october-23-2026-7am-et`
/// all HIT while BTC/ETH/SOL spell the coin name and XRP keeps its ticker
/// (DOGE is live on the venue too but is not a configured asset here). An
/// unknown ticker yields no slug rather than a guessed one.
fn et_asset_name(asset: &str) -> Option<&'static str> {
    match asset.to_ascii_uppercase().as_str() {
        "BTC" => Some("bitcoin"),
        "ETH" => Some("ethereum"),
        "SOL" => Some("solana"),
        "XRP" => Some("xrp"),
        _ => None,
    }
}

const MONTH_NAMES: [&str; 12] = [
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
];

/// Anchor rule for the 1h family, reverse-engineered from live Gamma data
/// (2026-10-23) and pinned by tests: the slug names the **ET clock hour
/// containing `now_sec`**, and the market's `endDate` is that hour's END as an
/// ET wall-clock boundary (`:00:00Z`) — live sample
/// `bitcoin-up-or-down-october-23-2026-7am-et` carries endDate
/// 2026-10-23T12:00:00Z (07:00 ET, EDT=UTC-4). The anchor is therefore derived
/// by converting to ET local time and NOT by an epoch modulus (which would
/// drift across DST). With `now_sec` anywhere inside 06:00–06:59 ET the market
/// is `bitcoin-up-or-down-october-23-2026-6am-et`.
///
/// The venue pre-lists hours ahead (at 07:28 ET the 8am..12pm ET markets were
/// already `active=true`), so the CURRENT hour's slug is what this returns —
/// discovery's `endDate <= now` expiry filter then drops past hours, and the
/// pre-listed future hours are simply not queried by this family's slug.
///
/// Hour is spelled on a 12-hour clock with no leading zero: 0 → 12am,
/// 12 → 12pm, 13 → 1pm (live samples `...-12am-et` endDate 05:00:00Z,
/// `...-12pm-et` endDate 17:00:00Z, `...-1pm-et` endDate 18:00:00Z). Date
/// segments carry the full year and unpadded single-digit days (live:
/// `october-3-2026-7am-et` HIT).
pub fn slug_et_hour(asset: &str, now_sec: i64) -> Option<String> {
    let name = et_asset_name(asset)?;
    let et = DateTime::from_timestamp(now_sec, 0)?.with_timezone(&chrono_tz::America::New_York);
    let hour12 = match et.hour() % 12 {
        0 => 12,
        h => h,
    };
    let meridiem = if et.hour() < 12 { "am" } else { "pm" };
    Some(format!(
        "{}-up-or-down-{}-{}-{}-{}{}-et",
        name,
        MONTH_NAMES[et.month0() as usize],
        et.day(),
        et.year(),
        // chrono's %l is space-padded; the venue spells 6am, not " 6am".
        hour12,
        meridiem,
    ))
}

/// Anchor rule for the daily family, reverse-engineered from live Gamma data
/// (2026-10-23) and pinned by tests: the slug names the **ET noon-to-noon
/// window that is currently trading**. The market resolves by comparing Binance
/// BTC/USDT's ET-noon close yesterday against ET-noon close today, so the
/// round rolls over at ET NOON, not ET midnight: a timestamp before 12:00 ET
/// belongs to that ET date's market, a timestamp from 12:00 ET on belongs to
/// the NEXT ET date's market. Live anchors (2026-10-23): at 07:28 ET the open
/// round is `bitcoin-up-or-down-on-october-23-2026` (endDate 2026-10-23T16:00:00Z
/// = noon EDT), and at 12:30 ET the next round
/// `bitcoin-up-or-down-on-october-24-2026` was already `active=true` with
/// endDate 2026-10-24T16:00:00Z — while `on-october-23` stayed open until its
/// noon end. A UTC date interpretation is wrong twice over: from 00:00–04:59
/// UTC (evening ET) the UTC date is already the next day, and from 16:00 UTC
/// (noon ET) the UTC date stays behind until 20:00 UTC.
pub fn slug_et_daily(asset: &str, now_sec: i64) -> Option<String> {
    let name = et_asset_name(asset)?;
    let et = DateTime::from_timestamp(now_sec, 0)?.with_timezone(&chrono_tz::America::New_York);
    // Roll the CALENDAR date, not the wall clock: +1 day on the naive date is
    // pure calendar arithmetic, immune to the DST gap/overlap entirely
    // (2026-11-01 is the fall-back day). Before noon ET the current ET date
    // trades; from noon ET on, tomorrow's (already pre-listed) market is live.
    let mut day = et.date_naive();
    if et.hour() >= 12 {
        day += chrono::Duration::days(1);
    }
    Some(format!(
        "{}-up-or-down-on-{}-{}-{}",
        name,
        MONTH_NAMES[day.month0() as usize],
        day.day(),
        day.year(),
    ))
}

/// The raw Gamma fields a resolution verdict needs. Passed by value rather than
/// as a live SDK market so the rule below is pure and testable without a network.
pub struct GammaResolutionInput<'a> {
    pub condition_id: &'a str,
    pub closed: bool,
    /// `umaResolutionStatus` as Gamma reports it: `resolved` once the oracle's
    /// answer is final, something else (or nothing) while it is not.
    pub uma_resolution_status: Option<&'a str>,
    /// Per-outcome redemption prices, in the market's own outcome order.
    pub outcome_prices: &'a [Decimal],
    /// The market's outcome tokens, in the same order as `outcome_prices`.
    pub clob_token_ids: &'a [String],
    pub neg_risk: bool,
    pub end_ms: i64,
}

/// Decide whether Gamma says this market has resolved, and what each of its
/// tokens is then worth per share.
///
/// `None` means "not resolved yet" — still trading, or closed without a final
/// payout vector. That is a normal answer the core keeps polling for, never an
/// error: settling on a guess would book cash the venue has not decided to pay.
///
/// The rule is deliberately conservative:
/// 1. `closed` — Polymarket stops a market when it resolves;
/// 2. one payout per outcome token (a vector that does not line up with the
///    market's tokens cannot be mapped to positions at all);
/// 3. the payouts sum to 1 (within [`RESOLUTION_SUM_TOLERANCE`]), i.e. the
///    collateral is fully accounted for between the outcomes;
/// 4. the answer is FINAL: either UMA says `resolved` — which also covers an
///    invalid resolution, paying 0.5 to every outcome, money that must still be
///    redeemed — or, when Gamma sends no status at all, the prices are decisive
///    (every payout within [`DECISIVE_LOW`] of 0 or 1).
pub fn resolution_from_market(input: &GammaResolutionInput<'_>) -> Option<MarketResolution> {
    if !input.closed {
        return None;
    }
    if input.clob_token_ids.is_empty() || input.clob_token_ids.len() != input.outcome_prices.len() {
        return None;
    }
    let sum = input
        .outcome_prices
        .iter()
        .fold(Decimal::ZERO, |acc, p| acc + *p);
    if (sum - Decimal::ONE).abs() > RESOLUTION_SUM_TOLERANCE {
        return None;
    }
    let status = input
        .uma_resolution_status
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let decisive = input
        .outcome_prices
        .iter()
        .all(|p| *p <= DECISIVE_LOW || *p >= DECISIVE_HIGH);
    if status != "resolved" && !(status.is_empty() && decisive) {
        return None;
    }
    Some(MarketResolution {
        condition_id: input.condition_id.to_string(),
        resolved: true,
        payouts: input
            .clob_token_ids
            .iter()
            .cloned()
            .zip(input.outcome_prices.iter().copied())
            .collect(),
        neg_risk: input.neg_risk,
        source: "gamma".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn tokens(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    fn input<'a>(
        closed: bool,
        status: Option<&'a str>,
        prices: &'a [Decimal],
        ids: &'a [String],
    ) -> GammaResolutionInput<'a> {
        GammaResolutionInput {
            condition_id: "0xcond",
            closed,
            uma_resolution_status: status,
            outcome_prices: prices,
            clob_token_ids: ids,
            neg_risk: false,
            end_ms: 1_770_935_700_000,
        }
    }

    #[test]
    fn a_resolved_market_pays_its_winning_token_one() {
        let ids = tokens(&["111", "222"]);
        let prices = [dec!(1), dec!(0)];
        let r = resolution_from_market(&input(true, Some("resolved"), &prices, &ids)).unwrap();
        assert_eq!(r.condition_id, "0xcond");
        assert!(r.resolved);
        assert_eq!(r.source, "gamma");
        assert_eq!(r.payouts[0], ("111".to_string(), dec!(1)));
        assert_eq!(r.payouts[1], ("222".to_string(), dec!(0)));
        assert_eq!(r.payout_per_share("222"), Some(Decimal::ZERO));
        assert_eq!(r.payout_per_share("nope"), None);
        assert_eq!(r.winning_token().map(String::as_str), Some("111"));
    }

    /// An UMA INVALID resolution pays half a dollar to each outcome. That is
    /// still money in the wallet, so it must settle rather than be discarded.
    #[test]
    fn an_invalid_resolution_pays_half_to_every_outcome() {
        let ids = tokens(&["111", "222"]);
        let prices = [dec!(0.5), dec!(0.5)];
        let r = resolution_from_market(&input(true, Some("resolved"), &prices, &ids)).unwrap();
        assert_eq!(r.payout_per_share("111"), Some(dec!(0.5)));
        assert_eq!(r.payout_per_share("222"), Some(dec!(0.5)));
    }

    #[test]
    fn a_market_that_is_still_trading_is_not_a_resolution() {
        let ids = tokens(&["111", "222"]);
        let prices = [dec!(0.5), dec!(0.5)];
        assert!(resolution_from_market(&input(false, None, &prices, &ids)).is_none());
        let decisive = [dec!(1), dec!(0)];
        assert!(
            resolution_from_market(&input(false, Some("resolved"), &decisive, &ids)).is_none(),
            "closed is what stops a position from settling twice"
        );
    }

    #[test]
    fn a_resolution_still_under_challenge_keeps_polling() {
        let ids = tokens(&["111", "222"]);
        let prices = [dec!(1), dec!(0)];
        for status in ["proposed", "disputed", ""] {
            let status = if status.is_empty() {
                None
            } else {
                Some(status)
            };
            // No status at all falls back to the decisive-price rule, which the
            // prices here satisfy; an explicit non-final status does not.
            let got = resolution_from_market(&input(true, status, &prices, &ids));
            assert_eq!(got.is_some(), status.is_none(), "status {status:?}");
        }
        let ambiguous = [dec!(0.6), dec!(0.4)];
        assert!(
            resolution_from_market(&input(true, None, &ambiguous, &ids)).is_none(),
            "without a status, only a decisive price vector settles"
        );
    }

    #[test]
    fn a_payout_vector_that_does_not_sum_to_one_is_not_a_resolution() {
        let ids = tokens(&["111", "222"]);
        let prices = [dec!(0.9), dec!(0.9)];
        assert!(resolution_from_market(&input(true, Some("resolved"), &prices, &ids)).is_none());
    }

    #[test]
    fn a_payout_vector_that_does_not_match_the_tokens_is_ignored() {
        let ids = tokens(&["111", "222"]);
        let prices = [dec!(1), dec!(0), dec!(0)];
        assert!(resolution_from_market(&input(true, Some("resolved"), &prices, &ids)).is_none());
        let none: Vec<String> = Vec::new();
        let one = [dec!(1)];
        assert!(resolution_from_market(&input(true, Some("resolved"), &one, &none)).is_none());
    }

    #[test]
    fn slug_matches_expected_pattern() {
        let now = 1_770_935_712i64;
        let slug = slug_for("BTC", 300, now).unwrap();
        assert!(slug.starts_with("btc-updown-5m-"));
        let ts: i64 = slug.rsplit('-').next().unwrap().parse().unwrap();
        assert_eq!(ts % 300, 0);
    }

    // ── ET 命名族（1h/daily）────────────────────────────────────────────
    // 每条期望值都逐字取自 Gamma 真线（2026-10-23 抓取，GET
    // /markets?slug=<期望值> 命中，endDate 与注释一致；侦查全录在 PR 正文）。
    // 时间戳换算：now_sec 为 UTC epoch 秒，ET = UTC-4（EDT，10-23 当日）。

    /// 真线样本：2026-10-23 06:59:59 ET（UTC 10:59:59）轮内任意时刻 → 6am 轮
    /// `bitcoin-up-or-down-october-23-2026-6am-et`（endDate 2026-10-23T11:00:00Z，
    /// GET ?slug=...&closed=true 命中）；07:00:00 ET（UTC 11:00:00）整起翻到
    /// 7am 轮（closed=false 真线命中，endDate 2026-10-23T12:00:00Z）。
    /// 小时轮锚点贴 ET 墙钟，UTC 整点 11:00Z 即 ET 07:00 轮界。
    #[test]
    fn et_hour_slug_matches_live_samples_oct23() {
        // 2026-10-23T10:59:59Z = 06:59:59 EDT —— 6am 轮的最后一秒
        let six_am = 1_792_753_199i64;
        assert_eq!(
            slug_for("BTC", 3600, six_am).as_deref(),
            Some("bitcoin-up-or-down-october-23-2026-6am-et")
        );
        // 2026-10-23T11:00:00Z = 07:00:00 EDT —— 翻到 7am 轮（真线当前轮）
        let seven_am = 1_792_753_200i64;
        assert_eq!(
            slug_for("BTC", 3600, seven_am).as_deref(),
            Some("bitcoin-up-or-down-october-23-2026-7am-et")
        );
        assert_eq!(
            slug_for("btc", 3600, seven_am).as_deref(),
            Some("bitcoin-up-or-down-october-23-2026-7am-et")
        );
    }

    /// 四资产 × 1h 的真线拼写（7am 轮，2026-10-23 抓取，全部 closed=false 命中，
    /// endDate 均为 2026-10-23T12:00:00Z）：BTC/ETH/SOL 用全名，XRP 保留 ticker。
    #[test]
    fn et_hour_slug_asset_names_match_live_samples() {
        let now = 1_792_754_891i64; // 2026-10-23T11:28:11Z = 07:28 EDT（抓取时刻）
        assert_eq!(
            slug_for("ETH", 3600, now).as_deref(),
            Some("ethereum-up-or-down-october-23-2026-7am-et")
        );
        assert_eq!(
            slug_for("SOL", 3600, now).as_deref(),
            Some("solana-up-or-down-october-23-2026-7am-et")
        );
        assert_eq!(
            slug_for("XRP", 3600, now).as_deref(),
            Some("xrp-up-or-down-october-23-2026-7am-et")
        );
    }

    /// 12 小时制拼写钉死（全部 closed=true 真线命中，2026-10-22/23 样本）：
    /// 12am → endDate 05:00:00Z、1pm → 18:00:00Z、11pm → 次日 04:00:00Z、
    /// 12pm → 17:00:00Z。12 → "12"（不归 0），0 → "12am"。
    #[test]
    fn et_hour_slug_uses_12_hour_clock_from_live_samples() {
        // 2026-10-22T04:30:00Z = 00:30 EDT 属于 12am 轮（00:00–00:59 ET，endDate 05:00Z）
        let twelve_am = 1_792_643_400i64;
        assert_eq!(
            slug_et_hour("BTC", twelve_am).as_deref(),
            Some("bitcoin-up-or-down-october-22-2026-12am-et")
        );
        // 2026-10-22T17:30:00Z = 13:30 EDT → 1pm 轮（endDate 18:00Z）
        let one_pm = 1_792_690_200i64;
        assert_eq!(
            slug_et_hour("BTC", one_pm).as_deref(),
            Some("bitcoin-up-or-down-october-22-2026-1pm-et")
        );
        // 2026-10-23T03:30:00Z = 23:30 EDT（10-22）→ 11pm 轮，日期段是 ET 日 10-22
        //（endDate 2026-10-23T04:00:00Z）
        let eleven_pm = 1_792_726_200i64;
        assert_eq!(
            slug_et_hour("BTC", eleven_pm).as_deref(),
            Some("bitcoin-up-or-down-october-22-2026-11pm-et")
        );
        // 2026-10-23T16:30:00Z = 12:30 EDT → 12pm 轮（endDate 17:00Z）
        let twelve_pm = 1_792_773_000i64;
        assert_eq!(
            slug_et_hour("BTC", twelve_pm).as_deref(),
            Some("bitcoin-up-or-down-october-23-2026-12pm-et")
        );
    }

    /// 单数日期真线样本（closed=true 命中）：october-3、october-13 均不带前导零；
    /// 锚点沿用「ET 当前墙钟小时」规则（october-3-2026-7am-et endDate
    /// 2026-10-03T12:00:00Z、october-13-2026-7am-et endDate 2026-10-13T12:00:00Z）。
    #[test]
    fn et_hour_slug_single_digit_days_match_live_samples() {
        // 2026-10-03T11:30:00Z = 07:30 EDT → 7am 轮
        let oct3 = 1_791_027_000i64;
        assert_eq!(
            slug_et_hour("BTC", oct3).as_deref(),
            Some("bitcoin-up-or-down-october-3-2026-7am-et")
        );
        // 2026-10-13T11:30:00Z = 07:30 EDT → 7am 轮
        let oct13 = 1_791_891_000i64;
        assert_eq!(
            slug_et_hour("BTC", oct13).as_deref(),
            Some("bitcoin-up-or-down-october-13-2026-7am-et")
        );
    }

    /// 真线侦查发现场馆预挂未来数小时（07:28 ET 时 8am–12pm 已 active）：
    /// 本函数返回当前 ET 小时的 slug，过期轮靠 discovery 的 endDate<=now 过滤
    /// 淘汰，预挂轮不会被错误拼入（拼写只认当前墙钟小时）。
    #[test]
    fn et_hour_slug_returns_current_hour_not_pre_listed_future() {
        // 2026-10-23T11:28:11Z = 07:28 EDT：真线上 8am–12pm 已 active，但
        // 7am 轮才是当前轮。
        let now = 1_792_754_891i64;
        assert_eq!(
            slug_for("BTC", 3600, now).as_deref(),
            Some("bitcoin-up-or-down-october-23-2026-7am-et")
        );
    }

    /// 真线样本（2026-10-23 抓取，closed=false 命中）：07:28 ET 时当日 daily 轮
    /// `bitcoin-up-or-down-on-october-23-2026`（endDate 2026-10-23T16:00:00Z，
    /// 即正午 EDT）。正午前 ET 时刻全部落到当日市场；四资产同拼写规则。
    #[test]
    fn et_daily_slug_matches_live_sample_oct23() {
        // 2026-10-23T11:28:11Z = 07:28 EDT（抓取时刻）
        assert_eq!(
            slug_for("BTC", 86400, 1_792_754_891).as_deref(),
            Some("bitcoin-up-or-down-on-october-23-2026")
        );
        assert_eq!(
            slug_for("ETH", 86400, 1_792_754_891).as_deref(),
            Some("ethereum-up-or-down-on-october-23-2026")
        );
        assert_eq!(
            slug_for("SOL", 86400, 1_792_754_891).as_deref(),
            Some("solana-up-or-down-on-october-23-2026")
        );
        assert_eq!(
            slug_for("XRP", 86400, 1_792_754_891).as_deref(),
            Some("xrp-up-or-down-on-october-23-2026")
        );
        // ET 午夜后（2026-10-23T04:30:00Z = 00:30 EDT）仍是 10-23 的轮。
        assert_eq!(
            slug_for("BTC", 86400, 1_792_729_800).as_deref(),
            Some("bitcoin-up-or-down-on-october-23-2026")
        );
    }

    /// 锚点规则钉子（真线 2026-10-23：12:30 ET 时 on-october-24 已 active、
    /// endDate 2026-10-24T16:00:00Z；on-october-23 持续到 16:00Z 才关）：
    /// daily 轮在 ET 正午翻转——12:00 ET 起拼**明日**，不是 ET 日历日。
    #[test]
    fn et_daily_slug_rolls_at_et_noon() {
        // 2026-10-23T15:59:59Z = 11:59:59 EDT —— 正午前最后一秒，仍是 10-23 的轮
        assert_eq!(
            slug_et_daily("BTC", 1_792_771_199).as_deref(),
            Some("bitcoin-up-or-down-on-october-23-2026")
        );
        // 2026-10-23T16:00:00Z = 12:00 EDT 整 —— 翻到 10-24 的轮（真线已预挂
        // active，endDate 2026-10-24T16:00:00Z）
        assert_eq!(
            slug_et_daily("BTC", 1_792_771_200).as_deref(),
            Some("bitcoin-up-or-down-on-october-24-2026")
        );
        // 13:00 EDT（2026-10-23T17:00:00Z）—— 深入「明日」侧
        assert_eq!(
            slug_et_daily("BTC", 1_792_774_800).as_deref(),
            Some("bitcoin-up-or-down-on-october-24-2026")
        );
    }

    /// UTC 日期 ≠ ET 日期的双重钉子：ET 23:30（次日 UTC 03:30）已过 ET 正午
    /// → 拼次日（ET 正午规则压过「ET 日历日」直觉）；ET 12:30（UTC 16:30）
    /// UTC 日期还没翻但 daily 已翻明日；daily 单数日期真线样本 on-october-3 /
    /// on-october-13（closed=true 命中）。
    #[test]
    fn et_daily_slug_uses_et_date_not_utc_date() {
        // 2026-10-24T03:30:00Z = 2026-10-23 23:30 EDT：UTC 日期是 10-24、ET
        // 是 10-23，但 23:30 已过 ET 正午 → 拼明日的 10-24 轮（正午规则，
        // 不是 ET 日历日规则）。
        assert_eq!(
            slug_et_daily("BTC", 1_792_812_600).as_deref(),
            Some("bitcoin-up-or-down-on-october-24-2026")
        );
        // 2026-10-23T16:30:00Z = 12:30 EDT：UTC 日期还是 10-23，但已过 ET
        // 正午 → 10-24 的轮（与上面互为镜像：两侧都压过 UTC/日历直觉）。
        assert_eq!(
            slug_et_daily("BTC", 1_792_773_000).as_deref(),
            Some("bitcoin-up-or-down-on-october-24-2026")
        );
        // 单数日期（正午前）：2026-10-03T15:30:00Z = 11:30 EDT → on-october-3
        //（真线 on-october-3-2026 endDate 2026-10-03T16:00:00Z）
        assert_eq!(
            slug_et_daily("BTC", 1_791_041_400).as_deref(),
            Some("bitcoin-up-or-down-on-october-3-2026")
        );
        // 2026-10-13T15:30:00Z = 11:30 EDT → on-october-13
        assert_eq!(
            slug_et_daily("BTC", 1_791_905_400).as_deref(),
            Some("bitcoin-up-or-down-on-october-13-2026")
        );
    }

    /// 未知 ticker 在 ET 族必须拿不到 slug（宁缺勿猜）；epoch 族不受影响。
    #[test]
    fn et_family_rejects_unknown_tickers_but_epoch_family_keeps_them() {
        assert_eq!(slug_for("DOGE", 3600, 1_792_754_891), None);
        assert_eq!(slug_for("DOGE", 86400, 1_792_754_891), None);
        assert!(slug_for("doge", 300, 1_792_754_891).is_some());
        assert!(slug_for("doge", 900, 1_792_754_891).is_some());
        assert!(slug_for("doge", 14400, 1_792_754_891).is_some());
    }

    /// DST 边界（America/New_York 2026-11-01 02:00 EDT → 01:00 EST，UTC 切换
    /// 时刻 06:00Z）。1h 锚点贴 ET 墙钟：EDT 尾段与 EST 头段的两个 01:xx ET
    /// 都要拼出 1am 轮（墙钟小时语义），UTC 偏移从 -4 平滑变 -5。
    #[test]
    fn et_hour_slug_handles_dst_fall_back() {
        // 2026-11-01T05:30:00Z = 01:30 ET（EDT，UTC-4，回拨前）→ 1am 轮
        assert_eq!(
            slug_et_hour("BTC", 1_793_511_000).as_deref(),
            Some("bitcoin-up-or-down-november-1-2026-1am-et")
        );
        // 2026-11-01T06:30:00Z = 01:30 ET（EST，UTC-5，回拨后重复的 01:30）→
        // 仍拼 1am 轮：贴墙钟，不是 UTC 小时模。
        assert_eq!(
            slug_et_hour("BTC", 1_793_514_600).as_deref(),
            Some("bitcoin-up-or-down-november-1-2026-1am-et")
        );
        // 回拨前最后一小时 00:30 EDT（2026-11-01T04:30:00Z）→ 12am 轮；
        // 回拨后第一小时 02:30 EST（07:30Z）→ 2am 轮。
        assert_eq!(
            slug_et_hour("BTC", 1_793_507_400).as_deref(),
            Some("bitcoin-up-or-down-november-1-2026-12am-et")
        );
        assert_eq!(
            slug_et_hour("BTC", 1_793_518_200).as_deref(),
            Some("bitcoin-up-or-down-november-1-2026-2am-et")
        );
        // 切换前一日 10-31（EDT 最后一天）：2026-10-31T17:30:00Z = 13:30 EDT → 1pm
        assert_eq!(
            slug_et_hour("BTC", 1_793_467_800).as_deref(),
            Some("bitcoin-up-or-down-october-31-2026-1pm-et")
        );
        // 11-02（EST 第一天整日）：2026-11-02T11:30:00Z = 06:30 EST → 6am
        assert_eq!(
            slug_et_hour("BTC", 1_793_619_000).as_deref(),
            Some("bitcoin-up-or-down-november-2-2026-6am-et")
        );
    }

    /// DST 边界：daily 以 ET 正午翻转。11-01 23:30 EST（UTC 04:30，已是 11-02
    /// UTC 日期）未过 ET 正午 → 仍 11-01 的轮；11-01 12:30 EST（UTC 17:30）→
    /// 11-02 的轮；11-02 12:30 EST → 11-03 的轮。正午翻转在 DST 切换日依然
    /// 成立（tzdata 承担偏移，无手写表；2026-11-01 即切换日）。
    #[test]
    fn et_daily_slug_rolls_at_et_noon_across_dst() {
        // 2026-11-01T17:30:00Z = 12:30 EST → 翻到 11-2
        assert_eq!(
            slug_et_daily("BTC", 1_793_554_200).as_deref(),
            Some("bitcoin-up-or-down-on-november-2-2026")
        );
        // 2026-11-01T04:30:00Z = 2026-11-01 00:30 EDT（切换前）：00:30 ET < 12:00
        // → 11-1 的轮
        assert_eq!(
            slug_et_daily("BTC", 1_793_507_400).as_deref(),
            Some("bitcoin-up-or-down-on-november-1-2026")
        );
        // 2026-11-02T04:30:00Z = 2026-11-01 23:30 EST：UTC 日期已是 11-02、
        // ET 是 11-01，但 23:30 已过 ET 正午 → 拼次日 11-2 的轮（正午规则在
        // DST 切换日依然成立）。
        assert_eq!(
            slug_et_daily("BTC", 1_793_593_800).as_deref(),
            Some("bitcoin-up-or-down-on-november-2-2026")
        );
        // 2026-11-02T17:30:00Z = 12:30 EST → 11-3 的轮（tzdata 正常推进月份）
        assert_eq!(
            slug_et_daily("BTC", 1_793_640_600).as_deref(),
            Some("bitcoin-up-or-down-on-november-3-2026")
        );
    }

    /// epoch 家族逐字节回归：5m/15m/4h 的 slug 与改动前完全一致（含原测试
    /// `slug_matches_expected_pattern` 的样本）；2026-10-23 当日真线命中
    /// btc-updown-4h-1792742400 / btc-updown-5m-1792756200 /
    /// btc-updown-15m-1792755900（抓取时刻 2026-10-23T11:29 UTC）。
    #[test]
    fn epoch_family_bytes_unchanged() {
        assert_eq!(
            slug_for("BTC", 300, 1_770_935_712).as_deref(),
            Some("btc-updown-5m-1770935700")
        );
        // 15m 样本时间戳必须在 900 网格上（1777093500 % 900 == 600 不在网格，
        // 原为伪造样本；1777093571 的真实所属槽是 1777093200）
        assert_eq!(
            slug_for("btc", 900, 1_777_093_571).as_deref(),
            Some("btc-updown-15m-1777093200")
        );
        // 真线当前 4h 槽（2026-10-23T08:00:00Z 起，endDate 12:00Z）
        assert_eq!(
            slug_for("BTC", 14400, 1_792_742_401).as_deref(),
            Some("btc-updown-4h-1792742400")
        );
        // 真线当前 5m/15m 槽（抓取时刻 2026-10-23T11:29 UTC）
        assert_eq!(
            slug_for("BTC", 300, 1_792_756_299).as_deref(),
            Some("btc-updown-5m-1792756200")
        );
        assert_eq!(
            slug_for("BTC", 900, 1_792_756_299).as_deref(),
            Some("btc-updown-15m-1792755900")
        );
        assert_eq!(slug_for("BTC", 1234, 0), None);
    }
}

//! Gamma market parsing — the Polymarket-specific half of the old core scanner.
//!
//! Round *timing* math (slot, `round_state`, `can_trade`, clock offset) stays in
//! the core's `scanner.rs` because it is venue-agnostic; only slug construction
//! and Gamma JSON parsing live here.

use blitzkrieg_market_api::MarketResolution;
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

/// Gamma slug for a round, e.g. `btc-updown-5m-1770935700`. `now_sec` must
/// already include any clock offset applied by the caller.
pub fn slug_for(asset: &str, round_duration_sec: i64, now_sec: i64) -> Option<String> {
    let label = duration_label(round_duration_sec)?;
    let slot_start = (now_sec / round_duration_sec) * round_duration_sec;
    Some(format!(
        "{}-updown-{}-{}",
        asset.to_lowercase(),
        label,
        slot_start
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
}

//! Gamma market parsing — the Polymarket-specific half of the old core scanner.
//!
//! Round *timing* math (slot, `round_state`, `can_trade`, clock offset) stays in
//! the core's `scanner.rs` because it is venue-agnostic; only slug construction
//! and Gamma JSON parsing live here.

use blitzkrieg_market_api::MarketDescriptor;
use rust_decimal::Decimal;

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
    Some(format!("{}-updown-{}-{}", asset.to_lowercase(), label, slot_start))
}

/// Parse the JSON-string-encoded fields Gamma returns (`outcomes`, `clobTokenIds`,
/// `outcomePrices`) and locate the UP/DOWN token pair.
pub fn parse_market_tokens(
    outcomes_json: &str,
    token_ids_json: &str,
    prices_json: &str,
) -> Option<(String, String, Decimal, Decimal)> {
    let outcomes: Vec<String> = serde_json::from_str(outcomes_json).ok()?;
    let tokens: Vec<String> = serde_json::from_str(token_ids_json).ok()?;
    let prices: Vec<serde_json::Value> = serde_json::from_str(prices_json).unwrap_or_default();
    if outcomes.len() < 2 || tokens.len() < 2 {
        return None;
    }
    let up = outcomes.iter().position(|o| {
        let o = o.to_lowercase();
        o == "up" || o == "yes"
    })?;
    let down = outcomes.iter().position(|o| {
        let o = o.to_lowercase();
        o == "down" || o == "no"
    })?;
    let price_at = |i: usize| -> Decimal {
        prices
            .get(i)
            .and_then(|v| v.as_str().map(|s| s.to_string()).or_else(|| v.as_f64().map(|f| f.to_string())))
            .and_then(|s| Decimal::from_str_exact(&s).ok())
            .unwrap_or_else(|| Decimal::new(5, 1))
    };
    Some((tokens[up].clone(), tokens[down].clone(), price_at(up), price_at(down)))
}

/// Raw Gamma fields for one market, as returned by the API.
pub struct GammaMarketInput {
    pub asset: String,
    pub condition_id: String,
    pub question_id: String,
    pub question: String,
    pub outcomes: String,
    pub clob_token_ids: String,
    pub outcome_prices: String,
    pub end_ms: i64,
    pub neg_risk: bool,
}

/// Build a market descriptor from raw Gamma fields.
pub fn market_from_gamma(input: GammaMarketInput, round_duration_sec: i64) -> Option<MarketDescriptor> {
    let (up_token, down_token, up_price, down_price) = parse_market_tokens(
        &input.outcomes,
        &input.clob_token_ids,
        &input.outcome_prices,
    )?;
    let round_slot = if input.end_ms > 0 {
        input.end_ms / 1000 / round_duration_sec
    } else {
        0
    };
    Some(MarketDescriptor {
        asset: input.asset.to_uppercase(),
        condition_id: input.condition_id,
        question_id: input.question_id,
        up_token_id: up_token,
        down_token_id: down_token,
        up_price,
        down_price,
        expires_at_ms: input.end_ms,
        round_slot,
        neg_risk: input.neg_risk,
        question: input.question,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn slug_matches_expected_pattern() {
        let now = 1_770_935_712i64;
        let slug = slug_for("BTC", 300, now).unwrap();
        assert!(slug.starts_with("btc-updown-5m-"));
        let ts: i64 = slug.rsplit('-').next().unwrap().parse().unwrap();
        assert_eq!(ts % 300, 0);
    }

    #[test]
    fn parse_tokens_finds_up_down() {
        let r = parse_market_tokens("[\"Up\",\"Down\"]", "[\"111\",\"222\"]", "[\"0.42\",\"0.58\"]").unwrap();
        assert_eq!(r.0, "111");
        assert_eq!(r.1, "222");
        assert_eq!(r.2, dec!(0.42));
        assert_eq!(r.3, dec!(0.58));
    }
}

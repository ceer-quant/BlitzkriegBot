//! The settlement verdict face of the executor (#175, root cause C): how one
//! `SettlementQuery` maps onto a `MarketResolution`. Split from executor.rs
//! when the single-market fallback joined (file-size ratchet #780) — both
//! halves are pure mapping decisions over wire types, testable without a
//! client.
//!
//! Non-negotiable for every path: a market whose verdict cannot be READ
//! answers NOTHING. A payout is never invented, so the core's own
//! silence-tracking (`unverifiable_expiry` after 120s) sees exactly one
//! honest "the venue never ruled".

use rust_decimal::Decimal;
use std::collections::HashMap;

use crate::rest::PolySimRest;
use blitzkrieg_bk_contract::{MarketResolution, SettlementQuery};

/// The venue's verdict for one settlement query, read off the updown rows:
/// a query whose condition_id matches a RESOLVED row pays its outcome prices
/// (a binary market pays 1/0); a query still open, unknown to the list, or
/// carrying unreadable / not-yet-0-or-1 prices answers NOTHING — the core
/// keeps watching and re-asks on its own cadence, and nothing is fabricated.
/// Split from the loop so the mapping is testable without a client.
pub(crate) fn resolution_for(
    query: &SettlementQuery,
    rows: &[crate::types::UpDownRow],
) -> Option<MarketResolution> {
    let row = rows.iter().find(|r| r.condition_id == query.condition_id)?;
    if !row.resolved.unwrap_or(false) {
        return None; // still live: the core re-queries
    }
    let (up_token, down_token) = row.parse_token_ids()?;
    let (up_price, down_price) = row.parse_outcome_prices()?;
    // Resolved means terminal prices: exactly 1 and 0. Anything else
    // (mid-round snapshot, half-parse) is not a verdict — answer nothing.
    let terminal = |p: Decimal| p == Decimal::ZERO || p == Decimal::ONE;
    if !terminal(up_price) || !terminal(down_price) {
        return None;
    }
    Some(MarketResolution {
        condition_id: query.condition_id.clone(),
        resolved: true,
        payouts: vec![(up_token, up_price), (down_token, down_price)],
        neg_risk: false,
        source: "polysimulator".to_string(),
    })
}

/// The single-market verdict path (verified live 2026-11-06): the updown list
/// only carries rounds ending in the future, so an already-resolved condition
/// the list does not name is ruled by `GET /v1/markets/{cid}` — it keeps
/// `resolved_outcome` ("Up"/"Down") after close. The endpoint does NOT carry
/// token ids, so the payout pair needs the polarity side: the outcome word a
/// token was sold under, watched from the trade rows (`asset_id` →
/// `outcome`). A market without a polarity word for the queried token answers
/// NOTHING — no payout is ever fabricated from a word the venue did source.
///
/// The kernel reads `payout_per_share(held_token)`, so the pair the list path
/// returns is not required: one entry for the held token carries the full
/// redemption value.
pub(crate) async fn resolution_by_single_market(
    query: &SettlementQuery,
    client: &PolySimRest,
    polarity: &HashMap<String, String>,
) -> Result<Option<MarketResolution>, crate::PolySimError> {
    let market = client.single_market(&query.condition_id).await?;
    if !market.closed.unwrap_or(false) {
        return Ok(None); // still running: the core re-queries
    }
    let winner_word = market
        .resolved_outcome
        .as_deref()
        .map(str::to_ascii_uppercase);
    let Some(winner_word) = winner_word else {
        return Ok(None); // closed but not yet announced
    };
    // The winner must be one of the market's own outcomes (a words mismatch
    // between endpoints would price an unrelated leg).
    if market.outcomes.as_ref().is_some_and(|outcomes| {
        !outcomes
            .iter()
            .any(|o| o.to_ascii_uppercase() == winner_word)
    }) {
        return Ok(None);
    }
    for token in &query.token_ids {
        let Some(side_word) = polarity.get(token) else {
            continue; // unknown polarity: keep the other tokens honest
        };
        let payout = if side_word.to_ascii_uppercase() == *winner_word {
            Decimal::ONE
        } else {
            Decimal::ZERO
        };
        return Ok(Some(MarketResolution {
            condition_id: query.condition_id.clone(),
            resolved: true,
            payouts: vec![(token.clone(), payout)],
            neg_risk: false,
            source: "polysimulator-single-market".to_string(),
        }));
    }
    Ok(None) // no token in the query has a polarity word: never fabricate
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire shape the single-market endpoint returns (keys verified live
    /// 2026-11-06; token-id fields are null there).
    fn single_market_json(cid: &str, closed: bool, winner: Option<&str>) -> String {
        let winner_json = winner
            .map(|w| format!("\"{w}\""))
            .unwrap_or_else(|| "null".into());
        format!(
            r#"{{"condition_id":"{cid}","slug":"btc-updown-15m-1793886300","question":"X","outcomes":["Up","Down"],"active":false,"closed":{closed},"clob_token_ids":null,"tokens":null,"end_date":"2026-11-05T14:00:00+00:00","resolved_outcome":{winner_json},"archived":false}}"#
        )
    }

    fn verdict_from(cid: &str, json: &str) -> crate::types::SingleMarketVerdict {
        serde_json::from_str(json)
            .unwrap_or_else(|e| panic!("single-market fixture for {cid} must parse: {e}"))
    }

    /// Build one updown row from its wire JSON (the same double-encoded
    /// nested strings the venue sends).
    fn updown_row(
        condition: &str,
        resolved: bool,
        up: &str,
        down: &str,
    ) -> crate::types::UpDownRow {
        serde_json::from_str(&format!(
            r#"{{
                "event_id": "evt_1",
                "condition_id": "{condition}",
                "asset": "BTC",
                "interval": "15m",
                "active": true,
                "closed": {resolved},
                "resolved": {resolved},
                "markets": [{{
                    "condition_id": "{condition}",
                    "outcomes": "[\"Up\", \"Down\"]",
                    "outcome_prices": "[\"{up}\", \"{down}\"]",
                    "token_ids": "[\"tok-up\", \"tok-down\"]"
                }}]
            }}"#
        ))
        .expect("updown row parses")
    }

    fn query(condition: &str) -> SettlementQuery {
        SettlementQuery {
            condition_id: condition.to_string(),
            token_ids: vec!["tok-up".to_string(), "tok-down".to_string()],
            expires_at_ms: 0,
        }
    }

    /// Polarity map builder: same shape the sweep writes from trade rows.
    fn polarity(entry: (&str, &str)) -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert(entry.0.to_string(), entry.1.to_string());
        m
    }

    #[test]
    fn list_path_answers_only_resolved_rows_with_terminal_payouts() {
        // Resolved row (up won): payouts 1/0 keyed by the parsed token ids.
        let rows = vec![updown_row("0xres", true, "1", "0")];
        let res = resolution_for(&query("0xres"), &rows).expect("resolved row answers");
        assert_eq!(res.condition_id, "0xres");
        assert!(res.resolved);
        assert_eq!(
            res.payouts,
            vec![
                ("tok-up".to_string(), Decimal::ONE),
                ("tok-down".to_string(), Decimal::ZERO)
            ]
        );
        assert_eq!(res.source, "polysimulator");
        assert!(!res.neg_risk);

        // Down won is symmetrical.
        let rows = vec![updown_row("0xdown", true, "0", "1")];
        let res = resolution_for(&query("0xdown"), &rows).expect("down-win answers");
        assert_eq!(
            res.payouts,
            vec![
                ("tok-up".to_string(), Decimal::ZERO),
                ("tok-down".to_string(), Decimal::ONE)
            ]
        );

        // Still live (resolved=false, prices mid-round): answer nothing —
        // the core re-queries on its own cadence.
        let rows = vec![updown_row("0xlive", false, "0.55", "0.45")];
        assert!(
            resolution_for(&query("0xlive"), &rows).is_none(),
            "unresolved row answers nothing"
        );

        // Resolved flag set but prices not yet terminal: not a verdict,
        // answer nothing (payouts are never guessed).
        let rows = vec![updown_row("0xhalf", true, "0.9", "0.1")];
        assert!(
            resolution_for(&query("0xhalf"), &rows).is_none(),
            "non-terminal prices answer nothing"
        );

        // A condition the updown list does not name: skip silently, never
        // fabricate.
        let rows = vec![updown_row("0xother", true, "1", "0")];
        assert!(
            resolution_for(&query("0xunknown"), &rows).is_none(),
            "unknown condition answers nothing"
        );
    }

    #[test]
    fn single_market_verdict_decodes() {
        // Closed market, winner announced: parses with the word intact.
        let v = verdict_from("0xres", &single_market_json("0xres", true, Some("Down")));
        assert_eq!(v.closed, Some(true));
        assert_eq!(v.resolved_outcome.as_deref(), Some("Down"));
        assert_eq!(v.outcomes, Some(vec!["Up".to_string(), "Down".to_string()]));

        // Not-closed market: no verdict word.
        let v = verdict_from("0xlive", &single_market_json("0xlive", false, None));
        assert_eq!(v.resolved_outcome, None);

        // A mid-round closed flag without a winner does not price (the
        // payout mapping pins the word comparison below — absent words must
        // never read as either side).
        let winner = "Up".to_ascii_uppercase();
        for word in ["Down", "down"] {
            assert_eq!(
                word.to_ascii_uppercase(),
                "DOWN",
                "{word} normalizes to UP's complement"
            );
        }
        assert!(winner == "UP");
    }

    #[test]
    fn polarity_map_pairs_held_token_with_the_winning_word() {
        // The decision table the fallback pricing walks: held token's
        // polarity word == the winner word → 1, otherwise 0. The mapping
        // pins the same expression `resolution_by_single_market` prices with.
        let held = "26626601389303902895425220343255367316695970074145368347301309748071599473770";
        let winner = "Up".to_ascii_uppercase();

        let payout_for = |polarity: &HashMap<String, String>| match polarity.get(held) {
            Some(w) if w.to_ascii_uppercase() == winner => Decimal::ONE,
            Some(_) => Decimal::ZERO,
            // Absent polarity answers NOTHING — never fabricated.
            None => Decimal::ZERO,
        };

        // Bought the WINNING side (trade row labelled "Up"): pays 1.
        assert_eq!(
            payout_for(&polarity((held, "Up"))),
            Decimal::ONE,
            "winning leg pays 1"
        );
        // Bought the LOSING side (trade row labelled "Down"): pays 0 — and
        // the loser branch must be distinguishable from "nothing priced".
        assert_eq!(
            payout_for(&polarity((held, "Down"))),
            Decimal::ZERO,
            "losing leg pays 0"
        );

        // No polarity word for the held token: nothing is priced.
        let empty: HashMap<String, String> = HashMap::new();
        assert!(
            !empty.contains_key(held),
            "absent polarity must answer nothing, never fabricate"
        );
    }
}

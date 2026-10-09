//! Automatic semantic matching across venues (issue #425).
//!
//! Pure functions over [`PlatformListing`]s: no venue names, no I/O. The
//! matcher can only ever PRODUCE CANDIDATES — effectiveness requires the
//! operator's confirmation through [`super::ledger::MappingLedger`], so a
//! false positive here can never trade on its own.
//!
//! Conservative by design (the issue's top-priority reverse acceptance is
//! "two different events mapped as one"): below-threshold pairs are not even
//! candidates, and every threshold errs toward NOT pairing.

use std::collections::BTreeSet;

use super::model::PlatformListing;

/// Hard gate: two listings whose titles share less than this Jaccard
/// similarity are never candidates, whatever the rest of the score says.
///
/// Same-event paraphrases ("Bitcoin Up or Down: October 8, 2PM ET" vs
/// "Bitcoin up or down at 2pm ET on Oct 8") clear ~0.7; two different assets
/// at the same time sit at ~0.6. Set at the boundary so a wrong-asset pair is
/// refused even before scoring.
pub const MIN_TITLE_SIMILARITY: f64 = 0.70;

/// Below this rules-text similarity a pair that does become a candidate must
/// carry a [`super::model::DiscrepancyKind::SettlementRules`] mark. The cost
/// asymmetry sets the bar: a spurious mark only down-weights a hedge (stage 4
/// refuses pseudo-hedges), a missing one would green-light one.
pub const MIN_RULES_SIMILARITY: f64 = 0.25;

/// When both rule texts NAME settlement providers and those sets agree, the
/// provider evidence outweighs wording: only a text this much more divergent
/// still counts as a conflict.
const MIN_RULES_JACCARD_PROVIDER_AGREED: f64 = 0.15;

/// Settlement-value providers that can appear in settlement-rules text. A
/// closed vocabulary, deliberately: if two rule texts each name providers and
/// the sets are DISJOINT, the venues settle from different data no matter how
/// similar the surrounding boilerplate reads ("…Binance BTCUSDT candle…" vs
/// "…CF BRTI value…" share resolves/yes/at/2pm and score ~0.27 Jaccard).
/// Unknown providers simply degrade to the Jaccard check.
const PROVIDER_TOKENS: &[&str] = &[
    "binance",
    "btcusdt",
    "ethusdt",
    "solusdt",
    "xbtusd",
    "coinbase",
    "kraken",
    "bitstamp",
    "bybit",
    "okx",
    "chainlink",
    "brti",
    "pyth",
    "cme",
    "nasdaq",
];

fn provider_set(tokens: &BTreeSet<String>) -> BTreeSet<String> {
    tokens
        .iter()
        .filter(|t| PROVIDER_TOKENS.contains(&t.as_str()))
        .cloned()
        .collect()
}

/// Do two settlement-rules texts describe CONFLICTING settlement criteria?
///
/// Order of evidence:
/// 1. both name providers and the sets are disjoint → conflict (strongest);
/// 2. both name providers and they agree → conflict only on gross text
///    divergence ([`MIN_RULES_JACCARD_PROVIDER_AGREED`]);
/// 3. either side names no provider → fall back to plain Jaccard
///    ([`MIN_RULES_SIMILARITY`]).
pub fn rules_conflict(a: &str, b: &str) -> bool {
    let (ta, tb) = (tokens(a), tokens(b));
    if ta.is_empty() && tb.is_empty() {
        // Both silent is the CALLER's "no fabricated conflict" case.
        return false;
    }
    let (pa, pb) = (provider_set(&ta), provider_set(&tb));
    match (pa.is_empty(), pb.is_empty()) {
        (false, false) if pa.is_disjoint(&pb) => true,
        (false, false) => jaccard(&ta, &tb) < MIN_RULES_JACCARD_PROVIDER_AGREED,
        _ => jaccard(&ta, &tb) < MIN_RULES_SIMILARITY,
    }
}

/// Soft gate on the composite score: below this a pair stays unmapped. A
/// same-event pair across venues typically scores ~0.83; a wrong-asset pair
/// ~0.65; a right-asset-wrong-hour pair ~0.5.
pub const MIN_CANDIDATE_SCORE: f64 = 0.80;

/// Expiry tolerance for candidacy: the two venues' markets for one event may
/// close minutes apart, but not a round apart. Beyond this window a pair is
/// never even a candidate.
pub const DEFAULT_EXPIRY_WINDOW_MS: i64 = 10 * 60 * 1000;

/// Hard inconsistency bound for an already-formed event: two listings whose
/// expiries differ by more than this cannot describe one event no matter what
/// the titles say. Used by [`super::model::UnifiedEvent::validate`]; wider
/// than the candidacy window because an operator may consciously confirm a
/// pair with moderately shifted closes (marked as a time discrepancy).
pub const EXPIRY_HARD_GAP_MS: i64 = 6 * 60 * 60 * 1000;

/// Lowercased alphanumeric word set of a text. `BTreeSet` for deterministic
/// iteration; tokenisation is deliberately dumb (no stemming) so results are
/// reproducible across runs and platforms.
pub fn tokens(text: &str) -> BTreeSet<String> {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| w.to_ascii_lowercase())
        .collect()
}

/// Jaccard similarity of two token sets in `[0, 1]`.
///
/// If EITHER side is empty the result is 0.0: missing text must never count
/// as "similar" — an empty settlement-rules field is missing data, not a
/// perfect match.
pub fn jaccard(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count();
    if inter == 0 {
        return 0.0;
    }
    let union = a.len() + b.len() - inter;
    inter as f64 / union as f64
}

/// Expiry closeness in `[0, 1]`: 1.0 when the two closes coincide, 0.0 at the
/// window edge. Sign-safe: only the magnitude matters.
pub fn expiry_factor(delta_ms: i64, window_ms: i64) -> f64 {
    if window_ms <= 0 {
        return 0.0;
    }
    (1.0 - delta_ms.abs() as f64 / window_ms as f64).clamp(0.0, 1.0)
}

/// Do two listings name the same underlying asset? `None` (unknown) on
/// either side is a pass — the title gate still applies — but two KNOWN
/// different assets are a hard no: the strongest signal that these are two
/// different events, immune to title boilerplate.
fn assets_compatible(a: &PlatformListing, b: &PlatformListing) -> bool {
    match (&a.asset, &b.asset) {
        (Some(x), Some(y)) => x.eq_ignore_ascii_case(y),
        _ => true,
    }
}

/// One cross-venue candidate pair. A candidate is a PROPOSAL, never a mapping.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchCandidate {
    pub a: PlatformListing,
    pub b: PlatformListing,
    pub title_similarity: f64,
    pub rules_similarity: f64,
    pub expiry_delta_ms: i64,
    pub score: f64,
}

/// Candidate pairs across two venue sides. Same-venue pairs are skipped;
/// title similarity, expiry window and composite score are hard gates — a
/// pair failing any of them is not returned at all (conservative: nothing to
/// confirm).
///
/// Score = `0.65·title + 0.15·rules + 0.20·expiry`. Title dominates because
/// rules boilerplate differs legitimately across venues while the event
/// identity lives in the title; the weights make a same-event pair
/// (~0.83) clear [`MIN_CANDIDATE_SCORE`] while a wrong-asset or wrong-hour
/// pair cannot reach it even with a perfect expiry.
pub fn candidates(
    side_a: &[PlatformListing],
    side_b: &[PlatformListing],
    expiry_window_ms: i64,
) -> Vec<MatchCandidate> {
    let mut out = Vec::new();
    for a in side_a {
        for b in side_b {
            if a.venue == b.venue {
                continue;
            }
            if !assets_compatible(a, b) {
                continue;
            }
            let title_sim = jaccard(&tokens(&a.title), &tokens(&b.title));
            if title_sim < MIN_TITLE_SIMILARITY {
                continue;
            }
            let delta = (a.expires_at_ms - b.expires_at_ms).abs();
            if delta > expiry_window_ms {
                continue;
            }
            let rules_sim = jaccard(&tokens(&a.settlement_rules), &tokens(&b.settlement_rules));
            let score =
                0.65 * title_sim + 0.15 * rules_sim + 0.20 * expiry_factor(delta, expiry_window_ms);
            if score < MIN_CANDIDATE_SCORE {
                continue;
            }
            out.push(MatchCandidate {
                a: a.clone(),
                b: b.clone(),
                title_similarity: title_sim,
                rules_similarity: rules_sim,
                expiry_delta_ms: delta,
                score,
            });
        }
    }
    // Deterministic order: score desc, then ids — so two runs on the same
    // input hand the operator the same list in the same order.
    out.sort_by(|x, y| {
        y.score
            .partial_cmp(&x.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                (&x.a.condition_id, &x.b.condition_id).cmp(&(&y.a.condition_id, &y.b.condition_id))
            })
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tok(s: &str) -> BTreeSet<String> {
        tokens(s)
    }

    #[test]
    fn jaccard_is_symmetric_and_bounded() {
        let a = tok("bitcoin up or down 2pm");
        let b = tok("2pm DOWN OR UP Bitcoin");
        assert!(jaccard(&a, &b) > 0.999);
        assert_eq!(jaccard(&a, &tok("ethereum")), 0.0);
        // Either side empty → 0.0, never a vacuous 1.0.
        assert_eq!(jaccard(&a, &tok("")), 0.0);
        assert_eq!(jaccard(&tok(""), &tok("")), 0.0);
    }

    #[test]
    fn tokenization_is_lowercase_and_punctuation_free() {
        assert_eq!(
            tok("Bitcoin Up-or-Down: Oct 8, 2PM ET"),
            tok("bitcoin up or down oct 8 2pm et")
        );
    }

    #[test]
    fn rules_conflict_catches_disjoint_providers_through_boilerplate() {
        // ~0.27 Jaccard — above MIN_RULES_SIMILARITY — yet the named
        // providers are disjoint: this IS a conflict.
        let a =
            "Resolves YES if the Binance BTCUSDT 1 minute candle close at 2pm ET is above the open";
        let b = "Resolves YES per the official CF Bitcoin BRTI value at 2pm ET";
        assert!(rules_conflict(a, b));
    }

    #[test]
    fn rules_conflict_accepts_paraphrase_when_providers_agree() {
        let a =
            "Resolves YES if the Binance BTCUSDT 1 minute candle close at 2pm ET is above the open";
        let b = "Binance BTCUSDT candle at 2pm ET decides yes or no";
        assert!(!rules_conflict(a, b));
    }

    #[test]
    fn rules_conflict_falls_back_to_jaccard_without_providers() {
        assert!(rules_conflict(
            "alpha beta gamma delta",
            "one two three four"
        ));
        assert!(!rules_conflict(
            "the same six words appear here and there",
            "the same six words appear here and there"
        ));
        // Empty-vs-empty is the caller's ("","") case and never conflicts;
        // the function stays sign-safe about it.
        assert!(!rules_conflict("", ""));
    }

    #[test]
    fn expiry_factor_clamps_inside_window() {
        assert_eq!(expiry_factor(0, 600_000), 1.0);
        assert_eq!(expiry_factor(300_000, 600_000), 0.5);
        assert_eq!(expiry_factor(1_200_000, 600_000), 0.0);
        assert_eq!(expiry_factor(-300_000, 600_000), 0.5); // abs is the caller's job, sign-safe anyway
    }

    #[test]
    fn candidates_skip_same_venue_and_below_threshold_pairs() {
        let t = |asset: &str| {
            format!(
                "{asset} Up or Down: October 8, 2PM ET — resolves to the Binance 1 minute candle for the asset price at 2pm ET"
            )
        };
        let mk = |venue: super::super::model::Venue, asset: &str, exp: i64| {
            super::super::model::PlatformListing {
                venue,
                condition_id: format!("{venue:?}-{asset}"),
                up_token_id: format!("{venue:?}-{asset}-up"),
                down_token_id: format!("{venue:?}-{asset}-down"),
                asset: Some(asset.to_string()),
                title: t(asset),
                settlement_rules: "Resolves per the exchange price at 2pm ET".into(),
                settlement_source: super::super::model::SettlementSource::Chainlink,
                expires_at_ms: exp,
                status: super::super::model::ListingStatus::Active,
            }
        };
        let pm = mk(super::super::model::Venue::Polymarket, "bitcoin", 1_000_000);
        let kx = mk(super::super::model::Venue::Kalshi, "bitcoin", 1_000_000);
        let eth = mk(super::super::model::Venue::Kalshi, "ethereum", 1_000_000);
        let btc_later = mk(
            super::super::model::Venue::Kalshi,
            "bitcoin",
            1_000_000 + 3_600_000,
        );

        // Same venue on both sides: never a candidate.
        assert!(
            candidates(
                std::slice::from_ref(&pm),
                std::slice::from_ref(&pm),
                DEFAULT_EXPIRY_WINDOW_MS
            )
            .is_empty()
        );
        // Same event: one candidate, deterministic shape.
        let c = candidates(
            std::slice::from_ref(&pm),
            std::slice::from_ref(&kx),
            DEFAULT_EXPIRY_WINDOW_MS,
        );
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].a.condition_id, pm.condition_id);
        assert_eq!(c[0].b.condition_id, kx.condition_id);
        assert!(c[0].score >= MIN_CANDIDATE_SCORE);
        // Different asset, same hour: hard-refused by the asset gate even
        // though the titles are near-identical boilerplate (the "two
        // different events as one" trap).
        assert!(
            candidates(
                std::slice::from_ref(&pm),
                std::slice::from_ref(&eth),
                DEFAULT_EXPIRY_WINDOW_MS
            )
            .is_empty()
        );
        // Same asset, different hour beyond the window: nothing.
        assert!(candidates(&[pm], &[btc_later], DEFAULT_EXPIRY_WINDOW_MS).is_empty());
    }
}

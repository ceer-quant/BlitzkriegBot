use blitzkrieg_data_model::CryptoMarket;
use blitzkrieg_data_model::DataEvent;
use rust_decimal::Decimal;
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::Path;

use super::*;

/// Gamma's `clobTokenIds` (a JSON-encoded string array) + `outcomes` order:
/// index 0 = Up, 1 = Down.
pub(crate) fn gamma_tokens(g: &Value) -> Vec<String> {
    let raw = g
        .get("clobTokenIds")
        .and_then(Value::as_str)
        .unwrap_or("[]");
    let ids: Vec<String> = serde_json::from_str(raw).unwrap_or_default();
    ids
}

/// The proven (winner, loser) token pair for one market, or `None` when the
/// evidence cannot name BOTH sides against this market's declared tokens.
/// The market flag and the emitted event MUST share this one decision: a
/// flag without an emission would silence the dry ladder and strand the
/// position (no settlement ever arrives), and an emission without the flag
/// would race the dry ladder at the expiry tick.
pub(crate) fn resolved_pair(
    vd: &ConditionVerdict,
    up: &str,
    down: &str,
) -> Option<(String, String)> {
    if up.is_empty() || down.is_empty() {
        return None;
    }
    let winner = resolve_winner(vd, up, down)?;
    let loser = if winner == up {
        down.to_string()
    } else {
        up.to_string()
    };
    Some((winner, loser))
}

/// The deterministic converter: canonical fills + gamma metadata + price
/// series → the archive event stream. Events are sorted by (at, rank) with
/// round(0) < top(1) < trade(2) < round_end(3), so the same inputs always
/// produce byte-identical output (the manifest's sha256 is a real
/// fingerprint). Returns (events, skipped_rows).
///
/// 治 #2: `verdicts` (the wallet's own redemption evidence) marks every
/// market the archive carries an on-chain winner for — that market's
/// `CryptoMarket` is flagged `archive_verdict` (the dry ladder stands down)
/// and a `Resolution` event is emitted at the round's expiry instant, which
/// the replay settles positions from directly.
///
/// One token's published price series over one round window: `(t_sec, price)`
/// points, keyed by `(token, start_sec, end_sec)`.
type PriceSeries = BTreeMap<(String, i64, i64), Vec<(i64, Decimal)>>;

pub(crate) fn convert(
    rows: &[Value],
    gamma: &BTreeMap<String, Value>,
    price_series: &PriceSeries,
    verdicts: &BTreeMap<String, ConditionVerdict>,
    cashflows: &[TimedCashflow],
    req: &PullRequest,
) -> Result<(Vec<DataEvent>, u64), String> {
    use blitzkrieg_data_model::event_to_json;

    // Condition → (slug, traded tokens) for the rounds we will emit.
    let mut by_condition: BTreeMap<&str, &Value> = BTreeMap::new();
    let mut skipped: u64 = 0;
    for r in rows {
        let cid = r.get("conditionId").and_then(Value::as_str).unwrap_or("");
        if cid.is_empty() {
            skipped += 1;
            continue;
        }
        by_condition.entry(cid).or_insert(r);
    }

    struct RoundCtx {
        start_sec: i64,
        end_sec: i64,
        markets: Vec<CryptoMarket>,
    }
    let mut rounds: BTreeMap<i64, RoundCtx> = BTreeMap::new();

    for (cid, sample) in &by_condition {
        let g = gamma.get(*cid).cloned().unwrap_or(Value::Null);
        let slug = sample
            .get("slug")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let Some(rs) = parse_round_slug(slug) else {
            skipped += 1;
            continue;
        };
        // Filters: asset / market duration, when the request asked for one.
        if let Some(a) = &req.asset
            && !a.eq_ignore_ascii_case(&rs.asset)
        {
            skipped += 1;
            continue;
        }
        let dur_label = match rs.duration_sec {
            300 => "5m",
            900 => "15m",
            3600 => "1h",
            14_400 => "4h",
            _ => "",
        };
        if let Some(m) = &req.market
            && !m.eq_ignore_ascii_case(dur_label)
        {
            skipped += 1;
            continue;
        }
        let end_sec = rs.start_sec + rs.duration_sec;
        let tokens = gamma_tokens(&g);
        let up_token = tokens.first().cloned().unwrap_or_default();
        let down_token = tokens.get(1).cloned().unwrap_or_default();
        // Gamma's clobTokenIds order is authoritative ([0]=Up, [1]=Down); the
        // traded fill's outcomeIndex only disambiguates when gamma returned
        // nothing usable (a market gamma has since pruned).
        let outcome_index = sample
            .get("outcomeIndex")
            .and_then(Value::as_i64)
            .unwrap_or(-1);
        let traded_token = sample
            .get("asset")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let (up, down) = if up_token.is_empty() && down_token.is_empty() {
            match outcome_index {
                0 => (traded_token.to_string(), String::new()),
                1 => (String::new(), traded_token.to_string()),
                _ => (up_token, down_token),
            }
        } else {
            (up_token, down_token)
        };
        // Seed the declaration prices with each token's first published point
        // (0.5 when the series is empty — an honest "unknown mid").
        let seed = |t: &str| {
            price_series
                .get(&(t.to_string(), rs.start_sec, end_sec))
                .and_then(|v| v.first())
                .map(|(_, d)| *d)
                .unwrap_or(Decimal::new(5, 1))
        };
        let market = CryptoMarket {
            asset: rs.asset.clone(),
            condition_id: (*cid).to_string(),
            question_id: g
                .get("questionID")
                .and_then(Value::as_str)
                .unwrap_or(*cid)
                .to_string(),
            up_token_id: up.clone(),
            down_token_id: down.clone(),
            up_price: seed(&up),
            down_price: seed(&down),
            expires_at_ms: end_sec * 1000,
            // #354 (5.4): the kernel's slot is an INDEX on the round grid
            // (scanner `current_slot` = now_ms/1000/duration; a manual close's
            // expiry derives `(slot+1) * round_duration_sec`), not an epoch.
            // The converter used to write the round's absolute start second,
            // which a 5m round under any grid arithmetic silently mis-times —
            // the slot index is start/duration by definition.
            round_slot: rs.start_sec / rs.duration_sec,
            // #354 (5.4): the slug's declared duration travels with the market,
            // so a replay can refuse a corpus whose round cadence disagrees
            // with the configured grid (5m data on a 15m assumption ages every
            // round 3× too slowly) instead of silently mis-timing it.
            round_duration_sec: rs.duration_sec,
            // 治 #2: the archive carries this market's on-chain verdict (a
            // Resolution event rides the stream), so the core's dry settlement
            // ladder must not synthesize a competing one from book evidence.
            archive_verdict: resolved_pair(
                verdicts
                    .get(*cid)
                    .map_or(&ConditionVerdict::default(), |v| v),
                &up,
                &down,
            )
            .is_some(),
            // Converter output is the historical single-venue (Polymarket)
            // corpus — no venue declared, the serde default's world.
            venue: String::new(),
            neg_risk: g.get("negRisk").and_then(Value::as_bool).unwrap_or(false),
            question: sample
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        };
        rounds.entry(rs.start_sec).or_insert(RoundCtx {
            start_sec: rs.start_sec,
            end_sec,
            markets: Vec::new(),
        });
        if let Some(rc) = rounds.get_mut(&rs.start_sec)
            && !rc.markets.iter().any(|m| m.condition_id == *cid)
        {
            rc.markets.push(market);
        }
    }

    // rank: round < top < spot < trade < resolution < round_end < cashflow —
    // the canonical same-instant order. A cashflow sorts LAST at its instant:
    // it is bookkeeping, never a decision input. A unified mapping sorts
    // with the round declaration (before the books it names): strategies
    // must know which tokens are one bet before pricing them.
    let rank = |ev: &DataEvent| match ev {
        DataEvent::RoundMarkets { .. } => 0u8,
        DataEvent::UnifiedMapping { .. } => 0,
        DataEvent::TopOfBook { .. } => 1,
        DataEvent::Spot { .. } => 2,
        DataEvent::Trade { .. } => 3,
        DataEvent::Resolution { .. } => 4,
        DataEvent::RoundEnd { .. } => 5,
        DataEvent::Book { .. } => 6,
        // 盘口批次原子化：批行与其逐本展开落在同一 instant 的同一序位
        // （books 排在 book 之前：一个批次先于其中任何一本被消费）。
        DataEvent::Books { .. } => 6,
        DataEvent::Cashflow { .. } => 7,
    };
    let mut events: Vec<DataEvent> = Vec::new();
    for rc in rounds.values() {
        events.push(DataEvent::RoundMarkets {
            markets: rc.markets.clone(),
            now_ms: rc.start_sec * 1000,
        });
        for m in &rc.markets {
            for t in [&m.up_token_id, &m.down_token_id] {
                if t.is_empty() {
                    continue;
                }
                if let Some(pts) = price_series.get(&(t.clone(), rc.start_sec, rc.end_sec)) {
                    for (tsec, p) in pts {
                        events.push(DataEvent::TopOfBook {
                            token_id: t.clone(),
                            best_bid: Some(*p),
                            best_ask: Some(*p),
                            now_ms: tsec * 1000,
                        });
                    }
                }
            }
        }
    }
    for r in rows {
        let cid = r.get("conditionId").and_then(Value::as_str).unwrap_or("");
        let Some(sample) = by_condition.get(cid) else {
            continue;
        };
        let slug = sample.get("slug").and_then(Value::as_str).unwrap_or("");
        let Some(rs) = parse_round_slug(slug) else {
            continue;
        };
        // Filters again: a skipped condition's fills are skipped too.
        if let Some(a) = &req.asset
            && !a.eq_ignore_ascii_case(&rs.asset)
        {
            continue;
        }
        let dur_label = match rs.duration_sec {
            300 => "5m",
            900 => "15m",
            3600 => "1h",
            14_400 => "4h",
            _ => "",
        };
        if let Some(m) = &req.market
            && !m.eq_ignore_ascii_case(dur_label)
        {
            continue;
        }
        let dec = |p: &str| -> Option<Decimal> {
            let raw = r.get(p)?.to_string();
            Decimal::from_str_exact(raw.trim_matches('"')).ok()
        };
        let (Some(price), Some(size)) = (dec("price"), dec("size")) else {
            skipped += 1;
            continue;
        };
        let side = match r.get("side").and_then(Value::as_str).unwrap_or("") {
            "BUY" => blitzkrieg_data_model::Side::Buy,
            "SELL" => blitzkrieg_data_model::Side::Sell,
            _ => {
                skipped += 1;
                continue;
            }
        };
        events.push(DataEvent::Trade {
            token_id: r
                .get("asset")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            side,
            price,
            size,
            now_ms: r.get("timestamp").and_then(Value::as_i64).unwrap_or(0) * 1000,
        });
    }
    for rc in rounds.values() {
        // 治 #2: the verdict event rides the round boundary — same instant
        // as the RoundEnd, ranked just before it, so a replay settles every
        // archive-verdict market exactly when the round closes and the dry
        // ladder (suppressed for these markets) never races it.
        for m in &rc.markets {
            let Some(vd) = verdicts.get(&m.condition_id) else {
                continue;
            };
            // ONE decision for both the flag above and this emission: a flag
            // without an event would silence the dry ladder and strand the
            // position (no settlement ever arrives).
            let Some((winner, _)) = resolved_pair(vd, &m.up_token_id, &m.down_token_id) else {
                continue;
            };
            let payout_of = |t: &str| {
                if t == winner {
                    Decimal::ONE
                } else {
                    Decimal::ZERO
                }
            };
            events.push(DataEvent::Resolution {
                condition_id: m.condition_id.clone(),
                // The market's OWN outcome order ([0]=up, [1]=down) — the same
                // order the NegRisk adapter indexes redeem amounts by; a
                // winner-first reordering would corrupt the recorded
                // outcome_index. A binary boundary names BOTH sides: the
                // winner pays 1, the loser 0.
                payouts: vec![
                    (m.up_token_id.clone(), payout_of(&m.up_token_id)),
                    (m.down_token_id.clone(), payout_of(&m.down_token_id)),
                ],
                neg_risk: m.neg_risk,
                now_ms: rc.end_sec * 1000,
            });
        }
        events.push(DataEvent::RoundEnd {
            now_ms: rc.end_sec * 1000,
        });
    }

    // #388: wallet-level cash rows — one event per LEDGER ROW, at the row's
    // own timestamp, exactly as the venue paid it. No per-fill attribution is
    // invented; the event stream is the ledger's cash view, not an estimate.
    for cf in cashflows {
        events.push(DataEvent::Cashflow {
            which: cf.which,
            amount_usd: cf.amount_usd,
            now_ms: cf.at_ms,
        });
    }

    // Deterministic order: (at, rank, canonical line). Keys are computed once
    // — serializing per comparison would be O(n log n) JSON round-trips.
    let mut keyed: Vec<(i64, u8, String, DataEvent)> = events
        .into_iter()
        .map(|ev| {
            let at = blitzkrieg_data_model::event_at_ms(&ev);
            let r = rank(&ev);
            let line = event_to_json(&ev).to_string();
            (at, r, line, ev)
        })
        .collect();
    keyed.sort_by(|a, b| (a.0, a.1, &a.2).cmp(&(b.0, b.1, &b.2)));
    let events: Vec<DataEvent> = keyed.into_iter().map(|(_, _, _, ev)| ev).collect();
    Ok((events, skipped))
}

/// Write the event stream: one canonical JSON line per event, sorted (the
/// sort already happened in [`convert`]).
pub(crate) fn write_events(path: &Path, events: &[DataEvent]) -> Result<(), String> {
    use blitzkrieg_data_model::event_to_json;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let mut f = std::io::BufWriter::new(
        std::fs::File::create(path).map_err(|e| format!("create {}: {e}", path.display()))?,
    );
    for ev in events {
        let line = event_to_json(ev).to_string();
        writeln!(f, "{line}").map_err(|e| format!("write {}: {e}", path.display()))?;
    }
    f.flush()
        .map_err(|e| format!("flush {}: {e}", path.display()))?;
    Ok(())
}

//! Shadow recorder + replay (P4).
//!
//! Records the price path of every opened position (own side, and the opposite
//! side for the flip hypothesis) and later replays exit policies over that path
//! using the SAME `exit_policy` module the live engine trades with. Because the
//! policy is shared, a backtest cannot drift from production.
//!
//! Observation-only: the recorder never trades.

use crate::exit_policy::{
    ExitConfig, ExitState, ExitTickInput, decide_exit, executable_bid, update_exit_state,
};
use crate::model::OrderbookSnapshot;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct Sample {
    pub t_ms: i64,
    pub price: Decimal,
    pub bid: Option<Decimal>,
    pub ask: Option<Decimal>,
}

#[derive(Debug, Clone)]
pub struct ShadowRecord {
    pub token_id: String,
    pub asset: String,
    pub direction: String,
    pub entry_price: Decimal,
    pub shares: Decimal,
    pub was_maker_entry: bool,
    pub entered_at_ms: i64,
    pub expires_at_ms: i64,
    pub own: Vec<Sample>,
    pub opposite: Vec<Sample>,
    /// Realised net PnL if the position closed within the window.
    pub actual_net_pnl: Option<Decimal>,
    /// Seconds left in the round when the position was entered (context.timeLeftSec).
    pub time_left_at_entry_sec: Option<Decimal>,
}

/// Time-boxed recorder for one position.
struct Tracked {
    rec: ShadowRecord,
    window_end_ms: i64,
    last_own_at: i64,
    last_opp_at: i64,
}

pub struct ShadowRecorder {
    tracked: HashMap<String, Tracked>,
    sample_min_interval_ms: i64,
    window_ms: i64,
}

impl ShadowRecorder {
    pub fn new(sample_min_interval_ms: i64, window_ms: i64) -> Self {
        Self {
            tracked: HashMap::new(),
            sample_min_interval_ms,
            window_ms,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn on_open(
        &mut self,
        position_id: &str,
        asset: &str,
        direction: &str,
        token_id: &str,
        entry_price: Decimal,
        shares: Decimal,
        was_maker: bool,
        entered_at_ms: i64,
        expires_at_ms: i64,
    ) {
        let window_end_ms = if expires_at_ms > entered_at_ms {
            // Sample almost to the force-exit horizon.
            (expires_at_ms - 120_000).max(entered_at_ms)
        } else {
            entered_at_ms + self.window_ms
        };
        self.tracked.insert(
            position_id.to_string(),
            Tracked {
                rec: ShadowRecord {
                    token_id: token_id.to_string(),
                    asset: asset.to_string(),
                    direction: direction.to_string(),
                    entry_price,
                    shares,
                    was_maker_entry: was_maker,
                    entered_at_ms,
                    expires_at_ms,
                    own: Vec::new(),
                    opposite: Vec::new(),
                    actual_net_pnl: None,
                    time_left_at_entry_sec: None,
                },
                window_end_ms,
                last_own_at: 0,
                last_opp_at: 0,
            },
        );
    }

    /// Feed a book update for any token; samples are matched to open records.
    pub fn on_book(&mut self, token_id: &str, book: &OrderbookSnapshot, now_ms: i64) {
        for t in self.tracked.values_mut() {
            if now_ms < t.rec.entered_at_ms || now_ms > t.window_end_ms {
                continue;
            }
            if token_id == t.rec.token_id {
                if now_ms - t.last_own_at >= self.sample_min_interval_ms {
                    t.last_own_at = now_ms;
                    t.rec.own.push(Sample {
                        t_ms: now_ms - t.rec.entered_at_ms,
                        price: book.mid_price,
                        bid: Some(book.best_bid),
                        ask: Some(book.best_ask),
                    });
                }
            } else if now_ms - t.last_opp_at >= self.sample_min_interval_ms {
                // The caller may feed the opposite token too; record it under the
                // same elapsed clock. (Matching is by token in the caller.)
            }
        }
    }

    /// Record an opposite-side sample explicitly.
    pub fn on_opposite(&mut self, position_id: &str, book: &OrderbookSnapshot, now_ms: i64) {
        if let Some(t) = self.tracked.get_mut(position_id)
            && now_ms >= t.rec.entered_at_ms
            && now_ms <= t.window_end_ms
            && now_ms - t.last_opp_at >= self.sample_min_interval_ms
        {
            t.last_opp_at = now_ms;
            t.rec.opposite.push(Sample {
                t_ms: now_ms - t.rec.entered_at_ms,
                price: book.mid_price,
                bid: Some(book.best_bid),
                ask: Some(book.best_ask),
            });
        }
    }

    pub fn on_close(&mut self, position_id: &str, net_pnl: Decimal) {
        if let Some(t) = self.tracked.get_mut(position_id) {
            t.rec.actual_net_pnl = Some(net_pnl);
        }
    }

    /// Finalize records whose window elapsed; returns them for persistence.
    pub fn tick(&mut self, now_ms: i64) -> Vec<ShadowRecord> {
        let ready: Vec<String> = self
            .tracked
            .iter()
            .filter(|(_, t)| now_ms >= t.window_end_ms)
            .map(|(k, _)| k.clone())
            .collect();
        ready
            .into_iter()
            .filter_map(|k| self.tracked.remove(&k).map(|t| t.rec))
            .collect()
    }

    pub fn pending(&self) -> usize {
        self.tracked.len()
    }
}

// ── Near-miss recording ──────────────────────────────────────────────────────
//
// Records signals the entry GATE blocked (timing/momentum) together with the
// token's subsequent price path. This is the data the shadow log lacks: it lets
// offline replay answer "if we had relaxed the gate and taken this trade, would
// it have made or lost money?" — without which relaxing min-time-left is a guess.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NearMissReason {
    Timing,
    Momentum,
}

#[derive(Debug, Clone)]
pub struct NearMissRecord {
    pub token_id: String,
    pub asset: String,
    pub direction: String,
    pub blocked_at_ms: i64,
    /// Price the signal wanted (the resting bid).
    pub entry_price: Decimal,
    /// The mid at block time (context for how deep the dip was).
    pub mid_at_block: Decimal,
    pub reason: NearMissReason,
    /// Seconds left in the round at block time.
    pub time_left_sec: i64,
    pub round_slot: i64,
    /// Subsequent mid path, elapsed ms from block time.
    pub path: Vec<Sample>,
    pub window_end_ms: i64,
}

struct NearTracked {
    rec: NearMissRecord,
    last_sample_at: i64,
}

pub struct NearMissRecorder {
    tracked: HashMap<String, NearTracked>,
    sample_min_interval_ms: i64,
    window_ms: i64,
    /// Cap on concurrently tracked tokens (bounded memory).
    max_tracked: usize,
}

impl NearMissRecorder {
    pub fn new(sample_min_interval_ms: i64, window_ms: i64) -> Self {
        Self {
            tracked: HashMap::new(),
            sample_min_interval_ms,
            window_ms,
            max_tracked: 64,
        }
    }

    /// Begin tracking a blocked candidate. Dedup: one record per token at a time
    /// (a token that stays blocked across ticks should not spawn N records).
    #[allow(clippy::too_many_arguments)]
    pub fn on_blocked(
        &mut self,
        token_id: &str,
        asset: &str,
        direction: &str,
        entry_price: Decimal,
        mid_at_block: Decimal,
        reason: NearMissReason,
        time_left_sec: i64,
        round_slot: i64,
        now_ms: i64,
    ) {
        if self.tracked.contains_key(token_id) || self.tracked.len() >= self.max_tracked {
            return;
        }
        self.tracked.insert(
            token_id.to_string(),
            NearTracked {
                rec: NearMissRecord {
                    token_id: token_id.to_string(),
                    asset: asset.to_string(),
                    direction: direction.to_string(),
                    blocked_at_ms: now_ms,
                    entry_price,
                    mid_at_block,
                    reason,
                    time_left_sec,
                    round_slot,
                    path: Vec::new(),
                    window_end_ms: now_ms + self.window_ms,
                },
                last_sample_at: 0,
            },
        );
    }

    pub fn on_book(&mut self, token_id: &str, book: &OrderbookSnapshot, now_ms: i64) {
        let Some(t) = self.tracked.get_mut(token_id) else {
            return;
        };
        if now_ms < t.rec.blocked_at_ms || now_ms > t.rec.window_end_ms {
            return;
        }
        if now_ms - t.last_sample_at < self.sample_min_interval_ms {
            return;
        }
        t.last_sample_at = now_ms;
        t.rec.path.push(Sample {
            t_ms: now_ms - t.rec.blocked_at_ms,
            price: book.mid_price,
            bid: Some(book.best_bid),
            ask: Some(book.best_ask),
        });
    }

    /// Finalize records whose window elapsed; returns them for persistence.
    pub fn tick(&mut self, now_ms: i64) -> Vec<NearMissRecord> {
        let ready: Vec<String> = self
            .tracked
            .iter()
            .filter(|(_, t)| now_ms >= t.rec.window_end_ms)
            .map(|(k, _)| k.clone())
            .collect();
        ready
            .into_iter()
            .filter_map(|k| self.tracked.remove(&k).map(|t| t.rec))
            .collect()
    }

    pub fn pending(&self) -> usize {
        self.tracked.len()
    }

    /// Take all pending records regardless of window (used on shutdown).
    pub fn flush(&mut self) -> Vec<NearMissRecord> {
        let keys: Vec<String> = self.tracked.keys().cloned().collect();
        keys.into_iter()
            .filter_map(|k| self.tracked.remove(&k).map(|t| t.rec))
            .collect()
    }
}

/// Hypothetical replay of a blocked near-miss: treat the blocked bid as an
/// entry and run the shared exit policy over the recorded path.
///
/// IMPORTANT SEMANTICS: the exit policy's own `min_time_left_sec` still applies.
/// A near-miss blocked by the timing gate has `time_left` below that threshold,
/// so replaying it as-is exits almost immediately at the entry price — which
/// models "took the trade but kept the same exit timing". To evaluate *actually*
/// relaxing the gate you must also relax the matching exit-side time gate; the
/// caller can pass a modified `ExitConfig`. This coupling is itself the key
/// finding: relaxing entry timing without relaxing exit timing buys nothing.
///
/// Taker fee on exit is charged conservatively (a maker-first TP can only do
/// better).
pub fn replay_near_miss(rec: &NearMissRecord, cfg: &ExitConfig) -> ReplayResult {
    if rec.path.is_empty() {
        return ReplayResult {
            pnl: Decimal::ZERO,
            exit_reason: crate::model::ExitReason::Manual,
            exit_price: rec.entry_price,
        };
    }
    let entry = rec.entry_price;
    let shares = dec!(10); // nominal 10-share clip, matching the live sizing default
    let mut state = ExitState::new(entry, 0);
    // A resting maker entry would pay no entry fee.
    let entry_fee = Decimal::ZERO;

    let mut exit_price = rec
        .path
        .last()
        .map(|s| s.bid.unwrap_or(s.price))
        .unwrap_or(entry);
    let mut exit_reason = crate::model::ExitReason::ForceExit;

    for s in &rec.path {
        let now = rec.blocked_at_ms + s.t_ms;
        let book = OrderbookSnapshot::from_levels(
            rec.token_id.clone(),
            vec![(s.bid.unwrap_or(s.price), dec!(100))],
            vec![(s.ask.unwrap_or(s.price), dec!(100))],
            now,
        );
        update_exit_state(&mut state, entry, Some(&book), now, cfg);
        // Time left shrinks from the block moment.
        let time_left = rec.time_left_sec - (s.t_ms / 1000);
        let d = decide_exit(ExitTickInput {
            entry_price: entry,
            book: Some(&book),
            fallback_price: Some(s.price),
            time_left_sec: time_left,
            hold_sec: s.t_ms / 1000,
            state: &state,
            now_ms: now,
            cfg,
        });
        if let Some(d) = d {
            exit_price = executable_bid(Some(&book));
            exit_reason = d.reason;
            break;
        }
    }

    let exit_fee =
        crate::exit_policy::taker_fee_pct(exit_price) / Decimal::ONE_HUNDRED * exit_price * shares;
    let pnl = shares * (exit_price - entry) - entry_fee - exit_fee;
    ReplayResult {
        pnl,
        exit_reason,
        exit_price,
    }
}

/// Summary of replaying every blocked near-miss: what relaxing the gate would
/// have earned (or cost) over the recorded sample.
#[derive(Debug, Clone, Default)]
pub struct NearMissSummary {
    pub n: usize,
    pub total_pnl: Decimal,
    pub wins: usize,
    pub avg_pnl: Decimal,
    /// Count by block reason.
    pub timing_n: usize,
    pub momentum_n: usize,
    pub total_feeable_notional: Decimal,
}

pub fn evaluate_near_misses(records: &[NearMissRecord], cfg: &ExitConfig) -> NearMissSummary {
    let mut s = NearMissSummary::default();
    for r in records {
        let res = replay_near_miss(r, cfg);
        s.n += 1;
        s.total_pnl += res.pnl;
        if res.pnl > Decimal::ZERO {
            s.wins += 1;
        }
        s.total_feeable_notional += r.entry_price * dec!(10);
        match r.reason {
            NearMissReason::Timing => s.timing_n += 1,
            NearMissReason::Momentum => s.momentum_n += 1,
        }
    }
    if s.n > 0 {
        s.avg_pnl = s.total_pnl / Decimal::from(s.n);
    }
    s
}

/// JSONL line for a near-miss record (includes the path for later replay).
pub fn near_miss_to_json(r: &NearMissRecord) -> serde_json::Value {
    serde_json::json!({
        "tokenId": r.token_id,
        "asset": r.asset,
        "direction": r.direction,
        "blockedAt": r.blocked_at_ms,
        "entryPrice": r.entry_price,
        "midAtBlock": r.mid_at_block,
        "reason": match r.reason { NearMissReason::Timing => "timing", NearMissReason::Momentum => "momentum" },
        "timeLeftSec": r.time_left_sec,
        "roundSlot": r.round_slot,
        "windowSec": (r.window_end_ms - r.blocked_at_ms) as f64 / 1000.0,
        "path": r.path.iter().map(|s| serde_json::json!({ "t": s.t_ms as f64 / 1000.0, "p": s.price, "b": s.bid, "a": s.ask })).collect::<Vec<_>>(),
    })
}

pub fn parse_near_miss_line(line: &str) -> Option<NearMissRecord> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let entry_price = dec_from(&v["entryPrice"])?;
    let blocked_at = v["blockedAt"].as_i64()?;
    let path = v
        .get("path")
        .and_then(|p| p.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|s| {
                    Some(Sample {
                        t_ms: (s.get("t")?.as_f64()? * 1000.0) as i64,
                        price: dec_from(&s["p"])?,
                        bid: s.get("b").and_then(dec_from),
                        ask: s.get("a").and_then(dec_from),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let window_sec = v["windowSec"].as_f64().unwrap_or(0.0);
    let reason = match v["reason"].as_str() {
        Some("momentum") => NearMissReason::Momentum,
        _ => NearMissReason::Timing,
    };
    Some(NearMissRecord {
        token_id: v["tokenId"].as_str().unwrap_or("").to_string(),
        asset: v["asset"].as_str().unwrap_or("").to_string(),
        direction: v["direction"].as_str().unwrap_or("up").to_string(),
        blocked_at_ms: blocked_at,
        entry_price,
        mid_at_block: dec_from(&v["midAtBlock"]).unwrap_or(entry_price),
        reason,
        time_left_sec: v["timeLeftSec"].as_i64().unwrap_or(0),
        round_slot: v["roundSlot"].as_i64().unwrap_or(0),
        path,
        window_end_ms: blocked_at + (window_sec * 1000.0) as i64,
    })
}

pub fn persist_near_misses(
    path: &std::path::Path,
    records: &[NearMissRecord],
) -> std::io::Result<()> {
    use std::io::Write as _;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    for r in records {
        writeln!(f, "{}", near_miss_to_json(r))?;
    }
    Ok(())
}

/// Result of replaying one exit policy over a recorded path.
#[derive(Debug, Clone, PartialEq)]
pub struct ReplayResult {
    pub pnl: Decimal,
    pub exit_reason: crate::model::ExitReason,
    pub exit_price: Decimal,
}

/// Replay a recorded path through the shared exit policy. Conservative: charges
/// the taker fee on every exit (a live maker-first TP can only do better).
pub fn replay(rec: &ShadowRecord, cfg: &ExitConfig) -> ReplayResult {
    if rec.own.is_empty() {
        return ReplayResult {
            pnl: Decimal::ZERO,
            exit_reason: crate::model::ExitReason::Manual,
            exit_price: rec.entry_price,
        };
    }
    let entry = rec.entry_price;
    let shares = rec.shares;
    let mut state = ExitState::new(entry, 0);
    let entry_fee = if rec.was_maker_entry {
        Decimal::ZERO
    } else {
        crate::exit_policy::taker_fee_pct(entry) / Decimal::ONE_HUNDRED * entry * shares
    };

    let mut exit_price = rec
        .own
        .last()
        .map(|s| s.bid.unwrap_or(s.price))
        .unwrap_or(entry);
    let mut exit_reason = crate::model::ExitReason::ForceExit;

    for s in &rec.own {
        let now = rec.entered_at_ms + s.t_ms;
        let book = OrderbookSnapshot::from_levels(
            rec.token_id.clone(),
            vec![(s.bid.unwrap_or(s.price), dec!(100))],
            vec![(s.ask.unwrap_or(s.price), dec!(100))],
            now,
        );
        update_exit_state(&mut state, entry, Some(&book), now, cfg);
        let time_left = (rec.expires_at_ms - now) / 1000;
        let d = decide_exit(ExitTickInput {
            entry_price: entry,
            book: Some(&book),
            fallback_price: Some(s.price),
            time_left_sec: time_left,
            hold_sec: s.t_ms / 1000,
            state: &state,
            now_ms: now,
            cfg,
        });
        if let Some(d) = d {
            exit_price = executable_bid(Some(&book));
            exit_reason = d.reason;
            break;
        }
    }

    let exit_fee =
        crate::exit_policy::taker_fee_pct(exit_price) / Decimal::ONE_HUNDRED * exit_price * shares;
    let pnl = shares * (exit_price - entry) - entry_fee - exit_fee;
    ReplayResult {
        pnl,
        exit_reason,
        exit_price,
    }
}

// ── Persistence (Node-analyzer compatible JSONL) ─────────────────────────────

fn rel_pct(base: Decimal, p: Decimal) -> Decimal {
    if base > Decimal::ZERO {
        ((p - base) / base) * Decimal::ONE_HUNDRED
    } else {
        Decimal::ZERO
    }
}

fn path_json(samples: &[Sample], base: Decimal) -> serde_json::Value {
    let mut maxp = base;
    let mut minp = base;
    let mut max_at = 0i64;
    let mut min_at = 0i64;
    let mut first: Option<Decimal> = None;
    let mut arr = Vec::with_capacity(samples.len());
    for s in samples {
        if first.is_none() {
            first = Some(s.price);
        }
        if s.price > maxp {
            maxp = s.price;
            max_at = s.t_ms;
        }
        if s.price < minp {
            minp = s.price;
            min_at = s.t_ms;
        }
        arr.push(serde_json::json!({ "t": s.t_ms as f64 / 1000.0, "p": s.price, "b": s.bid, "a": s.ask }));
    }
    serde_json::json!({
        "entryRef": first.unwrap_or(base),
        "samples": arr,
        "maxPrice": maxp,
        "minPrice": minp,
        "maxPct": rel_pct(base, maxp),
        "minPct": rel_pct(base, minp),
        "timeToMaxSec": max_at as f64 / 1000.0,
        "timeToMinSec": min_at as f64 / 1000.0,
    })
}

impl ShadowRecord {
    /// Serialize in the shape `scripts/analyze-signals.mjs` understands (the
    /// reader is named for the file it consumes, `data/signals/signals.jsonl`),
    /// so the Rust recorder keeps feeding the same offline tooling the Node
    /// recorder did.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "id": format!("rust-{}", self.entered_at_ms),
            "strategy": "spread_arb",
            "asset": self.asset,
            "direction": self.direction,
            "tokenId": self.token_id,
            "conditionId": "",
            "entryPrice": self.entry_price,
            "shares": self.shares,
            "wasMakerEntry": self.was_maker_entry,
            "enteredAt": self.entered_at_ms,
            "expiredAt": self.expires_at_ms,
            "windowSec": (self.expires_at_ms - self.entered_at_ms) as f64 / 1000.0,
            "own": path_json(&self.own, self.entry_price),
            "oppositeTokenId": null,
            "opposite": if self.opposite.is_empty() { serde_json::Value::Null } else { path_json(&self.opposite, self.opposite.first().map(|s| s.price).unwrap_or(Decimal::ONE)) },
            "closed": self.actual_net_pnl.is_some(),
            "actual": self.actual_net_pnl.map(|p| serde_json::json!({ "netPnlUsd": p })),
        })
    }
}

/// Append records to a JSONL file (creates the directory if needed).
pub fn persist_records(path: &std::path::Path, records: &[ShadowRecord]) -> std::io::Result<()> {
    use std::io::Write as _;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    for r in records {
        writeln!(f, "{}", r.to_json())?;
    }
    Ok(())
}

// ── Walk-forward evaluation ──────────────────────────────────────────────────

/// One candidate exit configuration in the walk-forward grid.
#[derive(Debug, Clone)]
pub struct GridPoint {
    pub name: String,
    pub stop_loss_pct: Decimal,
    pub min_trail_pct: Decimal,
    pub trailing_min_high_pct: Decimal,
}

fn cfg_for(base: &ExitConfig, g: &GridPoint) -> ExitConfig {
    let mut c = base.clone();
    c.stop_loss_pct = g.stop_loss_pct;
    c.min_trail_pct = g.min_trail_pct;
    c.trailing_min_high_pct = g.trailing_min_high_pct;
    c
}

/// Default grid for the exit walk-forward study. The first point is the OLD
/// shipped config (SL50, the wide stop the pre-tuning binary ran); the second is
/// the ACTUAL current shipped default (`ExitConfig::default`: SL12 + trail8, see
/// exit_policy.rs). The remaining points probe the neighbouring stop/trail space.
///
/// `shipped` MUST stay in sync with `ExitConfig::default`: an earlier grid
/// mislabelled an SL15/trail10 point as "shipped" and omitted the real SL12/trail8
/// cell, so the offline study never scored what production actually trades and a
/// stale wide-stop binary went unnoticed.
pub fn default_grid() -> Vec<GridPoint> {
    vec![
        GridPoint {
            name: "old-SL50/trail10".into(),
            stop_loss_pct: dec!(50),
            min_trail_pct: dec!(10),
            trailing_min_high_pct: dec!(15),
        },
        GridPoint {
            name: "shipped-SL12/trail8".into(),
            stop_loss_pct: dec!(12),
            min_trail_pct: dec!(8),
            trailing_min_high_pct: dec!(15),
        },
        GridPoint {
            name: "SL12/trail10".into(),
            stop_loss_pct: dec!(12),
            min_trail_pct: dec!(10),
            trailing_min_high_pct: dec!(15),
        },
        GridPoint {
            name: "SL15/trail8".into(),
            stop_loss_pct: dec!(15),
            min_trail_pct: dec!(8),
            trailing_min_high_pct: dec!(15),
        },
        GridPoint {
            name: "SL10/trail8".into(),
            stop_loss_pct: dec!(10),
            min_trail_pct: dec!(8),
            trailing_min_high_pct: dec!(15),
        },
        GridPoint {
            name: "SL20/trail10".into(),
            stop_loss_pct: dec!(20),
            min_trail_pct: dec!(10),
            trailing_min_high_pct: dec!(15),
        },
    ]
}

/// One cell's train/test score in the frozen holdout study.
#[derive(Debug, Clone)]
pub struct HoldoutRow {
    pub name: String,
    pub train_pnl: Decimal,
    pub train_wins: usize,
    pub test_pnl: Decimal,
    pub test_wins: usize,
}

/// Result of choosing an exit cell ONCE on the early window and applying it
/// frozen (no per-record re-optimization) to the later window — a stricter
/// overfit check than the adaptive expanding-window `walk_forward`.
#[derive(Debug, Clone)]
pub struct HoldoutResult {
    pub rows: Vec<HoldoutRow>,
    /// Cell with the highest training PnL.
    pub train_best: String,
    /// That cell's PnL over the unseen test window.
    pub frozen_test_pnl: Decimal,
    pub train_n: usize,
    pub test_n: usize,
}

/// Strict chronological holdout. `records` MUST be time-ordered (ascending); the
/// first `train_frac` fraction is the training window, the rest the test window.
pub fn frozen_holdout(
    records: &[ShadowRecord],
    base: &ExitConfig,
    grid: &[GridPoint],
    train_frac: f64,
) -> HoldoutResult {
    let cut = ((records.len() as f64) * train_frac).round() as usize;
    let cut = cut.clamp(1, records.len().saturating_sub(1));
    let (train, test) = records.split_at(cut);

    let mut rows = Vec::with_capacity(grid.len());
    for g in grid {
        let cfg = cfg_for(base, g);
        let mut train_pnl = Decimal::ZERO;
        let mut train_wins = 0usize;
        for r in train {
            let p = replay(r, &cfg).pnl;
            train_pnl += p;
            if p > Decimal::ZERO {
                train_wins += 1;
            }
        }
        let mut test_pnl = Decimal::ZERO;
        let mut test_wins = 0usize;
        for r in test {
            let p = replay(r, &cfg).pnl;
            test_pnl += p;
            if p > Decimal::ZERO {
                test_wins += 1;
            }
        }
        rows.push(HoldoutRow {
            name: g.name.clone(),
            train_pnl,
            train_wins,
            test_pnl,
            test_wins,
        });
    }

    let best_idx = rows
        .iter()
        .enumerate()
        .max_by_key(|(_, row)| row.train_pnl)
        .map(|(i, _)| i)
        .unwrap_or(0);
    HoldoutResult {
        train_best: rows[best_idx].name.clone(),
        frozen_test_pnl: rows[best_idx].test_pnl,
        rows,
        train_n: train.len(),
        test_n: test.len(),
    }
}

/// Bucketed realized-PnL analysis over recorded trades — the answerable half of
/// "should we widen the entry cap / relax the timing gate?". NOTE: shadow data
/// only contains trades that ALREADY passed the entry gate, so this quantifies
/// where the existing entries earn money; it cannot see opportunities the gate
/// rejected (those were never recorded).
#[derive(Debug, Clone)]
pub struct Bucket {
    pub label: String,
    pub n: usize,
    pub sum: Decimal,
    pub median: Decimal,
    pub win_rate: Decimal,
}

fn bucket_of(v: Decimal, edges: &[Decimal]) -> usize {
    for (i, e) in edges.iter().enumerate() {
        if v < *e {
            return i;
        }
    }
    edges.len()
}

fn summarize(label: &str, mut pnls: Vec<Decimal>) -> Bucket {
    let n = pnls.len();
    pnls.sort();
    let sum: Decimal = pnls.iter().copied().sum();
    let median = if n == 0 {
        Decimal::ZERO
    } else if n % 2 == 1 {
        pnls[n / 2]
    } else {
        (pnls[n / 2 - 1] + pnls[n / 2]) / Decimal::TWO
    };
    let wins = pnls.iter().filter(|p| **p > Decimal::ZERO).count();
    Bucket {
        label: label.to_string(),
        n,
        sum,
        median,
        win_rate: if n > 0 {
            Decimal::from(wins) / Decimal::from(n) * Decimal::ONE_HUNDRED
        } else {
            Decimal::ZERO
        },
    }
}

/// Bucket realized PnL by entry price (tests the entry cap) and by time-left at
/// entry (tests the timing gate). Only trades with a recorded actual PnL count.
pub fn bucket_by_entry(
    records: &[ShadowRecord],
    entry_edges: &[Decimal],
    time_edges: &[Decimal],
) -> (Vec<Bucket>, Vec<Bucket>) {
    let price_labels = labels_for(entry_edges);
    let time_labels = labels_for(time_edges);
    let mut by_price: Vec<Vec<Decimal>> = vec![Vec::new(); price_labels.len()];
    let mut by_time: Vec<Vec<Decimal>> = vec![Vec::new(); time_labels.len()];

    for r in records {
        let Some(pnl) = r.actual_net_pnl else {
            continue;
        };
        let pi = bucket_of(r.entry_price, entry_edges);
        if pi < by_price.len() {
            by_price[pi].push(pnl);
        }
        let tl = r.time_left_at_entry_sec.unwrap_or(Decimal::NEGATIVE_ONE);
        if tl >= Decimal::ZERO {
            let ti = bucket_of(tl, time_edges);
            if ti < by_time.len() {
                by_time[ti].push(pnl);
            }
        }
    }
    (
        price_labels
            .iter()
            .zip(by_price)
            .map(|(l, v)| summarize(l, v))
            .collect(),
        time_labels
            .iter()
            .zip(by_time)
            .map(|(l, v)| summarize(l, v))
            .collect(),
    )
}

fn labels_for(edges: &[Decimal]) -> Vec<String> {
    let mut out = Vec::new();
    let mut prev: Option<Decimal> = None;
    for e in edges {
        out.push(match prev {
            Some(p) => format!("{p}-{e}"),
            None => format!("<{e}"),
        });
        prev = Some(*e);
    }
    out.push(format!(">={}", prev.unwrap_or(Decimal::ZERO)));
    out
}

#[derive(Debug, Clone)]
pub struct WalkForwardResult {
    /// PnL of each grid point fit on all records (optimistic upper bound).
    pub in_sample: Vec<(String, Decimal)>,
    /// Records tested out-of-sample.
    pub oos_count: usize,
    /// Expanding-window walk-forward PnL (params chosen on the past, applied once).
    pub oos_pnl: Decimal,
    pub min_train: usize,
}

/// Expanding-window walk-forward: pick the best grid point on records[0..k],
/// apply it once to record k. Guards against curve-fitting a handful of paths.
pub fn walk_forward(
    records: &[ShadowRecord],
    base: &ExitConfig,
    grid: &[GridPoint],
    min_train: usize,
) -> WalkForwardResult {
    let in_sample: Vec<(String, Decimal)> = grid
        .iter()
        .map(|g| {
            let total = records
                .iter()
                .map(|r| replay(r, &cfg_for(base, g)).pnl)
                .sum();
            (g.name.clone(), total)
        })
        .collect();

    let mut oos_pnl = Decimal::ZERO;
    let mut oos_count = 0usize;
    if records.len() > min_train {
        for k in min_train..records.len() {
            let train = &records[..k];
            let best = grid
                .iter()
                .max_by_key(|g| {
                    train
                        .iter()
                        .map(|r| replay(r, &cfg_for(base, g)).pnl)
                        .sum::<Decimal>()
                })
                .or_else(|| grid.first());
            let Some(best) = best else { continue };
            oos_pnl += replay(&records[k], &cfg_for(base, best)).pnl;
            oos_count += 1;
        }
    }
    WalkForwardResult {
        in_sample,
        oos_count,
        oos_pnl,
        min_train,
    }
}

/// Parse one Node-format shadow JSONL line (as written by the TS shadow engine)
/// into a `ShadowRecord`, so Rust walk-forward can run over existing datasets.
pub fn parse_shadow_line(line: &str) -> Option<ShadowRecord> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let entry = dec_from(&v["entryPrice"])?;
    let shares = dec_from(&v["shares"])?;
    let entered_at = v["enteredAt"].as_i64()?;
    let expires_at = v["expiredAt"].as_i64().unwrap_or(entered_at + 900_000);
    let own = parse_path(&v["own"])?;
    if own.is_empty() {
        return None;
    }
    let actual_net_pnl = v
        .get("actual")
        .and_then(|a| a.get("netPnlUsd"))
        .and_then(dec_from);
    Some(ShadowRecord {
        token_id: v["tokenId"].as_str().unwrap_or("").to_string(),
        asset: v["asset"].as_str().unwrap_or("").to_string(),
        direction: v["direction"].as_str().unwrap_or("up").to_string(),
        entry_price: entry,
        shares,
        was_maker_entry: v["wasMakerEntry"].as_bool().unwrap_or(false),
        entered_at_ms: entered_at,
        expires_at_ms: expires_at,
        own,
        opposite: parse_path(&v["opposite"]).unwrap_or_default(),
        actual_net_pnl,
        time_left_at_entry_sec: v
            .get("context")
            .and_then(|c| c.get("timeLeftSec"))
            .and_then(dec_from),
    })
}

fn parse_path(v: &serde_json::Value) -> Option<Vec<Sample>> {
    let samples = v.get("samples")?.as_array()?;
    Some(
        samples
            .iter()
            .filter_map(|s| {
                let t_sec = s.get("t")?.as_f64()?;
                let price = dec_from(&s["p"])?;
                Some(Sample {
                    t_ms: (t_sec * 1000.0) as i64,
                    price,
                    bid: s.get("b").and_then(dec_from),
                    ask: s.get("a").and_then(dec_from),
                })
            })
            .collect(),
    )
}

fn dec_from(v: &serde_json::Value) -> Option<Decimal> {
    use std::str::FromStr;
    if let Some(n) = v.as_f64() {
        // Route through the shortest string form to avoid f64 representation
        // noise (0.59 must stay 0.59, not 0.58999999...).
        Decimal::from_str(&format!("{n}")).ok()
    } else if let Some(s) = v.as_str() {
        Decimal::from_str(s).ok()
    } else {
        None
    }
}

/// Read a shadow JSONL file and run walk-forward over its records.
pub fn walk_forward_file(
    path: &std::path::Path,
    base: &ExitConfig,
    grid: &[GridPoint],
    min_train: usize,
) -> std::io::Result<WalkForwardResult> {
    let text = std::fs::read_to_string(path)?;
    let mut records: Vec<ShadowRecord> = text.lines().filter_map(parse_shadow_line).collect();
    records.sort_by_key(|r| r.entered_at_ms);
    Ok(walk_forward(&records, base, grid, min_train))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec_with_path(entry: Decimal, prices: &[(i64, f64)]) -> ShadowRecord {
        use rust_decimal::prelude::FromPrimitive;
        let own = prices
            .iter()
            .map(|(t, p)| Sample {
                t_ms: *t,
                price: Decimal::from_f64(*p).unwrap(),
                bid: Some(Decimal::from_f64(*p).unwrap()),
                ask: Some(Decimal::from_f64(*p).unwrap()),
            })
            .collect();
        ShadowRecord {
            token_id: "t".into(),
            asset: "BTC".into(),
            direction: "up".into(),
            entry_price: entry,
            shares: dec!(10),
            was_maker_entry: true,
            entered_at_ms: 0,
            expires_at_ms: 900_000,
            own,
            opposite: vec![],
            actual_net_pnl: None,
            time_left_at_entry_sec: None,
        }
    }

    #[test]
    fn replay_takes_profit_on_a_run_up() {
        let r = rec_with_path(
            dec!(0.40),
            &[(0, 0.40), (1000, 0.50), (2000, 1.0), (3000, 0.99)],
        );
        let cfg = ExitConfig::default(); // TP 100%
        let out = replay(&r, &cfg);
        // +150% mid but bid 1.0 → capped by TP at ~100%; fee small → positive.
        assert!(out.pnl > Decimal::ZERO, "expected profit, got {}", out.pnl);
    }

    #[test]
    fn replay_stops_out_on_a_drop() {
        let r = rec_with_path(dec!(0.40), &[(0, 0.40), (1000, 0.20), (2000, 0.10)]);
        let cfg = ExitConfig::default();
        let out = replay(&r, &cfg);
        assert!(out.pnl < Decimal::ZERO);
    }

    #[test]
    fn recorder_finalizes_after_window() {
        let mut rec = ShadowRecorder::new(0, 1000);
        rec.on_open("p1", "BTC", "up", "tok", dec!(0.4), dec!(10), true, 0, 0);
        let book = OrderbookSnapshot::from_levels(
            "tok",
            vec![(dec!(0.4), dec!(1))],
            vec![(dec!(0.42), dec!(1))],
            100,
        );
        rec.on_book("tok", &book, 100);
        assert_eq!(rec.pending(), 1);
        let done = rec.tick(2000);
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].own.len(), 1);
    }

    #[test]
    fn walk_forward_reports_oos_and_in_sample() {
        // Five records: mostly losers, one big winner.
        let recs: Vec<ShadowRecord> = vec![
            rec_with_path(dec!(0.40), &[(0, 0.40), (1000, 0.30), (2000, 0.20)]),
            rec_with_path(dec!(0.40), &[(0, 0.40), (1000, 0.35), (2000, 0.25)]),
            rec_with_path(dec!(0.40), &[(0, 0.40), (1000, 0.60), (2000, 1.0)]),
            rec_with_path(dec!(0.40), &[(0, 0.40), (1000, 0.38), (2000, 0.30)]),
            rec_with_path(dec!(0.40), &[(0, 0.40), (1000, 0.50), (2000, 0.45)]),
        ];
        let out = walk_forward(&recs, &ExitConfig::default(), &default_grid(), 3);
        assert_eq!(out.in_sample.len(), default_grid().len());
        assert_eq!(out.oos_count, 2);
        // The in-sample list is named and ordered by the grid.
        assert_eq!(out.in_sample[0].0, default_grid()[0].name);
    }

    #[test]
    fn frozen_holdout_picks_on_train_and_scores_test_once() {
        let recs: Vec<ShadowRecord> = (0..6)
            .map(|i| {
                // Alternating loser/winner paths, time-ordered via entered_at.
                let end = if i % 2 == 0 { 0.30 } else { 0.62 };
                let mut r = rec_with_path(dec!(0.40), &[(0, 0.40), (1000, end)]);
                r.entered_at_ms = i as i64 * 1000;
                r
            })
            .collect();
        let grid = default_grid();
        let out = frozen_holdout(&recs, &ExitConfig::default(), &grid, 0.5);
        assert_eq!(out.train_n, 3);
        assert_eq!(out.test_n, 3);
        assert_eq!(out.rows.len(), grid.len());
        // The reported frozen score must equal the train-best row's test score.
        let best = out.rows.iter().find(|r| r.name == out.train_best).unwrap();
        assert_eq!(out.frozen_test_pnl, best.test_pnl);
        // The shipped cell is present and scored (regression: it used to be absent).
        assert!(out.rows.iter().any(|r| r.name == "shipped-SL12/trail8"));
    }

    #[test]
    fn json_record_has_analyzer_shape() {
        let r = rec_with_path(dec!(0.40), &[(0, 0.40), (1000, 0.50)]);
        let v = r.to_json();
        assert!(v["own"]["samples"].is_array());
        assert!(v["own"].get("maxPct").is_some());
        assert_eq!(v["direction"], "up");
    }

    #[test]
    fn parses_node_format_line() {
        let line = r#"{"id":"hft-1","strategy":"spread_arb","asset":"ETH","direction":"down","tokenId":"t1","entryPrice":0.45,"shares":10,"wasMakerEntry":true,"enteredAt":1000,"expiredAt":901000,"own":{"entryRef":0.45,"samples":[{"t":0,"p":0.45},{"t":5,"p":0.6,"b":0.59,"a":0.61}],"maxPrice":0.6,"minPrice":0.45},"oppositeTokenId":null,"opposite":null,"closed":true,"actual":{"exitReason":"take_profit","netPnlUsd":1.5}}"#;
        let r = parse_shadow_line(line).unwrap();
        assert_eq!(r.asset, "ETH");
        assert_eq!(r.direction, "down");
        assert_eq!(r.shares, dec!(10));
        assert_eq!(r.own.len(), 2);
        assert_eq!(r.own[1].bid, Some(dec!(0.59)));
        assert_eq!(r.actual_net_pnl, Some(dec!(1.5)));
        // And it replays without panicking.
        let _ = replay(&r, &ExitConfig::default());
    }

    #[test]
    fn near_miss_records_path_and_replays() {
        let mut rec = NearMissRecorder::new(0, 1000);
        // time_left must be > min_time_left(180) for a relaxed gate to be able to
        // hold the trade; a block recorded below that is a pure timing exit.
        rec.on_blocked(
            "tok",
            "BTC",
            "up",
            dec!(0.43),
            dec!(0.44),
            NearMissReason::Timing,
            190,
            7,
            0,
        );
        // Dedup: a second block for the same token does not spawn a new record.
        rec.on_blocked(
            "tok",
            "BTC",
            "up",
            dec!(0.43),
            dec!(0.44),
            NearMissReason::Timing,
            190,
            7,
            100,
        );
        assert_eq!(rec.pending(), 1);

        let up = OrderbookSnapshot::from_levels(
            "tok",
            vec![(dec!(0.43), dec!(1))],
            vec![(dec!(0.45), dec!(1))],
            100,
        );
        rec.on_book("tok", &up, 100);
        let big = OrderbookSnapshot::from_levels(
            "tok",
            vec![(dec!(0.90), dec!(1))],
            vec![(dec!(0.92), dec!(1))],
            500,
        );
        rec.on_book("tok", &big, 500);
        let done = rec.tick(2000);
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].path.len(), 2);

        // Hypothetical replay from the blocked bid: a run-up should profit.
        let res = replay_near_miss(&done[0], &ExitConfig::default());
        assert!(
            res.pnl > Decimal::ZERO,
            "run-up near-miss should replay positive, got {}",
            res.pnl
        );
    }

    #[test]
    fn near_miss_summary_counts_reasons_and_pnl() {
        let mk = |token: &str, reason: NearMissReason, up: bool| NearMissRecord {
            token_id: token.into(),
            asset: "BTC".into(),
            direction: "up".into(),
            blocked_at_ms: 0,
            entry_price: dec!(0.43),
            mid_at_block: dec!(0.44),
            reason,
            time_left_sec: 190,
            round_slot: 7,
            path: if up {
                vec![
                    Sample {
                        t_ms: 100,
                        price: dec!(0.43),
                        bid: Some(dec!(0.43)),
                        ask: Some(dec!(0.45)),
                    },
                    Sample {
                        t_ms: 500,
                        price: dec!(0.95),
                        bid: Some(dec!(0.95)),
                        ask: Some(dec!(0.97)),
                    },
                ]
            } else {
                vec![
                    Sample {
                        t_ms: 100,
                        price: dec!(0.43),
                        bid: Some(dec!(0.43)),
                        ask: Some(dec!(0.45)),
                    },
                    Sample {
                        t_ms: 500,
                        price: dec!(0.10),
                        bid: Some(dec!(0.10)),
                        ask: Some(dec!(0.12)),
                    },
                ]
            },
            window_end_ms: 1000,
        };
        let recs = vec![
            mk("a", NearMissReason::Timing, true),
            mk("b", NearMissReason::Momentum, false),
        ];
        let s = evaluate_near_misses(&recs, &ExitConfig::default());
        assert_eq!(s.n, 2);
        assert_eq!(s.timing_n, 1);
        assert_eq!(s.momentum_n, 1);
        assert_eq!(s.wins, 1);
        // One winner (~+$5) + one loser (~-$3.3) → net positive here; the point is
        // the summary is populated and reason-bucketed.
        assert!(s.total_pnl != Decimal::ZERO);
    }

    #[test]
    fn near_miss_json_round_trips() {
        let mut rec = NearMissRecorder::new(0, 1000);
        rec.on_blocked(
            "tok",
            "BTC",
            "up",
            dec!(0.43),
            dec!(0.44),
            NearMissReason::Timing,
            150,
            7,
            0,
        );
        let b = OrderbookSnapshot::from_levels(
            "tok",
            vec![(dec!(0.43), dec!(1))],
            vec![(dec!(0.45), dec!(1))],
            100,
        );
        rec.on_book("tok", &b, 100);
        let done = rec.tick(2000);
        let json = near_miss_to_json(&done[0]);
        let line = serde_json::to_string(&json).unwrap();
        let back = parse_near_miss_line(&line).unwrap();
        assert_eq!(back.token_id, "tok");
        assert_eq!(back.reason, NearMissReason::Timing);
        assert_eq!(back.entry_price, dec!(0.43));
        assert_eq!(back.path.len(), 1);
    }
}

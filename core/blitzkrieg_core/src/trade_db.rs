//! Trade database — persists closed trades in the SAME JSONL format the Node
//! side used (`data/trades/trades.jsonl` plus `summary.json`), so the existing
//! web panel and offline analysis scripts keep working after the core took over
//! trading. Field names and units mirror `src/strategies/crypto-hft/trade-db.ts`,
//! now deleted with the Node source layer (`62b16c88`) — the format is the
//! contract that outlives it.

use crate::position::ClosedPosition;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// One closed trade, wire-compatible with the Node `TradeRecord`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TradeRecord {
    pub id: String,
    pub strategy: String,
    pub asset: String,
    pub direction: String,
    pub token_id: String,
    pub condition_id: String,
    #[serde(with = "crate::decimal")]
    pub entry_price: Decimal,
    #[serde(with = "crate::decimal")]
    pub exit_price: Decimal,
    #[serde(with = "crate::decimal")]
    pub shares: Decimal,
    #[serde(with = "crate::decimal")]
    pub cost_usd: Decimal,
    #[serde(with = "crate::decimal")]
    pub gross_pnl_usd: Decimal,
    #[serde(with = "crate::decimal")]
    pub entry_fee_pct: Decimal,
    #[serde(with = "crate::decimal")]
    pub exit_fee_pct: Decimal,
    #[serde(with = "crate::decimal")]
    pub fees_usd: Decimal,
    #[serde(with = "crate::decimal")]
    pub net_pnl_usd: Decimal,
    #[serde(with = "crate::decimal")]
    pub net_pnl_pct: Decimal,
    pub hold_time_sec: i64,
    /// Milliseconds since epoch (matches the Node record).
    pub entry_time: i64,
    pub exit_time: i64,
    pub exit_reason: String,
    pub was_maker_entry: bool,
    pub was_maker_exit: bool,
    #[serde(with = "crate::decimal")]
    pub market_price_at_entry: Decimal,
    #[serde(with = "crate::decimal")]
    pub market_price_at_exit: Decimal,
    #[serde(with = "crate::decimal")]
    pub high_pnl_pct: Decimal,
    #[serde(with = "crate::decimal")]
    pub low_pnl_pct: Decimal,
}

impl TradeRecord {
    /// Build from a closed position (no I/O).
    pub fn from_closed(c: &ClosedPosition) -> Self {
        let entry_fee_usd = (c.entry_fee_pct / Decimal::ONE_HUNDRED) * c.entry_price * c.shares;
        let exit_fee_usd = (c.exit_fee_pct / Decimal::ONE_HUNDRED) * c.exit_price * c.shares;
        // The old Node record derived gross from the same fee-aware formula.
        let gross = c.net_pnl_usd + entry_fee_usd + exit_fee_usd;
        Self {
            id: c.id.clone(),
            strategy: c.strategy.clone(),
            asset: c.asset.clone(),
            direction: c.direction.as_str().to_string(),
            token_id: c.token_id.clone(),
            condition_id: c.condition_id.clone(),
            entry_price: c.entry_price,
            exit_price: c.exit_price,
            shares: c.shares,
            cost_usd: c.cost_usd,
            gross_pnl_usd: gross,
            entry_fee_pct: c.entry_fee_pct,
            exit_fee_pct: c.exit_fee_pct,
            fees_usd: entry_fee_usd + exit_fee_usd,
            net_pnl_usd: c.net_pnl_usd,
            net_pnl_pct: c.net_pnl_pct,
            hold_time_sec: c.hold_time_sec,
            entry_time: c.entered_at_ms,
            exit_time: c.exited_at_ms,
            // ExitReason derives serde(snake_case); use it so reasons match the
            // Node engine's values exactly (trailing_stop, take_profit, ...).
            exit_reason: serde_json::to_value(c.exit_reason)
                .ok()
                .and_then(|v| v.as_str().map(|s| s.to_string()))
                .unwrap_or_else(|| format!("{:?}", c.exit_reason).to_lowercase()),
            was_maker_entry: c.was_maker_entry,
            was_maker_exit: c.was_maker_exit,
            market_price_at_entry: c.entry_price,
            market_price_at_exit: c.exit_price,
            high_pnl_pct: c.high_pnl_pct,
            low_pnl_pct: c.low_pnl_pct,
        }
    }
}

/// Aggregate summary mirroring the Node `summary.json`.
#[derive(Debug, Clone, Default, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TradeSummary {
    pub total_trades: u64,
    pub wins: u64,
    pub losses: u64,
    pub win_rate: f64,
    pub total_gross_pnl: f64,
    pub total_fees: f64,
    pub total_net_pnl: f64,
    pub avg_hold_time_sec: f64,
    pub best_trade_pnl: f64,
    pub worst_trade_pnl: f64,
    pub last_updated: i64,
}

fn f64_of(d: Decimal) -> f64 {
    use std::str::FromStr;
    f64::from_str(&d.to_string()).unwrap_or(0.0)
}

/// A trade log bound to a JSONL path.
pub struct TradeDb {
    jsonl_path: PathBuf,
    summary_path: PathBuf,
    summary: TradeSummary,
}

impl TradeDb {
    /// Open (or create) the log. `jsonl_path` defaults to
    /// `data/trades/trades.jsonl`; the summary sits beside it.
    pub fn new(jsonl_path: impl AsRef<Path>) -> Self {
        let jsonl_path = jsonl_path.as_ref().to_path_buf();
        let summary_path = jsonl_path
            .parent()
            .map(|p| p.join("summary.json"))
            .unwrap_or_else(|| PathBuf::from("summary.json"));
        let summary = std::fs::read_to_string(&summary_path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        Self {
            jsonl_path,
            summary_path,
            summary,
        }
    }

    /// Append a closed trade and update the summary (best effort, never panics).
    pub fn record(&mut self, rec: &TradeRecord, now_ms: i64) {
        crate::jsonl::append(&self.jsonl_path, rec);
        self.fold_summary(rec);
        self.summary.last_updated = now_ms;
        self.persist_summary();
    }

    /// Fold one record into the running summary (record/retract share the
    /// arithmetic so a retract is the exact inverse of its record).
    fn fold_summary(&mut self, rec: &TradeRecord) {
        let net = f64_of(rec.net_pnl_usd);
        self.summary.total_trades += 1;
        if net >= 0.0 {
            self.summary.wins += 1;
        } else {
            self.summary.losses += 1;
        }
        self.summary.win_rate = if self.summary.total_trades > 0 {
            (self.summary.wins as f64 / self.summary.total_trades as f64) * 100.0
        } else {
            0.0
        };
        self.summary.total_gross_pnl += f64_of(rec.gross_pnl_usd);
        self.summary.total_fees += f64_of(rec.fees_usd);
        self.summary.total_net_pnl += net;
        let n = self.summary.total_trades as f64;
        self.summary.avg_hold_time_sec =
            ((self.summary.avg_hold_time_sec * (n - 1.0)) + rec.hold_time_sec as f64) / n;
        self.summary.best_trade_pnl = self.summary.best_trade_pnl.max(net);
        self.summary.worst_trade_pnl = self.summary.worst_trade_pnl.min(net);
    }

    fn persist_summary(&self) {
        if let Ok(text) = serde_json::to_string_pretty(&self.summary) {
            let _ = std::fs::write(&self.summary_path, text);
        }
    }

    /// F4: withdraw a recorded trade whose venue execution later FAILED. The
    /// JSONL is append-only for history, so the retraction REWRITES the file
    /// without the offending line and rebuilds the summary from the remaining
    /// records — a trade that never happened must not live in the books, not
    /// even as a compensating entry the win-rate maths would still count.
    ///
    /// Matches the record by (id, exit_time, net_pnl_usd): id alone can repeat
    /// across restarts, and the retraction must not eat a different close.
    /// No-op when no matching line exists (never recorded, already retracted).
    pub fn retract(&mut self, rec: &TradeRecord, now_ms: i64) {
        let Ok(text) = std::fs::read_to_string(&self.jsonl_path) else {
            return;
        };
        let mut remaining: Vec<TradeRecord> = text
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        let Some(pos) = remaining.iter().position(|r| {
            r.id == rec.id && r.exit_time == rec.exit_time && r.net_pnl_usd == rec.net_pnl_usd
        }) else {
            return;
        };
        remaining.remove(pos);
        let Ok(body) = remaining
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<Vec<_>, _>>()
            .map(|lines| lines.join("\n"))
        else {
            return;
        };
        let mut out = body;
        if !out.is_empty() {
            out.push('\n');
        }
        if std::fs::write(&self.jsonl_path, out).is_err() {
            return;
        }
        // Rebuild the summary from what is actually on file now.
        self.summary = TradeSummary::default();
        for r in &remaining {
            self.fold_summary(r);
        }
        self.summary.last_updated = now_ms;
        self.persist_summary();
    }

    pub fn summary(&self) -> &TradeSummary {
        &self.summary
    }
}

/// Read the last `limit` records from a trades JSONL file (newest last).
/// Used by the IPC history endpoint so the panel can render closed trades.
pub fn read_recent(path: &Path, limit: usize) -> Vec<serde_json::Value> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut all: Vec<serde_json::Value> = text
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .collect();
    if limit > 0 && all.len() > limit {
        all.drain(0..all.len() - limit);
    }
    all
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ExitReason, OrderRole, SignalDirection};
    use crate::position::ClosedPosition;

    fn closed() -> ClosedPosition {
        ClosedPosition {
            id: "hft-1".into(),
            strategy: "spread_arb".into(),
            asset: "BTC".into(),
            direction: SignalDirection::Up,
            token_id: "tok".into(),
            condition_id: "cond".into(),
            entry_price: rust_decimal_macros::dec!(0.40),
            exit_price: rust_decimal_macros::dec!(0.60),
            shares: rust_decimal_macros::dec!(10),
            cost_usd: rust_decimal_macros::dec!(4),
            was_maker_entry: true,
            was_maker_exit: false,
            entry_fee_pct: Decimal::ZERO,
            exit_fee_pct: rust_decimal_macros::dec!(0.5),
            pnl_usd: rust_decimal_macros::dec!(2),
            pnl_pct: rust_decimal_macros::dec!(50),
            net_pnl_usd: rust_decimal_macros::dec!(1.97),
            net_pnl_pct: rust_decimal_macros::dec!(49.25),
            high_pnl_pct: rust_decimal_macros::dec!(52),
            low_pnl_pct: rust_decimal_macros::dec!(-3),
            hold_time_sec: 64,
            exit_reason: ExitReason::TakeProfit,
            entered_at_ms: 1000,
            exited_at_ms: 65000,
            entry_role: OrderRole::Maker,
            exit_role: OrderRole::Taker,
            dust_shares: Decimal::ZERO,
        }
    }

    #[test]
    fn record_has_node_compatible_shape() {
        let rec = TradeRecord::from_closed(&closed());
        let v = serde_json::to_value(&rec).unwrap();
        // Fields the panel / analysis scripts read.
        for k in [
            "id",
            "strategy",
            "asset",
            "direction",
            "entryPrice",
            "exitPrice",
            "netPnlUsd",
            "netPnlPct",
            "exitReason",
            "entryTime",
            "exitTime",
            "holdTimeSec",
        ] {
            assert!(v.get(k).is_some(), "missing field {k}");
        }
        assert_eq!(v["direction"], "up");
        assert_eq!(v["exitReason"], "take_profit");
        assert!(v["netPnlUsd"].as_f64().unwrap() > 1.9);
    }

    #[test]
    fn writes_jsonl_and_updates_summary() {
        let dir = std::env::temp_dir().join(format!("bktrade-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("trades.jsonl");
        let mut db = TradeDb::new(&path);
        db.record(&TradeRecord::from_closed(&closed()), 100);
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 1);
        assert!(db.summary().total_trades == 1 && db.summary().wins == 1);
        // Second write appends and accumulates.
        db.record(&TradeRecord::from_closed(&closed()), 200);
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 2);
        assert_eq!(db.summary().total_trades, 2);
        // read_recent returns them in order.
        let recent = read_recent(&path, 1);
        assert_eq!(recent.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

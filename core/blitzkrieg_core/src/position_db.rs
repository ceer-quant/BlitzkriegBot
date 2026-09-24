//! Position database — durable storage for the core's OPEN positions.
//!
//! Companion to `order_db`: the OME learned to persist resting orders, but the
//! position book (`PositionManager`) was still memory-only. A restart/crash
//! therefore forgot every open position — the bot would no longer value it,
//! run its exit rules, or know it held the token, so the trade could drift to
//! expiry unmanaged (same failure class as the orphan-order bug, §29).
//!
//! Positions are few (bounded by `max_positions`) and short-lived, so unlike the
//! append-only order log we simply REWRITE the current open set on every change:
//! that is O(few) and needs no tombstones to express a close. `load` reads the
//! snapshot back; unparseable lines are skipped, not fatal.

use crate::position::OpenPosition;
use std::path::{Path, PathBuf};

/// A position snapshot file bound to a path.
pub struct PositionDb {
    path: PathBuf,
    /// Side file holding the reconciliation watermark (the newest external
    /// fill already folded into the position book). Kept out of the JSONL so
    /// the typed position file stays pure.
    recon_path: PathBuf,
}

impl PositionDb {
    pub fn new(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().to_path_buf();
        let recon_path = path.with_extension("recon");
        Self { path, recon_path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Rewrite the file with exactly the current open positions. Best effort: a
    /// write failure must never interrupt trading (memory stays authoritative).
    pub fn save(&self, positions: &[OpenPosition]) {
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let mut buf = String::new();
        for p in positions {
            if let Ok(line) = serde_json::to_string(p) {
                buf.push_str(&line);
                buf.push('\n');
            }
        }
        let _ = std::fs::write(&self.path, buf);
    }

    /// Persist the newest external (unknown-order) fill timestamp already
    /// applied to the position book. Without this, a restart would re-apply
    /// every manual close the sweep still reports. Best effort.
    pub fn save_watermark(&self, ms: i64) {
        if let Some(dir) = self.recon_path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&self.recon_path, ms.to_string());
    }

    /// Load the reconciliation watermark. Missing file = 0 (apply everything
    /// the sweep reports; on a fresh book that is nothing anyway).
    pub fn load_watermark(&self) -> i64 {
        std::fs::read_to_string(&self.recon_path)
            .ok()
            .and_then(|t| t.trim().parse().ok())
            .unwrap_or(0)
    }

    /// Load the persisted open positions. Missing file = none. Unparseable lines
    /// are skipped (a foreign/legacy file must not brick startup).
    pub fn load(&self) -> Vec<OpenPosition> {
        crate::jsonl::load::<OpenPosition>(&self.path, "position log: skipped unparseable lines")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exit_policy::ExitState;
    use crate::model::SignalDirection;
    use rust_decimal_macros::dec;

    fn pos(id: &str, shares: rust_decimal::Decimal) -> OpenPosition {
        OpenPosition {
            id: id.into(),
            strategy: "spread_arb".into(),
            asset: "BTC".into(),
            direction: SignalDirection::Up,
            token_id: "tok".into(),
            condition_id: "cond".into(),
            entry_price: dec!(0.43),
            current_price: dec!(0.45),
            prev_price: dec!(0.44),
            shares,
            cost_usd: dec!(4.3),
            was_maker_entry: true,
            entry_fee_pct: rust_decimal::Decimal::ZERO,
            entry_role: crate::model::OrderRole::Maker,
            exit_role: crate::model::OrderRole::Pending,
            target_exit_price: None,
            entered_at_ms: 1000,
            expires_at_ms: 900_000,
            last_book_ts: 0,
            state: ExitState::new(dec!(0.43), 1000),
            flows: crate::position::CashFlows {
                entry_cost_usd: dec!(4.3),
                opened_shares: shares,
                ..Default::default()
            },
        }
    }

    #[test]
    fn save_then_load_roundtrips_open_positions() {
        let dir = std::env::temp_dir().join(format!("positiondb-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let db = PositionDb::new(dir.join("positions.jsonl"));

        db.save(&[pos("hft-1", dec!(10)), pos("hft-2", dec!(4))]);
        let loaded = db.load();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].id, "hft-1");
        assert_eq!(loaded[1].shares, dec!(4));
        assert_eq!(loaded[0].entry_price, dec!(0.43));

        // A subsequent save reflects a close (one position gone) with no tombstone.
        db.save(&[pos("hft-2", dec!(4))]);
        let after = db.load();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].id, "hft-2");

        // Watermark round-trips in its side file.
        db.save_watermark(123_456);
        assert_eq!(db.load_watermark(), 123_456);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_or_corrupt_file_loads_empty_not_fatal() {
        let dir = std::env::temp_dir().join(format!("positiondb-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let db = PositionDb::new(dir.join("positions.jsonl"));
        assert!(db.load().is_empty(), "missing file = no positions");

        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(db.path(), "not json\n{\"id\":\"x\"}\n").unwrap();
        assert!(db.load().is_empty(), "corrupt/foreign lines are skipped");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

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
    ///
    /// #463: the write is atomic (temp + fsync + rename) AND keeps one previous
    /// generation as `positions.bak` — the snapshot the loader falls back to
    /// when the canonical file is unreadable. The position book is the one file
    /// whose silent loss means unmanaged inventory, so it carries a backup.
    pub fn save(&self, positions: &[OpenPosition]) {
        let mut buf = String::new();
        for p in positions {
            if let Ok(line) = serde_json::to_string(p) {
                buf.push_str(&line);
                buf.push('\n');
            }
        }
        // #463: the write is atomic (temp + fsync + rename) AND mirrors the
        // last GOOD generation into `positions.bak` — the snapshot the loader
        // falls back to when the canonical file is unreadable. The position
        // book is the one file whose silent loss means unmanaged inventory, so
        // it carries a backup.
        if crate::jsonl::rewrite_atomic(&self.path, buf.as_bytes()) {
            let _ = std::fs::copy(&self.path, self.path.with_extension("bak"));
        } else {
            tracing::warn!(
                path = %self.path.display(),
                "position book persist failed — memory stays authoritative for the run"
            );
        }
    }

    /// Persist the newest external (unknown-order) fill timestamp already
    /// applied to the position book. Without this, a restart would re-apply
    /// every manual close the sweep still reports. Best effort.
    pub fn save_watermark(&self, ms: i64) {
        if !crate::jsonl::rewrite_atomic(&self.recon_path, ms.to_string().as_bytes()) {
            tracing::warn!(
                path = %self.recon_path.display(),
                "recon watermark persist failed"
            );
        }
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
    ///
    /// #463: an unreadable canonical file (truncated by a crash, empty, or every
    /// line corrupt) is NOT silently an empty book — the loader first falls back
    /// to the `.bak` generation `save` keeps, and if that too is unusable it
    /// errors loudly. A position book that quietly loads empty is a core that
    /// restarts into unmanaged inventory; that must be visible, never a warn.
    pub fn load(&self) -> Vec<OpenPosition> {
        let canonical = std::fs::read_to_string(&self.path).ok();
        let parsed = |text: &str| -> Vec<OpenPosition> {
            let mut out = Vec::new();
            let mut skipped = 0usize;
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                match serde_json::from_str::<OpenPosition>(line) {
                    Ok(v) => out.push(v),
                    Err(_) => skipped += 1,
                }
            }
            if skipped > 0 {
                tracing::warn!(
                    skipped,
                    path = %self.path.display(),
                    "position log: skipped unparseable lines"
                );
            }
            out
        };
        if let Some(text) = canonical {
            let rows = parsed(&text);
            if !text.trim().is_empty() && rows.is_empty() {
                // The file EXISTS but nothing in it parsed — corrupt, not empty.
                tracing::error!(
                    path = %self.path.display(),
                    "position book unreadable (crash-truncated or corrupt) — trying the .bak generation"
                );
            } else {
                return rows;
            }
        } else {
            // No canonical file at all: either a fresh data dir, or a crash
            // before the first save. A .bak without a canonical file means the
            // crash hit DURING the rename's sibling operations — take it, but
            // loudly.
            if let Some(rows) = self.load_backup() {
                return rows;
            }
            return Vec::new();
        }
        if let Some(rows) = self.load_backup() {
            return rows;
        }
        tracing::error!(
            path = %self.path.display(),
            "POSITION BOOK UNREADABLE and no .bak — starting with ZERO open positions; \
             any open inventory from the previous run is now UNMANAGED (exits, expiry, \
             settlement). Inspect the file before trusting this core with new entries."
        );
        Vec::new()
    }

    /// The `.bak` generation, used only when the canonical file cannot serve.
    /// Returns None when there is no backup or it parses to nothing.
    fn load_backup(&self) -> Option<Vec<OpenPosition>> {
        let bak = self.path.with_extension("bak");
        let rows = crate::jsonl::load::<OpenPosition>(
            &bak,
            "position log .bak: skipped unparseable lines",
        );
        if rows.is_empty() {
            tracing::error!(
                path = %bak.display(),
                "position book .bak is empty or unparseable"
            );
            return None;
        }
        tracing::warn!(
            path = %bak.display(),
            count = rows.len(),
            "position book recovered from the .bak generation"
        );
        Some(rows)
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
            account_id: crate::model::default_account_id(),
            venue: String::new(),
            partner_token_id: String::new(),
            last_book_ts: 0,
            round_duration_sec: 0,
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

    /// #463: a truncated canonical file falls back to the .bak generation —
    /// the crash that used to yield a silent empty book now recovers the
    /// previous snapshot, so inventory stays managed.
    #[test]
    fn a_truncated_book_recovers_from_the_bak_generation() {
        let dir = std::env::temp_dir().join(format!("positiondb-bak-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = PositionDb::new(dir.join("positions.jsonl"));

        // Two saves: the first writes two positions (and seeds .bak on the
        // second), the second leaves one.
        db.save(&[pos("hft-1", dec!(10)), pos("hft-2", dec!(4))]);
        db.save(&[pos("hft-1", dec!(10))]);
        assert_eq!(db.load().len(), 1);

        // Simulate the crash window: the canonical file is truncated to garbage.
        std::fs::write(db.path(), "{\"id\":\"hft-1\"").unwrap();
        let recovered = db.load();
        assert_eq!(recovered.len(), 1, "the .bak generation serves");
        assert_eq!(recovered[0].id, "hft-1");
        // And the recovered generation is exactly the last healthy one — the
        // close of hft-2 is not undone by the recovery.
        assert_eq!(recovered[0].shares, dec!(10));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #463: a canonical file that exists but parses to NOTHING, with no .bak,
    /// is the one case where zero positions must be loud — the empty set is
    /// returned (trading continues) but the contract is the error line, so the
    /// test pins it via the tracing capture being the loader's job: here we pin
    /// the DATA side — an empty-but-real file must not be mistaken for a
    /// corrupt one (no .bak consultation, no error path), because a book that
    /// legitimately closed every position writes exactly that shape.
    #[test]
    fn an_empty_but_present_file_is_not_a_corruption() {
        let dir = std::env::temp_dir().join(format!("positiondb-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = PositionDb::new(dir.join("positions.jsonl"));

        // An all-positions-closed book: the file exists, is newline-terminated,
        // and carries zero rows. This is the NORMAL post-flatten shape.
        std::fs::write(db.path(), "").unwrap();
        assert!(db.load().is_empty());
        // No .bak was consulted or created.
        assert!(!dir.join("positions.bak").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

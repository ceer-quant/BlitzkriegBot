//! Order database — durable storage for the OME's tracked orders.
//!
//! The Node engine kept `data/orders/orders.jsonl` and hydrated from it on
//! restart; the Rust core originally kept orders in memory only, so a restart
//! (or crash) forgot every resting order while the venue still held it — the
//! "orphan order" failure. This module restores that guarantee for the core.
//!
//! Each state change appends the order's CURRENT snapshot (same append-only
//! shape as the trade log); `load` folds the log to the latest snapshot per
//! order id. `compact` rewrites the file with only the live orders so it does
//! not grow without bound across restarts.

use crate::model::TrackedOrder;
use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// An order log bound to a JSONL path.
pub struct OrderDb {
    jsonl_path: PathBuf,
}

impl OrderDb {
    pub fn new(jsonl_path: impl AsRef<Path>) -> Self {
        Self {
            jsonl_path: jsonl_path.as_ref().to_path_buf(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.jsonl_path
    }

    /// Append the order's current state. Best effort: a write failure must never
    /// interrupt trading (the in-memory OME stays authoritative for this run).
    pub fn append(&self, order: &TrackedOrder) {
        if let Some(dir) = self.jsonl_path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(line) = serde_json::to_string(order) {
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.jsonl_path)
            {
                let _ = writeln!(f, "{line}");
            }
        }
    }

    /// Load the latest snapshot per order id (later lines win). Tolerant of a
    /// legacy Node-format file: unparseable lines are skipped, not fatal.
    pub fn load(&self) -> Vec<TrackedOrder> {
        let Ok(text) = std::fs::read_to_string(&self.jsonl_path) else {
            return Vec::new();
        };
        let mut latest: HashMap<String, TrackedOrder> = HashMap::new();
        let mut skipped = 0usize;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<TrackedOrder>(line) {
                Ok(o) => {
                    latest.insert(o.order_id.clone(), o);
                }
                Err(_) => skipped += 1,
            }
        }
        if skipped > 0 {
            tracing::warn!(
                skipped,
                path = %self.jsonl_path.display(),
                "order log: skipped unparseable lines (legacy/foreign format?)"
            );
        }
        latest.into_values().collect()
    }

    /// Rewrite the log with exactly `orders` (used after a restart to drop
    /// terminal history and keep only what still needs managing).
    pub fn compact(&self, orders: &[TrackedOrder]) {
        if let Some(dir) = self.jsonl_path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let mut buf = String::new();
        for o in orders {
            if let Ok(line) = serde_json::to_string(o) {
                buf.push_str(&line);
                buf.push('\n');
            }
        }
        let _ = std::fs::write(&self.jsonl_path, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    fn order(id: &str, key: &str, status: OrderStatus) -> TrackedOrder {
        TrackedOrder {
            order_id: id.into(),
            internal_key: key.into(),
            strategy: "spread_arb".into(),
            asset: "BTC".into(),
            direction: "up".into(),
            token_id: "tok".into(),
            condition_id: "cond".into(),
            side: Side::Buy,
            mode: FillPolicy::MakerThenTaker,
            price: dec!(0.43),
            size: dec!(10),
            filled_size: Decimal::ZERO,
            avg_fill_price: None,
            status,
            round_slot: 1,
            submitted_at_ms: 1,
            updated_at_ms: 1,
            venue_order_id: Some("0xabc".into()),
            escalate_at_ms: None,
            role: OrderRole::Maker,
        }
    }

    #[test]
    fn append_and_load_latest_per_order() {
        let dir = std::env::temp_dir().join(format!("orderdb-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let db = OrderDb::new(dir.join("orders.jsonl"));

        // Two snapshots of the same order + one of another.
        db.append(&order("o1", "k1", OrderStatus::Pending));
        db.append(&order("o2", "k2", OrderStatus::Live));
        let mut o1_live = order("o1", "k1", OrderStatus::Live);
        o1_live.venue_order_id = Some("0xdef".into());
        db.append(&o1_live);

        let loaded = db.load();
        assert_eq!(loaded.len(), 2, "one latest snapshot per order id");
        let o1 = loaded.iter().find(|o| o.order_id == "o1").unwrap();
        assert_eq!(o1.status, OrderStatus::Live, "later snapshot wins");
        assert_eq!(
            o1.venue_order_id.as_deref(),
            Some("0xdef"),
            "venue id preserved"
        );

        // Compact keeps only what we pass.
        db.compact(&[order("o2", "k2", OrderStatus::Live)]);
        let after = db.load();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].order_id, "o2");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_lines_are_skipped_not_fatal() {
        let dir = std::env::temp_dir().join(format!("orderdb-legacy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("orders.jsonl");
        // A legacy Node-format line (side 'BUY', status 'SUBMITTED') followed by a
        // valid core line: the loader must keep the valid one and skip the other.
        let mut body =
            String::from("{\"orderId\":\"old\",\"side\":\"BUY\",\"status\":\"SUBMITTED\"}\n");
        body.push_str(&serde_json::to_string(&order("new", "kn", OrderStatus::Live)).unwrap());
        body.push('\n');
        std::fs::write(&path, body).unwrap();

        let db = OrderDb::new(&path);
        let loaded = db.load();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].order_id, "new");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

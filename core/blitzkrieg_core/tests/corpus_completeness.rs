//! #354 (5.2) — corpus completeness, measured on the REAL frozen corpus.
//!
//! The spot corpus (`data/corpus/spot/`) is gitignored content, so this target
//! is `#[ignore]`d by default and runs explicitly:
//!
//! ```text
//! cargo test -p blitzkrieg-core --test corpus_completeness -- --ignored --nocapture
//! ```
//!
//! One pass per pinned file, three claims each:
//!   1. content integrity — the file's sha256 equals its pin in
//!      `manifest.sha256` (a drifted corpus cannot be reasoned about);
//!   2. row reconciliation — spot + book + round rows == total rows: every row
//!      is a known kind, so a corpus that DROPPED its spot rows shows up here
//!      (and the replay source's own `spot_events` counter must agree with the
//!      raw tally, end to end);
//!   3. TOP_LEVELS=3 truncation measurement — how much quoted depth the frozen
//!      corpus transform (last 3 levels per side) actually keeps, in numbers
//!      rather than in a claim.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

use rust_decimal::Decimal;
use serde_json::Value;
use sha2::{Digest, Sha256};

use blitzkrieg_core::data_source::{DataSource, open_replay};

fn corpus_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data/corpus/spot")
}

/// The pins: `(sha256, filename)` in manifest order.
fn pinned() -> Vec<(String, String)> {
    let manifest = std::fs::read_to_string(corpus_dir().join("manifest.sha256"))
        .expect("manifest.sha256 must exist next to the corpus");
    manifest
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let hash = it.next()?.to_string();
            let name = it.next()?.to_string();
            Some((hash, name))
        })
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
#[ignore = "needs the frozen spot corpus (data/corpus/spot) — run with --ignored"]
fn corpus_rows_reconcile_and_top_levels_truncation_is_measured() {
    let dir = corpus_dir();
    if !dir.is_dir() {
        eprintln!("skip: {} absent (the corpus is gitignored)", dir.display());
        return;
    }
    let pins = pinned();
    assert!(!pins.is_empty(), "the manifest pins at least one file");

    let mut spots = 0u64;
    let mut books = 0u64;
    let mut rounds = 0u64;
    let mut total_rows = 0u64;
    // Truncation accumulators over every book row of the corpus.
    let mut books_deeper_than_3 = 0u64;
    let mut depth_all = Decimal::ZERO;
    let mut depth_top3 = Decimal::ZERO;

    for (hash, name) in &pins {
        let path = dir.join(name);
        let f = File::open(&path).unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
        let mut reader = BufReader::new(f);
        let mut hasher = Sha256::new();
        let mut line = String::new();
        let (mut spot, mut book, mut round, mut total) = (0u64, 0u64, 0u64, 0u64);
        loop {
            line.clear();
            let n = reader.read_line(&mut line).unwrap();
            if n == 0 {
                break;
            }
            hasher.update(line.as_bytes());
            let row = line.trim_end_matches(['\n', '\r']);
            if row.is_empty() {
                continue;
            }
            let v: Value =
                serde_json::from_str(row).unwrap_or_else(|e| panic!("{name}: bad row: {e}: {row}"));
            total += 1;
            match v.get("k").and_then(Value::as_str) {
                Some("spot") => spot += 1,
                Some("round") => round += 1,
                Some("book") => {
                    book += 1;
                    let mut deepest = 0usize;
                    for key in ["b", "a"] {
                        let Some(levels) = v.get(key).and_then(Value::as_array) else {
                            continue;
                        };
                        deepest = deepest.max(levels.len());
                        for (i, lv) in levels.iter().enumerate() {
                            let size = lv
                                .as_array()
                                .and_then(|p| p.get(1))
                                .and_then(Value::as_str)
                                .and_then(|s| Decimal::from_str_exact(s).ok())
                                .unwrap_or(Decimal::ZERO);
                            depth_all += size;
                            // The frozen transform keeps the LAST TOP_LEVELS
                            // rows of each side — the levels nearest the touch.
                            if levels.len() - i <= 3 {
                                depth_top3 += size;
                            }
                        }
                    }
                    if deepest > 3 {
                        books_deeper_than_3 += 1;
                    }
                }
                other => panic!("{name}: unexpected kind {other:?} in row {row}"),
            }
        }
        let got = hex(&hasher.finalize());
        assert_eq!(&got, hash, "{name}: bytes drifted from the pinned sha256");
        assert_eq!(
            spot + book + round,
            total,
            "{name}: every row is a known kind — nothing was silently dropped"
        );
        println!("{name}: rows {total} (spot {spot}, book {book}, round {round}) — sha256 OK");

        // The replay source's own counter must agree with the raw tally.
        let mut src = open_replay(&path.to_string_lossy())
            .unwrap_or_else(|e| panic!("replay-open {}: {e}", path.display()));
        while src.next_event().is_some() {}
        let st = src.stats();
        assert_eq!(st.events, total, "{name}: the replay sees every row");
        assert_eq!(
            st.spot_events, spot,
            "{name}: the replay spot counter matches the raw tally"
        );
        assert_eq!(st.malformed_lines, 0, "{name}: nothing skipped");

        spots += spot;
        books += book;
        rounds += round;
        total_rows += total;
    }

    println!();
    println!("corpus totals: {total_rows} rows (spot {spots}, book {books}, round {rounds})");
    assert!(spots > 0, "the corpus carries spot rows to reconcile");
    assert!(books > 0, "the corpus carries book rows to measure");
    let keep_pct = if depth_all.is_zero() {
        Decimal::ONE_HUNDRED
    } else {
        depth_top3 / depth_all * Decimal::ONE_HUNDRED
    };
    let deep_pct = if books == 0 {
        Decimal::ZERO
    } else {
        Decimal::from(books_deeper_than_3) / Decimal::from(books) * Decimal::ONE_HUNDRED
    };
    println!(
        "TOP_LEVELS=3 measurement: {books_deeper_than_3}/{books} book rows ({deep_pct:.1}%) are \
         deeper than 3 levels; the kept top-3-per-side hold {keep_pct:.1}% of all quoted depth"
    );
}

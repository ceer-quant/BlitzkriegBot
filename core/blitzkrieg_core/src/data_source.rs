//! Data abstraction (P-1.2/P-1.3): one market-data event stream shared by live
//! and backtest.
//!
//! Live path: every event the engine consumes passes through
//! [`Core::engine_on_data`](crate::service::Core::engine_on_data), which is the
//! single choke point for Book / TopOfBook / Spot / RoundMarkets. With
//! `--event-archive <path>` the core mirrors each event into an [`EventArchive`]
//! ([`DataSink`]) while it trades.
//!
//! Backtest path: [`ReplaySource`] ([`DataSource`]) reads that same archive back
//! in timestamp order and feeds it to the backtester, so both sides speak one
//! format and one event taxonomy.
//!
//! Archive format — JSONL, one event per line, Decimals as strings (exact, unlike
//! the f64-number convention used on the IPC wire):
//!
//! ```text
//! {"at":1757851200123,"k":"book","t":"<token>","b":[["0.42","100"]],"a":[["0.43","50"]]}
//! {"at":1757851200150,"k":"top","t":"<token>","bb":"0.42","ba":"0.43"}
//! {"at":1757851200200,"k":"spot","s":"BTC","p":"62850.12"}
//! {"at":1757851200000,"k":"round","m":[{...CryptoMarket camelCase...}]}
//! ```
//!
//! `at` is the event's own `now_ms` (venue time when available) — the same value
//! the live core stamped the event with, so a replay reconstructs the identical
//! decision clock.

use crate::engine::DataEvent;
use rust_decimal::Decimal;
use serde_json::{json, Value};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// One market-data event plus the timestamp it must be replayed at.
#[derive(Debug, Clone)]
pub struct TimedEvent {
    pub at_ms: i64,
    pub event: DataEvent,
}

/// Counters a source reports into a backtest report (zeros when a source has no
/// parser, e.g. an in-memory list).
#[derive(Debug, Clone, Default, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SourceStats {
    pub events: u64,
    /// Lines that could not be parsed (skipped, not fatal).
    pub malformed_lines: u64,
    /// Events whose timestamp went backwards relative to the previous event.
    pub out_of_order_events: u64,
}

/// Pull side: a timestamp-ordered supply of market-data events.
pub trait DataSource {
    /// Next event, or `None` when the source is exhausted.
    fn next_event(&mut self) -> Option<TimedEvent>;
    /// Human-readable provenance for logs/reports (path, counts, …).
    fn describe(&self) -> String;
    /// Parser/stream counters for the report.
    fn stats(&self) -> SourceStats {
        SourceStats::default()
    }
}

/// Push side: where market-data events are recorded (live → archive).
pub trait DataSink {
    fn record(&mut self, ev: &DataEvent);
    fn flush(&mut self);
    /// Human-readable status for logs/diagnostics.
    fn describe(&self) -> String;
}

/// The event's own timestamp (ms).
pub fn event_at_ms(ev: &DataEvent) -> i64 {
    match ev {
        DataEvent::Book { now_ms, .. }
        | DataEvent::TopOfBook { now_ms, .. }
        | DataEvent::Spot { now_ms, .. }
        | DataEvent::RoundMarkets { now_ms, .. } => *now_ms,
    }
}

/// The event's kind as it appears in the archive (`book`/`top`/`spot`/`round`).
pub fn event_kind(ev: &DataEvent) -> &'static str {
    match ev {
        DataEvent::Book { .. } => "book",
        DataEvent::TopOfBook { .. } => "top",
        DataEvent::Spot { .. } => "spot",
        DataEvent::RoundMarkets { .. } => "round",
    }
}

// ── encoding ────────────────────────────────────────────────────────────────

/// Encode one event as an archive line value (see the module docs for the schema).
pub fn event_to_json(ev: &DataEvent) -> Value {
    match ev {
        DataEvent::Book { token_id, bids, asks, now_ms } => json!({
            "at": now_ms,
            "k": "book",
            "t": token_id,
            "b": levels_json(bids),
            "a": levels_json(asks),
        }),
        DataEvent::TopOfBook { token_id, best_bid, best_ask, now_ms } => json!({
            "at": now_ms,
            "k": "top",
            "t": token_id,
            "bb": best_bid.map(|d| d.to_string()),
            "ba": best_ask.map(|d| d.to_string()),
        }),
        DataEvent::Spot { asset, price, now_ms } => json!({
            "at": now_ms,
            "k": "spot",
            "s": asset,
            "p": price.to_string(),
        }),
        DataEvent::RoundMarkets { markets, now_ms } => json!({
            "at": now_ms,
            "k": "round",
            // CryptoMarket already has the camelCase serde form used on the wire.
            "m": serde_json::to_value(markets).unwrap_or(Value::Null),
        }),
    }
}

fn levels_json(levels: &[(Decimal, Decimal)]) -> Value {
    Value::Array(
        levels
            .iter()
            .map(|(p, s)| {
                Value::Array(vec![Value::String(p.to_string()), Value::String(s.to_string())])
            })
            .collect(),
    )
}

/// Decode one archive line value back into a [`DataEvent`].
pub fn event_from_json(v: &Value) -> Result<DataEvent, String> {
    let now_ms = v
        .get("at")
        .and_then(Value::as_i64)
        .ok_or_else(|| "missing/invalid `at`".to_string())?;
    let kind = v.get("k").and_then(Value::as_str).ok_or_else(|| "missing `k`".to_string())?;
    match kind {
        "book" => Ok(DataEvent::Book {
            token_id: str_field(v, "t")?,
            bids: levels(v.get("b"))?,
            asks: levels(v.get("a"))?,
            now_ms,
        }),
        "top" => Ok(DataEvent::TopOfBook {
            token_id: str_field(v, "t")?,
            best_bid: opt_dec(v.get("bb"))?,
            best_ask: opt_dec(v.get("ba"))?,
            now_ms,
        }),
        "spot" => Ok(DataEvent::Spot {
            asset: str_field(v, "s")?,
            price: dec_field(v, "p")?,
            now_ms,
        }),
        "round" => {
            let m = v.get("m").ok_or_else(|| "missing `m`".to_string())?;
            let markets: Vec<crate::model::CryptoMarket> =
                serde_json::from_value(m.clone()).map_err(|e| format!("bad round markets: {e}"))?;
            Ok(DataEvent::RoundMarkets { markets, now_ms })
        }
        other => Err(format!("unknown event kind `{other}`")),
    }
}

fn str_field(v: &Value, key: &str) -> Result<String, String> {
    v.get(key).and_then(Value::as_str).map(str::to_string).ok_or_else(|| format!("missing `{key}`"))
}

fn dec_field(v: &Value, key: &str) -> Result<Decimal, String> {
    let raw = v.get(key).ok_or_else(|| format!("missing `{key}`"))?;
    dec_value(raw)
}

fn dec_value(v: &Value) -> Result<Decimal, String> {
    match v {
        Value::String(s) => Decimal::from_str_exact(s.trim()).map_err(|e| format!("bad decimal `{s}`: {e}")),
        Value::Number(n) => {
            Decimal::from_str_exact(&n.to_string()).map_err(|e| format!("bad decimal `{n}`: {e}"))
        }
        other => Err(format!("expected decimal, got {other}")),
    }
}

fn opt_dec(v: Option<&Value>) -> Result<Option<Decimal>, String> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(x) => dec_value(x).map(Some),
    }
}

fn levels(v: Option<&Value>) -> Result<Vec<(Decimal, Decimal)>, String> {
    let arr = match v {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Array(a)) => a,
        Some(other) => return Err(format!("expected level array, got {other}")),
    };
    let mut out = Vec::with_capacity(arr.len());
    for lvl in arr {
        let pair = lvl.as_array().ok_or_else(|| format!("expected [price,size], got {lvl}"))?;
        if pair.len() != 2 {
            return Err(format!("expected [price,size], got {lvl}"));
        }
        out.push((dec_value(&pair[0])?, dec_value(&pair[1])?));
    }
    Ok(out)
}

// ── archive writer (live → disk) ────────────────────────────────────────────

/// Append-only JSONL archive of the events the engine consumed.
///
/// `max_bytes == 0` means unlimited. When the cap is reached the archive stops
/// recording (counting `dropped`) instead of deleting or truncating anything —
/// an operator decides what to do with a full archive.
pub struct EventArchive {
    path: PathBuf,
    file: BufWriter<File>,
    max_bytes: u64,
    bytes: u64,
    events: u64,
    dropped: u64,
    stopped: bool,
    /// Caller clock (ms) of the last periodic flush, for `flush_if_due`.
    last_flush_ms: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveStatus {
    pub path: String,
    pub events: u64,
    pub bytes: u64,
    pub dropped: u64,
    pub recording: bool,
}

impl EventArchive {
    /// Open (append/create) an archive at `path`.
    pub fn open(path: &Path, max_bytes: u64) -> std::io::Result<Self> {
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)?;
            }
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let bytes = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self {
            path: path.to_path_buf(),
            file: BufWriter::new(file),
            max_bytes,
            bytes,
            events: 0,
            dropped: 0,
            stopped: false,
            last_flush_ms: i64::MIN / 2,
        })
    }

    pub fn status(&self) -> ArchiveStatus {
        ArchiveStatus {
            path: self.path.display().to_string(),
            events: self.events,
            bytes: self.bytes,
            dropped: self.dropped,
            recording: !self.stopped,
        }
    }

    /// Record `ev` at an explicit replay timestamp (`at`), overriding the event's
    /// own `now_ms`. The live core stamps this with the event's venue time (the
    /// clock the engine decided on); `now_ms` is only a fallback for a source
    /// that left the event timestamp unset.
    pub fn record_at(&mut self, at: i64, ev: &DataEvent) {
        let stamped = stamped_event(at, ev);
        self.record(&stamped);
    }

    /// Flush at most once per `interval_ms` of the caller's clock, so a reader of
    /// the live file (or an operator tailing it) sees events without waiting for
    /// the BufWriter to fill. Returns whether a flush happened.
    pub fn flush_if_due(&mut self, now_ms: i64, interval_ms: i64) -> bool {
        if self.stopped || self.events == 0 {
            return false;
        }
        if now_ms.saturating_sub(self.last_flush_ms) < interval_ms.max(0) {
            return false;
        }
        self.last_flush_ms = now_ms;
        if let Err(e) = self.file.flush() {
            tracing::warn!(error = %e, path = %self.path.display(), "event archive: flush failed");
        }
        true
    }
}

/// A copy of `ev` carrying `at` as its `now_ms`.
fn stamped_event(at: i64, ev: &DataEvent) -> DataEvent {
    let mut out = ev.clone();
    match &mut out {
        DataEvent::Book { now_ms, .. }
        | DataEvent::TopOfBook { now_ms, .. }
        | DataEvent::Spot { now_ms, .. }
        | DataEvent::RoundMarkets { now_ms, .. } => *now_ms = at,
    }
    out
}

impl DataSink for EventArchive {
    fn record(&mut self, ev: &DataEvent) {
        if self.stopped {
            self.dropped += 1;
            return;
        }
        let line = match serde_json::to_string(&event_to_json(ev)) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(error = %e, "event archive: encode failed; event dropped");
                self.dropped += 1;
                return;
            }
        };
        let n = line.len() as u64 + 1;
        if let Err(e) = self.file.write_all(line.as_bytes()).and_then(|_| self.file.write_all(b"\n")) {
            tracing::warn!(error = %e, path = %self.path.display(), "event archive: write failed; recording stopped");
            self.stopped = true;
            self.dropped += 1;
            return;
        }
        self.events += 1;
        self.bytes += n;
        if self.max_bytes > 0 && self.bytes >= self.max_bytes {
            let _ = self.file.flush();
            self.stopped = true;
            tracing::warn!(
                path = %self.path.display(),
                bytes = self.bytes,
                cap = self.max_bytes,
                "event archive cap reached — recording stopped (nothing deleted; start a new archive to continue)"
            );
        }
    }

    fn flush(&mut self) {
        if let Err(e) = self.file.flush() {
            tracing::warn!(error = %e, path = %self.path.display(), "event archive: flush failed");
        }
        self.last_flush_ms = i64::MIN / 2;
    }

    fn describe(&self) -> String {
        let s = self.status();
        format!(
            "archive {} ({} events, {} bytes{})",
            s.path,
            s.events,
            s.bytes,
            if s.recording { "" } else { ", recording stopped" }
        )
    }
}

// ── replay reader (disk → backtest) ─────────────────────────────────────────

/// Streams an [`EventArchive`] back in file order (which the live writer produced
/// in timestamp order).
///
/// Malformed lines are skipped and counted (one warn per run, not per line) so a
/// single corrupt tail cannot abort a long replay. Out-of-order timestamps are
/// counted and surfaced: the backtester clamps them to the current clock.
pub struct ReplaySource {
    path: PathBuf,
    reader: BufReader<File>,
    buf: String,
    line_no: u64,
    events: u64,
    skipped: u64,
    out_of_order: u64,
    last_at_ms: Option<i64>,
    warned: bool,
}

impl ReplaySource {
    pub fn open(path: &Path) -> std::io::Result<Self> {
        let file = File::open(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            reader: BufReader::new(file),
            buf: String::new(),
            line_no: 0,
            events: 0,
            skipped: 0,
            out_of_order: 0,
            last_at_ms: None,
            warned: false,
        })
    }

    pub fn skipped_lines(&self) -> u64 {
        self.skipped
    }
    pub fn out_of_order_events(&self) -> u64 {
        self.out_of_order
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl DataSource for ReplaySource {
    fn next_event(&mut self) -> Option<TimedEvent> {
        loop {
            self.buf.clear();
            match self.reader.read_line(&mut self.buf) {
                Ok(0) => return None,
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(error = %e, path = %self.path.display(), "replay source: read failed");
                    return None;
                }
            }
            self.line_no += 1;
            let line = self.buf.trim();
            if line.is_empty() {
                continue;
            }
            let value: Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(e) => {
                    self.skipped += 1;
                    if !self.warned {
                        tracing::warn!(line = self.line_no, error = %e, "replay source: skipping malformed lines");
                        self.warned = true;
                    }
                    continue;
                }
            };
            let event = match event_from_json(&value) {
                Ok(ev) => ev,
                Err(e) => {
                    self.skipped += 1;
                    if !self.warned {
                        tracing::warn!(line = self.line_no, error = %e, "replay source: skipping malformed lines");
                        self.warned = true;
                    }
                    continue;
                }
            };
            let at_ms = event_at_ms(&event);
            if let Some(prev) = self.last_at_ms {
                if at_ms < prev {
                    self.out_of_order += 1;
                }
            }
            self.last_at_ms = Some(at_ms.max(self.last_at_ms.unwrap_or(at_ms)));
            self.events += 1;
            return Some(TimedEvent { at_ms, event });
        }
    }

    fn describe(&self) -> String {
        // Streaming counters come from `stats()` after the run: a label printed
        // before the first read must not claim the archive is empty.
        format!("replay {}", self.path.display())
    }

    fn stats(&self) -> SourceStats {
        SourceStats {
            events: self.events,
            malformed_lines: self.skipped,
            out_of_order_events: self.out_of_order,
        }
    }
}

/// Convenience: open a replay source, mapping the io error to a message.
pub fn open_replay(path: &str) -> Result<ReplaySource, String> {
    ReplaySource::open(Path::new(path)).map_err(|e| format!("cannot open archive {path}: {e}"))
}

/// Parse a decimal the way the archive writes it (used by tests and tools).
pub fn parse_decimal(s: &str) -> Option<Decimal> {
    Decimal::from_str(s).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::CryptoMarket;
    use rust_decimal_macros::dec;

    fn market(now: i64) -> CryptoMarket {
        let slot = now / 1000 / 900;
        CryptoMarket {
            asset: "BTC".into(),
            condition_id: "cond".into(),
            question_id: "q".into(),
            up_token_id: "up".into(),
            down_token_id: "down".into(),
            up_price: dec!(0.6),
            down_price: dec!(0.4),
            expires_at_ms: (slot + 1) * 900 * 1000,
            round_slot: slot,
            neg_risk: true,
            question: "BTC up or down".into(),
        }
    }

    fn all_events(now: i64) -> Vec<DataEvent> {
        vec![
            DataEvent::RoundMarkets { markets: vec![market(now)], now_ms: now },
            DataEvent::Book {
                token_id: "up".into(),
                // Sizes with many digits: must survive the round trip exactly.
                bids: vec![(dec!(0.43), dec!(4.444444444444444444)), (dec!(0.42), dec!(100))],
                asks: vec![(dec!(0.45), dec!(12.5))],
                now_ms: now + 1,
            },
            DataEvent::TopOfBook {
                token_id: "up".into(),
                best_bid: Some(dec!(0.44)),
                best_ask: None,
                now_ms: now + 2,
            },
            DataEvent::Spot { asset: "BTC".into(), price: dec!(62850.123456789), now_ms: now + 3 },
        ]
    }

    fn eq(a: &DataEvent, b: &DataEvent) {
        match (a, b) {
            (
                DataEvent::Book { token_id: t1, bids: b1, asks: a1, now_ms: n1 },
                DataEvent::Book { token_id: t2, bids: b2, asks: a2, now_ms: n2 },
            ) => {
                assert_eq!((t1, b1, a1, n1), (t2, b2, a2, n2));
            }
            (
                DataEvent::TopOfBook { token_id: t1, best_bid: b1, best_ask: a1, now_ms: n1 },
                DataEvent::TopOfBook { token_id: t2, best_bid: b2, best_ask: a2, now_ms: n2 },
            ) => {
                assert_eq!((t1, b1, a1, n1), (t2, b2, a2, n2));
            }
            (
                DataEvent::Spot { asset: s1, price: p1, now_ms: n1 },
                DataEvent::Spot { asset: s2, price: p2, now_ms: n2 },
            ) => assert_eq!((s1, p1, n1), (s2, p2, n2)),
            (
                DataEvent::RoundMarkets { markets: m1, now_ms: n1 },
                DataEvent::RoundMarkets { markets: m2, now_ms: n2 },
            ) => assert_eq!((m1, n1), (m2, n2)),
            _ => panic!("kind mismatch: {a:?} vs {b:?}"),
        }
    }

    #[test]
    fn every_event_kind_round_trips_through_the_archive_format() {
        for ev in all_events(1_000_000) {
            let line = event_to_json(&ev);
            let back = event_from_json(&line).unwrap_or_else(|e| panic!("decode failed: {e}"));
            eq(&ev, &back);
        }
    }

    #[test]
    fn decimals_survive_round_trip_exactly() {
        let ev = DataEvent::Spot { asset: "BTC".into(), price: dec!(0.07), now_ms: 1 };
        let back = event_from_json(&event_to_json(&ev)).unwrap();
        match back {
            DataEvent::Spot { price, .. } => assert_eq!(price, dec!(0.07)),
            other => panic!("wrong kind: {other:?}"),
        }
        let ev = DataEvent::Book {
            token_id: "t".into(),
            bids: vec![(dec!(0.01), dec!(4.444444444444444444))],
            asks: vec![],
            now_ms: 1,
        };
        match event_from_json(&event_to_json(&ev)).unwrap() {
            DataEvent::Book { bids, .. } => assert_eq!(bids, vec![(dec!(0.01), dec!(4.444444444444444444))]),
            other => panic!("wrong kind: {other:?}"),
        }
    }

    #[test]
    fn archive_writes_and_replays_in_order() {
        let dir = std::env::temp_dir().join(format!("bk-archive-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("events.jsonl");
        let _ = std::fs::remove_file(&path);

        let events = all_events(1_000_000);
        {
            let mut a = EventArchive::open(&path, 0).unwrap();
            for ev in &events {
                a.record(ev);
            }
            a.flush();
            assert_eq!(a.status().events, events.len() as u64);
            assert!(a.status().recording);
        }

        let mut src = ReplaySource::open(&path).unwrap();
        let mut got = Vec::new();
        while let Some(te) = src.next_event() {
            assert_eq!(te.at_ms, event_at_ms(&te.event));
            got.push(te.event);
        }
        assert_eq!(got.len(), events.len());
        for (a, b) in events.iter().zip(got.iter()) {
            eq(a, b);
        }
        assert_eq!(src.out_of_order_events(), 0);
        assert_eq!(src.skipped_lines(), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_lines_are_skipped_and_counted() {
        let dir = std::env::temp_dir().join(format!("bk-archive-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("events.jsonl");
        std::fs::write(
            &path,
            "not json\n{\"at\":5,\"k\":\"spot\",\"s\":\"BTC\",\"p\":\"1.5\"}\n{\"k\":\"spot\"}\n{\"at\":6,\"k\":\"nope\"}\n",
        )
        .unwrap();

        let mut src = ReplaySource::open(&path).unwrap();
        let mut n = 0;
        while let Some(te) = src.next_event() {
            match te.event {
                DataEvent::Spot { price, .. } => assert_eq!(price, dec!(1.5)),
                other => panic!("wrong kind: {other:?}"),
            }
            n += 1;
        }
        assert_eq!(n, 1);
        assert_eq!(src.skipped_lines(), 3);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn out_of_order_events_are_counted_but_yielded() {
        let dir = std::env::temp_dir().join(format!("bk-archive-ooo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("events.jsonl");
        std::fs::write(
            &path,
            "{\"at\":10,\"k\":\"spot\",\"s\":\"BTC\",\"p\":\"1\"}\n{\"at\":5,\"k\":\"spot\",\"s\":\"BTC\",\"p\":\"2\"}\n",
        )
        .unwrap();

        let mut src = ReplaySource::open(&path).unwrap();
        assert_eq!(src.next_event().unwrap().at_ms, 10);
        assert_eq!(src.next_event().unwrap().at_ms, 5);
        assert_eq!(src.out_of_order_events(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn archive_cap_stops_recording_without_deleting() {
        let dir = std::env::temp_dir().join(format!("bk-archive-cap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("events.jsonl");
        let _ = std::fs::remove_file(&path);

        let ev = DataEvent::Spot { asset: "BTC".into(), price: dec!(1), now_ms: 1 };
        let mut a = EventArchive::open(&path, 60).unwrap();
        for _ in 0..50 {
            a.record(&ev);
        }
        let st = a.status();
        assert!(!st.recording, "cap must stop recording");
        assert!(st.events >= 1, "the events written before the cap are kept");
        assert!(st.dropped > 0, "after the cap, events are dropped (and counted)");
        // Nothing was deleted: the file still replays.
        let mut src = ReplaySource::open(&path).unwrap();
        let mut n = 0;
        while src.next_event().is_some() {
            n += 1;
        }
        assert_eq!(n as u64, st.events);

        let _ = std::fs::remove_dir_all(&dir);
    }
}

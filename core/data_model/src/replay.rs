//! The replay reader — moved VERBATIM from `blitzkrieg_core::data_source`
//! (the `DataSource` trait, `TimedEvent`, `SourceStats`, `ReplaySource`,
//! `SegmentSource`, `open_replay`, `open_replay_all`). Zero behaviour change;
//! the kernel re-exports these under the original paths. Only what the live
//! writer (`EventArchive`) does NOT need moved here: the archive writer stays
//! in the kernel — recording is a live-core concern, reading a stream is the
//! contract every consumer (backtester, offline tools) shares.

use crate::events::{DataEvent, event_at_ms, event_from_json};
use serde_json::Value;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

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
    /// #354 (5.2): Spot ticks the source parsed — the corpus-completeness
    /// anchor for the spot-driven strategies. A corpus that lost its spot rows
    /// reports a count far below the builder's tally instead of replaying
    /// "successfully" blind.
    pub spot_events: u64,
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

// ── replay reader (disk → backtest) ─────────────────────────────────────────

/// Streams an archive back in file order (which the live writer produced
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
    /// #354 (5.2): Spot ticks parsed — the completeness counter.
    spots: u64,
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
            spots: 0,
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
            if matches!(event, DataEvent::Spot { .. }) {
                self.spots += 1;
            }
            let at_ms = event_at_ms(&event);
            if let Some(prev) = self.last_at_ms
                && at_ms < prev
            {
                self.out_of_order += 1;
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
            spot_events: self.spots,
        }
    }
}

/// Convenience: open a replay source, mapping the io error to a message.
pub fn open_replay(path: &str) -> Result<ReplaySource, String> {
    ReplaySource::open(Path::new(path)).map_err(|e| format!("cannot open archive {path}: {e}"))
}

/// A 24/7 capture rotates, so a replay must be able to read a whole directory of
/// segments in time order rather than one file. Segments are the archive plus its
/// rotated siblings (`events.jsonl` + `events.<UTC>.jsonl`); ordering is by the
/// UTC stamp in the name, which is also the order they were written in.
pub struct SegmentSource {
    sources: Vec<ReplaySource>,
    current: usize,
    events: u64,
    skipped: u64,
    out_of_order: u64,
    /// #354 (5.2): Spot ticks folded in from exhausted segments.
    spots: u64,
    last_at_ms: Option<i64>,
    label: String,
}

impl SegmentSource {
    /// Open every `.jsonl` segment that belongs to `path` (the archive itself plus
    /// its UTC-stamped siblings), in write order. Returns an error when nothing
    /// matched, so a typo in the path is still reported.
    pub fn open_dir(path: &Path) -> Result<Self, String> {
        let dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "events".into());
        let live = path.file_name().map(|s| s.to_string_lossy().to_string());

        let mut names: Vec<String> = Vec::new();
        let entries = std::fs::read_dir(dir)
            .map_err(|e| format!("cannot read archive dir {}: {e}", dir.display()))?;
        for e in entries.flatten() {
            let Some(name) = e.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if !name.ends_with(".jsonl") {
                continue;
            }
            // Belongs to this archive when it is the archive itself, or a rotated
            // sibling in the exact shape the writer produces (`stem.<stamp>[-N].jsonl`).
            // A loose prefix rule would swallow sibling datasets like the #352
            // onchain `<stem>.trades.jsonl` and count every raw line malformed.
            let is_mine = Some(&name) == live.as_ref() || is_rotated_segment(&stem, &name);
            if is_mine {
                names.push(name);
            }
        }
        if names.is_empty() {
            return Err(format!(
                "no archive segments matching {} in {}",
                path.display(),
                dir.display()
            ));
        }
        // Order by write time, derived from the name, not by raw bytes: a rotated
        // segment is `stem.<stamp>[-<seq>].jsonl` and the live path is `stem.jsonl`.
        // Raw lexicographic order would put `-0002` before `.jsonl` and the live
        // path in the middle — replay would then mix different instants together.
        names.sort_by(|a, b| {
            segment_sort_key(&stem, a, live.as_deref()).cmp(&segment_sort_key(
                &stem,
                b,
                live.as_deref(),
            ))
        });

        let mut sources = Vec::with_capacity(names.len());
        for name in &names {
            let p = dir.join(name);
            sources.push(
                ReplaySource::open(&p)
                    .map_err(|e| format!("cannot open segment {}: {e}", p.display()))?,
            );
        }
        let label = format!("{} ({} segment(s))", path.display(), sources.len());
        Ok(Self {
            sources,
            current: 0,
            events: 0,
            skipped: 0,
            out_of_order: 0,
            spots: 0,
            last_at_ms: None,
            label,
        })
    }

    pub fn segments(&self) -> usize {
        self.sources.len()
    }
}

impl DataSource for SegmentSource {
    fn next_event(&mut self) -> Option<TimedEvent> {
        loop {
            let src = self.sources.get_mut(self.current)?;
            if let Some(te) = src.next_event() {
                // Carry the counters and the cross-segment clock clamp forward, so
                // a boundary that goes backwards is still reported like any other
                // out-of-order arrival.
                let at_ms = te.at_ms;
                if let Some(prev) = self.last_at_ms
                    && at_ms < prev
                {
                    self.out_of_order += 1;
                }
                self.last_at_ms = Some(at_ms.max(self.last_at_ms.unwrap_or(at_ms)));
                self.events += 1;
                return Some(te);
            }
            // Exhausted: fold in this segment's parse counters and move on.
            self.skipped += src.skipped_lines();
            self.spots += src.stats().spot_events;
            self.current += 1;
        }
    }

    fn describe(&self) -> String {
        format!("replay {}", self.label)
    }

    fn stats(&self) -> SourceStats {
        SourceStats {
            events: self.events,
            malformed_lines: self.skipped,
            out_of_order_events: self.out_of_order,
            spot_events: self.spots,
        }
    }
}

/// Open a replay that covers an archive and (when present) its rotated segments.
/// A single-file archive and a directory of segments both work; the returned
/// source reports aggregate counters either way.
pub fn open_replay_all(path: &str) -> Result<SegmentSource, String> {
    SegmentSource::open_dir(Path::new(path))
}

/// Does `name` match the exact shape `next_segment_path` produces —
/// `{stem}.{UTC-stamp|ms}[-NNNN].jsonl`? The old prefix-only rule
/// (`{stem}.` + any tail) swallowed sibling datasets like
/// `<stem>.trades.jsonl` (#352: the onchain raw-fills side file) as
/// phantom replay segments, so every raw line was counted malformed.
fn is_rotated_segment(stem: &str, name: &str) -> bool {
    let Some(body) = name
        .strip_prefix(&format!("{stem}."))
        .and_then(|b| b.strip_suffix(".jsonl"))
    else {
        return false;
    };
    let body = match body.rsplit_once('-') {
        // Trailing `-NNNN` is the rotate-clobber disambiguator.
        Some((base, seq)) if !seq.is_empty() && seq.chars().all(|c| c.is_ascii_digit()) => base,
        _ => body,
    };
    // The timestamp itself is a fixed-width UTC stamp (`YYYYMMDDTHHMMSSZ`:
    // digits with one `T` at index 8 and a trailing `Z`) or the numeric
    // fallback (venue ms). Anything else is a different dataset, not a
    // segment of this one.
    if !body.is_empty() && body.chars().all(|c| c.is_ascii_digit()) {
        return true;
    }
    body.len() == 16
        && body.as_bytes()[8] == b'T'
        && body.ends_with('Z')
        && body
            .char_indices()
            .all(|(i, c)| c.is_ascii_digit() || i == 8 || i == 15)
}

/// Sort key for an archive segment name: rotated segments first (in write order),
/// the live path last (it is the segment still being written). Within the rotated
/// group, `base` is the timestamp in the name — a fixed-width UTC stamp, or the
/// numeric fallback — so comparing it as text still matches chronological order.
fn segment_sort_key(stem: &str, name: &str, live: Option<&str>) -> (u8, String, u32) {
    if live == Some(name) {
        return (1, String::new(), 0);
    }
    let body = name
        .strip_prefix(&format!("{stem}."))
        .unwrap_or(name)
        .strip_suffix(".jsonl")
        .unwrap_or(name);
    match body.rsplit_once('-') {
        // `20250914T120000Z-0002` → ("20250914T120000Z", 2)
        Some((base, seq)) if !seq.is_empty() && seq.chars().all(|c| c.is_ascii_digit()) => {
            (0, base.to_string(), seq.parse().unwrap_or(0))
        }
        _ => (0, body.to_string(), 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::CashflowKind;
    use rust_decimal_macros::dec;

    fn all_events(now: i64) -> Vec<DataEvent> {
        vec![
            DataEvent::RoundMarkets {
                markets: vec![round_market(now)],
                now_ms: now,
            },
            DataEvent::Book {
                token_id: "up".into(),
                // Sizes with many digits: must survive the round trip exactly.
                bids: vec![
                    (dec!(0.43), dec!(4.444444444444444444)),
                    (dec!(0.42), dec!(100)),
                ],
                asks: vec![(dec!(0.45), dec!(12.5))],
                now_ms: now + 1,
            },
            DataEvent::TopOfBook {
                token_id: "up".into(),
                best_bid: Some(dec!(0.44)),
                best_ask: None,
                now_ms: now + 2,
            },
            DataEvent::Spot {
                asset: "BTC".into(),
                price: dec!(62850.123456789),
                now_ms: now + 3,
            },
            DataEvent::Resolution {
                condition_id: "0xcond".into(),
                // A payout with many digits must survive the round trip
                // exactly, like every other decimal here.
                payouts: vec![("up".into(), dec!(1)), ("down".into(), dec!(0.000000001))],
                neg_risk: true,
                now_ms: now + 4,
            },
            DataEvent::Cashflow {
                which: CashflowKind::Rebate,
                // A real MAKER_REBATE row's many-digit amount must survive the
                // round trip exactly (#388).
                amount_usd: dec!(331.2175),
                now_ms: now + 5,
            },
            DataEvent::Cashflow {
                which: CashflowKind::Reward,
                amount_usd: dec!(659.995),
                now_ms: now + 6,
            },
            DataEvent::UnifiedMapping {
                event: unified_event_fixture(),
                now_ms: now + 7,
            },
            // #548：批路径归档恢复后 `k:"books"` 行开始出现 —— 往返必须有
            // 覆盖（此前 all_events 不含它，eq() 落 kind-mismatch panic）。
            DataEvent::Books {
                books: vec![
                    (
                        "batch-up".into(),
                        vec![(dec!(0.43), dec!(4.444444444444444444))],
                        vec![(dec!(0.45), dec!(12.5))],
                    ),
                    ("batch-down".into(), vec![], vec![]),
                ],
                now_ms: now + 8,
            },
        ]
    }

    /// The kernel's `test_fixtures::round_market`, re-spelled locally so the
    /// leaf crate stays leaf (no dependency on the kernel's test support).
    fn round_market(now: i64) -> crate::market::CryptoMarket {
        crate::market::CryptoMarket {
            asset: "BTC".into(),
            condition_id: "0xcond".into(),
            question_id: "0xq".into(),
            up_token_id: "up".into(),
            down_token_id: "down".into(),
            up_price: dec!(0.55),
            down_price: dec!(0.45),
            expires_at_ms: now + 900_000,
            round_slot: now / 900_000,
            round_duration_sec: 900,
            archive_verdict: false,
            venue: String::new(),
            neg_risk: false,
            question: "Bitcoin Up or Down".into(),
        }
    }

    /// One operator-confirmed cross-venue mapping (#425 model) as the fixture
    /// for the archive round trip.
    fn unified_event_fixture() -> blitzkrieg_market_api::unified::UnifiedEvent {
        use blitzkrieg_market_api::unified::{
            ListingStatus, PlatformListing, SettlementSource, UnifiedEvent, Venue,
        };
        let listing = |venue: Venue, cond: &str| PlatformListing {
            venue,
            condition_id: cond.into(),
            up_token_id: format!("{cond}-up"),
            down_token_id: format!("{cond}-down"),
            asset: Some("bitcoin".into()),
            title: "Bitcoin Up or Down".into(),
            settlement_rules: "Binance 1m candle".into(),
            settlement_source: SettlementSource::Chainlink,
            expires_at_ms: 1_000_000,
            status: ListingStatus::Active,
        };
        let mut e = UnifiedEvent {
            id: "evt-1".into(),
            title: "Bitcoin Up or Down".into(),
            settlement_rules: "Binance 1m candle".into(),
            settlement_source: SettlementSource::Chainlink,
            listings: Default::default(),
            discrepancies: Vec::new(),
            status: blitzkrieg_market_api::unified::EventStatus::Paired,
        };
        e.listings
            .insert(Venue::Polymarket, listing(Venue::Polymarket, "0xpm"));
        e.listings
            .insert(Venue::Kalshi, listing(Venue::Kalshi, "kx-1"));
        e
    }

    fn eq(a: &DataEvent, b: &DataEvent) {
        match (a, b) {
            (
                DataEvent::Book {
                    token_id: t1,
                    bids: b1,
                    asks: a1,
                    now_ms: n1,
                },
                DataEvent::Book {
                    token_id: t2,
                    bids: b2,
                    asks: a2,
                    now_ms: n2,
                },
            ) => {
                assert_eq!((t1, b1, a1, n1), (t2, b2, a2, n2));
            }
            (
                DataEvent::TopOfBook {
                    token_id: t1,
                    best_bid: b1,
                    best_ask: a1,
                    now_ms: n1,
                },
                DataEvent::TopOfBook {
                    token_id: t2,
                    best_bid: b2,
                    best_ask: a2,
                    now_ms: n2,
                },
            ) => {
                assert_eq!((t1, b1, a1, n1), (t2, b2, a2, n2));
            }
            (
                DataEvent::Spot {
                    asset: s1,
                    price: p1,
                    now_ms: n1,
                },
                DataEvent::Spot {
                    asset: s2,
                    price: p2,
                    now_ms: n2,
                },
            ) => assert_eq!((s1, p1, n1), (s2, p2, n2)),
            (
                DataEvent::RoundMarkets {
                    markets: m1,
                    now_ms: n1,
                },
                DataEvent::RoundMarkets {
                    markets: m2,
                    now_ms: n2,
                },
            ) => assert_eq!((m1, n1), (m2, n2)),
            (
                DataEvent::Resolution {
                    condition_id: c1,
                    payouts: p1,
                    neg_risk: g1,
                    now_ms: n1,
                },
                DataEvent::Resolution {
                    condition_id: c2,
                    payouts: p2,
                    neg_risk: g2,
                    now_ms: n2,
                },
            ) => assert_eq!((c1, p1, g1, n1), (c2, p2, g2, n2)),
            (
                DataEvent::Cashflow {
                    which: w1,
                    amount_usd: a1,
                    now_ms: n1,
                },
                DataEvent::Cashflow {
                    which: w2,
                    amount_usd: a2,
                    now_ms: n2,
                },
            ) => assert_eq!((w1, a1, n1), (w2, a2, n2)),
            (
                DataEvent::UnifiedMapping {
                    event: e1,
                    now_ms: n1,
                },
                DataEvent::UnifiedMapping {
                    event: e2,
                    now_ms: n2,
                },
            ) => assert_eq!((e1, n1), (e2, n2)),
            (
                DataEvent::Books {
                    books: b1,
                    now_ms: n1,
                },
                DataEvent::Books {
                    books: b2,
                    now_ms: n2,
                },
            ) => assert_eq!((b1, n1), (b2, n2)),
            _ => panic!("kind mismatch: {a:?} vs {b:?}"),
        }
    }

    #[test]
    fn every_event_kind_round_trips_through_the_archive_format() {
        for ev in all_events(1_000_000) {
            let line = crate::events::event_to_json(&ev);
            let back = crate::events::event_from_json(&line)
                .unwrap_or_else(|e| panic!("decode failed: {e}"));
            eq(&ev, &back);
        }
    }

    #[test]
    fn decimals_survive_round_trip_exactly() {
        let ev = DataEvent::Spot {
            asset: "BTC".into(),
            price: dec!(0.07),
            now_ms: 1,
        };
        let back = crate::events::event_from_json(&crate::events::event_to_json(&ev)).unwrap();
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
        match crate::events::event_from_json(&crate::events::event_to_json(&ev)).unwrap() {
            DataEvent::Book { bids, .. } => {
                assert_eq!(bids, vec![(dec!(0.01), dec!(4.444444444444444444))])
            }
            other => panic!("wrong kind: {other:?}"),
        }
    }

    #[test]
    fn replay_writes_and_reads_in_order() {
        let dir = std::env::temp_dir().join(format!("bk-dm-replay-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("events.jsonl");
        let _ = std::fs::remove_file(&path);

        let events = all_events(1_000_000);
        {
            use std::io::Write as _;
            let mut w = std::io::BufWriter::new(File::create(&path).unwrap());
            for ev in &events {
                serde_json::to_writer(&mut w, &crate::events::event_to_json(ev)).unwrap();
                w.write_all(b"\n").unwrap();
            }
            w.flush().unwrap();
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
        assert_eq!(
            src.stats().spot_events,
            1,
            "the one Spot of all_events is tallied"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_dir_does_not_swallow_sibling_datasets_as_segments() {
        let dir = std::env::temp_dir().join(format!("bk-dm-sib-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("events.jsonl");
        std::fs::write(
            &path,
            "{\"at\":10,\"k\":\"spot\",\"s\":\"BTC\",\"p\":\"1\"}\n",
        )
        .unwrap();
        // The #352 onchain layout: a raw-fills side file sharing the stem. Its
        // lines are venue JSON without an `at` — they must never be read as a
        // replay segment (the old prefix rule counted every one malformed).
        std::fs::write(
            dir.join("events.trades.jsonl"),
            "{\"transactionHash\":\"0x1\"}\n{\"transactionHash\":\"0x2\"}\n",
        )
        .unwrap();
        // A genuine rotated sibling must still be picked up.
        std::fs::write(
            dir.join("events.20250914T120000Z.jsonl"),
            "{\"at\":5,\"k\":\"spot\",\"s\":\"BTC\",\"p\":\"2\"}\n",
        )
        .unwrap();

        let mut src = SegmentSource::open_dir(&path).unwrap();
        assert_eq!(src.segments(), 2, "live + the real rotated segment only");
        let mut n = 0;
        let mut last = 0i64;
        while let Some(te) = src.next_event() {
            last = te.at_ms;
            n += 1;
        }
        assert_eq!(n, 2, "every archive event, and nothing else");
        let st = src.stats();
        assert_eq!(
            st.malformed_lines, 0,
            "sibling dataset lines must not be read"
        );
        assert_eq!(st.out_of_order_events, 0);
        assert_eq!(
            st.spot_events, 2,
            "the spot rows of BOTH segments are folded into the tally"
        );
        assert_eq!(last, 10, "rotated segment first (older), live last");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_lines_are_skipped_and_counted() {
        let dir = std::env::temp_dir().join(format!("bk-dm-bad-{}", std::process::id()));
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
        let dir = std::env::temp_dir().join(format!("bk-dm-ooo-{}", std::process::id()));
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

    /// #354 (5.2): a corpus that lost its spot rows does not replay silently —
    /// the source's own tally is the difference the reconciliation catches.
    #[test]
    fn a_corpus_that_dropped_its_spot_rows_is_visible_in_the_tally() {
        let dir = std::env::temp_dir().join(format!("bk-dm-drops-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // The full shape: book + spot interleaved.
        std::fs::write(
            dir.join("full.jsonl"),
            "{\"at\":1,\"k\":\"book\",\"t\":\"tok\",\"b\":[[\"0.4\",\"10\"]],\"a\":[[\"0.5\",\"10\"]]}\n\
             {\"at\":2,\"k\":\"spot\",\"s\":\"BTC\",\"p\":\"60000\"}\n\
             {\"at\":3,\"k\":\"spot\",\"s\":\"BTC\",\"p\":\"60001\"}\n",
        )
        .unwrap();
        // The stripped shape the completeness gate must catch: same books, the
        // spots gone.
        std::fs::write(
            dir.join("stripped.jsonl"),
            "{\"at\":1,\"k\":\"book\",\"t\":\"tok\",\"b\":[[\"0.4\",\"10\"]],\"a\":[[\"0.5\",\"10\"]]}\n",
        )
        .unwrap();

        let mut full = ReplaySource::open(&dir.join("full.jsonl")).unwrap();
        while full.next_event().is_some() {}
        assert_eq!(full.stats().spot_events, 2);

        let mut stripped = ReplaySource::open(&dir.join("stripped.jsonl")).unwrap();
        while stripped.next_event().is_some() {}
        let st = stripped.stats();
        assert_eq!(st.events, 1, "the book survived");
        assert_eq!(
            st.spot_events, 0,
            "the dropped spot rows show as zero, not as success"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn segment_source_reads_every_rotated_segment_in_order() {
        let dir = std::env::temp_dir().join(format!("bk-dm-segments-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("events.jsonl");
        // Three segments written in chronological order: two rotated (UTC-stamped
        // like the live writer names them) and the live path last.
        std::fs::write(
            dir.join("events.20250914T120000Z.jsonl"),
            "{\"at\":1,\"k\":\"spot\",\"s\":\"BTC\",\"p\":\"1\"}\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("events.20250914T120001Z.jsonl"),
            "{\"at\":2,\"k\":\"spot\",\"s\":\"BTC\",\"p\":\"2\"}\n",
        )
        .unwrap();
        std::fs::write(
            &path,
            "{\"at\":3,\"k\":\"spot\",\"s\":\"BTC\",\"p\":\"3\"}\n",
        )
        .unwrap();

        let mut src = SegmentSource::open_dir(&path).unwrap();
        assert_eq!(src.segments(), 3, "live path + rotated siblings");
        let mut times = Vec::new();
        while let Some(te) = src.next_event() {
            times.push(te.at_ms);
        }
        assert_eq!(
            times,
            vec![1, 2, 3],
            "segments are read in write (time) order"
        );
        assert_eq!(src.stats().malformed_lines, 0);

        let _ = std::fs::remove_dir_all(&dir);
    }
}

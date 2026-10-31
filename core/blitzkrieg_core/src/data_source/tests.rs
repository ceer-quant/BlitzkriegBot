use super::*;
use crate::test_fixtures::round_market;
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
            which: crate::engine::CashflowKind::Rebate,
            // A real MAKER_REBATE row's many-digit amount must survive the
            // round trip exactly (#388).
            amount_usd: dec!(331.2175),
            now_ms: now + 5,
        },
        DataEvent::Cashflow {
            which: crate::engine::CashflowKind::Reward,
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
        let line = event_to_json(&ev);
        let back = event_from_json(&line).unwrap_or_else(|e| panic!("decode failed: {e}"));
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
        DataEvent::Book { bids, .. } => {
            assert_eq!(bids, vec![(dec!(0.01), dec!(4.444444444444444444))])
        }
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
    assert_eq!(
        src.stats().spot_events,
        1,
        "the one Spot of all_events is tallied"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn open_dir_does_not_swallow_sibling_datasets_as_segments() {
    let dir = std::env::temp_dir().join(format!("bk-archive-sib-{}", std::process::id()));
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

/// #354 (5.2): a corpus that lost its spot rows does not replay silently —
/// the source's own tally is the difference the reconciliation catches.
/// RED on the pre-#354 shape, where the count did not exist and a stripped
/// corpus replayed "successfully" blind.
#[test]
fn a_corpus_that_dropped_its_spot_rows_is_visible_in_the_tally() {
    let dir = std::env::temp_dir().join(format!("bk-archive-drops-{}", std::process::id()));
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
fn archive_rotates_segments_without_losing_events() {
    let dir = std::env::temp_dir().join(format!("bk-archive-rot-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("events.jsonl");
    let _ = std::fs::remove_file(&path);

    // 200 spot events, ~40 bytes each → rotate well before the session cap.
    let total = 200u64;
    let mut a = EventArchive::open_with_rotation(&path, 1_000_000, 500).unwrap();
    for i in 0..total {
        // Distinct venue timestamps so segment names are deterministic.
        let ev = DataEvent::Spot {
            asset: "BTC".into(),
            price: dec!(1),
            now_ms: 1_757_851_200_000 + i as i64 * 1000,
        };
        a.record_at(event_at_ms(&ev), &ev);
    }
    a.flush();
    let st = a.status();
    assert!(st.recording, "rotation must not stop recording");
    assert!(
        st.segments >= 2,
        "expected several segments, got {}",
        st.segments
    );
    assert_eq!(st.events, total, "session counter spans all segments");
    assert_eq!(st.dropped, 0, "rotation must not drop events");

    // Every event is still readable: the live path plus every rotated sibling.
    let mut segments: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "jsonl").unwrap_or(false))
        .collect();
    segments.sort();
    assert_eq!(
        segments.len() as u64,
        st.segments + 1,
        "segments found: {segments:?}"
    );
    let mut seen = 0u64;
    for seg in &segments {
        let mut src = ReplaySource::open(seg).unwrap();
        while src.next_event().is_some() {
            seen += 1;
        }
        assert_eq!(src.skipped_lines(), 0, "corrupt segment {}", seg.display());
    }
    assert_eq!(
        seen, total,
        "replaying all segments must yield every event exactly once"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn disk_guard_stops_before_the_volume_fills() {
    let dir = std::env::temp_dir().join(format!("bk-archive-disk-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("events.jsonl");
    let _ = std::fs::remove_file(&path);

    // A floor larger than any real volume: the first rotation must stop us.
    let mut a = EventArchive::open_full(&path, 0, 200, u64::MAX / 2).unwrap();
    for i in 0..200 {
        let ev = DataEvent::Spot {
            asset: "BTC".into(),
            price: dec!(1),
            now_ms: 1_757_851_200_000 + i,
        };
        a.record_at(event_at_ms(&ev), &ev);
    }
    let st = a.status();
    assert!(!st.recording, "guard must stop recording, not just warn");
    assert_eq!(st.stopped_reason.as_deref(), Some("disk"));
    assert!(
        st.dropped > 0,
        "events after the stop are counted as dropped"
    );

    // Nothing was deleted: every event written before the stop still replays,
    // across the rotated segment and the (now idle) live path.
    let mut segments: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "jsonl").unwrap_or(false))
        .collect();
    segments.sort();
    let mut n = 0u64;
    for seg in &segments {
        let mut src = ReplaySource::open(seg).unwrap();
        while src.next_event().is_some() {
            n += 1;
        }
    }
    assert!(
        n > 0,
        "the segments written before the stop are intact: {segments:?}"
    );
    assert!(n <= st.events, "replay cannot exceed what was recorded");
    assert_eq!(n, st.events, "every recorded event is still readable");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn segment_source_reads_every_rotated_segment_in_order() {
    let dir = std::env::temp_dir().join(format!("bk-segments-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("events.jsonl");
    for e in std::fs::read_dir(&dir).unwrap().flatten() {
        let _ = std::fs::remove_file(e.path());
    }

    let total = 150u64;
    let mut a = EventArchive::open_with_rotation(&path, 0, 400).unwrap();
    for i in 0..total {
        let ev = DataEvent::Spot {
            asset: "BTC".into(),
            price: dec!(1),
            now_ms: 1_757_851_200_000 + i as i64,
        };
        a.record_at(event_at_ms(&ev), &ev);
    }
    a.flush();
    assert!(a.status().segments >= 2);

    let mut src = SegmentSource::open_dir(&path).unwrap();
    assert!(src.segments() >= 3, "live path + rotated siblings");
    let mut times = Vec::new();
    while let Some(te) = src.next_event() {
        times.push(te.at_ms);
    }
    assert_eq!(
        times.len() as u64,
        total,
        "every event across every segment is replayed"
    );
    let mut sorted = times.clone();
    sorted.sort();
    assert_eq!(times, sorted, "segments are read in write (time) order");
    assert_eq!(src.stats().malformed_lines, 0);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn segment_source_ignores_unrelated_files_and_reports_a_typo() {
    let dir = std::env::temp_dir().join(format!("bk-segments-mix-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("events.jsonl");
    for e in std::fs::read_dir(&dir).unwrap().flatten() {
        let _ = std::fs::remove_file(e.path());
    }
    // Our archive, a rotated sibling, and two files that are NOT ours.
    std::fs::write(
        &path,
        "{\"at\":1,\"k\":\"spot\",\"s\":\"BTC\",\"p\":\"1\"}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("events.20250914T120000Z.jsonl"),
        "{\"at\":2,\"k\":\"spot\",\"s\":\"BTC\",\"p\":\"2\"}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("other.jsonl"),
        "{\"at\":3,\"k\":\"spot\",\"s\":\"BTC\",\"p\":\"3\"}\n",
    )
    .unwrap();
    std::fs::write(dir.join("notes.txt"), "not an archive\n").unwrap();

    let mut src = SegmentSource::open_dir(&path).unwrap();
    assert_eq!(src.segments(), 2, "only our two segments");
    let mut n = 0;
    while src.next_event().is_some() {
        n += 1;
    }
    assert_eq!(n, 2);

    assert!(
        SegmentSource::open_dir(&dir.join("nope.jsonl")).is_err(),
        "a typo must not silently replay nothing"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn segment_names_are_utc_stamped() {
    // 2025-09-14T12:00:00Z
    let name = next_segment_path(Path::new("/tmp/events.jsonl"), 1_757_851_200_000);
    assert_eq!(
        name.file_name().unwrap().to_string_lossy(),
        "events.20250914T120000Z.jsonl"
    );
    // A pre-epoch / unset timestamp still yields a usable distinct name.
    let fallback = next_segment_path(Path::new("/tmp/events.jsonl"), 0);
    assert_ne!(
        fallback.file_name().unwrap().to_string_lossy(),
        "events.jsonl"
    );
}

#[test]
fn archive_cap_stops_recording_without_deleting() {
    let dir = std::env::temp_dir().join(format!("bk-archive-cap-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("events.jsonl");
    let _ = std::fs::remove_file(&path);

    let ev = DataEvent::Spot {
        asset: "BTC".into(),
        price: dec!(1),
        now_ms: 1,
    };
    let mut a = EventArchive::open(&path, 60).unwrap();
    for _ in 0..50 {
        a.record(&ev);
    }
    let st = a.status();
    assert!(!st.recording, "cap must stop recording");
    assert!(st.events >= 1, "the events written before the cap are kept");
    assert!(
        st.dropped > 0,
        "after the cap, events are dropped (and counted)"
    );
    // Nothing was deleted: the file still replays.
    let mut src = ReplaySource::open(&path).unwrap();
    let mut n = 0;
    while src.next_event().is_some() {
        n += 1;
    }
    assert_eq!(n as u64, st.events);

    let _ = std::fs::remove_dir_all(&dir);
}

/// Two cores pointed at one archive would interleave lines and rotate the file
/// out from under each other. The second opener must lose cleanly (recording
/// off, reason visible) instead of corrupting the stream.
#[test]
fn a_second_writer_cannot_share_the_archive() {
    let dir = std::env::temp_dir().join(format!("bk-archive-lock-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("events.jsonl");
    let _ = std::fs::remove_file(&path);

    let ev = DataEvent::Spot {
        asset: "BTC".into(),
        price: dec!(1),
        now_ms: 1,
    };
    let mut first = EventArchive::open(&path, 0).unwrap();
    assert!(
        first.status().recording,
        "the first writer owns the archive"
    );
    first.record(&ev);

    let mut second = EventArchive::open(&path, 0).unwrap();
    let st = second.status();
    assert!(!st.recording, "the second writer must not record");
    assert_eq!(st.stopped_reason.as_deref(), Some("locked"));
    second.record(&ev);
    assert_eq!(second.status().events, 0, "the loser writes nothing");

    // Dropping the owner releases the lock for the next writer.
    drop(first);
    let third = EventArchive::open(&path, 0).unwrap();
    assert!(
        third.status().recording,
        "the lock is released with the owner"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

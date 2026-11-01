//! DataEvent JSONL codec goldens — issue #840 PR0.
//!
//! The archive's line codec (`event_to_json` / `event_from_json`) is
//! hand-written (not serde derive), so "the wire" here is these functions'
//! exact output. Codex ruling #6555067033/ⓒ: the codec freezes byte-for-byte
//! — decimal-as-string everywhere, unknown kind fail-closed, the compact
//! `k`/`at`/`t`/`b`/`a` key spellings.
//!
//! One canonical golden line per event kind (all ten), plus the fail-closed
//! and round-trip contracts. After Wave 2 moves `DataEvent` into
//! `bk_contract`, the same goldens must pass through the re-exports.

use blitzkrieg_data_model::{
    CashflowKind, CryptoMarket, DataEvent, event_at_ms, event_from_json, event_to_json,
};
use blitzkrieg_market_api::unified::{
    EventStatus, PlatformListing, SettlementSource, UnifiedEvent, Venue,
};
use rust_decimal::Decimal;
use std::str::FromStr;

/// Fixture timestamp — fixed, never `now`.
const TS: i64 = 1_000_000_000_000;

fn d(s: &str) -> Decimal {
    Decimal::from_str(s).expect("fixture decimal")
}

fn line(ev: &DataEvent) -> String {
    serde_json::to_string(&event_to_json(ev)).expect("event encodes")
}

fn market() -> CryptoMarket {
    CryptoMarket {
        asset: "BTC".into(),
        condition_id: "cond-1".into(),
        question_id: "q-1".into(),
        up_token_id: "tok-up".into(),
        down_token_id: "tok-down".into(),
        up_price: d("0.55"),
        down_price: d("0.45"),
        expires_at_ms: TS,
        round_slot: 7,
        round_duration_sec: 300,
        archive_verdict: false,
        venue: "polymarket".into(),
        neg_risk: false,
        question: "BTC up or down?".into(),
    }
}

#[test]
fn every_kind_has_a_frozen_golden_line() {
    // NOTE ON KEY ORDER: `event_to_json` builds `serde_json::json!` objects,
    // and without serde_json's `preserve_order` feature its Map is a
    // BTreeMap — the archive line's keys ride in ALPHABETICAL order. That is
    // the byte format being frozen here. (Struct payloads embedded via
    // serde_json::to_value — `m` for CryptoMarket, `e` for UnifiedEvent —
    // keep declaration order instead.)

    // k:"book" — levels are string-decimal pairs.
    let book = DataEvent::Book {
        token_id: "tok-up".into(),
        bids: vec![(d("0.55"), d("10.5"))],
        asks: vec![(d("0.56"), d("20"))],
        now_ms: TS,
    };
    assert_eq!(
        line(&book),
        concat!(
            r#"{"a":[["0.56","20"]],"at":1000000000000,"b":[["0.55","10.5"]],"#,
            r#""k":"book","t":"tok-up"}"#
        )
    );

    // k:"books" — the atomic batch carries BOTH `at` and `ts` (same value).
    let books = DataEvent::Books {
        books: vec![(
            "tok-up".into(),
            vec![(d("0.55"), d("10.5"))],
            vec![(d("0.56"), d("20"))],
        )],
        now_ms: TS,
    };
    assert_eq!(
        line(&books),
        concat!(
            r#"{"at":1000000000000,"books":[{"a":[["0.56","20"]],"b":[["0.55","10.5"]],"t":"tok-up"}],"#,
            r#""k":"books","ts":1000000000000}"#
        )
    );

    // k:"top" — absent sides ride as null.
    let top = DataEvent::TopOfBook {
        token_id: "tok-up".into(),
        best_bid: Some(d("0.55")),
        best_ask: None,
        now_ms: TS,
    };
    assert_eq!(
        line(&top),
        r#"{"at":1000000000000,"ba":null,"bb":"0.55","k":"top","t":"tok-up"}"#
    );

    // k:"spot"
    let spot = DataEvent::Spot {
        asset: "BTC".into(),
        price: d("65000.5"),
        now_ms: TS,
    };
    assert_eq!(
        line(&spot),
        r#"{"at":1000000000000,"k":"spot","p":"65000.5","s":"BTC"}"#
    );

    // k:"round" — CryptoMarket rides in its camelCase serde form. Note: the
    // payload goes through `serde_json::to_value` first, so even the struct's
    // fields come out in the Value map's ALPHABETICAL order (numbers via the
    // decimal helper, unlike the archive's own string decimals).
    let round = DataEvent::RoundMarkets {
        markets: vec![market()],
        now_ms: TS,
    };
    assert_eq!(
        line(&round),
        r#"{"at":1000000000000,"k":"round","m":[{"archiveVerdict":false,"asset":"BTC","conditionId":"cond-1","downPrice":0.45,"downTokenId":"tok-down","expiresAtMs":1000000000000,"negRisk":false,"question":"BTC up or down?","questionId":"q-1","roundDurationSec":300,"roundSlot":7,"upPrice":0.55,"upTokenId":"tok-up","venue":"polymarket"}]}"#
    );

    // k:"trade" — SCREAMING side, string decimals.
    let trade = DataEvent::Trade {
        token_id: "tok-up".into(),
        side: blitzkrieg_market_api::Side::Buy,
        price: d("0.55"),
        size: d("10.5"),
        now_ms: TS,
    };
    assert_eq!(
        line(&trade),
        r#"{"at":1000000000000,"k":"trade","p":"0.55","q":"10.5","s":"BUY","t":"tok-up"}"#
    );

    // k:"round_end"
    let round_end = DataEvent::RoundEnd { now_ms: TS };
    assert_eq!(line(&round_end), r#"{"at":1000000000000,"k":"round_end"}"#);

    // k:"resolution" — both sides named explicitly.
    let resolution = DataEvent::Resolution {
        condition_id: "cond-1".into(),
        payouts: vec![("tok-up".into(), d("1")), ("tok-down".into(), d("0"))],
        neg_risk: false,
        now_ms: TS,
    };
    assert_eq!(
        line(&resolution),
        concat!(
            r#"{"at":1000000000000,"c":"cond-1","k":"resolution","n":false,"#,
            r#""p":[["tok-up","1"],["tok-down","0"]]}"#
        )
    );

    // k:"cashflow"
    let cashflow = DataEvent::Cashflow {
        which: CashflowKind::Rebate,
        amount_usd: d("0.01"),
        now_ms: TS,
    };
    assert_eq!(
        line(&cashflow),
        r#"{"at":1000000000000,"k":"cashflow","usd":"0.01","which":"rebate"}"#
    );

    // k:"unified" — the #425 UnifiedEvent rides verbatim in its camelCase
    // serde form, alphabetized by the Value map like every json! object.
    let mut event = UnifiedEvent {
        id: "btc-1m-20261101".into(),
        title: "BTC above 65000 at 10:00".into(),
        settlement_rules: "Chainlink feed".into(),
        settlement_source: SettlementSource::Chainlink,
        listings: Default::default(),
        discrepancies: Vec::new(),
        status: EventStatus::Paired,
    };
    event.listings.insert(
        Venue::Polymarket,
        PlatformListing {
            venue: Venue::Polymarket,
            condition_id: "cond-1".into(),
            up_token_id: "tok-up".into(),
            down_token_id: "tok-down".into(),
            asset: Some("bitcoin".into()),
            title: "BTC above 65000 at 10:00".into(),
            settlement_rules: "Chainlink feed".into(),
            settlement_source: SettlementSource::Chainlink,
            expires_at_ms: TS,
            status: blitzkrieg_market_api::unified::ListingStatus::Active,
        },
    );
    let unified = DataEvent::UnifiedMapping { event, now_ms: TS };
    assert_eq!(
        line(&unified),
        concat!(
            r#"{"at":1000000000000,"e":{"discrepancies":[],"id":"btc-1m-20261101","#,
            r#""listings":{"polymarket":{"asset":"bitcoin","conditionId":"cond-1","downTokenId":"tok-down","#,
            r#""expiresAtMs":1000000000000,"settlementRules":"Chainlink feed","settlementSource":"chainlink","#,
            r#""status":"active","title":"BTC above 65000 at 10:00","upTokenId":"tok-up","venue":"polymarket"}},"#,
            r#""settlementRules":"Chainlink feed","settlementSource":"chainlink","status":"paired","title":"BTC above 65000 at 10:00"},"k":"unified"}"#
        )
    );
}

#[test]
fn every_golden_line_decodes_back_to_the_same_event() {
    // Round-trip through the DEcoder too — byte-stable in both directions.
    let cases: Vec<DataEvent> = vec![
        DataEvent::Book {
            token_id: "tok-up".into(),
            bids: vec![(d("0.55"), d("10.5"))],
            asks: vec![],
            now_ms: TS,
        },
        DataEvent::Books {
            books: vec![("tok-up".into(), vec![], vec![])],
            now_ms: TS,
        },
        DataEvent::TopOfBook {
            token_id: "tok-up".into(),
            best_bid: None,
            best_ask: Some(d("0.56")),
            now_ms: TS,
        },
        DataEvent::Spot {
            asset: "BTC".into(),
            price: d("65000.5"),
            now_ms: TS,
        },
        DataEvent::RoundMarkets {
            markets: vec![market()],
            now_ms: TS,
        },
        DataEvent::Trade {
            token_id: "tok-up".into(),
            side: blitzkrieg_market_api::Side::Sell,
            price: d("0.55"),
            size: d("10.5"),
            now_ms: TS,
        },
        DataEvent::RoundEnd { now_ms: TS },
        DataEvent::Resolution {
            condition_id: "cond-1".into(),
            payouts: vec![("tok-up".into(), d("1"))],
            neg_risk: true,
            now_ms: TS,
        },
        DataEvent::Cashflow {
            which: CashflowKind::Reward,
            amount_usd: d("0.02"),
            now_ms: TS,
        },
    ];
    for ev in cases {
        let encoded = line(&ev);
        let decoded = event_from_json(&serde_json::from_str(&encoded).expect("valid json"))
            .unwrap_or_else(|e| panic!("{e}: {encoded}"));
        assert_eq!(line(&decoded), encoded, "round-trip drifted");
        assert_eq!(event_at_ms(&decoded), event_at_ms(&ev));
    }
}

#[test]
fn unknown_kind_and_malformed_lines_fail_closed() {
    let v = serde_json::json!({"at": TS, "k": "mystery"});
    assert_eq!(
        event_from_json(&v).unwrap_err(),
        "unknown event kind `mystery`",
        "a future kind must be REJECTED by old readers, never guessed"
    );
    assert_eq!(
        event_from_json(&serde_json::json!({"k": "book"})).unwrap_err(),
        "missing/invalid `at`"
    );
    assert_eq!(
        event_from_json(&serde_json::json!({"at": TS})).unwrap_err(),
        "missing `k`"
    );
    assert!(
        event_from_json(
            &serde_json::json!({"at": TS, "k": "book", "t": "t", "b": [["x","1"]], "a": []})
        )
        .unwrap_err()
        .starts_with("bad decimal `x`"),
        "bad decimal names the offender"
    );
    // Bad trade side is fail-closed with the offending token.
    assert_eq!(
        event_from_json(
            &serde_json::json!({"at": TS, "k": "trade", "t": "t", "s": "LONG", "p": "1", "q": "1"})
        )
        .unwrap_err(),
        "bad trade side `LONG`"
    );
}

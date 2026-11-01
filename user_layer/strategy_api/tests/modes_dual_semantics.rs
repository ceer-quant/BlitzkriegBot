//! Modes dual-semantics goldens — issue #840 PR0.
//!
//! Freezes the §7.4 contract's TWO spellings byte-for-byte before any type
//! moves into `bk_contract`:
//!
//! * `blitzkrieg_market_api::modes::parse_modes` (plugin side) **accepts**
//!   a `capabilities` key and parses it into display bits;
//! * `blitzkrieg_strategy_api::modes::parse_strategy_modes` (strategy side)
//!   **refuses** the same key with a positioned `CapabilityForbidden`.
//!
//! Codex ruling #6555067033/ⓓ: this asymmetry is the contract — the two
//! parsers must NOT be merged, and neither error text nor index may change.
//! On every payload the two sides agree on (no `capabilities` key), the two
//! parsers must produce byte-identical `ModeError` text — that symmetry is
//! asserted side-by-side with the asymmetry so a future "unify the parsers"
//! refactor cannot silently pick a side.
//!
//! Also pins `ModeError`'s seven Display texts and the `mode_wire_json` /
//! `modes_payload_json` codec shapes (snake_case keys, absent structure key).

use blitzkrieg_market_api::modes::{capabilities_to_names, capability_bit, parse_modes};
use blitzkrieg_market_api::{MarketCapabilities, MarketStructure, MarketType, ModeError};
use blitzkrieg_strategy_api::StrategyMode;
use blitzkrieg_strategy_api::modes::{mode_wire_json, parse_strategy_modes};

/// The payload that splits the two sides: same bytes, plugin accepts,
/// strategy refuses (the regression guard for the 2026-10 plugin-switch
/// incompatibility).
const SPLIT_PAYLOAD: &str = concat!(
    r#"{"modes":[{"market_type":"prediction","#,
    r#""structure":"binary_outcome_wheel","#,
    r#""capabilities":["websocket_feed","level2_snapshot"]}]}"#
);

/// The shared subset payload: both sides accept identical shapes.
const COMMON_PAYLOAD: &str =
    r#"{"modes":[{"market_type":"prediction","structure":"binary_outcome_wheel"}]}"#;

#[test]
fn same_payload_plugin_accepts_strategy_refuses() {
    // Plugin side: capabilities parse into display bits (1<<0 | 1<<1 = 3).
    let plugin = parse_modes(SPLIT_PAYLOAD).expect("plugin side accepts capabilities");
    assert_eq!(plugin.len(), 1);
    assert_eq!(plugin[0].market_type, MarketType::Prediction);
    assert_eq!(
        plugin[0].structure,
        Some(MarketStructure::BinaryOutcomeWheel)
    );
    assert_eq!(plugin[0].capabilities, MarketCapabilities(3));

    // Strategy side: the same key is a positioned error.
    let err = parse_strategy_modes(SPLIT_PAYLOAD).unwrap_err();
    match &err {
        ModeError::CapabilityForbidden { index, got } => {
            assert_eq!(*index, 0);
            // `got` is the RAW JSON value, compact-printed.
            assert_eq!(got, r#"["websocket_feed","level2_snapshot"]"#);
        }
        other => panic!("expected CapabilityForbidden, got {other:?}"),
    }
    // Display text frozen byte-for-byte (ruling ⓓ: text may not change).
    assert_eq!(
        err.to_string(),
        concat!(
            r#"mode[index=0] carries capabilities (forbidden on a strategy: "#,
            r#"it may declare market_type and structure only) "#,
            r#"got="[\"websocket_feed\",\"level2_snapshot\"]""#
        ),
        "CapabilityForbidden Display drifted — [CONTRACT-BREAKING] territory"
    );
}

#[test]
fn common_subset_symmetry_both_accept() {
    let plugin = parse_modes(COMMON_PAYLOAD).expect("plugin side");
    let strategy = parse_strategy_modes(COMMON_PAYLOAD).expect("strategy side");
    assert_eq!(plugin[0].market_type, MarketType::Prediction);
    assert_eq!(
        plugin[0].structure,
        Some(MarketStructure::BinaryOutcomeWheel)
    );
    assert_eq!(plugin[0].capabilities, MarketCapabilities::NONE);
    assert_eq!(
        strategy,
        vec![StrategyMode {
            market_type: MarketType::Prediction,
            structure: Some(MarketStructure::BinaryOutcomeWheel),
        }]
    );
}

#[test]
fn mode_wire_json_is_snake_keyed_and_structure_is_conditional() {
    // §2.3 wire: snake_case keys (NOT the DTO camelCase), `structure` key
    // present only when concrete.
    let with_structure = mode_wire_json(&StrategyMode {
        market_type: MarketType::Prediction,
        structure: Some(MarketStructure::BinaryOutcomeWheel),
    });
    assert_eq!(
        serde_json::to_string(&with_structure).unwrap(),
        r#"{"market_type":"prediction","structure":"binary_outcome_wheel"}"#
    );
    let no_structure = mode_wire_json(&StrategyMode {
        market_type: MarketType::Futures,
        structure: None,
    });
    assert_eq!(
        serde_json::to_string(&no_structure).unwrap(),
        r#"{"market_type":"futures"}"#
    );
}

#[test]
fn shared_error_table_is_byte_identical_on_both_sides() {
    // Every payload the two sides agree on must produce the SAME variant,
    // index and Display text — the "one contract" half of §7.4.
    let cases: [(&str, &str); 4] = [
        (
            r#"{"modes":[]}"#,
            "modes payload declares an empty array (Empty: declared-but-empty is an error, not 'undeclared')",
        ),
        (
            r#"{"modes":[{"structure":"central_limit_order_book"}]}"#,
            "mode[index=0] is missing market_type (the only required field)",
        ),
        (
            r#"{"modes":[{"market_type":"nft_market"}]}"#,
            r#"mode[index=0] has unknown market_type got="nft_market""#,
        ),
        (
            r#"{"modes":[{"market_type":"spot"},{"market_type":"spot","structure":"not_a_structure"}]}"#,
            r#"mode[index=1] has unknown structure got="not_a_structure""#,
        ),
    ];
    for (payload, expected) in cases {
        let plugin = parse_modes(payload).unwrap_err().to_string();
        let strategy = parse_strategy_modes(payload).unwrap_err().to_string();
        assert_eq!(plugin, expected, "plugin side drifted: {payload}");
        assert_eq!(strategy, expected, "strategy side drifted: {payload}");
        assert_eq!(plugin, strategy, "sides disagree: {payload}");
    }

    // Non-JSON: same variant on both sides; message carries the serde error,
    // so pin only the stable prefix.
    let plugin = parse_modes("not json at all").unwrap_err().to_string();
    let strategy = parse_strategy_modes("not json at all")
        .unwrap_err()
        .to_string();
    assert!(
        plugin.starts_with("modes payload is not JSON: "),
        "{plugin}"
    );
    assert_eq!(plugin, strategy);

    // Not-an-object-with-modes: the custom inner message is fully stable.
    let plugin = parse_modes(r#"{"modes":"nope"}"#).unwrap_err();
    assert_eq!(
        plugin.to_string(),
        "modes payload is not JSON: payload is not an object with a `modes` array"
    );
}

#[test]
fn capability_handling_splits_on_non_array_too() {
    // capabilities present but not an array: plugin = UnknownCapability with
    // the raw JSON value; strategy = CapabilityForbidden with the same raw
    // value. Presence is the strategy-side trigger, not the value shape.
    let payload = r#"{"modes":[{"market_type":"spot","capabilities":"websocket_feed"}]}"#;
    let plugin = parse_modes(payload).unwrap_err();
    // `got` = the raw JSON value's own text (`"websocket_feed"`, quotes and
    // all), then Display's {:?} escapes those quotes again.
    assert_eq!(
        plugin.to_string(),
        r#"mode[index=0] has unknown capability got="\"websocket_feed\"""#,
        "plugin parses the value shape even when invalid"
    );
    let strategy = parse_strategy_modes(payload).unwrap_err();
    assert_eq!(
        strategy.to_string(),
        r#"mode[index=0] carries capabilities (forbidden on a strategy: it may declare market_type and structure only) got="\"websocket_feed\"""#,
        "strategy refuses on presence before shape"
    );

    // Unknown capability NAME: plugin rejects the name; strategy still fires
    // CapabilityForbidden on presence first (before per-item validation).
    let payload = r#"{"modes":[{"market_type":"earn","capabilities":["time_travel"]}]}"#;
    assert_eq!(
        parse_modes(payload).unwrap_err().to_string(),
        r#"mode[index=0] has unknown capability got="time_travel""#
    );
    let strategy = parse_strategy_modes(payload).unwrap_err();
    assert!(matches!(
        strategy,
        ModeError::CapabilityForbidden { index: 0, .. }
    ));
}

#[test]
fn capability_name_table_round_trip() {
    // The bit-order table (frozen in market_api's wire golden too; here we
    // assert the parse side lands exactly these bits).
    for (bit, name) in blitzkrieg_market_api::modes::CAPABILITY_NAMES {
        let modes = parse_modes(&format!(
            r#"{{"modes":[{{"market_type":"spot","capabilities":["{name}"]}}]}}"#
        ))
        .expect("table name must parse");
        assert_eq!(modes[0].capabilities, MarketCapabilities(bit));
        assert_eq!(capability_bit(name), Some(MarketCapabilities(bit)));
    }
    // Bit-order encoding.
    assert_eq!(
        capabilities_to_names(MarketCapabilities(1025)),
        vec!["websocket_feed", "simulated"]
    );
}

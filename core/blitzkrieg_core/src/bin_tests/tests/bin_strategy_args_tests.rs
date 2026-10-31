use super::*;

// ── #351 反向验收 · the honest-friction floor is enforced ───────────────

/// The spec's floor: "mine 模式拒绝 slippage=0（fail-closed）". A sweep
/// that asked for a frictionless run must be refused at the knob, not run
/// and quietly print fantasy PnL.
#[test]
fn mine_mode_refuses_zero_slippage() {
    let base = blitzkrieg_core::sim::FillModel {
        taker_slippage_ticks: 0,
        ..Default::default()
    };
    let err = blitzkrieg_core::backtest::mode_rung_model("mine", base, 0)
        .expect_err("zero slippage must die");
    assert!(
        err.contains("refuses zero taker slippage"),
        "the refusal must name the floor: {err}"
    );
    // The floor is mode-independent: verify and sweep run on the same
    // honest-friction contract.
    assert!(blitzkrieg_core::backtest::mode_rung_model("verify", base, 0).is_err());
    assert!(blitzkrieg_core::backtest::mode_rung_model("sweep", base, 0).is_err());
}

/// The other side of the knob contract: mine zeroes the latency dials
/// (0 decision→venue distance, real slippage/fees kept from `base`), while
/// verify/sweep stamp the rung onto every latency.
#[test]
fn mode_rung_model_wires_the_latency_dials_per_mode() {
    let base = blitzkrieg_core::sim::FillModel {
        taker_slippage_ticks: 1,
        taker_latency_ms: 7,
        taker_rtt_ms: 11,
        maker_latency_ms: 13,
        ..Default::default()
    };
    let mine =
        blitzkrieg_core::backtest::mode_rung_model("mine", base, 0).expect("mine above the floor");
    assert_eq!(mine.taker_latency_ms, 0);
    assert_eq!(mine.taker_rtt_ms, 0);
    assert_eq!(mine.maker_latency_ms, 0);
    assert_eq!(
        mine.taker_slippage_ticks, 1,
        "mine keeps the real friction; latency is the only dial it turns"
    );
    let verify = blitzkrieg_core::backtest::mode_rung_model("verify", base, 286)
        .expect("verify above the floor");
    assert_eq!(verify.taker_latency_ms, 286);
    assert_eq!(verify.taker_rtt_ms, 286);
    assert_eq!(verify.maker_latency_ms, 286);
    assert_eq!(verify.taker_slippage_ticks, 1);
}

#[test]
fn three_segment_form_still_parses_as_caps_only() {
    let l = parse_one("spread_arb:0:-").expect("legacy form must still parse");
    assert_eq!(l.max_open_positions, Some(0));
    assert_eq!(l.max_open_notional_usd, None);
    assert!(l.size_usd.is_none() && l.min_shares.is_none() && l.max_shares.is_none());
    assert!(
        !l.sizing().overrides_anything(),
        "caps-only must not masquerade as a sizing override"
    );

    let l2 = parse_one("spread_arb:-:12.5").expect("blank cap segment = uncapped");
    assert_eq!(l2.max_open_positions, None);
    assert_eq!(l2.max_open_notional_usd, Some(dec!(12.5)));
}

#[test]
fn six_segment_form_parses_per_strategy_sizing() {
    let l = parse_one("spread_arb:2:20:1.5:4:8").expect("extended form must parse");
    assert_eq!(l.max_open_positions, Some(2));
    assert_eq!(l.max_open_notional_usd, Some(dec!(20)));
    assert_eq!(l.size_usd, Some(dec!(1.5)));
    assert_eq!(l.min_shares, Some(dec!(4)));
    assert_eq!(l.max_shares, Some(dec!(8)));
    assert!(l.sizing().overrides_anything());

    // "-" inherits the global per dimension, independently of the others.
    let l2 = parse_one("x:-:-:-:3:-").expect("blank sizing segments inherit the globals");
    assert_eq!(l2.size_usd, None);
    assert_eq!(l2.min_shares, Some(dec!(3)));
    assert_eq!(l2.max_shares, None);
    assert!(
        l2.sizing().overrides_anything(),
        "one set dimension is enough"
    );
}

#[test]
fn seven_segment_form_parses_allocation_weight() {
    // E16/#98: the 7th segment is the leg's allocation weight.
    let l = parse_one("spread_arb:2:20:1.5:4:8:0.5").expect("weight form must parse");
    assert_eq!(l.size_weight, Some(dec!(0.5)));
    assert!(l.sizing().overrides_anything());
    // "-" = unweighted; an absent 7th segment is the 6-segment form.
    let l2 = parse_one("spread_arb:2:20:1.5:4:8:-").expect("blank weight = unweighted");
    assert_eq!(l2.size_weight, None);
    // Bad weight drops the whole flag.
    assert!(parse_one("spread_arb:0:-:1:2:3:x").is_none());
}

#[test]
fn malformed_strategy_limits_are_dropped_not_half_applied() {
    for bad in [
        "spread_arb:0",               // too few segments
        "spread_arb:0:-:1:2:3:4:5:6", // too many
        ":0:-",                       // no name
        "spread_arb:x:-",             // bad position cap
        "spread_arb:0:abc",           // bad notional
        "spread_arb:0:-:abc:1:2",     // bad size_usd
        "spread_arb:0:-:1:abc:2",     // bad min_shares
        "spread_arb:0:-:1:2:abc",     // bad max_shares
        "spread_arb:0:-:1:2:3:4:x",   // bad size_pct (#202)
    ] {
        assert!(parse_one(bad).is_none(), "must be ignored: {bad}");
    }
    assert!(parse_strategy_limits(&[]).is_empty());
}

#[test]
fn repeated_flags_keep_the_last_entry_per_name() {
    let m = parse_strategy_limits(&[
        "a:1:-".to_string(),
        "b:2:-:1:1:1".to_string(),
        "a:5:-".to_string(),
    ]);
    assert_eq!(m.len(), 2);
    assert_eq!(m["a"].max_open_positions, Some(5), "last flag wins");
    assert_eq!(m["b"].min_shares, Some(dec!(1)));
}

// ── E-VA: the --virtual-limit grammar ───────────────────────────────────

#[test]
fn virtual_limit_parses_name_cap_accounts() {
    let m = parse_virtual_limits(&["cross_venue_arb:500:default,paper".to_string()]);
    assert_eq!(m.len(), 1);
    let pot = m.get("cross_venue_arb").expect("the named pot");
    assert_eq!(pot.max_batch_notional_usd, Some(dec!(500)));
    assert_eq!(
        pot.accounts,
        vec!["default".to_string(), "paper".to_string()],
        "comma-separated accounts split and kept in order"
    );
}

#[test]
fn virtual_limit_trims_and_drops_blank_segments() {
    // Whitespace around every segment is trimmed; empty account slots
    // (a trailing comma, doubled commas) drop out rather than become an
    // account named "".
    let m = parse_virtual_limits(&[" s : 12.5 : a , , b, ".to_string()]);
    let pot = m.get("s").expect("trimmed pot");
    assert_eq!(pot.max_batch_notional_usd, Some(dec!(12.5)));
    assert_eq!(pot.accounts, vec!["a".to_string(), "b".to_string()]);
}

#[test]
fn malformed_virtual_limits_are_dropped_not_half_applied() {
    for bad in [
        "cross_venue_arb:500",           // too few segments
        "cross_venue_arb:500:a,b:extra", // too many
        ":500:default",                  // no name
        "cross_venue_arb:abc:default",   // bad cap
        "cross_venue_arb:-:default",     // no cap = disarmed pot, dropped
        "cross_venue_arb:500:,",         // no accounts
        "cross_venue_arb:500:",          // empty account segment
    ] {
        let m = parse_virtual_limits(&[bad.to_string()]);
        assert!(m.is_empty(), "must be ignored, not half-applied: {bad}");
    }
    // An empty flag list parses to an empty map (the zero-config posture).
    assert!(parse_virtual_limits(&[]).is_empty());
}

#[test]
fn repeated_virtual_limit_flags_keep_the_last_per_name() {
    let m = parse_virtual_limits(&[
        "a:10:default".to_string(),
        "b:20:paper".to_string(),
        "a:99:default,paper".to_string(),
    ]);
    assert_eq!(m.len(), 2, "one pot per strategy name");
    let a = m.get("a").expect("pot a");
    assert_eq!(a.max_batch_notional_usd, Some(dec!(99)), "last flag wins");
    assert_eq!(a.accounts, vec!["default".to_string(), "paper".to_string()]);
    assert_eq!(
        m.get("b").expect("pot b").max_batch_notional_usd,
        Some(dec!(20))
    );
}

#[test]
fn engine_session_records_by_default() {
    let (path, max, rotate, min_free) = resolve_event_archive(true, false, None, None, None, None);
    assert_eq!(path.as_deref(), Some(DEFAULT_EVENT_ARCHIVE));
    assert_eq!(
        max, 0,
        "no session cap: rotation + the space floor bound the volume"
    );
    assert_eq!(rotate, 256);
    assert_eq!(min_free, 5120);
}

#[test]
fn non_engine_session_does_not_record() {
    // The plain trading core (no --engine) has no market-data consumers, so
    // there is nothing to archive; the default must not leave an empty file.
    let (path, _, rotate, _) = resolve_event_archive(false, false, None, None, None, None);
    assert_eq!(path, None);
    assert_eq!(rotate, 0);
}

#[test]
fn explicit_path_opts_in_without_engine() {
    let (path, ..) = resolve_event_archive(false, false, Some("/tmp/x.jsonl"), None, None, None);
    assert_eq!(path.as_deref(), Some("/tmp/x.jsonl"));
}

#[test]
fn explicit_tuning_beats_the_default() {
    let (path, max, rotate, min_free) =
        resolve_event_archive(true, false, None, Some(64), Some(8), Some(100));
    assert_eq!(path.as_deref(), Some(DEFAULT_EVENT_ARCHIVE));
    assert_eq!((max, rotate, min_free), (64, 8, 100));
}

#[test]
fn explicit_zero_tuning_is_honoured_not_defaulted() {
    // 0 is a meaningful request (no cap / never rotate / no space guard) and
    // must survive; only an ABSENT flag falls back to the default.
    let (_, max, rotate, min_free) =
        resolve_event_archive(true, false, None, Some(0), Some(0), Some(0));
    assert_eq!((max, rotate, min_free), (0, 0, 0));
}

#[test]
fn no_event_archive_disables_capture_even_with_a_path() {
    let (path, max, rotate, min_free) = resolve_event_archive(
        true,
        true,
        Some("/tmp/x.jsonl"),
        Some(64),
        Some(8),
        Some(100),
    );
    assert_eq!((path, max, rotate, min_free), (None, 0, 0, 0));
}

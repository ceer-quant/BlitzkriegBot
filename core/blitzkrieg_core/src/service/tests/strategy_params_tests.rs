#![cfg(test)]
//! Issue #481 (G3/G4): the strategy-tunables edit surface
//! (`strategy.params.get`/`set`) and the unified-mapping readout
//! (`unified.list`). The write path is the `risk.setExit` posture: plan
//! against the strategy's OWN declared domains, write later, audit the
//! receipt; a refused patch moves nothing.

use super::*;
use crate::risk::RiskConfig;
use rust_decimal_macros::dec;

fn core_with_strategies() -> Core {
    let mut c = Core::new(CoreConfig {
        risk: RiskConfig::default(),
        dry_seed_balance: dec!(1000),
        ..Default::default()
    });
    let mut engine = crate::engine::Engine::new(crate::engine::EngineConfig::default());
    crate::strategies::test_support::host_disabled(
        &mut engine,
        Default::default(),
        Default::default(),
    );
    c.enable_engine(engine);
    c
}

fn knob_map(knobs: &[(&str, &str)]) -> std::collections::BTreeMap<String, serde_json::Value> {
    knobs
        .iter()
        .map(|(k, v)| (k.to_string(), serde_json::Value::String(v.to_string())))
        .collect()
}

/// The readout exposes the strategy's OWN declaration (names + domains)
/// and the values in force; the kernel invents nothing.
#[test]
fn params_get_reports_the_declared_domains_and_values() {
    let c = core_with_strategies();
    let r = c.strategy_params_readout("spread_arb");
    assert!(r.found);
    assert!(r.evolvable);
    assert!(
        !r.tunables.is_empty(),
        "spread_arb declares knobs; the readout must carry them"
    );
    let knob = r
        .tunables
        .iter()
        .find(|t| t.name == "trend_max_entry_price")
        .expect("the spread_arb cap knob is declared");
    // The declared value in force, and a real domain [min, max].
    assert_eq!(knob.value, "0.45");
    let (min, max) = (
        Decimal::from_str_exact(&knob.min).unwrap(),
        Decimal::from_str_exact(&knob.max).unwrap(),
    );
    assert!(min <= max);
    assert!(min <= dec!(0.45) && dec!(0.45) <= max);

    // A strategy that declared nothing reports an empty list — the
    // editor renders 「没有可调参数」, never an invented domain.
    let inert = c.strategy_params_readout("nonexistent_strategy");
    assert!(!inert.found);
    assert!(!inert.evolvable);
    assert!(inert.tunables.is_empty());
}

/// The happy path: one knob moves inside its domain, the receipt names
/// old→new with effect=live, the registry cell carries the new value,
/// and the OTHER knobs keep their values (a single-knob patch merges).
#[test]
fn params_set_moves_one_knob_and_keeps_the_others() {
    let mut c = core_with_strategies();
    let before = c.strategy_params_readout("spread_arb");
    let other_before = before
        .tunables
        .iter()
        .find(|t| t.name == "trend_max_entry_price")
        .map(|t| t.value.clone())
        .unwrap();
    let target_before = before
        .tunables
        .iter()
        .find(|t| t.name != "trend_max_entry_price")
        .expect("spread_arb declares more than one knob")
        .clone();

    let update = c
        .apply_strategy_params_set(
            "spread_arb",
            &knob_map(&[(&target_before.name, &format!("{}", dec!(0.30)))]),
            "uid:tester",
            Some("calibration".into()),
            1_000,
        )
        .expect("an in-domain write applies");
    assert_eq!(update.name, "spread_arb");
    assert_eq!(update.applied.len(), 1);
    assert_eq!(update.applied[0].knob, target_before.name);
    assert_eq!(update.applied[0].from, target_before.value);
    assert_eq!(update.applied[0].to, "0.30");
    assert_eq!(update.applied[0].effect, "live");
    assert_eq!(update.actor, "uid:tester");
    assert!(!update.persisted);

    // The cell in force moved; the untouched knob did not.
    let after = c.strategy_params_readout("spread_arb");
    let moved = after
        .tunables
        .iter()
        .find(|t| t.name == target_before.name)
        .unwrap();
    assert_eq!(moved.value, "0.30");
    let kept = after
        .tunables
        .iter()
        .find(|t| t.name == "trend_max_entry_price")
        .unwrap();
    assert_eq!(kept.value, other_before);
    // The write attached the overlay (the strategy reads the cell on its
    // next evaluate — that is what `effect: live` means).
    assert!(c.has_hot_params());
}

/// The three refusals: undeclared knob, out-of-domain value, unparsable
/// value — every one moves NOTHING (plan first, write later).
#[test]
fn params_set_refuses_bad_patches_without_moving_anything() {
    let mut c = core_with_strategies();
    let before = c.strategy_params_readout("spread_arb");
    let snapshot = |r: &crate::ipc::schema::StrategyParamsReadout| {
        r.tunables
            .iter()
            .map(|t| (t.name.clone(), t.value.clone()))
            .collect::<Vec<_>>()
    };
    let before_pairs = snapshot(&before);

    // 1. an undeclared knob
    let err = c
        .apply_strategy_params_set(
            "spread_arb",
            &knob_map(&[("no_such_knob", "0.5")]),
            "uid:tester",
            None,
            1_000,
        )
        .unwrap_err();
    assert_eq!(err.code, crate::model::CoreErrorCode::InvalidParams);
    assert!(err.message.contains("no_such_knob"), "got {}", err.message);
    assert!(err.message.contains("nothing was applied"));

    // 2. a value outside the declared domain
    let knob = &before.tunables[0];
    let out_of_domain = format!("{}", knob_max(&knob.max) + dec!(1));
    let err = c
        .apply_strategy_params_set(
            "spread_arb",
            &knob_map(&[(&knob.name, &out_of_domain)]),
            "uid:tester",
            None,
            1_001,
        )
        .unwrap_err();
    assert!(
        err.message.contains("outside the declared domain"),
        "got {}",
        err.message
    );

    // 3. an unparsable value (a float with excess precision, not a decimal)
    let err = c
        .apply_strategy_params_set(
            "spread_arb",
            &knob_map(&[(&knob.name, "not-a-decimal")]),
            "uid:tester",
            None,
            1_002,
        )
        .unwrap_err();
    assert!(
        err.message.contains("not an exact decimal"),
        "got {}",
        err.message
    );

    // 4. an unknown strategy
    let err = c
        .apply_strategy_params_set(
            "ghost",
            &knob_map(&[("anything", "1")]),
            "uid:tester",
            None,
            1_003,
        )
        .unwrap_err();
    assert!(
        err.message.contains("declares no tunables"),
        "got {}",
        err.message
    );

    // Nothing moved.
    let after = snapshot(&c.strategy_params_readout("spread_arb"));
    assert_eq!(
        after, before_pairs,
        "every refusal left the values in force"
    );
}

fn knob_max(max: &str) -> Decimal {
    Decimal::from_str_exact(max).unwrap()
}

/// A multi-knob patch is atomic: one bad value refuses the WHOLE patch.
#[test]
fn params_set_is_atomic_across_knobs() {
    let mut c = core_with_strategies();
    let before = c.strategy_params_readout("spread_arb");
    let knob = &before.tunables[0];
    let err = c
        .apply_strategy_params_set(
            "spread_arb",
            &knob_map(&[
                (&knob.name, &knob.value), // in-domain, would apply
                ("no_such_knob", "1"),     // undeclared — refuses all
            ]),
            "uid:tester",
            None,
            1_000,
        )
        .unwrap_err();
    assert!(err.message.contains("no_such_knob"));
    let after = c.strategy_params_readout("spread_arb");
    assert_eq!(
        after
            .tunables
            .iter()
            .find(|t| t.name == knob.name)
            .unwrap()
            .value,
        knob.value,
        "the good half of a refused patch must not land"
    );
}

/// Issue #481: an operator's tunables write must survive the rewires the
/// service layer triggers elsewhere — toggling a SIBLING strategy
/// re-registers the whole host set, and for an adapter whose declaration
/// is a static manifest default (the Lua package shape) the old code
/// stomped the cell back to that default. The in-tree adapters derive
/// their declaration from the effective config, so the carried value
/// equals the declared one and only the end-to-end shape proves the
/// carry-over; the static-declaration unit tests live on
/// shadow_evolution (re_registration_*).
#[test]
fn params_set_survives_a_sibling_strategy_toggle() {
    let mut c = core_with_strategies();
    let knob = c
        .strategy_params_readout("spread_arb")
        .tunables
        .into_iter()
        .find(|t| t.name == "trend_max_entry_price")
        .expect("spread_arb declares the cap knob");
    assert!(
        knob.value != "0.52",
        "precondition: the test starts away from the value it writes"
    );
    c.apply_strategy_params_set(
        "spread_arb",
        &knob_map(&[("trend_max_entry_price", "0.52")]),
        "uid:tester",
        None,
        1_000,
    )
    .expect("the write lands");

    // Toggle a sibling off and back on: two full host re-registrations.
    assert!(c.set_strategy_enabled("trend_follow", false));
    assert!(c.set_strategy_enabled("trend_follow", true));

    let after = c.strategy_params_readout("spread_arb");
    assert_eq!(
        after
            .tunables
            .iter()
            .find(|t| t.name == "trend_max_entry_price")
            .unwrap()
            .value,
        "0.52",
        "a sibling toggle re-registers the host set; the operator's write must survive it"
    );
}

/// G4: the unified readout serializes the confirmed mappings (venues as
/// data, discrepancies as names) and stays empty on an engine without
/// mappings.
#[test]
fn unified_list_serializes_the_confirmed_mappings() {
    let mut c = core_with_strategies();
    assert!(
        c.unified_list().is_empty(),
        "no mappings fed, none reported"
    );

    let mut event = blitzkrieg_market_api::unified::UnifiedEvent {
        id: "evt-1".into(),
        title: "BTC above 100k".into(),
        settlement_rules: "resolves YES".into(),
        settlement_source: blitzkrieg_market_api::unified::SettlementSource::Uma,
        listings: Default::default(),
        discrepancies: vec![],
        status: blitzkrieg_market_api::unified::EventStatus::Paired,
    };
    event.listings.insert(
        blitzkrieg_market_api::unified::Venue::Polymarket,
        blitzkrieg_market_api::unified::PlatformListing {
            venue: blitzkrieg_market_api::unified::Venue::Polymarket,
            condition_id: "0xabc".into(),
            up_token_id: "tok-up".into(),
            down_token_id: "tok-down".into(),
            asset: Some("bitcoin".into()),
            title: "BTC above 100k".into(),
            settlement_rules: "resolves YES".into(),
            settlement_source: blitzkrieg_market_api::unified::SettlementSource::Uma,
            expires_at_ms: 1_800_000,
            status: blitzkrieg_market_api::unified::ListingStatus::Active,
        },
    );
    event.listings.insert(
        blitzkrieg_market_api::unified::Venue::Kalshi,
        blitzkrieg_market_api::unified::PlatformListing {
            venue: blitzkrieg_market_api::unified::Venue::Kalshi,
            condition_id: "KXBTC".into(),
            up_token_id: "k-up".into(),
            down_token_id: "k-down".into(),
            asset: Some("bitcoin".into()),
            title: "BTC above 100k".into(),
            settlement_rules: "resolves YES".into(),
            settlement_source: blitzkrieg_market_api::unified::SettlementSource::Uma,
            expires_at_ms: 1_800_000,
            status: blitzkrieg_market_api::unified::ListingStatus::Active,
        },
    );
    c.engine
        .as_mut()
        .unwrap()
        .put_unified_mapping(event.clone());

    let list = c.unified_list();
    assert_eq!(list.len(), 1);
    let row = &list[0];
    assert_eq!(row["id"], "evt-1");
    assert_eq!(row["status"], "paired");
    let venues = row["venues"].as_array().unwrap();
    assert_eq!(venues.len(), 2);
    let names: Vec<&str> = venues
        .iter()
        .map(|v| v["venue"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"polymarket") && names.contains(&"kalshi"));
    assert_eq!(row["discrepancies"].as_array().unwrap().len(), 0);
}

// ── ④ 统一映射接线：提议只提议、confirm 才生效、revoke 全链拔除 ─────

/// ④ fixture: a Core with an engine and one crypto market per venue in
/// the SAME round, same asset, near-identical titles (above the confirm
/// similarity bar). Single venue (or undeclared venue) rounds propose
/// nothing.
fn unified_core() -> Core {
    let mut c = core_with_strategies();
    let now = 1_000_000_000i64;
    let slot = now / 1000 / 900;
    let market = |venue: &str, cond: &str, up: &str, down: &str| {
        crate::model::CryptoMarket {
        asset: "BTC".into(),
        condition_id: cond.into(),
        question_id: "q".into(),
        up_token_id: up.into(),
        down_token_id: down.into(),
        up_price: dec!(0.5),
        down_price: dec!(0.5),
        expires_at_ms: (slot + 1) * 900 * 1000,
        round_slot: slot,
        round_duration_sec: 900,
        archive_verdict: false,
        venue: venue.into(),
        neg_risk: false,
        question: "Bitcoin Up or Down: October 16, 2PM ET — resolves to the Binance 1 minute candle for the BTC price at 2pm ET".into(),
    }
    };
    c.engine_on_data(
        crate::engine::DataEvent::RoundMarkets {
            markets: vec![
                market("polymarket", "0xpm", "pm-up", "pm-down"),
                market("kalshi", "KXBTC", "k-up", "k-down"),
            ],
            now_ms: now,
        },
        now,
    );
    c
}

/// The auto-matcher PROPOSES (candidates JSON carries the pair with
/// normalized asset + token ids) but never CONFIRMS: no ledger event, no
/// Engine mapping, `unified.list` stays empty. This is the service-side
/// twin of the cross-venue replay negative gate (a proposal that traded
/// on its own would be the bug).
#[test]
fn round_markets_propose_candidates_without_confirming() {
    let c = unified_core();

    let cands = c.unified_candidates();
    assert_eq!(cands.len(), 1, "one polymarket × one kalshi = one proposal");
    let a = &cands[0]["a"];
    let b = &cands[0]["b"];
    // The matcher's ordering is score-internal; identify by venue.
    let (pm, ks) = if a["venue"] == "polymarket" {
        (a, b)
    } else {
        (b, a)
    };
    assert_eq!(pm["venue"], "polymarket");
    assert_eq!(pm["conditionId"], "0xpm");
    assert_eq!(pm["upTokenId"], "pm-up");
    assert_eq!(
        pm["asset"], "btc",
        "the discovered asset arrives as given (lowercased)"
    );
    assert_eq!(ks["conditionId"], "KXBTC");

    assert!(
        c.unified_list().is_empty(),
        "a proposal is NOT a mapping — unified.list must stay empty"
    );
    assert!(
        c.engine.as_ref().unwrap().unified_mappings().is_empty(),
        "the engine never sees an unconfirmed candidate"
    );
}

/// Rounds without two declared venues propose nothing (single-plugin
/// world unchanged: empty shelf, empty list, zero engine mappings).
#[test]
fn single_venue_round_proposes_nothing() {
    let mut c = core_with_strategies();
    let now = 1_000_000_000i64;
    let mut m = crate::test_fixtures::round_market(now);
    m.venue = "polymarket".into();
    c.engine_on_data(
        crate::engine::DataEvent::RoundMarkets {
            markets: vec![m],
            now_ms: now,
        },
        now,
    );
    assert!(c.unified_candidates().is_empty());
    assert!(c.unified_list().is_empty());
    // And the pre-#427 undeclared-venue shape stays silent too.
    let mut c2 = core_with_strategies();
    let m2 = crate::test_fixtures::round_market(now);
    c2.engine_on_data(
        crate::engine::DataEvent::RoundMarkets {
            markets: vec![m2],
            now_ms: now,
        },
        now,
    );
    assert!(c2.unified_candidates().is_empty());
}

/// Confirming the proposed pair makes it real everywhere at once: the
/// ledger holds it (persisted), `unified.list` reports it, the Engine
/// mapping view carries it (what `bk.unified_events()` reads), and the
/// confirmed pair leaves the proposal shelf.
#[test]
fn confirm_pushes_the_mapping_into_the_ledger_list_and_engine() {
    let mut c = unified_core();
    let cands = c.unified_candidates();
    assert_eq!(
        cands.len(),
        1,
        "precondition: the fixture proposed one pair"
    );
    let listings: Vec<blitzkrieg_market_api::unified::PlatformListing> = vec![
        serde_json::from_value(cands[0]["a"].clone()).expect("candidate a round-trips"),
        serde_json::from_value(cands[0]["b"].clone()).expect("candidate b round-trips"),
    ];
    let event = c
        .unified_confirm("Bitcoin Up or Down 2PM ET", "uid:op", listings, 1_000)
        .expect("a matching pair confirms");

    assert_eq!(c.unified_list().len(), 1);
    assert_eq!(c.unified_list()[0]["id"], event.id);
    let mappings = c.engine.as_ref().unwrap().unified_mappings();
    assert_eq!(
        mappings.len(),
        1,
        "the engine view (bk.unified_events) sees it"
    );
    assert!(mappings.contains_key(&event.id));
    assert!(
        c.unified_candidates().is_empty(),
        "a confirmed pair leaves the proposal shelf"
    );
}

/// The ledger's own refusals surface as `Err` strings and move nothing:
/// a below-bar title pair is refused even from the operator side.
#[test]
fn confirm_refusal_is_an_error_and_moves_nothing() {
    let mut c = core_with_strategies();
    let listing = |venue: blitzkrieg_market_api::unified::Venue, cond: &str, title: &str| {
        blitzkrieg_market_api::unified::PlatformListing {
            venue,
            condition_id: cond.into(),
            up_token_id: format!("{cond}-up"),
            down_token_id: format!("{cond}-down"),
            asset: Some("bitcoin".into()),
            title: title.into(),
            settlement_rules: String::new(),
            settlement_source: blitzkrieg_market_api::unified::SettlementSource::Unknown,
            expires_at_ms: 2_000_000,
            status: blitzkrieg_market_api::unified::ListingStatus::Active,
        }
    };
    let listings = vec![
        listing(
            blitzkrieg_market_api::unified::Venue::Polymarket,
            "0xa",
            "totally unrelated question about weather",
        ),
        listing(
            blitzkrieg_market_api::unified::Venue::Kalshi,
            "KXA",
            "will the rocket launch before friday night",
        ),
    ];
    let err = c
        .unified_confirm("arbitrary title", "uid:op", listings, 1_000)
        .expect_err("dissimilar titles refuse even on explicit confirm");
    assert!(err.contains("refused"), "the refusal names itself: {err}");
    assert!(
        c.unified_list().is_empty(),
        "a refused confirm maps nothing"
    );
}

/// Revocation pulls the mapping out of all three places at once: the
/// ledger (a later confirm of the same pair works again), the
/// `unified.list` readout and the Engine view.
#[test]
fn revoke_removes_the_mapping_everywhere() {
    let mut c = unified_core();
    let cands = c.unified_candidates();
    let listings: Vec<blitzkrieg_market_api::unified::PlatformListing> = vec![
        serde_json::from_value(cands[0]["a"].clone()).unwrap(),
        serde_json::from_value(cands[0]["b"].clone()).unwrap(),
    ];
    let event = c
        .unified_confirm("Bitcoin Up or Down 2PM ET", "uid:op", listings, 1_000)
        .expect("precondition: confirm lands");
    assert_eq!(c.unified_list().len(), 1);

    c.unified_revoke(&event.id, "uid:op", 2_000)
        .expect("revoking a live mapping succeeds");
    assert!(c.unified_list().is_empty(), "unified.list drops it");
    assert!(
        c.engine.as_ref().unwrap().unified_mappings().is_empty(),
        "the engine view drops it (strategies stop seeing it immediately)"
    );
    // The ledger freed the listings: the same pair confirms again. The
    // re-proposal needs a NEW round slot — the same slot re-fires the
    // #498 top-up predicate (the scanner already holds it), and a top-up
    // deliberately skips the proposal step (old candidates stay valid).
    let now = 1_000_000_000i64;
    let now2 = now + 900_000;
    let slot2 = now2 / 1000 / 900;
    let market = |venue: &str, cond: &str, up: &str, down: &str| {
        crate::model::CryptoMarket {
        asset: "BTC".into(),
        condition_id: cond.into(),
        question_id: "q".into(),
        up_token_id: up.into(),
        down_token_id: down.into(),
        up_price: dec!(0.5),
        down_price: dec!(0.5),
        expires_at_ms: (slot2 + 1) * 900 * 1000,
        round_slot: slot2,
        round_duration_sec: 900,
        archive_verdict: false,
        venue: venue.into(),
        neg_risk: false,
        question: "Bitcoin Up or Down: October 16, 2PM ET — resolves to the Binance 1 minute candle for the BTC price at 2pm ET".into(),
    }
    };
    c.engine_on_data(
        crate::engine::DataEvent::RoundMarkets {
            markets: vec![
                market("polymarket", "0xpm", "pm-up", "pm-down"),
                market("kalshi", "KXBTC", "k-up", "k-down"),
            ],
            now_ms: now2,
        },
        now2,
    );
    assert_eq!(
        c.unified_candidates().len(),
        1,
        "after revoke the same pair is proposed again"
    );
}

/// A confirmed mapping survives a Core rebuild on the same ledger path:
/// `MappingLedger::load` re-reads it, `install_engine` re-feeds it into
/// the fresh Engine, and `unified.list` sees it without any new round.
#[test]
fn confirmed_mappings_survive_a_rebuild_through_the_ledger_refeed() {
    let scratch = super::settlement_service_tests::scratch("unified-rebuild");
    let path = scratch.join("unified-mapping.json");
    let build = |ledger_path: Option<&std::path::Path>| {
        let mut c = Core::new(CoreConfig {
            risk: RiskConfig::default(),
            dry_seed_balance: dec!(1000),
            unified_ledger_path: ledger_path.map(|p| p.to_string_lossy().into_owned()),
            ..Default::default()
        });
        let mut engine = crate::engine::Engine::new(crate::engine::EngineConfig::default());
        crate::strategies::test_support::host_disabled(
            &mut engine,
            Default::default(),
            Default::default(),
        );
        c.enable_engine(engine);
        c
    };

    let mut c = build(Some(&path));
    let now = 1_000_000_000i64;
    let slot = now / 1000 / 900;
    let market = |venue: &str, cond: &str, up: &str, down: &str| {
        crate::model::CryptoMarket {
        asset: "BTC".into(),
        condition_id: cond.into(),
        question_id: "q".into(),
        up_token_id: up.into(),
        down_token_id: down.into(),
        up_price: dec!(0.5),
        down_price: dec!(0.5),
        expires_at_ms: (slot + 1) * 900 * 1000,
        round_slot: slot,
        round_duration_sec: 900,
        archive_verdict: false,
        venue: venue.into(),
        neg_risk: false,
        question: "Bitcoin Up or Down: October 16, 2PM ET — resolves to the Binance 1 minute candle for the BTC price at 2pm ET".into(),
    }
    };
    c.engine_on_data(
        crate::engine::DataEvent::RoundMarkets {
            markets: vec![
                market("polymarket", "0xpm", "pm-up", "pm-down"),
                market("kalshi", "KXBTC", "k-up", "k-down"),
            ],
            now_ms: now,
        },
        now,
    );
    let cands = c.unified_candidates();
    let listings: Vec<blitzkrieg_market_api::unified::PlatformListing> = vec![
        serde_json::from_value(cands[0]["a"].clone()).unwrap(),
        serde_json::from_value(cands[0]["b"].clone()).unwrap(),
    ];
    let event = c
        .unified_confirm("Bitcoin Up or Down 2PM ET", "uid:op", listings, 1_000)
        .expect("confirm persists");
    drop(c);

    // A rebuilt core on the same path: no new round arrives. The ledger
    // re-feed lives in `install_engine` (the real boot path), so the
    // rebuild goes through it — exactly what a plugin switch does.
    let mut c2 = build(Some(&path));
    let cfg2 = CoreConfig {
        risk: RiskConfig::default(),
        dry_seed_balance: dec!(1000),
        unified_ledger_path: Some(path.to_string_lossy().into_owned()),
        ..Default::default()
    };
    cfg2.install_engine(&mut c2)
        .expect("an empty enable-set installs");
    assert_eq!(
        c2.unified_list().len(),
        1,
        "the confirmed mapping survived the rebuild"
    );
    assert_eq!(c2.unified_list()[0]["id"], event.id);
    assert!(
        c2.engine
            .as_ref()
            .unwrap()
            .unified_mappings()
            .contains_key(&event.id),
        "install_engine re-fed the ledger into the fresh Engine"
    );
}

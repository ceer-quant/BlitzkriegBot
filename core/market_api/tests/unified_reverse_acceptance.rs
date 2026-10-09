//! Reverse-acceptance gates for the unified event mapping (issue #425).
//!
//! These are the tests the issue REQUIRES to be red on misbehaviour:
//! 1. two different events mapped as one → test fails (the top-priority trap);
//! 2. a settlement discrepancy left unmarked → test fails.
//!
//! They exercise the PUBLIC surface only (`MappingLedger::confirm`, the
//! matcher, `HedgeVerdict`), the same path stage 4 will consume.

use std::path::PathBuf;

use blitzkrieg_market_api::unified::{
    ConfirmError, DiscrepancyKind, EventStatus, HedgeVerdict, ListingStatus, MIN_CANDIDATE_SCORE,
    MIN_CONFIRM_TITLE_SIMILARITY, MappingLedger, PlatformListing, SettlementSource, UnifiedEvent,
    Venue,
};

// ── fixtures ────────────────────────────────────────────────────────────────

/// Same real-world event phrased on two venues with the same boilerplate.
const BTC_PM_TITLE: &str = "Bitcoin Up or Down: October 8, 2PM ET — resolves to the Binance 1 minute candle for the asset price at 2pm ET";
const BTC_KX_TITLE: &str = "Bitcoin up or down at 2pm ET on October 8 — resolves to the Binance 1 minute candle for the asset price at 2pm ET";
/// A DIFFERENT real-world event: same hour, same boilerplate, different asset.
const ETH_KX_TITLE: &str = "Ethereum up or down at 2pm ET on October 8 — resolves to the Binance 1 minute candle for the asset price at 2pm ET";

fn listing(venue: Venue, condition: &str, asset: Option<&str>, title: &str) -> PlatformListing {
    PlatformListing {
        venue,
        condition_id: condition.into(),
        up_token_id: format!("{condition}-up"),
        down_token_id: format!("{condition}-down"),
        asset: asset.map(Into::into),
        title: title.into(),
        settlement_rules: "Resolves per the Binance 1 minute candle at 2pm ET".into(),
        settlement_source: SettlementSource::Uma,
        expires_at_ms: 1_760_000_000_000,
        status: ListingStatus::Active,
    }
}

fn tmpdir(tag: &str) -> PathBuf {
    // Disk-write policy: only ever on the Hard Disk volume.
    let d =
        PathBuf::from("/Volumes/Hard Disk/bk-wt-033/target/unified-reverse-tests").join(format!(
            "{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

// ── reverse acceptance 1: two different events mapped as one ───────────────

/// The automatic matcher must not propose a BTC/ETH pair, even though the
/// titles are ~90% identical boilerplate. (With identical boilerplate the
/// title Jaccard alone is ~0.9 — dilution is exactly why the asset gate
/// exists; this test proves the gate carries weight.)
#[test]
fn reverse_a_matcher_never_pairs_different_assets() {
    let pm = listing(Venue::Polymarket, "pm-btc", Some("bitcoin"), BTC_PM_TITLE);
    let kx_eth = listing(Venue::Kalshi, "kx-eth", Some("ethereum"), ETH_KX_TITLE);

    let cands = MappingLedger::new().candidates(&[pm], &[kx_eth], 10 * 60 * 1000);
    assert!(
        cands.is_empty(),
        "matcher proposed {cands:?} — two different assets were treated as one event"
    );
}

/// Even a human confirm() cannot pair two different assets: the ledger
/// refuses with `AssetMismatch` and stores nothing.
#[test]
fn reverse_b_confirmation_cannot_force_different_assets_together() {
    let mut ledger = MappingLedger::new();
    let pm = listing(Venue::Polymarket, "pm-btc", Some("bitcoin"), BTC_PM_TITLE);
    let kx_eth = listing(Venue::Kalshi, "kx-eth", Some("ethereum"), ETH_KX_TITLE);

    let err = ledger
        .confirm("misclick", "operator", &[pm, kx_eth], 0)
        .unwrap_err();
    assert!(
        matches!(err, ConfirmError::AssetMismatch { .. }),
        "expected AssetMismatch, got {err:?}"
    );
    assert_eq!(ledger.events().count(), 0, "the refused mapping was stored");
}

/// Without asset info, the title gate alone must still refuse a materially
/// different event — proving the title bar carries real weight.
#[test]
fn reverse_c_confirmation_refuses_dissimilar_titles_without_assets() {
    let mut ledger = MappingLedger::new();
    let pm = listing(
        Venue::Polymarket,
        "pm-btc",
        None,
        "Federal Reserve holds rates steady in December?",
    );
    let kx = listing(
        Venue::Kalshi,
        "kx-fed",
        None,
        "Bitcoin closes above $100k on December 31?",
    );

    let err = ledger
        .confirm("misclick", "operator", &[pm, kx], 0)
        .unwrap_err();
    assert!(
        matches!(err, ConfirmError::TitleTooDissimilar { .. }),
        "{err:?}"
    );
    assert_eq!(ledger.events().count(), 0);
}

/// A candidate below the composite bar never reaches the operator's list:
/// same asset, same boilerplate, but the OTHER hour → below title window
/// and expiry window.
#[test]
fn reverse_d_matcher_refuses_same_asset_wrong_hour() {
    let pm = listing(
        Venue::Polymarket,
        "pm-btc-2pm",
        Some("bitcoin"),
        BTC_PM_TITLE,
    );
    let mut kx = listing(Venue::Kalshi, "kx-btc-3pm", Some("bitcoin"), BTC_KX_TITLE);
    kx.title = kx.title.replace("2pm", "3pm");
    kx.expires_at_ms += 3_600_000;

    let cands = MappingLedger::new().candidates(&[pm], &[kx], 10 * 60 * 1000);
    assert!(
        cands.is_empty(),
        "matcher proposed {cands:?} — 2pm and 3pm events treated as one"
    );
}

// ── reverse acceptance 2: settlement discrepancy must be marked ────────────

/// Two venues declaring DIFFERENT settlement sources for the "same" event
/// must produce a `SettlementDiscrepancy` mark — and the stage-4 verdict
/// must be PseudoHedge, never Allowed.
#[test]
fn reverse_e_different_settlement_sources_are_marked_and_blocked() {
    let mut ledger = MappingLedger::new();
    let mut pm = listing(Venue::Polymarket, "pm-btc", Some("bitcoin"), BTC_PM_TITLE);
    pm.settlement_source = SettlementSource::Uma;
    let mut kx = listing(Venue::Kalshi, "kx-btc", Some("bitcoin"), BTC_KX_TITLE);
    kx.settlement_source = SettlementSource::CfBrti;

    let event = ledger.confirm("BTC 2pm", "operator", &[pm, kx], 0).unwrap();
    assert!(
        event
            .discrepancies
            .contains(&DiscrepancyKind::SettlementSource),
        "settlement discrepancy NOT marked on {:?}",
        event
    );
    assert_eq!(
        HedgeVerdict::for_event(&event),
        HedgeVerdict::PseudoHedge(event.clone()),
        "a settlement-discrepancy pair was green-lit for hedging"
    );
}

/// Two `Unknown` sources are two "we don't know"s — never "the same source".
#[test]
fn reverse_f_unknown_source_is_not_a_match() {
    let mut ledger = MappingLedger::new();
    let mut pm = listing(Venue::Polymarket, "pm-btc", Some("bitcoin"), BTC_PM_TITLE);
    pm.settlement_source = SettlementSource::Unknown;
    let mut kx = listing(Venue::Kalshi, "kx-btc", Some("bitcoin"), BTC_KX_TITLE);
    kx.settlement_source = SettlementSource::Unknown;

    let event = ledger.confirm("BTC 2pm", "operator", &[pm, kx], 0).unwrap();
    assert!(
        event
            .discrepancies
            .contains(&DiscrepancyKind::SettlementSource),
        "two Unknown sources passed as the same source"
    );
}

/// Materially different settlement-rules texts (identical sources) must be
/// marked too — the sources can be declared the same while the rules differ.
#[test]
fn reverse_g_conflicting_settlement_rules_are_marked() {
    let mut ledger = MappingLedger::new();
    let mut pm = listing(Venue::Polymarket, "pm-btc", Some("bitcoin"), BTC_PM_TITLE);
    pm.settlement_rules =
        "Resolves YES if the Binance BTCUSDT 1 minute candle close at 2pm ET is above the open"
            .into();
    let mut kx = listing(Venue::Kalshi, "kx-btc", Some("bitcoin"), BTC_KX_TITLE);
    kx.settlement_rules = "Resolves YES per the official CF Bitcoin BRTI value at 2pm ET".into();

    let event = ledger.confirm("BTC 2pm", "operator", &[pm, kx], 0).unwrap();
    assert!(
        event
            .discrepancies
            .contains(&DiscrepancyKind::SettlementRules),
        "rules discrepancy NOT marked: {event:?}"
    );
}

/// An operator hand-editing the persisted file to strip discrepancies must
/// not survive reload: `load` revalidates from the listings.
#[test]
fn reverse_h_persisted_file_cannot_smuggle_a_stripped_discrepancy() {
    let dir = tmpdir("strip");
    let path = dir.join("mappings.json");
    let mut ledger = MappingLedger::load(&path);
    let mut pm = listing(Venue::Polymarket, "pm-btc", Some("bitcoin"), BTC_PM_TITLE);
    pm.settlement_source = SettlementSource::Uma;
    let mut kx = listing(Venue::Kalshi, "kx-btc", Some("bitcoin"), BTC_KX_TITLE);
    kx.settlement_source = SettlementSource::CfBrti;
    ledger.confirm("BTC 2pm", "operator", &[pm, kx], 0).unwrap();
    drop(ledger);

    // Hand-edit: empty the discrepancies array behind the core's back.
    let text = std::fs::read_to_string(&path).unwrap();
    let mut doc: serde_json::Value = serde_json::from_str(&text).unwrap();
    for e in doc["events"].as_array_mut().unwrap() {
        e["discrepancies"] = serde_json::json!([]);
    }
    std::fs::write(&path, doc.to_string()).unwrap();

    let reloaded = MappingLedger::load(&path);
    let event = reloaded.events().next().unwrap();
    assert!(
        event
            .discrepancies
            .contains(&DiscrepancyKind::SettlementSource),
        "reload accepted a discrepancy-stripped file — persistence is authoritative over the model"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

// ── lifecycle: single-leg semantics ────────────────────────────────────────

#[test]
fn lifecycle_one_leg_halts_degrades_to_single_leg_and_verdict_follows() {
    let mut ledger = MappingLedger::new();
    let pm = listing(Venue::Polymarket, "pm-btc", Some("bitcoin"), BTC_PM_TITLE);
    let kx = listing(Venue::Kalshi, "kx-btc", Some("bitcoin"), BTC_KX_TITLE);
    let event = ledger.confirm("BTC 2pm", "operator", &[pm, kx], 0).unwrap();
    assert_eq!(event.status, EventStatus::Paired);
    assert!(matches!(
        HedgeVerdict::for_event(&event),
        HedgeVerdict::Allowed(_)
    ));

    // Kalshi halts while Polymarket keeps trading.
    let mut degraded = event.clone();
    degraded.listings.get_mut(&Venue::Kalshi).unwrap().status = ListingStatus::Halted;
    degraded.revalidate();
    assert_eq!(degraded.status, EventStatus::SingleLegAvailable);
    assert_eq!(
        HedgeVerdict::for_event(&degraded),
        HedgeVerdict::NotHedgeable,
        "single-leg availability was still hedgeable"
    );
}

// ── the happy path the acceptance list asks for: ≥3 venues ─────────────────

#[test]
fn three_venue_mapping_confirms_audits_and_persists() {
    let dir = tmpdir("three");
    let path = dir.join("mappings.json");
    let mut ledger = MappingLedger::load(&path);
    let pm = listing(Venue::Polymarket, "pm-btc", Some("bitcoin"), BTC_PM_TITLE);
    let kx = listing(Venue::Kalshi, "kx-btc", Some("bitcoin"), BTC_KX_TITLE);
    let pf = listing(Venue::PredictFun, "pf-btc", Some("bitcoin"), BTC_PM_TITLE);

    let event: UnifiedEvent = ledger
        .confirm(
            "BTC 2pm ET",
            "operator-7",
            &[pm.clone(), kx.clone(), pf.clone()],
            1234,
        )
        .unwrap();
    assert_eq!(event.listings.len(), 3);
    assert!(event.discrepancies.is_empty());
    assert!(matches!(
        HedgeVerdict::for_event(&event),
        HedgeVerdict::Allowed(_)
    ));

    // Same-event candidate across venues shows up for the operator UI.
    let cands = ledger.candidates(&[pm], &[kx], 10 * 60 * 1000);
    assert_eq!(cands.len(), 1);
    assert!(cands[0].score >= MIN_CANDIDATE_SCORE);
    assert!(
        cands[0].title_similarity >= MIN_CONFIRM_TITLE_SIMILARITY - f64::EPSILON,
        "matcher and confirmation gate must agree on the title bar"
    );

    // Audit trail: who confirmed which mapping when, persisted.
    let ledger2 = MappingLedger::load(&path);
    assert_eq!(ledger2.events().count(), 1);
    let audit = ledger2.audit_trail();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].actor, "operator-7");
    assert_eq!(audit[0].at_ms, 1234);
    assert_eq!(audit[0].event_id, event.id);
    assert_eq!(audit[0].listing_count, 3);
    std::fs::remove_dir_all(&dir).unwrap();
}

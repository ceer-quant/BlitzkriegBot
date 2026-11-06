//! The #843 P1 capability-model goldens, as integration tests.
//!
//! The unit tests inside `capability.rs` pin the module's own shapes; these
//! are the CROSS-surface pins a refactor could otherwise quietly weaken:
//!
//! * the declaration map is STABLE against today's wire (the three shipped
//!   payload spellings for gate exemptions render from the shared keys);
//! * the three-state verdict survives BOTH encodings and a malformed
//!   out-of-range int degrades to "no declaration" (never a false Trail);
//! * the fail-closed grant: default-nothing, host posture exactly equal to
//!   the shipped visibility, narrowing denials, and period bounds;
//! * the manifest gate value is the same "1.0" the §6.4 docs describe.

use blitzkrieg_strategy_api::capability::{
    CapabilityGrantBuilder, DECLARATION_MAP, DEFAULT_GRANTED_SURFACES, DataSurface, DeclCap,
    GATE_MOMENTUM_KEY, GATE_TIMING_FLOOR_KEY, GATE_TIMING_KEY, MANIFEST_API_VERSION,
    STRATEGY_CAPABILITY_MODEL_VERSION, TrailingExitVerdict, host_grant,
};

/// The C-ABI gate-exemptions payload for a WIDE opt-out, rendered from the
/// shared keys — byte-identical to what `GateExemptions::to_json` produced
/// before this module existed (the payload is the frozen wire).
#[test]
fn gate_wide_payload_rendered_from_shared_keys() {
    let wide: serde_json::Map<String, serde_json::Value> = vec![
        (GATE_TIMING_KEY.to_string(), serde_json::Value::Bool(true)),
        (GATE_MOMENTUM_KEY.to_string(), serde_json::Value::Bool(true)),
    ]
    .into_iter()
    .collect();
    let text = serde_json::Value::Object(wide).to_string();
    assert_eq!(
        text, r#"{"momentum":true,"timing":true}"#,
        "the C-ABI payload's KEY SPELLING is frozen (order free, words exact)"
    );
}

/// The floor key joins the same payload for a D-31 declaration, spelled by
/// the shared constant alone.
#[test]
fn gate_floor_payload_uses_the_shared_key() {
    let v = serde_json::json!({
        GATE_TIMING_KEY: true,
        GATE_TIMING_FLOOR_KEY: 30,
    });
    assert_eq!(v["timing"], serde_json::Value::Bool(true));
    assert_eq!(v["timing_min_time_left_sec"], serde_json::json!(30));
    let parsed: serde_json::Value = serde_json::from_str(&v.to_string()).expect("round trip");
    assert_eq!(
        parsed.get(GATE_TIMING_FLOOR_KEY).and_then(|n| n.as_i64()),
        Some(30)
    );
}

/// The embroidered conformance matrix: BOTH runtimes' declaration spellings
/// for every capability, asserted per-row (the table the issue's "Lua and C
/// ABI pass the same capability conformance matrix" names).
#[test]
fn capability_conformance_matrix_every_row_crosses_both_sides() {
    for row in DECLARATION_MAP {
        match row.cap {
            // The map rows the wire has both spellings for:
            DeclCap::Modes => {
                assert_eq!(
                    row.c_abi_symbol,
                    Some(b"bk_strategy_declare_modes\0".as_slice())
                );
                assert_eq!(row.lua_manifest_key, Some("modes"));
            }
            DeclCap::GateExemptions => {
                assert_eq!(
                    row.c_abi_symbol,
                    Some(b"bk_strategy_gate_exemptions\0".as_slice())
                );
                assert_eq!(row.lua_manifest_key, Some("gate_exemptions"));
            }
            DeclCap::EvolvableKnobs => {
                assert_eq!(
                    row.c_abi_symbol,
                    Some(b"bk_strategy_evolvable_knobs\0".as_slice())
                );
                assert_eq!(row.lua_manifest_key, Some("tunables"));
            }
            DeclCap::HoldsToSettlement => {
                assert_eq!(
                    row.c_abi_symbol,
                    Some(b"bk_strategy_settlement_holds\0".as_slice())
                );
                assert_eq!(row.lua_manifest_key, Some("holds_to_settlement"));
            }
            DeclCap::TrailingExit => {
                assert_eq!(
                    row.c_abi_symbol,
                    Some(b"bk_strategy_trailing_exit\0".as_slice())
                );
                assert_eq!(row.lua_manifest_key, Some("trailing_exit"));
            }
            DeclCap::AtomicBatch => {
                assert_eq!(
                    row.c_abi_symbol,
                    Some(b"bk_strategy_atomic_batch\0".as_slice())
                );
                assert_eq!(row.lua_manifest_key, Some("atomic_batch"));
            }
            DeclCap::EntryMaker => {
                // Manifest key only, until an ABI v3 ever adds a symbol.
                assert_eq!(row.c_abi_symbol, None);
                assert_eq!(row.lua_manifest_key, Some("entry_maker"));
            }
            // The C-only rows (the Lua side answers through `bk.*`
            // projections, not a manifest key):
            DeclCap::BindEvalCtx => {
                assert_eq!(
                    row.c_abi_symbol,
                    Some(b"bk_strategy_bind_eval_ctx\0".as_slice())
                );
                assert_eq!(row.lua_manifest_key, None);
            }
            DeclCap::ConfigView => {
                assert_eq!(
                    row.c_abi_symbol,
                    Some(b"bk_strategy_config_view\0".as_slice())
                );
                assert_eq!(row.lua_manifest_key, None);
            }
            DeclCap::OnKline => {
                assert_eq!(row.c_abi_symbol, Some(b"bk_strategy_on_kline\0".as_slice()));
                assert_eq!(row.lua_manifest_key, None);
            }
            DeclCap::EntryPair => {
                assert_eq!(
                    row.c_abi_symbol,
                    Some(b"bk_strategy_entry_pair\0".as_slice())
                );
                assert_eq!(row.lua_manifest_key, None);
            }
        }
    }
}

/// A malformed verdict int can never widen: anything outside {-1, 0, 1} is
/// "no declaration" — the fail-closed test the issue's "reject incompatible
/// packages fail-closed" names, at the verdict level.
#[test]
fn verdict_out_of_range_is_never_a_declaration() {
    for bad in [i32::MIN, -2, 2, 3, i32::MAX] {
        assert_eq!(
            TrailingExitVerdict::from_abi_int(bad),
            TrailingExitVerdict::FollowKernel,
            "{bad} reads as no-declaration"
        );
        // And the round-trip of the decoded result is stable.
        assert_eq!(
            TrailingExitVerdict::from_abi_int(TrailingExitVerdict::from_abi_int(bad).to_abi_int()),
            TrailingExitVerdict::FollowKernel
        );
    }
}

/// The grant is fail-closed by construction AND the host posture equals the
/// shipped default surface list, so the visibility story is (a) typed and
/// (b) honest.
#[test]
fn grant_fail_closed_and_host_posture() {
    let empty = CapabilityGrantBuilder::new().build();
    assert_eq!(
        empty.granted_surfaces(),
        Vec::<&str>::new(),
        "deny by default"
    );
    assert_eq!(empty.account_id(), "");

    let host = host_grant("default");
    assert_eq!(host.account_id(), "default");
    let names = host.granted_surfaces();
    assert_eq!(names.len(), DEFAULT_GRANTED_SURFACES.len());
    for s in [
        DataSurface::Books,
        DataSurface::OwnHoldings,
        DataSurface::AccountDisplay,
        DataSurface::Params,
    ] {
        assert!(host.is_granted(s));
    }
    // Narrowing: a grant without AccountDisplay denies the account view.
    let narrowed = CapabilityGrantBuilder::new()
        .account("acct-9")
        .grant(DataSurface::Books)
        .grant(DataSurface::OwnHoldings)
        .build();
    assert!(narrowed.is_granted(DataSurface::Books));
    assert!(
        !narrowed.is_granted(DataSurface::AccountDisplay),
        "not granted = denied"
    );
    assert_eq!(narrowed.account_id(), "acct-9");
}

/// Period bounds ride the grant (the issue's "undeclared period" deny
/// vocabulary: the grant is the place a period floor/ceiling is STATED).
#[test]
fn grant_carries_period_bounds() {
    let g = CapabilityGrantBuilder::new()
        .period_bounds(Some(15), Some(240))
        .build();
    assert_eq!(g.period_min_time_left_sec(), Some(15));
    assert_eq!(g.period_max_time_left_sec(), Some(240));
    // The receipt line carries them (operator-visible).
    let rendered = g.to_string();
    assert!(
        rendered.contains("period_min_time_left_sec=15"),
        "{rendered}"
    );
    assert!(
        rendered.contains("period_max_time_left_sec=240"),
        "{rendered}"
    );
}

/// The model version pair is (1, "1.0") — bumped only with a disclosed PR.
#[test]
fn version_constants_freeze() {
    assert_eq!(STRATEGY_CAPABILITY_MODEL_VERSION, 1);
    assert_eq!(MANIFEST_API_VERSION, "1.0");
}

/// The variance guard: a map row added WITHOUT a variant must fail here, so
/// `DeclCap` and the map can never disagree about size. (Rust guarantees
/// the enum/table pairing by construction; this test pins the NUMBER so a
/// table truncation is visible in diff review.)
#[test]
fn declaration_map_has_eleven_rows() {
    assert_eq!(DECLARATION_MAP.len(), 11);
}

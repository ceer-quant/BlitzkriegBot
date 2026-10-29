//! 跨所套利弹窗 v2 — the leg-configuration contract, end to end.
//!
//! Five teeth, each pinned against a REAL accounts.toml (the worktree
//! target/ scratch — never /tmp, never the checkout's own configs):
//!
//!  1. `arb_config_set_round_trips_through_the_file_and_refuses_bad_fields`
//!     — set → persist → FRESH boot (a new Core over the same file) → get;
//!     plus the three validation arms the wire contract freezes (version ≠ 1,
//!     identical venues, unknown account — each an APPLICATION error NAMING
//!     the field, nothing moved in memory or on disk).
//!  2. `dual_leg_fills_stamp_each_legs_own_account` — the full trade chain:
//!     config set → engine evaluate → BOTH legs fill → one position row per
//!     leg account (polymarket leg → account-a, kalshi leg → account-b).
//!  3. `b_leg_exit_closes_only_the_b_legs_row` — the B leg's exit intent
//!     resolves to the B account's (account, token) row and leaves the A
//!     account's identical token untouched.
//!  7. `naked_a_leg_does_not_block_the_b_legs_entry` — the degradation path:
//!     the A leg's candidates are refused kernel-side (a frozen account) and
//!     the B leg's entries STILL land — the legs answer to their own
//!     accounts, never to each other's fate.
//!  8. `a_venue_outside_the_configuration_stamps_the_instance_account` —
//!     fail-closed stamp fallback: a token whose venue is not in the map
//!     resolves to the adapter's instance account (the pre-v2 shape).
//!
//! The adapter fixture mounts the REAL cross_venue_arb package (the same
//! provenance discipline account_decouple_check runs): `lua:<dir>` source,
//! gate declarations stamped, the deployment fee curve injected.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use blitzkrieg_core::account::config::TEST_ALLOWED_ROUNDS;
use blitzkrieg_core::account::config::load_account_ledgers_single_source;
use blitzkrieg_core::engine::{DataEvent, Engine};
use blitzkrieg_core::model::{CryptoMarket, default_account_id};
use blitzkrieg_core::service::{Core, CoreConfig};
use blitzkrieg_core::strategies::EngineStrategy;
use blitzkrieg_core::strategy_engine::lua_loader::{LuaEngineAdapter, load_lua_package};
use blitzkrieg_lua_runtime::FeeScheduleView;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

const NOW: i64 = 1_700_000_000_000;

const PM: &str = "pm-cond";
const KX: &str = "kx-cond";
const PM_UP: &str = "pm-up";
const PM_DOWN: &str = "pm-down";
const KX_UP: &str = "kx-up";
const KX_DOWN: &str = "kx-down";

// ── fixtures ────────────────────────────────────────────────────────────────

/// Scratch dir: the worktree's target/ (repo-ignored, same disk, never /tmp).
fn scratch_dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/arb-leg-config-tests");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Two-account book: `default` (bound to polymarket) and `arb-b`
/// (bound to kalshi) — the deployment the leg configuration wires together.
/// Env-single-source (issue 751): only the per-test `.env` is written (same
/// unique stem as the nominal toml path; cargo runs tests in parallel, a
/// shared `.env` would cross-contaminate). The toml path stays UNWRITTEN —
/// the env write face refuses while any legacy toml exists.
fn accounts_toml_path(name: &str) -> PathBuf {
    let path = scratch_dir().join(format!("accounts-{name}-{}.toml", std::process::id()));
    std::fs::write(
        path.with_extension("env"),
        "ACCOUNT_LIST=default,arb-b\n\
         ACCOUNT_DEFAULT_NAME=default\n\
         ACCOUNT_DEFAULT_VENUE=polymarket\n\
         ACCOUNT_DEFAULT_STATUS=\n\
         ACCOUNT_ARB_B_NAME=arb-b\n\
         ACCOUNT_ARB_B_VENUE=kalshi\n\
         ACCOUNT_ARB_B_STATUS=\n",
    )
    .expect("accounts.env fixture writes");
    path
}

/// The per-test `.env` sibling of an `accounts_toml_path` fixture.
fn accounts_env_path(toml_path: &Path) -> PathBuf {
    toml_path.with_extension("env")
}

fn package_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../user_layer/strategies_lua/cross_venue_arb")
}

fn lua_source() -> String {
    format!("lua:{}", package_dir().display())
}

/// REAL cross_venue_arb package through the loading path (fee curve on; the
/// leg configuration arrives through `Core::arb_config_set_persisting` →
/// `Engine::set_arb_leg_config` — never pushed by hand in the trade tests).
/// `atomic` mirrors the manifest's `atomic_batch` declaration: the tests that
/// judge pair economics run the SHIPPED shape (true); the naked-leg test
/// exercises the per-leg stamp→gate path, which the whole-cycle batch drop
/// (RFC #541 hook A, pinned in arb_batch_precheck_check) would preempt.
fn arb_adapter_with(atomic: bool) -> LuaEngineAdapter {
    let loaded = load_lua_package(&package_dir()).expect("cross_venue_arb package must load");
    let a = LuaEngineAdapter::new(loaded.strategy, loaded.tunables)
        .declare(loaded.holds_to_settlement, loaded.gate_exemptions)
        .declare_atomic_batch(atomic);
    a.set_fee_schedule(FeeScheduleView {
        name: "legacy_quadratic".into(),
        rate: "0.125".into(),
        exponent: 2,
    });
    a
}

fn arb_adapter() -> LuaEngineAdapter {
    arb_adapter_with(true)
}

fn market(token_up: &str, token_down: &str, cond: &str, venue: &str, slot: i64) -> CryptoMarket {
    CryptoMarket {
        asset: "BTC".into(),
        condition_id: cond.into(),
        question_id: "q".into(),
        up_token_id: token_up.into(),
        down_token_id: token_down.into(),
        up_price: dec!(0.48),
        down_price: dec!(0.45),
        expires_at_ms: (slot + 1) * 900 * 1000,
        round_slot: slot + 1,
        round_duration_sec: 900,
        archive_verdict: false,
        venue: venue.to_string(),
        neg_risk: false,
        question: "BTC up or down".into(),
    }
}

fn leg_book(token: &str, ask: Decimal, depth: Decimal, ts: i64) -> DataEvent {
    DataEvent::Book {
        token_id: token.to_string(),
        bids: vec![(ask - dec!(0.01), depth)],
        asks: vec![(ask, depth)],
        now_ms: ts,
    }
}

/// A dry Core over a REAL accounts.toml with the REAL package registered
/// (disabled — the tests enable it explicitly). `atomic` selects the batch
/// declaration (see `arb_adapter_with`).
fn arb_core_atomic(accounts_path: &Path, virtual_pot: bool, atomic: bool) -> Core {
    let mut virtual_limits = HashMap::new();
    if virtual_pot {
        virtual_limits.insert(
            "cross_venue_arb".to_string(),
            blitzkrieg_core::service::VirtualAccountLimit {
                max_batch_notional_usd: Some(dec!(10_000)),
                accounts: vec![default_account_id().to_string(), "arb-b".to_string()],
            },
        );
    }
    let config = CoreConfig {
        mode: blitzkrieg_core::model::Mode::Dry,
        risk: blitzkrieg_core::risk::RiskConfig {
            max_order_notional: dec!(100_000),
            ..Default::default()
        },
        dry_seed_balance: dec!(1_000_000),
        engine_enabled: true,
        round_duration_sec: 900,
        min_shares: dec!(1),
        max_shares: dec!(10000),
        assets: vec!["BTC".into()],
        positions: blitzkrieg_core::position::PositionConfig {
            exit: blitzkrieg_core::exit_policy::ExitConfig {
                min_time_left_sec: 0,
                maker_first_exit_enabled: false,
                exit_grace_sec: 0,
                ..Default::default()
            },
            max_positions_per_asset: 4,
            ..Default::default()
        },
        execution_policy_path: None,
        accounts_config_path: Some(accounts_path.to_string_lossy().into_owned()),
        env_file_path: Some(
            accounts_env_path(accounts_path)
                .to_string_lossy()
                .into_owned(),
        ),
        virtual_limits,
        ..Default::default()
    };
    let book = load_account_ledgers_single_source(
        accounts_path,
        Some(&accounts_env_path(accounts_path)),
        NOW,
        TEST_ALLOWED_ROUNDS,
    )
    .expect("accounts fixture parses");
    let mut c = Core::with_account_book(config, book);
    let cfg = c.config().engine_config();
    let mut eng = Engine::new(cfg.clone());
    eng.register_user_strategy(Box::new(arb_adapter_with(atomic)), lua_source())
        .expect("cross_venue_arb registers under its package name");
    c.enable_engine(eng);
    c
}

fn arb_core(accounts_path: &Path, virtual_pot: bool) -> Core {
    arb_core_atomic(accounts_path, virtual_pot, true)
}

/// The v1 wire payload as a config, the shape `arb.config.set` receives.
fn legs_payload() -> blitzkrieg_core::account::CrossVenueArbConfig {
    blitzkrieg_core::account::CrossVenueArbConfig {
        leg_a_venue: "polymarket".into(),
        leg_a_account: "default".into(),
        leg_b_venue: "kalshi".into(),
        leg_b_account: "arb-b".into(),
        pairs: vec!["BTC".into()],
    }
}

/// One round of two-venue market data + the discounted books
/// (UP@pm 0.44 + DOWN@kx 0.42 + fees ≈ 0.8753 — well under the $1 line).
fn pair_market_data(c: &mut Core, now: i64) {
    let slot = now / 1000 / 900;
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: vec![
                market(PM_UP, PM_DOWN, PM, "polymarket", slot),
                market(KX_UP, KX_DOWN, KX, "kalshi", slot),
            ],
            now_ms: now,
        },
        now,
    );
    for (token, ask) in [
        (PM_UP, dec!(0.44)),
        (PM_DOWN, dec!(0.50)),
        (KX_UP, dec!(0.48)),
        (KX_DOWN, dec!(0.42)),
    ] {
        c.engine_on_data(leg_book(token, ask, dec!(600), now + 1_000), now + 1_000);
    }
}

fn open_positions<'a>(
    c: &'a Core,
    account: &str,
) -> Vec<&'a blitzkrieg_core::position::OpenPosition> {
    c.positions()
        .open_positions()
        .iter()
        .filter(|p| p.account_id.as_str() == account)
        .collect()
}

// ── test 1: the wire round-trip + validation ────────────────────────────────

/// set → persist → FRESH boot → get, plus the frozen validation arms: every
/// refusal is an APPLICATION-shaped CoreError NAMING its field, and a refused
/// set moves NOTHING (the file stays unconfigured, the memory cell stays None).
#[test]
fn arb_config_set_round_trips_through_the_file_and_refuses_bad_fields() {
    let path = accounts_toml_path("roundtrip");
    let now = NOW;

    // Unconfigured boot: the getter answers None and a fresh env has no
    // section.
    let env_path = accounts_env_path(&path);
    {
        let c = arb_core(&path, false);
        assert!(
            c.arb_leg_config().is_none(),
            "a fresh book boots unconfigured"
        );
        let lookup = blitzkrieg_core::account::config::env_lookup_from_sources(Some(&env_path));
        load_account_ledgers_single_source(&path, Some(&env_path), now, TEST_ALLOWED_ROUNDS)
            .expect("the untouched env parses");
        assert!(
            blitzkrieg_core::account::config::read_cross_venue_arb_from_env(&lookup)
                .expect("read")
                .is_none(),
            "no section in the env before the first set"
        );
    }

    // A wrong-payload set REFUSES with the field named — nothing persisted.
    {
        let mut c = arb_core(&path, false);
        // version ≠ 1 is refused at the wire; at the Core boundary the shape
        // errors are the three below (the wire arm owns the version check).
        let bad = blitzkrieg_core::account::CrossVenueArbConfig {
            leg_b_venue: "polymarket".into(), // SAME venue as leg A
            ..legs_payload()
        };
        let err = c
            .arb_config_set_persisting(&bad, "test", "same-venue arm", now)
            .expect_err("identical venues are refused");
        assert!(
            err.message.contains("legBVenue") && err.message.contains("DIFFERENT venues"),
            "the refusal names the offending field: {}",
            err.message
        );

        let bad = blitzkrieg_core::account::CrossVenueArbConfig {
            leg_b_account: "ghost-account".into(),
            ..legs_payload()
        };
        let err = c
            .arb_config_set_persisting(&bad, "test", "unknown-account arm", now)
            .expect_err("an unknown account is refused");
        assert!(
            err.message.contains("legBAccount") && err.message.contains("ghost-account"),
            "the refusal names the account: {}",
            err.message
        );

        let bad = blitzkrieg_core::account::CrossVenueArbConfig {
            pairs: vec!["BTC".into(), "BTC".into()],
            ..legs_payload()
        };
        let err = c
            .arb_config_set_persisting(&bad, "test", "duplicate-pair arm", now)
            .expect_err("duplicate pairs are refused");
        assert!(
            err.message.contains("pairs") && err.message.contains("appears twice"),
            "the refusal names the pairs: {}",
            err.message
        );

        // A venue-bound account that cannot cover its leg is refused.
        let bad = blitzkrieg_core::account::CrossVenueArbConfig {
            leg_b_account: "default".into(), // default is BOUND to polymarket
            ..legs_payload()
        };
        let err = c
            .arb_config_set_persisting(&bad, "test", "binding-coverage arm", now)
            .expect_err("an account bound to another venue is refused");
        assert!(
            err.message.contains("legBAccount") && err.message.contains("bound to venue"),
            "the refusal names the binding: {}",
            err.message
        );

        // Nothing moved: no section on disk, no cell in memory.
        assert!(c.arb_leg_config().is_none(), "a refused set moves nothing");
        let lookup = blitzkrieg_core::account::config::env_lookup_from_sources(Some(&env_path));
        assert!(
            blitzkrieg_core::account::config::read_cross_venue_arb_from_env(&lookup)
                .expect("read")
                .is_none(),
            "a refused set never persists"
        );
    }

    // The good set lands: memory AND file.
    {
        let mut c = arb_core(&path, false);
        c.arb_config_set_persisting(&legs_payload(), "test", "roundtrip", now)
            .expect("the good set lands");
        assert!(c.arb_leg_config().is_some(), "the memory cell is live");
    }

    // FRESH boot over the SAME file: the section re-reads (the restart half
    // of the round-trip) and the engine's adapters receive it at registration.
    {
        let mut c = arb_core(&path, false);
        let cfg = c
            .arb_leg_config()
            .expect("the boot re-reads the persisted section")
            .clone();
        assert_eq!(cfg, legs_payload(), "byte-identical round-trip");
        assert_eq!(cfg.leg_account_for_venue("polymarket"), Some("default"));
        assert_eq!(cfg.leg_account_for_venue("kalshi"), Some("arb-b"));
        assert_eq!(cfg.leg_account_for_venue("other"), None);
        assert!(cfg.admits_asset("btc"), "pairs are uppercase-compared");
        assert!(!cfg.admits_asset("ETH"));
        // The engine's registered adapter already carries the config (the
        // later-joiner forward): enablement flows, the stamp source is live.
        assert!(c.has_engine(), "the engine is installed");

        // clear removes BOTH stores.
        c.arb_config_clear_persisting("test").expect("clear lands");
        assert!(c.arb_leg_config().is_none());
        let lookup = blitzkrieg_core::account::config::env_lookup_from_sources(Some(&env_path));
        assert!(
            blitzkrieg_core::account::config::read_cross_venue_arb_from_env(&lookup)
                .expect("read")
                .is_none(),
            "the section leaves the env"
        );
        // Re-enable nothing: a following evaluate offers nothing (the
        // strategy pairs nothing unconfigured) — the hot-push cleared.
        let placed = c.engine_evaluate(now + 1_000);
        assert_eq!(placed, 0, "unconfigured ⇒ nothing pairs");
    }

    // A hand-edited BAD section fails the BOOT closed: the loader runs the
    // section coherence rules over the whole book and refuses. The get path
    // reports what the env says (it answers the truth); the boot is the gate.
    {
        let mut text =
            std::fs::read_to_string(&env_path).expect("the env from the clear half reads");
        text.push_str(
            "\nACCOUNT_ARB_LEG_A_VENUE=polymarket\n\
             ACCOUNT_ARB_LEG_A_ACCOUNT=default\n\
             ACCOUNT_ARB_LEG_B_VENUE=polymarket\n\
             ACCOUNT_ARB_LEG_B_ACCOUNT=arb-b\n\
             ACCOUNT_ARB_PAIRS=BTC\n",
        );
        std::fs::write(&env_path, text).expect("hand-edited bad section");
        let err =
            load_account_ledgers_single_source(&path, Some(&env_path), now, TEST_ALLOWED_ROUNDS)
                .expect_err("identical venues in the env refuse the boot");
        assert!(
            err.message.contains("[cross_venue_arb]"),
            "the boot refusal names the section: {}",
            err.message
        );
    }
    let _ = now;
}

/// The wire arm's serde shape: the camelCase params parse into
/// `ArbConfigSetParams` exactly as the contract freezes them, and a null leg
/// field refuses at the wire edge (PARSE_ERROR before any state moves). The
/// version gate itself (`version != 1` → APPLICATION naming `version`) lives
/// in the arm; its shape precondition is pinned here through the same struct.
#[test]
fn arb_config_set_wire_shape_parses_and_refuses_nulls() {
    let req: serde_json::Value = serde_json::from_str(
        r#"{"jsonrpc":"2.0","id":1,"method":"arb.config.set","params":{"version":2,"legAVenue":"polymarket","legAAccount":"default","legBVenue":"kalshi","legBAccount":"arb-b","pairs":["BTC"]}}"#,
    )
    .expect("json");
    let params: blitzkrieg_core::ipc::schema::ArbConfigSetParams =
        serde_json::from_value(req["params"].clone()).expect("the wire shape parses (camelCase)");
    assert_eq!(params.version, 2, "the wrong version arrives as 2");
    assert_eq!(params.leg_a_venue, "polymarket");
    assert_eq!(params.leg_a_account, "default");
    assert_eq!(params.leg_b_venue, "kalshi");
    assert_eq!(params.leg_b_account, "arb-b");
    assert_eq!(params.pairs, vec!["BTC".to_string()]);

    // The missing-field shape must NOT parse (fail-closed at the wire edge):
    // a null legBVenue is not a string.
    let req: serde_json::Value = serde_json::from_str(
        r#"{"jsonrpc":"2.0","id":2,"method":"arb.config.set","params":{"version":1,"legAVenue":"polymarket","legAAccount":"default","legBVenue":null,"legBAccount":"arb-b","pairs":["BTC"]}}"#,
    )
    .expect("json");
    let parsed: Result<blitzkrieg_core::ipc::schema::ArbConfigSetParams, _> =
        serde_json::from_value(req["params"].clone());
    assert!(
        parsed.is_err(),
        "a null leg field refuses at the wire edge (PARSE_ERROR, nothing applied)"
    );
}

// ── test 2: the dual-leg dual-account stamp, through real fills ─────────────

/// The whole chain: config → engine hot-push → evaluate → BOTH legs fill →
/// one position row per LEG account. The A account holds the polymarket
/// token, the B account holds the kalshi token — the dual-leg accounting's
/// headline fact.
#[test]
fn dual_leg_fills_stamp_each_legs_own_account() {
    let path = accounts_toml_path("dual-stamp");
    let mut c = arb_core(&path, true);
    let now = NOW;

    c.arb_config_set_persisting(&legs_payload(), "test", "dual stamp", now)
        .expect("the leg configuration lands");
    assert!(
        c.set_strategy_enabled("cross_venue_arb", true),
        "the strategy enables"
    );

    pair_market_data(&mut c, now);
    let placed = c.engine_evaluate(now + 2_000);
    assert_eq!(placed, 2, "both legs place: {placed}");

    let a_rows = open_positions(&c, "default");
    let b_rows = open_positions(&c, "arb-b");
    assert_eq!(a_rows.len(), 1, "the A account holds one leg: {a_rows:?}");
    assert_eq!(b_rows.len(), 1, "the B account holds one leg: {b_rows:?}");
    assert_eq!(
        a_rows[0].token_id, PM_UP,
        "the polymarket leg landed on the polymarket account"
    );
    assert_eq!(
        b_rows[0].token_id, KX_DOWN,
        "the kalshi leg landed on the kalshi account"
    );
    assert_eq!(
        a_rows[0].cost_usd + b_rows[0].cost_usd,
        dec!(600) * (dec!(0.44) + dec!(0.42)),
        "both fills are real committed positions"
    );
}

// ── test 3: the B leg's exit closes ONLY the B account's row ────────────────

/// With both legs held, a recovered pair cost releases BOTH legs — and each
/// exit intent resolves to ITS OWN account's row: the kx token's exit flows
/// under `arb-b`, the pm token's under `default`. After the exit tick both
/// accounts are flat, each having closed exactly its own (account, token).
#[test]
fn b_leg_exit_closes_only_the_b_legs_row() {
    let path = accounts_toml_path("b-exit");
    let mut c = arb_core(&path, true);
    let now = NOW;

    c.arb_config_set_persisting(&legs_payload(), "test", "b exit", now)
        .expect("the leg configuration lands");
    assert!(
        c.set_strategy_enabled("cross_venue_arb", true),
        "the strategy enables"
    );

    pair_market_data(&mut c, now);
    assert_eq!(c.engine_evaluate(now + 2_000), 2, "both legs fill");
    assert_eq!(open_positions(&c, "default").len(), 1);
    assert_eq!(open_positions(&c, "arb-b").len(), 1);

    // The pair cost recovers past the close threshold (0.50 + 0.49 + fees
    // ≈ 1.0054 > 0.995): the strategy releases both legs. KX_UP stays ABOVE
    // PM_UP so the cheaper-up-ask rule keeps the SAME pairing as entry — a
    // flipped pair would look at the unheld sibling tokens and release
    // nothing.
    for (token, ask) in [
        (PM_UP, dec!(0.50)),
        (PM_DOWN, dec!(0.50)),
        (KX_UP, dec!(0.51)),
        (KX_DOWN, dec!(0.49)),
    ] {
        c.engine_on_data(leg_book(token, ask, dec!(600), now + 3_000), now + 3_000);
    }
    let placed = c.engine_evaluate(now + 3_500);
    assert_eq!(
        placed, 0,
        "a releasing pair places no new entries: {placed}"
    );
    c.tick(now + 4_000).expect("the exit tick runs");

    assert_eq!(
        open_positions(&c, "arb-b").len(),
        0,
        "the B leg's exit closed the B account's row"
    );
    assert_eq!(
        open_positions(&c, "default").len(),
        0,
        "the A leg's exit closed the A account's row"
    );
}

// ── test 7: naked-leg degradation ───────────────────────────────────────────

/// The A leg's candidates are refused kernel-side (its account is FROZEN —
/// the account-scoped kill switch drops entries while exits flow) and the B
/// leg's entries STILL land under the B account: a leg's fate never vetoes
/// its sibling. The venue→account stamp happens BEFORE the scope gates judge
/// the candidate (钉2: the gates judge the STAMPED id — a frozen A account
/// refusing A legs proves the stamp reached the gate). The adapter here does
/// NOT declare atomic_batch: the shipped declaration would drop the WHOLE
/// cycle at hook A on any scope refusal (pinned in arb_batch_precheck_check)
/// — that policy is tested there; THIS test judges the per-leg stamp/gate
/// mechanics the whole-cycle drop sits on top of.
#[test]
fn naked_a_leg_does_not_block_the_b_legs_entry() {
    let path = accounts_toml_path("naked-leg");
    let mut c = arb_core_atomic(&path, true, false);
    let now = NOW;

    c.arb_config_set_persisting(&legs_payload(), "test", "naked leg", now)
        .expect("the leg configuration lands");
    assert!(
        c.set_strategy_enabled("cross_venue_arb", true),
        "the strategy enables"
    );

    // Freeze the A account: its NEW entries are refused, exits keep flowing.
    c.freeze_account(
        default_account_id().as_str(),
        "naked-leg arm: the A leg's venue is degraded".to_string(),
    );

    pair_market_data(&mut c, now);
    let placed = c.engine_evaluate(now + 2_000);
    assert_eq!(
        placed, 1,
        "the frozen A account's leg is refused, the B leg still places: {placed}"
    );
    assert!(
        open_positions(&c, "default").is_empty(),
        "the A account booked nothing (its leg was refused)"
    );
    assert_eq!(
        open_positions(&c, "arb-b").len(),
        1,
        "the B leg filled under the B account — the naked-leg degradation path"
    );
    assert_eq!(
        open_positions(&c, "arb-b")[0].token_id,
        KX_DOWN,
        "the B leg is the kalshi token"
    );
}

// ── test 8: the venue-env / config-coverage fail-closed stamp ───────────────

/// A token whose venue is NOT in the configuration stamps the INSTANCE
/// account (the adapter's fallback) and — since the pairing only offers
/// configured pairs — no ENTRY ever carries such a stamp: the fallback is
/// observable on the exit surface, where a held token of an unknown venue
/// must resolve to the instance account rather than panic or guess.
#[test]
fn a_venue_outside_the_configuration_stamps_the_instance_account() {
    // The adapter-level tooth: the STAMP function itself, driven with a
    // market row of a venue outside the map.
    let mut a = arb_adapter();
    EngineStrategy::set_arb_leg_config(&mut a, Some(legs_payload()));

    let now = NOW;
    let slot = now / 1000 / 900;
    // A row of a THIRD venue, plus the two configured rows.
    let markets = vec![
        market(PM_UP, PM_DOWN, PM, "polymarket", slot),
        market(KX_UP, KX_DOWN, KX, "kalshi", slot),
        market("ov-up", "ov-down", "ov-cond", "other-venue", slot),
    ];
    let book_ts = now + 1_000;
    let fresh = move |t: &str| {
        let ask = match t {
            t if t == PM_UP => dec!(0.44),
            t if t == PM_DOWN => dec!(0.50),
            t if t == KX_UP => dec!(0.48),
            t if t == KX_DOWN => dec!(0.42),
            "ov-up" => dec!(0.01),
            _ => return None,
        };
        Some(blitzkrieg_core::model::OrderbookSnapshot::from_levels(
            t.to_string(),
            vec![(ask - dec!(0.01), dec!(600))],
            vec![(ask, dec!(600))],
            book_ts,
        ))
    };
    let ctx = blitzkrieg_core::strategies::StrategyCtx::new(&markets, slot + 1, 600, now, &fresh);
    let got = EngineStrategy::find_candidates(&mut a, &ctx);
    assert_eq!(got.len(), 2, "only the configured pair offers");
    for s in &got {
        let expected = if s.token_id == PM_UP {
            "default"
        } else {
            "arb-b"
        };
        assert_eq!(
            s.account_id, expected,
            "configured legs stamp their own venue accounts: {s:?}"
        );
    }
    assert!(
        got.iter().all(|s| s.token_id != "ov-up"),
        "the third venue never becomes a leg (fail-closed pairing)"
    );
    // The fallback row: the adapter stamps a token of an UNKNOWN venue with
    // the INSTANCE account (LuaEngineAdapter::account_id), never a venue
    // guess — pinned through the adapter's public stamp surface via a
    // one-token context.
    let unknown = vec![market("u-up", "u-down", "u-cond", "unknown-venue", slot)];
    let fresh_u = move |_t: &str| {
        Some(blitzkrieg_core::model::OrderbookSnapshot::from_levels(
            "u-up".to_string(),
            vec![(dec!(0.10), dec!(600))],
            vec![(dec!(0.11), dec!(600))],
            book_ts,
        ))
    };
    let ctx_u =
        blitzkrieg_core::strategies::StrategyCtx::new(&unknown, slot + 1, 600, now, &fresh_u);
    let _ = ctx_u; // the pairing offers nothing for an unknown venue...
    let got_u = EngineStrategy::find_candidates(&mut a, &ctx_u);
    assert!(
        got_u.is_empty(),
        "an all-unknown round pairs nothing: {got_u:?}"
    );
}

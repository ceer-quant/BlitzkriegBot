//! lua_momentum thesis-death acceptance checks (#961) — the REAL package
//! (`user_layer/strategies_lua/lua_momentum`) driven through the engine
//! adapter, exactly the surface the kernel runs. The mad_dog check
//! (`mad_dog_strategy_check.rs`) is the harness precedent; this file pins the
//! Exit-First contract on the package that declares it:
//!
//!   acceptance 1 — a reverse condition (the held token's own bid reversed
//!                  past the death band) makes the SAME evaluate emit ONE
//!                  exit intent tagged `thesis_death_exit`, taker, unpriced
//!                  (the kernel prices it);
//!   acceptance 2 — after the death close, the package proposes NO further
//!                  entry on that condition in the same round (命中后不再
//!                  加仓/续持: the death is a same-tick close and the entry
//!                  budget refuses the re-entry);
//!   flip        — a premise flip in PROFIT is still a death: the momentum
//!                  that armed the entry pointing the other way kills the
//!                  thesis even above water;
//!   alive       — a sub-band, sub-threshold drift emits nothing.
//!
//! The exit intents here flow through `StrategyCtx::with_positions` → the
//! loader's on_holdings push → `bk.holdings()` — the same seam the kernel
//! drives, and the forced scan is the runtime's (a strategy cannot suppress
//! it; the strategy's own exits table never carries `thesis_death_exit`).

use std::path::PathBuf;

use blitzkrieg_core::model::CryptoMarket;
use blitzkrieg_core::model::OrderbookSnapshot;
use blitzkrieg_core::strategies::{EngineStrategy, HeldPosition, StrategyCtx, StrategyExitIntent};
use blitzkrieg_core::strategy_engine::lua_loader::{LuaEngineAdapter, load_lua_package};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

const NOW: i64 = 1_700_000_000_000;
const UP: &str = "up";
const DOWN: &str = "down";

fn package_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../user_layer/strategies_lua/lua_momentum")
}

fn adapter() -> LuaEngineAdapter {
    let loaded = load_lua_package(&package_dir()).expect("lua_momentum package must load");
    LuaEngineAdapter::new(loaded.strategy, loaded.tunables)
}

fn market() -> CryptoMarket {
    CryptoMarket {
        asset: "BTC".into(),
        condition_id: "cond".into(),
        question_id: "q".into(),
        up_token_id: UP.into(),
        down_token_id: DOWN.into(),
        up_price: dec!(0.6),
        down_price: dec!(0.4),
        expires_at_ms: NOW + 900_000,
        round_slot: NOW / 900_000,
        round_duration_sec: 900,
        archive_verdict: false,
        venue: String::new(),
        neg_risk: false,
        question: "BTC up or down".into(),
    }
}

fn token_book(token: &str, bid: Decimal, ask: Decimal, ts: i64) -> OrderbookSnapshot {
    OrderbookSnapshot::from_levels(
        token.to_string(),
        vec![(bid, dec!(600))],
        vec![(ask, dec!(600))],
        ts,
    )
}

fn ctx<'a>(
    markets: &'a [CryptoMarket],
    time_left_sec: i64,
    now_ms: i64,
    fresh: &'a dyn Fn(&str) -> Option<OrderbookSnapshot>,
) -> StrategyCtx<'a> {
    StrategyCtx::new(markets, NOW / 900_000, time_left_sec, now_ms, fresh)
}

/// Arm the package's own premise: the DOWN leg's own mid crashes >= threshold
/// (default 4%) in one tick → the "down" direction arms and one PURE MAKER
/// bid rests at the leg's best_bid. Returns the suggested entry price.
fn arm_and_suggest(a: &mut LuaEngineAdapter) -> Decimal {
    // First observation (no comparison yet), then the crash.
    a.on_book(DOWN, &token_book(DOWN, dec!(0.50), dec!(0.52), NOW), NOW);
    let crash_ts = NOW + 1_000;
    a.on_book(
        DOWN,
        &token_book(DOWN, dec!(0.46), dec!(0.48), crash_ts),
        crash_ts,
    );
    let markets = vec![market()];
    let crash = token_book(DOWN, dec!(0.46), dec!(0.48), crash_ts);
    let fresh = move |t: &str| (t == DOWN).then(|| crash.clone());
    let got = a.find_candidates(&ctx(&markets, 600, crash_ts, &fresh));
    assert_eq!(got.len(), 1, "the crash must arm one DOWN bid: {got:?}");
    assert_eq!(got[0].token_id, DOWN);
    got[0].price
}

/// A held DOWN position at the suggested entry, shaped exactly the way the
/// host's flat book carries one (the with_positions seam, mad_dog precedent).
fn held_down(entry: &str) -> Vec<HeldPosition> {
    vec![HeldPosition {
        strategy: "lua_momentum".into(),
        account_id: blitzkrieg_core::model::default_account_id().to_string(),
        condition_id: "cond".into(),
        token_id: DOWN.into(),
        direction: "down".into(),
        shares: "1.00".into(),
        entry_price: entry.into(),
        round_slot: NOW / 900_000,
    }]
}

/// Acceptance 1, full episode: arm → fill → the knife keeps falling past the
/// death band (12% off the 0.46 entry: a 0.40 bid is 13% under) → the SAME
/// evaluate emits EXACTLY ONE `thesis_death_exit` exit intent, taker,
/// unpriced — and proposes no new entry while held.
#[test]
fn reversal_past_the_death_band_exits_same_tick() {
    let mut a = adapter();
    let entry_price = arm_and_suggest(&mut a);
    assert_eq!(
        entry_price,
        dec!(0.46),
        "the bid rests at the leg's own best_bid"
    );

    // Sub-threshold drift while held (confirmed clears, direction stays):
    // 1% adverse — alive, nothing emitted.
    let held = held_down("0.46");
    let drift_ts = NOW + 2_000;
    a.on_book(
        DOWN,
        &token_book(DOWN, dec!(0.455), dec!(0.475), drift_ts),
        drift_ts,
    );
    let drift = token_book(DOWN, dec!(0.455), dec!(0.475), drift_ts);
    let fresh = move |t: &str| (t == DOWN).then(|| drift.clone());
    let _ = a.find_candidates(&ctx(&[market()], 600, drift_ts, &fresh).with_positions(&held));
    assert!(
        a.take_exit_intents().is_empty(),
        "a sub-band drift is not a thesis death"
    );

    // The knife continues: bid 0.40 = 13.04% under the 0.46 entry — past the
    // 12% death band (mid move also re-arms the same down premise, so the
    // flip arm is silent; the BAND is what kills here).
    let death_ts = NOW + 3_000;
    a.on_book(
        DOWN,
        &token_book(DOWN, dec!(0.40), dec!(0.42), death_ts),
        death_ts,
    );
    let death = token_book(DOWN, dec!(0.40), dec!(0.42), death_ts);
    let fresh = move |t: &str| (t == DOWN).then(|| death.clone());
    let cands = a.find_candidates(&ctx(&[market()], 600, death_ts, &fresh).with_positions(&held));
    assert!(
        cands.is_empty(),
        "a held leg is never re-entered: {cands:?}"
    );
    let exits: Vec<StrategyExitIntent> = a.take_exit_intents();
    assert_eq!(
        exits.len(),
        1,
        "the death must be ONE forced verdict: {exits:?}"
    );
    assert_eq!(exits[0].token_id, DOWN);
    assert_eq!(exits[0].reason, "thesis_death_exit");
    assert!(!exits[0].use_maker, "a forced death is a full TAKER close");
    assert_eq!(
        exits[0].exit_price, None,
        "the kernel prices it off the live bid"
    );
}

/// Acceptance 2: after the death close resolves (the holding is gone), the
/// package proposes NO further entry on the same condition in the same round
/// — 命中后不再出现加仓/续持 tick (the entry budget counts the closed
/// attempt against max_entries_per_round, and the forced close went through
/// the held→flat seam exactly like a strategy-signal one).
#[test]
fn no_reentry_on_the_condition_after_the_death_close() {
    let mut a = adapter();
    let _entry = arm_and_suggest(&mut a);

    // Fill + immediate death in the same round (the crash never paused).
    let held = held_down("0.46");
    let death_ts = NOW + 2_000;
    a.on_book(
        DOWN,
        &token_book(DOWN, dec!(0.40), dec!(0.42), death_ts),
        death_ts,
    );
    let death = token_book(DOWN, dec!(0.40), dec!(0.42), death_ts);
    let fresh = move |t: &str| (t == DOWN).then(|| death.clone());
    let _ = a.find_candidates(&ctx(&[market()], 600, death_ts, &fresh).with_positions(&held));
    let exits: Vec<StrategyExitIntent> = a.take_exit_intents();
    assert_eq!(exits.len(), 1, "{exits:?}");
    assert_eq!(exits[0].reason, "thesis_death_exit");

    // The close filled: the position is gone. The next evaluate of the SAME
    // round must not re-enter the condition it just buried.
    let flat: Vec<HeldPosition> = vec![];
    let after_ts = NOW + 3_000;
    a.on_book(
        DOWN,
        &token_book(DOWN, dec!(0.39), dec!(0.41), after_ts),
        after_ts,
    );
    let after = token_book(DOWN, dec!(0.39), dec!(0.41), after_ts);
    let fresh = move |t: &str| (t == DOWN).then(|| after.clone());
    let cands = a.find_candidates(&ctx(&[market()], 590, after_ts, &fresh).with_positions(&flat));
    assert!(
        cands.is_empty(),
        "no re-entry on a condition its thesis died in this round: {cands:?}"
    );
    assert!(a.take_exit_intents().is_empty());
}

/// The premise flip is a death even IN PROFIT: the momentum that armed the
/// entry now points the other way (an opposite >= threshold move re-armed
/// the token "up" while the position entered under "down"). The reversal
/// band is silent (the bid is ABOVE entry) — the flip alone must force the
/// taker verdict.
#[test]
fn premise_flip_in_profit_is_still_a_death() {
    let mut a = adapter();
    let _entry = arm_and_suggest(&mut a);

    // Hold through a sub-threshold wobble (4.3% adverse, under the band).
    let held = held_down("0.46");
    let wobble_ts = NOW + 2_000;
    a.on_book(
        DOWN,
        &token_book(DOWN, dec!(0.44), dec!(0.46), wobble_ts),
        wobble_ts,
    );
    let wobble = token_book(DOWN, dec!(0.44), dec!(0.46), wobble_ts);
    let fresh = move |t: &str| (t == DOWN).then(|| wobble.clone());
    let _ = a.find_candidates(&ctx(&[market()], 600, wobble_ts, &fresh).with_positions(&held));
    assert!(
        a.take_exit_intents().is_empty(),
        "a wobble inside every band is alive"
    );

    // The opposite move: mid 0.45 → 0.50 (+11%, >= threshold) re-arms the
    // token "up". The bid 0.49 is ABOVE the 0.46 entry — in profit — yet the
    // thesis that bought the crash is dead.
    let flip_ts = NOW + 3_000;
    a.on_book(
        DOWN,
        &token_book(DOWN, dec!(0.49), dec!(0.51), flip_ts),
        flip_ts,
    );
    let flip = token_book(DOWN, dec!(0.49), dec!(0.51), flip_ts);
    let fresh = move |t: &str| (t == DOWN).then(|| flip.clone());
    let _ = a.find_candidates(&ctx(&[market()], 600, flip_ts, &fresh).with_positions(&held));
    let exits: Vec<StrategyExitIntent> = a.take_exit_intents();
    assert_eq!(
        exits.len(),
        1,
        "a flipped premise must force the exit: {exits:?}"
    );
    assert_eq!(exits[0].reason, "thesis_death_exit");
    assert_eq!(exits[0].token_id, DOWN);
    assert!(!exits[0].use_maker);
}

/// The package declares the hook through the REAL path (a `bk_thesis_death`
/// predicate calling the stdlib default), so the forced scan fires with the
/// manifest's own bands (12/6/30000) — not the stdlib defaults. A death by
/// the manifest's tighter band proves the tunables flowed.
#[test]
fn the_manifest_bands_flow_through_the_real_package() {
    let mut a = adapter();
    let _entry = arm_and_suggest(&mut a);
    let held = held_down("0.46");
    // 9% adverse: under lua_momentum's 12% band, over the stdlib default
    // check's sub-band path — with the DEFAULT stdlib bands (25%) this would
    // also be alive, so assert the tighter manifest band is what governs by
    // going one tick deeper (13% = past 12, under 25) and requiring the exit.
    let death_ts = NOW + 2_000;
    a.on_book(
        DOWN,
        &token_book(DOWN, dec!(0.425), dec!(0.445), death_ts),
        death_ts,
    );
    let nine = token_book(DOWN, dec!(0.425), dec!(0.445), death_ts);
    let fresh = move |t: &str| (t == DOWN).then(|| nine.clone());
    let _ = a.find_candidates(&ctx(&[market()], 600, death_ts, &fresh).with_positions(&held));
    assert!(
        a.take_exit_intents().is_empty(),
        "7.6% adverse (0.425 bid) is inside the 12% manifest band — alive"
    );
    let death2_ts = NOW + 3_000;
    a.on_book(
        DOWN,
        &token_book(DOWN, dec!(0.40), dec!(0.42), death2_ts),
        death2_ts,
    );
    let death = token_book(DOWN, dec!(0.40), dec!(0.42), death2_ts);
    let fresh = move |t: &str| (t == DOWN).then(|| death.clone());
    let _ = a.find_candidates(&ctx(&[market()], 600, death2_ts, &fresh).with_positions(&held));
    let exits: Vec<StrategyExitIntent> = a.take_exit_intents();
    assert_eq!(exits.len(), 1, "{exits:?}");
    assert_eq!(
        exits[0].reason, "thesis_death_exit",
        "13% crosses the manifest's 12%"
    );
}

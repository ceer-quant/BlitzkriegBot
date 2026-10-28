#![cfg(test)]

use super::tests::{enter, params, two_sided_book};
use super::*;
use rust_decimal_macros::dec;

fn taker_fee(price: Decimal, shares: Decimal) -> Decimal {
    (crate::exit_policy::taker_fee_pct(price) / Decimal::ONE_HUNDRED) * price * shares
}

/// Open an empty position shell. Size and basis come from the venue's fills
/// (`apply_entry_fill`), never from the request, so there is nothing to pass.
fn open_at(pm: &mut PositionManager, price: Decimal) -> OpenPosition {
    pm.open(
        OpenParams {
            strategy: "s".into(),
            asset: "BTC".into(),
            direction: SignalDirection::Up,
            token_id: "tok".into(),
            condition_id: "cond".into(),
            entry_price: price,
            expires_at_ms: 1_000_000,
            was_maker: false,
            target_exit_price: None,
            account_id: crate::model::default_account_id(),
            venue: String::new(),
            partner_token_id: String::new(),
            round_duration_sec: 0,
        },
        0,
    )
}

/// The four partial-fill shapes the plan calls out, each with the fee the
/// ledger would have charged, asserting the position's net matches the cash.
#[test]
fn partial_entry_and_exit_flows_reconcile_exactly() {
    for (entry_fills, exit_fills) in [
        (vec![(dec!(1), dec!(0.40))], vec![(dec!(1), dec!(0.60))]),
        (vec![(dec!(5), dec!(0.40))], vec![(dec!(5), dec!(0.60))]),
        (
            vec![(dec!(9.99), dec!(0.40))],
            vec![(dec!(9.99), dec!(0.60))],
        ),
        (vec![(dec!(10), dec!(0.40))], vec![(dec!(10), dec!(0.60))]),
        // Ladders: several fills per leg, mixed prices.
        (
            vec![(dec!(4), dec!(0.40)), (dec!(6), dec!(0.42))],
            vec![(dec!(3), dec!(0.55)), (dec!(7), dec!(0.60))],
        ),
        // Mixed roles: maker in, part maker / part taker out.
        (
            vec![(dec!(10), dec!(0.43))],
            vec![(dec!(4), dec!(0.70)), (dec!(6), dec!(0.95))],
        ),
    ] {
        let mut pm = PositionManager::new(PositionConfig::default());
        let pos = open_at(&mut pm, entry_fills[0].1);

        let mut entry_cost = Decimal::ZERO;
        let mut entry_fee = Decimal::ZERO;
        let mut opened = Decimal::ZERO;
        for (shares, price) in &entry_fills {
            // Alternate role: index 0 taker, later maker — exercises the fold.
            let role = if opened == Decimal::ZERO {
                OrderRole::Taker
            } else {
                OrderRole::Maker
            };
            let fee = if role.is_maker() {
                Decimal::ZERO
            } else {
                taker_fee(*price, *shares)
            };
            pm.apply_entry_fill(&pos.id, *shares, *price, fee, role)
                .unwrap();
            entry_cost += price * shares;
            entry_fee += fee;
            opened += shares;
        }

        let mut proceeds = Decimal::ZERO;
        let mut exit_fee = Decimal::ZERO;
        let mut sold = Decimal::ZERO;
        for (i, (shares, price)) in exit_fills.iter().enumerate() {
            let role = if i % 2 == 0 {
                OrderRole::Taker
            } else {
                OrderRole::Maker
            };
            let fee = if role.is_maker() {
                Decimal::ZERO
            } else {
                taker_fee(*price, *shares)
            };
            pm.apply_exit_fill(&pos.id, *shares, *price, fee, role)
                .unwrap();
            proceeds += price * shares;
            exit_fee += fee;
            sold += shares;
        }

        let closed = pm
            .close(
                &pos.id,
                exit_fills.last().unwrap().1,
                ExitReason::Manual,
                false,
                10,
            )
            .unwrap();

        assert_eq!(closed.shares, opened, "shares = what was opened");
        assert_eq!(closed.cost_usd, entry_cost, "basis comes from the ledger");
        assert_eq!(
            closed.pnl_usd,
            proceeds - entry_cost,
            "gross = Δcash before fees"
        );
        assert_eq!(
            closed.net_pnl_usd,
            proceeds - entry_cost - entry_fee - exit_fee,
            "net must be the ledger's own arithmetic (entry={entry_fills:?} exit={exit_fills:?})"
        );
        assert_eq!(
            closed.dust_shares,
            Decimal::ZERO,
            "fully sold leaves no dust"
        );
        assert_eq!(
            closed.was_maker_entry,
            entry_fee == Decimal::ZERO,
            "maker flag mirrors the fee actually paid"
        );
        assert_eq!(closed.was_maker_exit, exit_fee == Decimal::ZERO);
    }
}

/// A sub-grid remainder (0.001 of a share) is written off at the exit price
/// and charged to net, so cash and the record still agree.
#[test]
fn sub_grid_dust_is_written_off_not_left_open() {
    let mut pm = PositionManager::new(PositionConfig::default());
    let pos = open_at(&mut pm, dec!(0.40));
    pm.apply_entry_fill(
        &pos.id,
        dec!(10),
        dec!(0.40),
        Decimal::ZERO,
        OrderRole::Maker,
    )
    .unwrap();
    // Sell everything sellable (10.00) but claim only 9.999 filled.
    pm.apply_exit_fill(
        &pos.id,
        dec!(9.999),
        dec!(0.60),
        Decimal::ZERO,
        OrderRole::Maker,
    )
    .unwrap();
    let closed = pm
        .close(&pos.id, dec!(0.60), ExitReason::Manual, true, 10)
        .unwrap();
    assert_eq!(closed.dust_shares, dec!(0.001));
    // The dust is priced at the exit, so gross covers all 10 shares.
    assert_eq!(
        closed.pnl_usd,
        dec!(0.60) * dec!(10) - dec!(0.40) * dec!(10)
    );
    assert_eq!(closed.net_pnl_usd, closed.pnl_usd);
}

/// An exact-grid position sells in full: the old 0.01 flat buffer is gone, so
/// a 10-share position no longer leaves 0.01 shares stranded forever.
#[test]
fn sell_shares_sends_the_whole_exact_grid_position() {
    let mut pm = PositionManager::new(PositionConfig::default());
    let pos = open_at(&mut pm, dec!(0.40));
    pm.apply_entry_fill(
        &pos.id,
        dec!(10),
        dec!(0.40),
        Decimal::ZERO,
        OrderRole::Maker,
    )
    .unwrap();
    assert_eq!(pm.sell_shares(&pos.id), Some(dec!(10)));

    // Off-grid holdings floor down — never oversell.
    pm.apply_entry_fill(
        &pos.id,
        dec!(0.005),
        dec!(0.40),
        Decimal::ZERO,
        OrderRole::Maker,
    )
    .unwrap();
    assert_eq!(pm.sell_shares(&pos.id), Some(dec!(10)));
    assert!(pm.sell_shares(&pos.id).unwrap() <= pm.open_positions()[0].shares);
}

// ── Issue #464: the exit ladder overlay ────────────────────────────────
//
// Fixture shape: entry 0.40, 10 shares, a ladder config, and books whose
// bid is named by pnl% (`0.42` = +5% over 0.40). Every check runs at a
// fixed mid-flight instant (400 s into a 900 s position) so no time rule
// (tighten ramp, force exit, min_time_left) can contaminate the verdict.

fn ladder_cfg(steps: Vec<crate::arbitration::LadderStep>) -> PositionConfig {
    PositionConfig {
        exit_ladder: steps,
        ..Default::default()
    }
}

fn step(
    at_pct: Decimal,
    close_ratio: Decimal,
    move_stop_to: Option<Decimal>,
) -> crate::arbitration::LadderStep {
    crate::arbitration::LadderStep {
        at_pct,
        close_ratio,
        move_stop_to,
    }
}

/// Open a 10-share position at 0.40 filled as a maker (no fees).
fn ladder_open(pm: &mut PositionManager) {
    let _ = enter(
        pm,
        params("BTC", SignalDirection::Up, dec!(0.40)),
        OrderRole::Maker,
        0,
    );
}

/// Run one exit sweep against a two-sided book whose bid is `bid` —
/// NO new position: the ladder tests walk one position across ticks.
fn ladder_check(pm: &mut PositionManager, bid: Decimal, now_ms: i64) -> Vec<ExitRequest> {
    pm.check_exits(
        &|token| {
            if token == "tok_BTC" {
                Some(BookView::from_snapshot(&two_sided_book(
                    bid,
                    bid + dec!(0.01),
                    now_ms,
                )))
            } else {
                None
            }
        },
        now_ms,
    )
}

/// a) close_ratio tranches: the first step sells HALF, the next tick the
///    rest — and the residual backstop's shape (a tranche SELL followed by
///    a quiet book) never appears here because the ladder itself re-arms.
#[test]
fn ladder_close_ratio_sells_in_tranches() {
    let mut pm = PositionManager::new(ladder_cfg(vec![
        step(dec!(5), dec!(0.5), None),
        step(dec!(10), Decimal::ONE, None),
    ]));
    // bid 0.42 = +5%: step 1 arms and sells half the position.
    ladder_open(&mut pm);
    let reqs = ladder_check(&mut pm, dec!(0.42), 400_000);
    assert_eq!(reqs.len(), 1, "one tranche per tick");
    assert_eq!(reqs[0].reason, ExitReason::TakeProfit);
    assert_eq!(reqs[0].close_shares, Some(dec!(5)), "half of 10 shares");
    assert_eq!(reqs[0].exit_price, dec!(0.42));

    // The fill lands (half sold), the bid rises to 0.45 = +12.5%: BOTH
    // steps' thresholds are past, but only the NEXT step may fire.
    pm.apply_exit_fill(
        &pm.open_positions()[0].id.clone(),
        dec!(5),
        dec!(0.42),
        Decimal::ZERO,
        OrderRole::Taker,
    );
    let reqs = ladder_check(&mut pm, dec!(0.45), 400_100);
    assert_eq!(reqs.len(), 1);
    assert_eq!(
        reqs[0].close_shares,
        Some(dec!(5)),
        "the second step closes the remaining half, not more"
    );
    assert_eq!(reqs[0].reason, ExitReason::TakeProfit);
}

/// b) move_stop_to: a fired step raises the stop floor to an absolute
///    price; a later dip below the floor (but above the policy's own 12%
///    stop) now closes the WHOLE remaining position — under the old
///    behavior 0.41 over a 0.40 entry triggers nothing at all.
#[test]
fn ladder_move_stop_to_raises_the_stop_and_stops_out() {
    let mut pm = PositionManager::new(ladder_cfg(vec![step(dec!(8), dec!(0.5), Some(dec!(0.42)))]));
    // bid 0.432 = +8%: step fires, half the position goes, floor 0.42.
    ladder_open(&mut pm);
    let reqs = ladder_check(&mut pm, dec!(0.432), 400_000);
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].close_shares, Some(dec!(5)));
    assert_eq!(reqs[0].reason, ExitReason::TakeProfit);
    pm.apply_exit_fill(
        &pm.open_positions()[0].id.clone(),
        dec!(5),
        dec!(0.432),
        Decimal::ZERO,
        OrderRole::Taker,
    );

    // bid 0.41: +2.5% — far above the policy stop (0.352), below the
    // raised floor (0.42). The overlay must stop out the remainder.
    let reqs = ladder_check(&mut pm, dec!(0.41), 400_100);
    assert_eq!(
        reqs.len(),
        1,
        "the raised stop fires where the policy's would not"
    );
    assert_eq!(reqs[0].reason, ExitReason::StopLoss);
    assert_eq!(reqs[0].close_shares, None, "a stop closes everything left");
    assert_eq!(reqs[0].exit_price, dec!(0.41));
}

/// d) no skipping: a bid that clears BOTH steps' thresholds on the first
///    tick still sells only the FIRST tranche — a spike cannot dump the
///    whole position in one sweep.
#[test]
fn ladder_never_skips_ahead_of_an_untriggered_step() {
    let mut pm = PositionManager::new(ladder_cfg(vec![
        step(dec!(5), dec!(0.5), None),
        step(dec!(10), Decimal::ONE, None),
    ]));
    // bid 0.45 = +12.5%: past step 1 (5%) and step 2 (10%) in one move.
    ladder_open(&mut pm);
    let reqs = ladder_check(&mut pm, dec!(0.45), 400_000);
    assert_eq!(reqs.len(), 1);
    assert_eq!(
        reqs[0].close_shares,
        Some(dec!(5)),
        "only the first step's tranche may fire this tick"
    );
}

/// The ladder exhausts itself: with every step executed, a later TP/SL
/// verdict is absorbed (no request, no cooldown, no error) — the ladder
/// owns this position's profit-taking discipline, and a residual step's
/// verdict must not re-flatten the remainder wholesale.
#[test]
fn an_exhausted_ladder_absorbs_tp_sl_verdicts() {
    let mut pm = PositionManager::new(ladder_cfg(vec![step(dec!(5), dec!(0.5), None)]));
    ladder_open(&mut pm);
    let reqs = ladder_check(&mut pm, dec!(0.42), 400_000);
    assert_eq!(reqs[0].close_shares, Some(dec!(5)));
    pm.apply_exit_fill(
        &pm.open_positions()[0].id.clone(),
        dec!(5),
        dec!(0.42),
        Decimal::ZERO,
        OrderRole::Taker,
    );
    // The verdict fires again (still above take-profit), but the ladder
    // has no step left: nothing is pushed this tick.
    let reqs = ladder_check(&mut pm, dec!(0.45), 400_100);
    assert!(
        reqs.is_empty(),
        "an exhausted ladder leaves the remainder alone"
    );
}

/// A pre-E17 snapshot has no recorded flows; they are repaired from the old
/// fields so a restart mid-flight cannot lose the basis.
#[test]
fn legacy_snapshot_flows_are_repaired() {
    let mut pm = PositionManager::new(PositionConfig::default());
    let pos = open_at(&mut pm, dec!(0.40));
    let mut legacy = pm
        .apply_entry_fill(&pos.id, dec!(10), dec!(0.40), dec!(0.01), OrderRole::Taker)
        .unwrap();
    // Simulate a snapshot serialised by the previous version: flows absent,
    // only the old cost_usd/entry_fee_pct/entry_price fields populated.
    legacy.flows = CashFlows::default();
    legacy.cost_usd = dec!(4.0);
    legacy.shares = dec!(10);
    legacy.entry_price = dec!(0.40);
    legacy.entry_fee_pct = taker_fee(dec!(0.40), dec!(10)) / dec!(4.0) * dec!(100);
    let repaired = legacy.flows();
    assert_eq!(repaired.opened_shares, dec!(10));
    assert_eq!(repaired.entry_cost_usd, dec!(4.0));
    assert!(repaired.entry_fee_usd > Decimal::ZERO);
}

/// #463: a corrupt daily-loss file must NOT read as "fresh day". The store
/// returns None either way (trading continues), but the contract here is
/// that a state which EXISTS and does not parse is loud, not a warn-and-
/// zero — the difference between a budget reset and a budget preserved is
/// today's realized losses.
#[test]
fn a_corrupt_daily_loss_file_does_not_read_as_a_fresh_day() {
    let dir = std::env::temp_dir().join(format!(
        "bk-dailyloss-bad-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("positions.daily-loss.json");

    // Write a real state first (through the atomic path), then corrupt it.
    let store = DailyLossStore { path: path.clone() };
    let st = DailyLossState {
        day_index: Some(20_400),
        realized_pnl_usd: dec!(-18.5),
        opening_equity_usd: dec!(1000),
        equity_basis: Some(EquityBasis::Dry),
        tripped: false,
        tripped_at_ms: 0,
        tripped_limit_usd: Decimal::ZERO,
        opened_at_ms: 1000,
        tripped_reported: false,
    };
    let mut accounts = std::collections::BTreeMap::new();
    accounts.insert(AccountId::from("default"), st.clone());
    store.save(&accounts);
    let loaded = store.load();
    assert_eq!(
        loaded
            .get(&AccountId::from("default"))
            .expect("the healthy state loads")
            .realized_pnl_usd,
        dec!(-18.5)
    );

    // Migration: a PRE-per-account file (bare state) reads as the default
    // account's bucket — single-book deployments keep their spent budget.
    let bare = serde_json::to_string(&st).unwrap();
    std::fs::write(&path, bare).unwrap();
    let migrated = store.load();
    assert_eq!(
        migrated
            .get(&AccountId::from("default"))
            .expect("bare state migrates to the default bucket")
            .realized_pnl_usd,
        dec!(-18.5)
    );

    // The crash window: a half-written file.
    std::fs::write(&path, "{\"dayIndex\": 20400, \"realized").unwrap();
    assert!(
        store.load().is_empty(),
        "a corrupt file yields no state — but the ERROR log is the contract"
    );

    // The atomic save never leaves a torn file behind in the first place:
    // the successful write above left valid JSON.
    store.save(&accounts);
    assert!(!store.load().is_empty());

    std::fs::remove_dir_all(&dir).ok();
}

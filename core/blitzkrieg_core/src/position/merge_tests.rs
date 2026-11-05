#![cfg(test)]

//! The MERGE suite (task 2.2): pair collection for pair-discount arbitrage.
//! Split out of `tests.rs` (#780 ratchet) — the builders stay `pub(crate)`
//! over there and are imported here, same shape as `precision_tests.rs`.

use super::tests::enter;
use super::*;
use rust_decimal_macros::dec;

/// One leg of a binary condition. `tests::params` pins a shared token;
/// a pair needs DISTINCT tokens on ONE condition.
fn leg_params(dir: SignalDirection, token: &str, condition: &str, entry: Decimal) -> OpenParams {
    OpenParams {
        strategy: "pair_discount_arb".into(),
        asset: "BTC".into(),
        direction: dir,
        token_id: token.into(),
        condition_id: condition.into(),
        entry_price: entry,
        expires_at_ms: 900_000,
        was_maker: true,
        target_exit_price: None,
        account_id: crate::model::default_account_id(),
        venue: String::new(),
        partner_token_id: String::new(),
        round_duration_sec: 0,
        round_slot: 0,
    }
}

/// Equal legs: the whole pair merges — both legs close as `Merge` at
/// $0.50/share (their notionals sum to the $1.00/pair the ledger credits),
/// nothing stays open, and NO cooldown is armed: a merge is collection,
/// not an exit, so the next round's entry is a fresh decision.
#[test]
fn merge_burns_a_complete_pair_into_collateral() {
    let mut pm = PositionManager::new(PositionConfig::default());
    let up = enter(
        &mut pm,
        leg_params(SignalDirection::Up, "tok-up", "cond", dec!(0.48)),
        OrderRole::Maker,
        0,
    );
    let down = enter(
        &mut pm,
        leg_params(SignalDirection::Down, "tok-down", "cond", dec!(0.45)),
        OrderRole::Maker,
        0,
    );
    let out = pm
        .merge_condition("cond", 1_000)
        .expect("a complete pair merges");
    assert_eq!(out.pairs_merged, dec!(10));
    assert_eq!(out.cash_credited_usd, dec!(10), "$1.00 per share-pair");
    assert_eq!(out.unpaired_shares, Decimal::ZERO);
    assert_eq!(out.closed.len(), 2);
    for c in &out.closed {
        assert_eq!(c.exit_reason, ExitReason::Merge);
        assert_eq!(c.exit_price, dec!(0.50));
        assert!(c.was_maker_exit, "a merge pays no fee");
        assert_eq!(c.exit_fee_pct, Decimal::ZERO);
    }
    // Leg PnL: exit notional 5.00 each; maker entries were fee-free.
    let up_closed = out.closed.iter().find(|c| c.id == up.id).unwrap();
    let down_closed = out.closed.iter().find(|c| c.id == down.id).unwrap();
    assert_eq!(up_closed.net_pnl_usd, dec!(5.0) - dec!(4.8));
    assert_eq!(down_closed.net_pnl_usd, dec!(5.0) - dec!(4.5));
    assert!(pm.open_positions().is_empty());
    // The collection channel must not lock the next round out.
    assert!(
        pm.can_open("default", Some("BTC"), Some(SignalDirection::Up), 2_000)
            .is_ok(),
        "merge arms no exit cooldown"
    );
    assert!(
        pm.can_open("default", Some("BTC"), Some(SignalDirection::Down), 2_000)
            .is_ok(),
        "merge arms no loss/asset cooldown either"
    );
}

/// Unequal legs: only complete pairs merge. The shorter leg is consumed
/// whole; the longer leg keeps its unpaired shares — they have no
/// collateral claim until the market resolves.
#[test]
fn merge_pairs_only_the_smaller_leg_and_leaves_the_rest() {
    let mut pm = PositionManager::new(PositionConfig::default());
    enter(
        &mut pm,
        leg_params(SignalDirection::Up, "tok-up", "cond", dec!(0.48)),
        OrderRole::Maker,
        0,
    );
    let down = enter(
        &mut pm,
        leg_params(SignalDirection::Down, "tok-down", "cond", dec!(0.45)),
        OrderRole::Maker,
        0,
    );
    // Trim the DOWN leg to 6 shares first: only 6 complete pairs exist.
    pm.apply_exit_fill(
        &down.id,
        dec!(4),
        dec!(0.44),
        Decimal::ZERO,
        OrderRole::Maker,
        Decimal::ZERO,
    );
    let out = pm
        .merge_condition("cond", 1_000)
        .expect("a partial pair still merges its complete part");
    assert_eq!(out.pairs_merged, dec!(6));
    assert_eq!(out.cash_credited_usd, dec!(6));
    // DOWN is consumed whole; UP keeps its unpaired 4 shares for settlement.
    assert_eq!(out.closed.len(), 1);
    assert_eq!(out.closed[0].direction, SignalDirection::Down);
    assert_eq!(out.closed[0].exit_reason, ExitReason::Merge);
    assert_eq!(out.unpaired_shares, dec!(4));
    assert_eq!(pm.open_positions().len(), 1);
    assert_eq!(pm.open_positions()[0].direction, SignalDirection::Up);
    assert_eq!(pm.open_positions()[0].shares, dec!(4));
}

/// Reverse: a ONE-SIDED position has nothing to burn — no merge, no close,
/// no cash. Unpaired inventory waits for settlement.
#[test]
fn merge_refuses_a_one_sided_position() {
    let mut pm = PositionManager::new(PositionConfig::default());
    enter(
        &mut pm,
        leg_params(SignalDirection::Up, "tok-up", "cond", dec!(0.48)),
        OrderRole::Maker,
        0,
    );
    assert!(pm.merge_condition("cond", 1_000).is_none());
    assert_eq!(pm.open_positions().len(), 1, "the lone leg is untouched");
    assert!(pm.closed_positions().is_empty());
    // An unknown condition has nothing to merge either.
    assert!(pm.merge_condition("other", 1_000).is_none());
}

/// Reverse: legs in DIFFERENT accounts are not one wallet's inventory —
/// an on-chain merge spends a single wallet's tokens, so it is refused
/// whole and no leg is touched.
#[test]
fn merge_refuses_legs_in_different_accounts() {
    let mut pm = PositionManager::new(PositionConfig::default());
    let mut up = leg_params(SignalDirection::Up, "tok-up", "cond", dec!(0.48));
    up.account_id = AccountId::from("acct-a");
    let mut down = leg_params(SignalDirection::Down, "tok-down", "cond", dec!(0.45));
    down.account_id = AccountId::from("acct-b");
    enter(&mut pm, up, OrderRole::Maker, 0);
    enter(&mut pm, down, OrderRole::Maker, 0);
    assert!(pm.merge_condition("cond", 1_000).is_none());
    assert_eq!(pm.open_positions().len(), 2, "no leg is touched");
    assert!(pm.closed_positions().is_empty());
}

/// Reverse: a remainder below the 0.01 share grid completes no pair —
/// the venue cannot merge fractional dust.
#[test]
fn merge_refuses_a_pair_below_the_share_grid() {
    let mut pm = PositionManager::new(PositionConfig::default());
    enter(
        &mut pm,
        leg_params(SignalDirection::Up, "tok-up", "cond", dec!(0.48)),
        OrderRole::Maker,
        0,
    );
    let down = enter(
        &mut pm,
        leg_params(SignalDirection::Down, "tok-down", "cond", dec!(0.45)),
        OrderRole::Maker,
        0,
    );
    // 0.005 shares left on DOWN: below the grid, no complete pair.
    pm.apply_exit_fill(
        &down.id,
        dec!(9.995),
        dec!(0.44),
        Decimal::ZERO,
        OrderRole::Maker,
        Decimal::ZERO,
    );
    assert!(pm.merge_condition("cond", 1_000).is_none());
    assert_eq!(pm.open_positions().len(), 2);
}

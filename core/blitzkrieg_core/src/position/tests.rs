#![cfg(test)]

use super::*;
use rust_decimal_macros::dec;

// `pub(crate)`: the #464 ladder fixtures live in `precision_tests` and
// reuse these three builders instead of cloning them.
pub(crate) fn params(asset: &str, dir: SignalDirection, entry: Decimal) -> OpenParams {
    OpenParams {
        strategy: "spread_arb".into(),
        asset: asset.into(),
        direction: dir,
        token_id: format!("tok_{asset}"),
        condition_id: "cond".into(),
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

/// Open a position AND accrue its entry fill — the two steps production
/// always performs together (service: `open` then `apply_entry_fill`).
pub(crate) fn enter(
    pm: &mut PositionManager,
    p: OpenParams,
    role: OrderRole,
    now_ms: i64,
) -> OpenPosition {
    let shares = dec!(10);
    let price = p.entry_price;
    let pos = pm.open(p, now_ms);
    let fee = if role.is_maker() {
        Decimal::ZERO
    } else {
        (crate::exit_policy::taker_fee_pct(price) / Decimal::ONE_HUNDRED) * price * shares
    };
    pm.apply_entry_fill(&pos.id, shares, price, fee, role)
        .unwrap()
}

fn one_sided_book(ask: Decimal, ts: i64) -> OrderbookSnapshot {
    // F6 regression fixture: NO bids at all, only an ask. The old mid
    // arithmetic turned this into a phantom sellable price of ask/2.
    OrderbookSnapshot::from_levels("tok_BTC".to_string(), vec![], vec![(ask, dec!(100))], ts)
}

pub(crate) fn two_sided_book(bid: Decimal, ask: Decimal, ts: i64) -> OrderbookSnapshot {
    OrderbookSnapshot::from_levels(
        "tok_BTC".to_string(),
        vec![(bid, dec!(100))],
        vec![(ask, dec!(100))],
        ts,
    )
}

/// F6: with no buyer in the book, no exit request may be produced at all —
/// not from the mid, not from the stale `current_price`. Deadline pressure
/// (force-exit territory) included.
#[test]
fn no_bid_mint_no_exit_request_even_at_deadline() {
    // Pins the guillotine ON: this is the F6 deadline mechanism test, and
    // the calibrated default now ships it OFF (`force_exit_sec = 0`).
    let mut pm = PositionManager::new(PositionConfig {
        exit: ExitConfig {
            force_exit_sec: 120,
            ..Default::default()
        },
        ..Default::default()
    });
    let p = enter(
        &mut pm,
        params("BTC", SignalDirection::Up, dec!(0.4)),
        OrderRole::Maker,
        0,
    );
    // 10 s before expiry: a live bid would fire ForceExit immediately.
    let now = p.expires_at_ms - 10_000;
    let reqs = pm.check_exits(
        &|token| {
            if token == "tok_BTC" {
                Some(BookView::from_snapshot(&one_sided_book(dec!(0.90), now)))
            } else {
                None
            }
        },
        now,
    );
    assert!(
        reqs.is_empty(),
        "no bid ⇒ no sellable quote ⇒ no exit request"
    );

    // Sanity: the same tick WITH a real bid still exits (gate works both ways).
    let reqs = pm.check_exits(
        &|token| {
            if token == "tok_BTC" {
                Some(BookView::from_snapshot(&two_sided_book(
                    dec!(0.50),
                    dec!(0.52),
                    now,
                )))
            } else {
                None
            }
        },
        now,
    );
    assert_eq!(reqs.len(), 1);
    assert_eq!(
        reqs[0].exit_price,
        dec!(0.50),
        "exit prices off the live bid"
    );
}

/// F6: a stale bid is not a buyer standing here now — an expired book must
/// not price a forced exit at an old price.
#[test]
fn stale_book_does_not_price_a_forced_exit() {
    // Pins the guillotine ON (mechanism test); the shipped default is OFF.
    let mut pm = PositionManager::new(PositionConfig {
        exit: ExitConfig {
            force_exit_sec: 120,
            ..Default::default()
        },
        ..Default::default()
    });
    let p = enter(
        &mut pm,
        params("BTC", SignalDirection::Up, dec!(0.4)),
        OrderRole::Maker,
        0,
    );
    let now = p.expires_at_ms - 10_000;
    // Bid 0.60 captured 2 minutes ago (budget: default 60 s).
    let stale = now - 120_000;
    let reqs = pm.check_exits(
        &|token| {
            if token == "tok_BTC" {
                Some(BookView::from_snapshot(&two_sided_book(
                    dec!(0.60),
                    dec!(0.62),
                    stale,
                )))
            } else {
                None
            }
        },
        now,
    );
    assert!(reqs.is_empty(), "stale book must not price an exit");
    assert_eq!(pos_book_ts(&pm, &p.id), stale, "last book seen is recorded");

    // The same bid, received NOW, is executable again.
    let reqs = pm.check_exits(
        &|token| {
            if token == "tok_BTC" {
                Some(BookView::from_snapshot(&two_sided_book(
                    dec!(0.60),
                    dec!(0.62),
                    now,
                )))
            } else {
                None
            }
        },
        now,
    );
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].exit_price, dec!(0.60));
}

fn pos_book_ts(pm: &PositionManager, id: &str) -> i64 {
    pm.open_positions()
        .iter()
        .find(|p| p.id == id)
        .map(|p| p.last_book_ts)
        .unwrap_or(0)
}

/// The close path prices the unsold remainder itself, so it must charge it
/// the ACTIVE schedule's taker fee rather than a curve restated at that call
/// site. 0.1 unsold share written off at 0.60 taker: the shipped legacy
/// curve charges 1.2% of the $0.06 notional = $0.00072, where the published
/// crypto curve (2.8% of price) would charge $0.00168 — $0.00096 more.
///
/// Both curves are priced through `dust_fee_for` — the expression the close
/// path itself runs (#234, item 5) — so the delta below is the close path's
/// delta and not a restatement's. The end-to-end half still runs under the one
/// schedule this process has installed (`OnceLock` is set-once, and installing
/// a second would poison every other test in the binary): what cannot be
/// shown in-process is the two closed positions side by side, and that is
/// stated here rather than papered over.
#[test]
fn close_dust_fee_follows_the_active_schedule() {
    assert_eq!(
        crate::exit_policy::fee_schedule().name,
        "legacy_quadratic",
        "no schedule is installed in tests, so the shipped default is active"
    );
    let mut pm = PositionManager::new(PositionConfig::default());
    let p = enter(
        &mut pm,
        params("BTC", SignalDirection::Up, dec!(0.4)),
        OrderRole::Maker,
        0,
    );
    // Sell 9.90 of 10 shares (sub-grid remainder stays) as a taker; that
    // fill's own fee is charged by the caller, as production does.
    let exit_fee = (dec!(1.2) / Decimal::ONE_HUNDRED) * dec!(0.6) * dec!(9.9);
    pm.apply_exit_fill(
        &p.id,
        dec!(9.9),
        dec!(0.6),
        exit_fee,
        OrderRole::Taker,
        Decimal::ZERO,
    )
    .unwrap();
    let closed = pm
        .close(&p.id, dec!(0.60), ExitReason::Manual, false, 1000)
        .unwrap();
    // gross = 5.94 (the 9.90 sold) + 0.06 (dust) − 4.00 (maker cost) = 2.00
    // fees  = 0.07128 (1.2% of the taker fill's 5.94) + 0.00072 (legacy
    //         dust) = 0.072
    assert_eq!(closed.net_pnl_usd, dec!(1.928));

    let dust_notional = dec!(0.6) * dec!(0.1);
    let legacy_dust = dust_fee_for(
        crate::exit_policy::legacy_quadratic_schedule(),
        dec!(0.60),
        dust_notional,
        false,
    );
    let official_dust = dust_fee_for(
        crate::exit_policy::official_schedule(),
        dec!(0.60),
        dust_notional,
        false,
    );
    // The absolute anchors, not only the delta: 1.2% and 2.8% of the $0.06
    // remainder, through the production expression.
    assert_eq!(legacy_dust, dec!(0.00072));
    assert_eq!(official_dust, dec!(0.00168));
    assert_eq!(official_dust - legacy_dust, dec!(0.00096));
    // A maker exit pays no dust fee under either schedule.
    for s in [
        crate::exit_policy::legacy_quadratic_schedule(),
        crate::exit_policy::official_schedule(),
    ] {
        assert_eq!(
            dust_fee_for(s, dec!(0.60), dust_notional, true),
            Decimal::ZERO,
            "{}: a maker exit pays nothing",
            s.name
        );
    }
}

#[test]
fn open_and_close_pnl_is_net_of_fees() {
    let mut pm = PositionManager::new(PositionConfig::default());
    // Maker entry (no fee), taker exit at 0.6 → gross 2.0 minus the exit fee.
    let p = enter(
        &mut pm,
        params("BTC", SignalDirection::Up, dec!(0.4)),
        OrderRole::Maker,
        0,
    );
    assert_eq!(p.shares, dec!(10));
    assert_eq!(p.cost_usd, dec!(4));
    assert_eq!(p.entry_fee_pct, Decimal::ZERO);
    let c = pm
        .close(&p.id, dec!(0.6), ExitReason::TakeProfit, false, 1000)
        .unwrap();
    assert!(c.net_pnl_usd < dec!(2.0));
    assert!(c.net_pnl_usd > dec!(1.9)); // fee is small around 0.6
    assert!(c.was_maker_entry);
    assert!(!c.was_maker_exit);
    assert_eq!(pm.daily_pnl_for("default"), c.net_pnl_usd);
    // Net is the ledger's own arithmetic: proceeds − basis − both fees.
    let entry_fee = (c.entry_fee_pct / Decimal::ONE_HUNDRED) * c.entry_price * c.shares;
    let exit_fee = (c.exit_fee_pct / Decimal::ONE_HUNDRED) * c.exit_price * c.shares;
    assert_eq!(c.net_pnl_usd, c.pnl_usd - entry_fee - exit_fee);
}

#[test]
fn capacity_and_same_asset_are_blocked() {
    let cfg = PositionConfig {
        max_positions: 2,
        asset_cooldown_sec: 0,
        loss_cooldown_sec: 0,
        exit_cooldown_sec: 0,
        ..Default::default()
    };
    let mut pm = PositionManager::new(cfg);
    enter(
        &mut pm,
        params("BTC", SignalDirection::Up, dec!(0.4)),
        OrderRole::Maker,
        0,
    );
    assert!(
        pm.can_open("default", Some("BTC"), Some(SignalDirection::Up), 1000)
            .is_err()
    );
    assert!(
        pm.can_open("default", Some("ETH"), Some(SignalDirection::Up), 1000)
            .is_ok()
    );
    enter(
        &mut pm,
        params("ETH", SignalDirection::Up, dec!(0.4)),
        OrderRole::Maker,
        1000,
    );
    assert!(
        pm.can_open("default", Some("SOL"), Some(SignalDirection::Up), 1000)
            .is_err()
    ); // max positions
}

/// #363: the per-asset gate is a COUNT. `max_positions_per_asset = 3`
/// admits the 2nd and 3rd same-asset entry and refuses the 4th — the
/// configurable form of the @Almach "same asset across concurrent rounds"
/// behaviour — while the default (`1`) keeps the historical exactly-one
/// rule with the SAME refusal text ("Already in {asset}").
#[test]
fn per_asset_cap_is_configurable_count() {
    let cfg = PositionConfig {
        max_positions: 5,
        max_positions_per_asset: 3,
        asset_cooldown_sec: 0,
        loss_cooldown_sec: 0,
        exit_cooldown_sec: 0,
        ..Default::default()
    };
    let mut pm = PositionManager::new(cfg);
    for i in 0..3 {
        assert!(
            pm.can_open("default", Some("BTC"), Some(SignalDirection::Up), 1000 + i)
                .is_ok(),
            "entry {i} of 3 must pass the per-asset cap"
        );
        enter(
            &mut pm,
            params("BTC", SignalDirection::Up, dec!(0.4)),
            OrderRole::Maker,
            1000 + i,
        );
    }
    // The 4th same-asset entry is refused, with the pinned vocabulary.
    let err = pm
        .can_open("default", Some("BTC"), Some(SignalDirection::Up), 2000)
        .unwrap_err();
    assert_eq!(err, "Already in BTC");
    // A different asset is unaffected by BTC's count.
    assert!(
        pm.can_open("default", Some("ETH"), Some(SignalDirection::Up), 2000)
            .is_ok()
    );
}

/// The expired-round slot leak (2026-10-11 production incident): two
/// positions from rounds that expired 12 days earlier held BOTH
/// `max_positions` slots — every entry for 6 days was refused with
/// "Max positions (2)" and a restart restores the book, so the freeze
/// was permanent. A position whose round has EXPIRED is settlement-dead
/// (winner redeems, loser zero — nothing new can happen to it), so it
/// must not count against the capacity gates; the same rule governs
/// `can_open`'s two caps and every `open_positions` fact the service
/// feeds to the policy/systemic judges.
#[test]
fn expired_round_positions_do_not_hold_capacity_slots() {
    let cfg = PositionConfig {
        max_positions: 2,
        asset_cooldown_sec: 0,
        loss_cooldown_sec: 0,
        exit_cooldown_sec: 0,
        ..Default::default()
    };
    let mut pm = PositionManager::new(cfg);
    // Two LIVE positions: capacity is full.
    enter(
        &mut pm,
        params("BTC", SignalDirection::Up, dec!(0.4)),
        OrderRole::Maker,
        0,
    );
    let second = enter(
        &mut pm,
        params("ETH", SignalDirection::Up, dec!(0.4)),
        OrderRole::Maker,
        0,
    );
    assert!(
        pm.can_open("default", Some("SOL"), Some(SignalDirection::Up), 1000)
            .is_err(),
        "both slots are held by live positions"
    );
    // The rounds expire (900_000 = 15m): capacity must free up even
    // though the positions cannot leave the book (no executable bid,
    // venue never answers).
    let expiry = second.expires_at_ms;
    assert!(
        pm.can_open(
            "default",
            Some("SOL"),
            Some(SignalDirection::Up),
            expiry + 1
        )
        .is_ok(),
        "an expired round's position must not hold a capacity slot"
    );
    // Same rule for the per-asset count: an expired BTC position no
    // longer blocks a new BTC entry.
    enter(
        &mut pm,
        params("BTC", SignalDirection::Up, dec!(0.4)),
        OrderRole::Maker,
        0,
    );
    let btc = pm
        .open_positions()
        .iter()
        .find(|p| p.asset == "BTC")
        .map(|p| p.expires_at_ms)
        .unwrap();
    assert!(
        pm.can_open("default", Some("BTC"), Some(SignalDirection::Up), btc + 1)
            .is_ok(),
        "an expired position must not count toward the per-asset cap"
    );
}

/// The boundary: a round counts as expired from its `expires_at_ms`
/// instant — the same comparison (`expires_at_ms > now_ms` = live) the
/// settlement tracker uses, so the two never disagree about whether a
/// position is still in its running round.
#[test]
fn capacity_expiry_boundary_is_the_expiry_millisecond() {
    let mut pm = PositionManager::new(PositionConfig {
        max_positions: 1,
        asset_cooldown_sec: 0,
        loss_cooldown_sec: 0,
        exit_cooldown_sec: 0,
        ..Default::default()
    });
    let pos = enter(
        &mut pm,
        params("BTC", SignalDirection::Up, dec!(0.4)),
        OrderRole::Maker,
        0,
    );
    let expiry = pos.expires_at_ms;
    assert!(
        pm.can_open(
            "default",
            Some("ETH"),
            Some(SignalDirection::Up),
            expiry - 1
        )
        .is_err()
    );
    assert!(
        pm.can_open("default", Some("ETH"), Some(SignalDirection::Up), expiry)
            .is_ok()
    );
}

/// #363: default config = the historical binary gate. The refusal text is
/// what `classify_rejection` buckets as `positions.already_in` — pinned
/// here so a wording drift fails a test instead of silently breaking the
/// WebUI's refusal attribution.
#[test]
fn default_per_asset_cap_is_one_with_pinned_text() {
    assert_eq!(PositionConfig::default().max_positions_per_asset, 1);
    let mut pm = PositionManager::new(PositionConfig {
        asset_cooldown_sec: 0,
        loss_cooldown_sec: 0,
        exit_cooldown_sec: 0,
        ..Default::default()
    });
    enter(
        &mut pm,
        params("BTC", SignalDirection::Up, dec!(0.4)),
        OrderRole::Maker,
        0,
    );
    let err = pm
        .can_open("default", Some("BTC"), Some(SignalDirection::Up), 1000)
        .unwrap_err();
    assert_eq!(err, "Already in BTC");
}

/// The capacity gates are PER-ACCOUNT books: account "a" holding every
/// `max_positions` slot (and its own per-asset cap) never blocks
/// account "b"'s first entry — the whole-book counting of the pre-fanout
/// manager would have refused "b" on "a"'s fullness.
#[test]
fn capacity_is_per_account_a_full_book_leaves_b_free() {
    let mut pm = PositionManager::new(PositionConfig {
        max_positions: 1,
        max_positions_per_asset: 1,
        asset_cooldown_sec: 0,
        loss_cooldown_sec: 0,
        exit_cooldown_sec: 0,
        ..Default::default()
    });
    // Account "a" holds the only slot, in BTC.
    let mut a = params("BTC", SignalDirection::Up, dec!(0.4));
    a.account_id = crate::model::AccountId("a".into());
    enter(&mut pm, a, OrderRole::Maker, 0);
    // "a" is full — both caps refuse it.
    assert!(
        pm.can_open("a", Some("ETH"), Some(SignalDirection::Up), 1000)
            .is_err(),
        "account a's own book is full"
    );
    assert!(
        pm.can_open("a", Some("BTC"), Some(SignalDirection::Down), 1000)
            .is_err(),
        "account a's per-asset cap holds too"
    );
    // "b" trades the SAME asset freely: "a"'s book is not "b"'s pot.
    assert!(
        pm.can_open("b", Some("BTC"), Some(SignalDirection::Up), 1000)
            .is_ok(),
        "account b's fresh book admits the asset account a is full in"
    );
    // ...and "b"'s own entry then binds "b"'s book without touching "a"'s.
    let mut b = params("BTC", SignalDirection::Up, dec!(0.4));
    b.account_id = crate::model::AccountId("b".into());
    enter(&mut pm, b, OrderRole::Maker, 0);
    assert!(
        pm.can_open("b", Some("ETH"), Some(SignalDirection::Up), 1000)
            .is_err(),
        "b's own cap now binds"
    );
}

#[test]
fn loss_sets_asset_cooldown() {
    let cfg = PositionConfig {
        asset_cooldown_sec: 90,
        loss_cooldown_sec: 180,
        ..Default::default()
    };
    let mut pm = PositionManager::new(cfg);
    let p = enter(
        &mut pm,
        params("BTC", SignalDirection::Up, dec!(0.4)),
        OrderRole::Maker,
        0,
    );
    pm.close(&p.id, dec!(0.3), ExitReason::StopLoss, false, 10_000)
        .unwrap();
    // Immediately after a loss, re-entry is blocked by asset/loss cooldown.
    assert!(
        pm.can_open("default", Some("BTC"), Some(SignalDirection::Down), 11_000)
            .is_err()
    );
    // Far in the future it clears.
    assert!(
        pm.can_open(
            "default",
            Some("BTC"),
            Some(SignalDirection::Down),
            10_000 + 200_000
        )
        .is_ok()
    );
}

#[test]
fn daily_loss_limit_blocks_new_positions() {
    let cfg = PositionConfig {
        max_daily_loss_usd: dec!(1),
        asset_cooldown_sec: 0,
        loss_cooldown_sec: 0,
        stop_loss_cooldown_sec: 0,
        ..Default::default()
    };
    let mut pm = PositionManager::new(cfg);
    let p = enter(
        &mut pm,
        params("BTC", SignalDirection::Up, dec!(0.4)),
        OrderRole::Maker,
        0,
    );
    pm.close(&p.id, dec!(0.05), ExitReason::StopLoss, false, 1000)
        .unwrap();
    assert!(pm.daily_pnl_for("default") < dec!(-1));
    assert!(
        pm.can_open("default", Some("ETH"), Some(SignalDirection::Up), 2000)
            .is_err()
    );
}

/// 每账户日亏（2026-10-25 操作者裁决）：一个账户把日亏预算烧穿，兄弟
/// 账户的当天分文不动、进场照旧——单槽时代 poly 的亏损会冻结 default。
#[test]
fn one_account_s_daily_loss_never_burns_a_sibling_s_budget() {
    let cfg = PositionConfig {
        max_daily_loss_usd: dec!(1),
        asset_cooldown_sec: 0,
        loss_cooldown_sec: 0,
        stop_loss_cooldown_sec: 0,
        ..Default::default()
    };
    let mut pm = PositionManager::new(cfg);
    // default 账户亏穿 cap（entry 0.4 → exit 0.05 × 10 股 ≈ −3.5）。
    let p = enter(
        &mut pm,
        params("BTC", SignalDirection::Up, dec!(0.4)),
        OrderRole::Maker,
        0,
    );
    pm.close(&p.id, dec!(0.05), ExitReason::StopLoss, false, 1000)
        .unwrap();
    assert!(pm.daily_loss_tripped_for("default"));
    assert!(
        pm.can_open("default", Some("ETH"), Some(SignalDirection::Up), 2000)
            .is_err()
    );
    // poly 账户自己的一天：从零开始，进场不被兄弟的冻结拦住。
    let mut poly_params = params("BTC", SignalDirection::Up, dec!(0.4));
    poly_params.account_id = AccountId::from("poly");
    let q = enter(&mut pm, poly_params, OrderRole::Maker, 2000);
    assert!(pm.daily_pnl_for("poly") == Decimal::ZERO);
    assert!(!pm.daily_loss_tripped_for("poly"));
    assert!(
        pm.can_open("poly", Some("ETH"), Some(SignalDirection::Up), 3000)
            .is_ok(),
        "a sibling's tripped budget must not freeze poly's entries"
    );
    // poly 自己也亏穿：只冻 poly。
    pm.close(&q.id, dec!(0.05), ExitReason::StopLoss, false, 4000)
        .unwrap();
    assert!(pm.daily_loss_tripped_for("poly"));
    assert!(
        pm.can_open("poly", Some("ETH"), Some(SignalDirection::Up), 5000)
            .is_err()
    );
    // 系统性事实（global_kill_switch 读数）= 两账户之和。
    assert_eq!(
        pm.daily_pnl_total(),
        pm.daily_pnl_for("default") + pm.daily_pnl_for("poly")
    );
}

// ── #464: the daily-loss breaker reads the NET mark (realized + unrealized)

/// A maker entry of `shares` at `price`, then the position's mark moved to
/// `mark` through the real per-tick valuation (a two-sided book whose bid
/// IS the mark). Returns the position id.
fn enter_marked(
    pm: &mut PositionManager,
    price: Decimal,
    shares: Decimal,
    mark: Decimal,
) -> String {
    let pos = pm.open(params("BTC", SignalDirection::Up, price), 0);
    pm.apply_entry_fill(&pos.id, shares, price, Decimal::ZERO, OrderRole::Maker)
        .unwrap();
    pm.tick(&pos.id, Some(&two_sided_book(mark, mark, 1000)), 1000);
    pos.id
}

/// #464: the breaker must read the day's NET loss. A realized −6 against a
/// 10 cap is still alive; the open position's −20 paper loss takes the
/// judgement input to ≈ −26, past the cap — entries freeze BEFORE the loss
/// has to be realized. Red if the judgement ever reverts to realized-only.
#[test]
fn daily_loss_breaker_trips_on_unrealized_loss() {
    let cfg = PositionConfig {
        max_daily_loss_usd: dec!(10),
        asset_cooldown_sec: 0,
        loss_cooldown_sec: 0,
        stop_loss_cooldown_sec: 0,
        ..Default::default()
    };
    let mut pm = PositionManager::new(cfg);
    // realized leg: 20 shares 0.40 → 0.10 (taker out) ≈ −6, inside the cap.
    let eth = enter_marked(&mut pm, dec!(0.40), dec!(20), dec!(0.40));
    pm.close(&eth, dec!(0.10), ExitReason::StopLoss, false, 1000)
        .unwrap();
    let realized = pm.daily_pnl_for("default");
    assert!(realized < Decimal::ZERO && realized > dec!(-10));
    assert!(
        !pm.daily_loss_tripped_for("default"),
        // Static message: CodeQL's cleartext-logging check flags test-assert
        // interpolation of measured values (#693's fix, same family) — the
        // actual figures are on the line above anyway.
        "realized alone is inside the 10 cap"
    );
    // unrealized leg: 100 shares entered 0.40, marked 0.20 → −20 on paper.
    enter_marked(&mut pm, dec!(0.40), dec!(100), dec!(0.20));
    assert_eq!(pm.unrealized_pnl_usd(), dec!(-20));
    assert!(realized + pm.unrealized_pnl_usd() <= dec!(-10));
    assert!(
        pm.daily_loss_tripped_for("default"),
        "realized + unrealized −20 must breach the 10 cap"
    );
    assert!(
        pm.can_open("default", Some("SOL"), Some(SignalDirection::Up), 2000)
            .is_err(),
        "the net breach freezes entries"
    );
}

/// #464 reverse acceptance: `daily_loss_marks_unrealized = false` restores
/// the pre-#464 breaker — the same books, the same realized loss, and the
/// entry the net reading refuses is allowed again.
#[test]
fn realized_only_config_ignores_unrealized_loss() {
    let cfg = PositionConfig {
        max_daily_loss_usd: dec!(10),
        daily_loss_marks_unrealized: false,
        asset_cooldown_sec: 0,
        loss_cooldown_sec: 0,
        stop_loss_cooldown_sec: 0,
        ..Default::default()
    };
    let mut pm = PositionManager::new(cfg);
    let eth = enter_marked(&mut pm, dec!(0.40), dec!(20), dec!(0.40));
    pm.close(&eth, dec!(0.10), ExitReason::StopLoss, false, 1000)
        .unwrap();
    let realized = pm.daily_pnl_for("default");
    assert!(realized + dec!(-20) <= dec!(-10));
    enter_marked(&mut pm, dec!(0.40), dec!(100), dec!(0.20));
    // The paper loss still READS off the book (the panel shows it)…
    assert_eq!(pm.unrealized_pnl_usd(), dec!(-20));
    // …but the breaker must not judge on it.
    assert!(
        !pm.daily_loss_tripped_for("default"),
        "realized-only against a 10 cap is not a trip"
    );
    assert!(
        pm.can_open("default", Some("SOL"), Some(SignalDirection::Up), 2000)
            .is_ok()
    );
}

/// #464 edge: a position with no mark (`current_price = 0`) contributes
/// nothing — a missing quote must never be read as an infinite loss (or an
/// infinite gain).
#[test]
fn a_zero_mark_contributes_no_unrealized_pnl() {
    let cfg = PositionConfig {
        max_daily_loss_usd: dec!(10),
        asset_cooldown_sec: 0,
        loss_cooldown_sec: 0,
        stop_loss_cooldown_sec: 0,
        ..Default::default()
    };
    let mut pm = PositionManager::new(cfg);
    let id = enter_marked(&mut pm, dec!(0.40), dec!(100), dec!(0.30));
    // Strip the mark the way a pre-book snapshot looks: no reference price.
    let mut held = pm
        .open_positions()
        .iter()
        .find(|p| p.id == id)
        .unwrap()
        .clone();
    held.current_price = Decimal::ZERO;
    pm.restore_open(vec![held]);
    assert_eq!(pm.unrealized_pnl_usd(), Decimal::ZERO);
    assert!(!pm.daily_loss_tripped_for("default"));
}

// ── #267 / #268: the exit ladder must keep BOTH halves on a broken book ──

/// #267, acceptance 2, through the real caller: the profit side fires on the
/// position's fresh last known price, there is no bid to price the SELL
/// against, and the only thing that comes out is ONE held-exit report naming
/// the rule that fired. A "fill at the mid" implementation would surface
/// here as a request priced off the ask/2 phantom this test blocks.
#[test]
fn a_profit_exit_with_no_bid_is_reported_never_priced() {
    // Trailing explicitly ON: this pins the #267 reporting MECHANISM (the
    // rule that wanted out is named), orthogonal to the calibrated default
    // of banking nothing via trailing sells.
    let mut pm = PositionManager::new(PositionConfig {
        exit: ExitConfig {
            trailing_enabled: true,
            ..Default::default()
        },
        ..Default::default()
    });
    let p = enter(
        &mut pm,
        params("BTC", SignalDirection::Up, dec!(0.40)),
        OrderRole::Maker,
        0,
    );
    // Value the position at +60%, then at +25%: the dashboard ticks on every
    // book update while exits are evaluated on their own cadence, so a fall
    // can be valued before the next evaluation judges it. Both quotes are
    // LIVE, so this is a market move and not a stale memory.
    pm.tick(
        &p.id,
        Some(&two_sided_book(dec!(0.64), dec!(0.66), 100_000)),
        100_000,
    );
    pm.tick(
        &p.id,
        Some(&two_sided_book(dec!(0.50), dec!(0.52), 490_000)),
        490_000,
    );

    // The buy side is swept: no bid at all, only an ask 0.90. The snapshot
    // carries no clock (`timestamp 0`), which is the "unknown age counts as
    // fresh" branch of #268 item 1 — not an outage.
    let now = 498_000;
    let reqs = pm.check_exits(
        &|_| Some(BookView::from_snapshot(&one_sided_book(dec!(0.90), 0))),
        now,
    );
    assert!(
        reqs.is_empty(),
        "no bid ⇒ no executable price ⇒ no SELL, whatever the judgement said: {reqs:?}"
    );

    let held = pm.drain_suppressed_stops();
    assert_eq!(held.len(), 1, "exactly one held exit: {held:?}");
    let ev = &held[0];
    assert_eq!(ev.cause, StopSuppressionCause::NoExecutableQuote);
    assert_eq!(
        ev.reason,
        Some(ExitReason::TrailingStop),
        "the rule that wanted out is named, not flattened into a stop"
    );
    assert_eq!(ev.bid, Decimal::ZERO, "the book showed no bid at all");
    assert_eq!(
        ev.pnl_pct_at_bid,
        Decimal::ZERO,
        "no bid is no pnl to report — `pnl_pct` would have called it -100%"
    );
    assert_eq!(
        ev.mid,
        dec!(0.50),
        "judged on the position's own last known price"
    );
    assert_eq!(ev.pnl_pct_at_mid, dec!(25));
    assert_eq!(
        ev.book_age_ms, None,
        "no clock on the snapshot ⇒ unknown age, which is not an outage"
    );
    // #267, item 4: "protective stop" is a claim about WHICH rule fired.
    assert!(
        !ev.message().contains("protective stop"),
        "{}",
        ev.message()
    );
    assert!(ev.message().contains("TrailingStop"), "{}", ev.message());
    assert_eq!(pm.suppressed_stop_count(), 1);
}

/// The strategies' own trailing declarations ride on top of the kernel-wide
/// config for their positions: kernel OFF + declaration `true` banks the
/// profit anyway (the spread_arb shape — its +46.7% peak dissolved into a
/// stop-loss because nothing banked it); kernel ON + declaration `false`
/// refuses the sell (the REDEEM-family shape the issue-356 calibration
/// came from); no declaration = follow the kernel value byte-identically.
#[test]
fn a_strategy_trailing_declaration_overrides_the_kernel_value_for_its_positions() {
    // Kernel-wide OFF — the shipped calibration. The book climbs to +60%
    // then falls back to +25%: a give-back the trail would bank.
    let mut pm = PositionManager::new(PositionConfig::default());
    // spread_arb declared `true`; a neighbor declared `false`; the default
    // fixture strategy declared nothing.
    pm.set_trailing_overrides(
        [("declared_on".into(), true), ("declared_off".into(), false)]
            .into_iter()
            .collect(),
    );
    let mut on = params("BTC", SignalDirection::Up, dec!(0.40));
    on.strategy = "declared_on".into();
    let on_pos = enter(&mut pm, on, OrderRole::Maker, 0);

    // Kernel OFF + declaration ON ⇒ the give-back IS banked: the ladder
    // fires TrailingStop once the give-back exceeds the floor (min 8%).
    pm.tick(
        &on_pos.id,
        Some(&two_sided_book(dec!(0.64), dec!(0.66), 100_000)),
        100_000,
    );
    pm.tick(
        &on_pos.id,
        Some(&two_sided_book(dec!(0.50), dec!(0.52), 490_000)),
        490_000,
    );
    let now = on_pos.expires_at_ms - 10_000;
    let reqs = pm.check_exits(
        &|_| {
            Some(BookView::from_snapshot(&two_sided_book(
                dec!(0.50),
                dec!(0.52),
                now,
            )))
        },
        now,
    );
    assert_eq!(
        reqs.len(),
        1,
        "kernel OFF + declaration ON ⇒ trail banks: {reqs:?}"
    );
    assert_eq!(reqs[0].reason, ExitReason::TrailingStop);

    // Kernel OFF + NO declaration ⇒ byte-identical to the historical
    // kernel: no trailing anywhere (the same give-back is held).
    let mut quiet = PositionManager::new(PositionConfig::default());
    let silent = enter(
        &mut quiet,
        params("BTC", SignalDirection::Up, dec!(0.40)),
        OrderRole::Maker,
        0,
    );
    quiet.tick(
        &silent.id,
        Some(&two_sided_book(dec!(0.64), dec!(0.66), 100_000)),
        100_000,
    );
    quiet.tick(
        &silent.id,
        Some(&two_sided_book(dec!(0.50), dec!(0.52), 490_000)),
        490_000,
    );
    let reqs = quiet.check_exits(
        &|_| {
            Some(BookView::from_snapshot(&two_sided_book(
                dec!(0.50),
                dec!(0.52),
                now,
            )))
        },
        now,
    );
    assert!(
        !reqs.iter().any(|r| r.reason == ExitReason::TrailingStop),
        "no declaration ⇒ follow the kernel OFF: {reqs:?}"
    );

    // Kernel ON + declaration OFF ⇒ the REDEEM family keeps its doctrine:
    // no profit-banking sell even though the kernel value would trail.
    let mut loud = PositionManager::new(PositionConfig {
        exit: ExitConfig {
            trailing_enabled: true,
            ..Default::default()
        },
        ..Default::default()
    });
    loud.set_trailing_overrides([("declared_off".into(), false)].into_iter().collect());
    let mut off = params("BTC", SignalDirection::Up, dec!(0.40));
    off.strategy = "declared_off".into();
    let off_pos = enter(&mut loud, off, OrderRole::Maker, 0);
    loud.tick(
        &off_pos.id,
        Some(&two_sided_book(dec!(0.64), dec!(0.66), 100_000)),
        100_000,
    );
    loud.tick(
        &off_pos.id,
        Some(&two_sided_book(dec!(0.50), dec!(0.52), 490_000)),
        490_000,
    );
    let reqs = loud.check_exits(
        &|_| {
            Some(BookView::from_snapshot(&two_sided_book(
                dec!(0.50),
                dec!(0.52),
                now,
            )))
        },
        now,
    );
    assert!(
        !reqs.iter().any(|r| r.reason == ExitReason::TrailingStop),
        "kernel ON + declaration OFF ⇒ the trail is disarmed for this strategy: {reqs:?}"
    );
}

/// A hold-to-settlement strategy's protective stop runs the WIDE containment
/// width (`holder_stop_pct`, flat — the tightening ramp may not creep back in
/// as expiry approaches), while a non-holder's stays the 12% ladder
/// byte-identically. The @almach compromise: a holder's exit is SUPPOSED to
/// be the settlement, so the normal 0.4 → 0.25 pre-settlement drift must ride
/// unrealized; only a real slide to zero (−50%) may be cut.
#[test]
fn a_holder_declaration_widens_the_protective_stop_for_its_positions() {
    let mut pm = PositionManager::new(PositionConfig::default());
    // lua_momentum declared; the default fixture strategy did not.
    pm.set_holder_stop_overrides(
        [("holder".into(), pm.config.exit.holder_stop_pct)]
            .into_iter()
            .collect(),
    );

    // Same entry and same price path for both: 0.40 entry, drifting to 0.26
    // (−35%) — inside the old 12%+tightening window, inside a holder's ride.
    let mut held = params("BTC", SignalDirection::Up, dec!(0.40));
    held.strategy = "holder".into();
    let holder_pos = enter(&mut pm, held, OrderRole::Maker, 0);
    let mut plain = params("ETH", SignalDirection::Up, dec!(0.40));
    plain.strategy = "bystander".into();
    let plain_pos = enter(&mut pm, plain, OrderRole::Maker, 0);

    let drift = |token: &str| -> Option<BookView> {
        if token != "tok_BTC" && token != "tok_ETH" {
            return None;
        }
        Some(BookView::from_snapshot(&two_sided_book(
            dec!(0.26),
            dec!(0.28),
            0,
        )))
    };

    // Far from expiry: the 12% ladder would already have fired at 0.348; the
    // holder rides the drift, the non-holder is cut.
    let now = holder_pos.expires_at_ms - 400_000;
    let reqs = pm.check_exits(&drift, now);
    assert_eq!(reqs.len(), 1, "exactly the non-holder exits: {reqs:?}");
    assert_eq!(reqs[0].position_id, plain_pos.id);
    assert_eq!(reqs[0].reason, ExitReason::StopLoss);
    assert!(
        !reqs.iter().any(|r| r.position_id == holder_pos.id),
        "a holder rides a −35% drift: {reqs:?}"
    );

    // Near expiry (inside the 300 s tightening ramp): the ramp's floor would
    // pull a widened stop back to ~10% — the holder's width stays FLAT, so
    // the very same drift is still not exitable.
    let mut pm2 = PositionManager::new(PositionConfig::default());
    pm2.set_holder_stop_overrides(
        [("holder".into(), pm2.config.exit.holder_stop_pct)]
            .into_iter()
            .collect(),
    );
    let mut held2 = params("BTC", SignalDirection::Up, dec!(0.40));
    held2.strategy = "holder".into();
    let holder2 = enter(&mut pm2, held2, OrderRole::Maker, 0);
    let now = holder2.expires_at_ms - 10_000;
    let reqs = pm2.check_exits(&drift, now);
    assert!(
        reqs.is_empty(),
        "no ramp re-tightening near expiry for a holder: {reqs:?}"
    );

    // Containment still bites: a slide past the wide width (0.40 → 0.18 is
    // −55% > 50%) IS cut — the compromise is a wider stop, not no stop.
    let mut pm3 = PositionManager::new(PositionConfig::default());
    pm3.set_holder_stop_overrides(
        [("holder".into(), pm3.config.exit.holder_stop_pct)]
            .into_iter()
            .collect(),
    );
    let mut held3 = params("BTC", SignalDirection::Up, dec!(0.40));
    held3.strategy = "holder".into();
    let holder3 = enter(&mut pm3, held3, OrderRole::Maker, 0);
    let now = holder3.expires_at_ms - 400_000;
    let reqs = pm3.check_exits(
        &|_| {
            Some(BookView::from_snapshot(&two_sided_book(
                dec!(0.18),
                dec!(0.20),
                now,
            )))
        },
        now,
    );
    assert_eq!(reqs.len(), 1, "the tail guard fires at −55%: {reqs:?}");
    assert_eq!(reqs[0].reason, ExitReason::StopLoss);
}

/// #268, item 2: a stale book is an outage, not a thin market. Nothing is
/// sold while it lasts; when a fresh book arrives the very same rule exits
/// normally — the hold is a wait, never a permanent trap — and the outage
/// clock restarts rather than accumulating a lifetime total.
#[test]
fn a_stale_book_is_an_outage_that_holds_and_then_releases() {
    // `0` = report every tick, so each of the three ticks is observed alone.
    // Trailing explicitly ON (mechanism test — the rule being held/released
    // is the trailing stop); the calibrated default ships it OFF.
    let cfg = PositionConfig {
        exit: ExitConfig {
            trailing_enabled: true,
            ..Default::default()
        },
        stop_suppression_repeat_sec: 0,
        ..Default::default()
    };
    let mut pm = PositionManager::new(cfg);
    let p = enter(
        &mut pm,
        params("BTC", SignalDirection::Up, dec!(0.40)),
        OrderRole::Maker,
        0,
    );
    pm.tick(
        &p.id,
        Some(&two_sided_book(dec!(0.64), dec!(0.66), 100_000)),
        100_000,
    );
    pm.tick(
        &p.id,
        Some(&two_sided_book(dec!(0.50), dec!(0.52), 490_000)),
        490_000,
    );

    // The feed dies: the book freezes at 490_000 and is two minutes old at
    // decision time (budget: the shipped 60 s).
    let frozen = two_sided_book(dec!(0.50), dec!(0.52), 490_000);

    let now = 610_000;
    let reqs = pm.check_exits(&|_| Some(BookView::from_snapshot(&frozen)), now);
    assert!(
        reqs.is_empty(),
        "a stale bid must not price a SELL: {reqs:?}"
    );
    let held = pm.drain_suppressed_stops();
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(
        held[0].cause,
        StopSuppressionCause::StaleBook,
        "a dead feed must be told apart from a thin market (#268, item 1)"
    );
    assert_eq!(held[0].book_age_ms, Some(120_000));
    assert_eq!(held[0].reason, Some(ExitReason::TrailingStop));
    assert!(!held[0].escalated, "two minutes is not yet worth a page");
    assert!(
        held[0].message().contains("stale book"),
        "{}",
        held[0].message()
    );

    // Recovery: the same bid, received NOW, is executable again and the held
    // rule exits at it.
    let now = 611_000;
    let reqs = pm.check_exits(
        &|_| {
            Some(BookView::from_snapshot(&two_sided_book(
                dec!(0.50),
                dec!(0.52),
                now,
            )))
        },
        now,
    );
    assert_eq!(
        reqs.len(),
        1,
        "recovery must release the held exit: {reqs:?}"
    );
    assert_eq!(reqs[0].reason, ExitReason::TrailingStop);
    assert_eq!(reqs[0].exit_price, dec!(0.50));
    assert!(pm.drain_suppressed_stops().is_empty());

    // …and the outage clock restarted: the next unpriceable tick counts from
    // itself, so what the escalation reads is a CONTINUOUS outage.
    let now = 612_000;
    let reqs = pm.check_exits(&|_| Some(BookView::from_snapshot(&frozen)), now);
    assert!(reqs.is_empty());
    let held = pm.drain_suppressed_stops();
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(
        held[0].unpriceable_for_ms, 0,
        "the clock restarted at recovery"
    );
    assert!(!held[0].escalated);
}

/// #268, item 4 (reverse acceptance): widen the budget past the outage and
/// the VERY SAME frozen book prices an exit. That is what proves the
/// staleness budget — and not some other gate — is what held it.
#[test]
fn a_widened_book_budget_prices_the_same_outage() {
    // Trailing explicitly ON (mechanism test — the rule pricing on the widened
    // budget is the trailing stop); the calibrated default ships it OFF.
    let cfg = PositionConfig {
        exit: ExitConfig {
            trailing_enabled: true,
            max_book_age_sec: 86_400,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut pm = PositionManager::new(cfg);
    let p = enter(
        &mut pm,
        params("BTC", SignalDirection::Up, dec!(0.40)),
        OrderRole::Maker,
        0,
    );
    pm.tick(
        &p.id,
        Some(&two_sided_book(dec!(0.64), dec!(0.66), 100_000)),
        100_000,
    );
    pm.tick(
        &p.id,
        Some(&two_sided_book(dec!(0.50), dec!(0.52), 490_000)),
        490_000,
    );

    let now = 610_000;
    let reqs = pm.check_exits(
        &|_| {
            Some(BookView::from_snapshot(&two_sided_book(
                dec!(0.50),
                dec!(0.52),
                490_000,
            )))
        },
        now,
    );
    assert_eq!(
        reqs.len(),
        1,
        "a 24 h budget must accept the same book: {reqs:?}"
    );
    assert_eq!(reqs[0].exit_price, dec!(0.50), "priced at the bid it shows");
    assert_eq!(reqs[0].reason, ExitReason::TrailingStop);
    assert!(
        pm.drain_suppressed_stops().is_empty(),
        "nothing was held, so nothing is reported"
    );
}

/// #268, item 2: an outage that outlives `N x` the book budget stops being a
/// routine hold. It is escalated through the SAME withheld-exit channel the
/// panel already turns into a risk alert (`Core::emit_suppressed_stops`),
/// and it says the position could not ACT rather than blaming a stop.
#[test]
fn a_long_outage_escalates_through_the_existing_alert_channel() {
    // A 10 s book budget shrinks the escalation point (3 budgets) to 30 s of
    // continuous silence, so the test does not wait out the shipped 180 s.
    let cfg = PositionConfig {
        exit: ExitConfig {
            max_book_age_sec: 10,
            ..Default::default()
        },
        stop_suppression_repeat_sec: 0,
        ..Default::default()
    };
    let mut pm = PositionManager::new(cfg);
    let p = enter(
        &mut pm,
        params("BTC", SignalDirection::Up, dec!(0.40)),
        OrderRole::Maker,
        0,
    );
    // +10%: a position no exit rule wants out of, so what the outage reports
    // is the inability to act rather than a rule that fired.
    pm.tick(
        &p.id,
        Some(&two_sided_book(dec!(0.44), dec!(0.46), 100_000)),
        100_000,
    );
    let frozen = two_sided_book(dec!(0.44), dec!(0.46), 100_000);

    // First unpriceable tick: the clock starts, nothing is escalated yet.
    let now = 501_000;
    let reqs = pm.check_exits(&|_| Some(BookView::from_snapshot(&frozen)), now);
    assert!(reqs.is_empty());
    assert!(
        pm.drain_suppressed_stops().is_empty(),
        "a quiet stale tick inside the budget is not yet a page"
    );

    // Past `N x` the book budget the same silence is escalated.
    let now = 532_000;
    let reqs = pm.check_exits(&|_| Some(BookView::from_snapshot(&frozen)), now);
    assert!(reqs.is_empty(), "an outage is never a reason to sell");
    let held = pm.drain_suppressed_stops();
    assert_eq!(held.len(), 1, "{held:?}");
    let ev = &held[0];
    assert_eq!(ev.cause, StopSuppressionCause::StaleBook);
    assert_eq!(
        ev.reason, None,
        "no rule fired — the position could not act"
    );
    assert_eq!(ev.book_age_ms, Some(432_000));
    assert_eq!(ev.unpriceable_for_ms, 31_000);
    assert!(
        ev.escalated,
        "31 s unpriceable against a 10 s budget is an outage"
    );
    assert!(
        ev.message().contains("position UNPRICEABLE"),
        "{}",
        ev.message()
    );
    assert!(ev.message().contains("ESCALATED"), "{}", ev.message());
    assert!(
        !ev.message().contains("protective stop"),
        "{}",
        ev.message()
    );
    assert_eq!(pm.suppressed_stop_count(), 1);
}
// ── MERGE (task 2.2): pair collection for pair-discount arbitrage ────────

/// One leg of a binary condition. `params` above pins a shared token;
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

#![cfg(test)]

//! issue #953 — the OPEN-position snapshot and the persisted strategy
//! enablement set follow the book. #941/#942 already split the cash/risk/
//! trade faces per mode (journal `.dry`, `daily-loss.dry.json`,
//! `risk-state.dry.json`, `trades-live.jsonl`); the position log and the
//! strategy-state file were the last two shared paths, so a same-directory
//! live→dry sequence switch made the dry ledger restore live's held
//! positions and seed its `hft-N` counter past live's id baseline.
//!
//! These tests prove the acceptance criteria: A (a live→dry switch boots
//! into dry's own snapshot family — live's held positions and id baseline
//! invisible), B (dry→live sees only live's own snapshot/history), and the
//! boundary's id half at the Core level. The boot wiring puts the book's
//! paths in the config exactly as shown here (`restore_positions` and the
//! #942 scan read nothing but their config's paths, so the path split IS
//! the isolation). Acceptance E: no existing file is moved, renamed or
//! deleted — the bare names (`positions.jsonl`, `strategy-state.json`)
//! BELONG to live from this split on, `--position-log` / `--strategy-state`
//! still win, and the strategies' face is covered by the two default names
//! asserted together in `bin_config_args_tests`.

use super::*;
use rust_decimal_macros::dec;

fn snap_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "bk-953-snap-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|t| t.subsec_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// The per-mode file names exactly as the binary derives them (#953):
/// trade history at its ledger, open positions keyed to the book. The
/// fixtures below address the files THROUGH THIS TABLE — the same one
/// `default_position_log_for` / `default_trade_log_path` produce — so a
/// drift between the helper and the test breaks loudly here.
fn family(dir: &std::path::Path, trading: bool) -> (std::path::PathBuf, std::path::PathBuf) {
    let positions = dir.join(if trading {
        "data/positions/positions.jsonl"
    } else {
        "data/positions/positions.dry.jsonl"
    });
    let trades = dir.join(if trading {
        "data/trades/trades-live.jsonl"
    } else {
        "data/trades/trades.jsonl"
    });
    (positions, trades)
}

/// A core on one side of the mode boundary, pointed at THAT mode's file
/// family inside `dir` — the same wiring `main` produces after #953 from
/// the resolved `Mode`, with no explicit path flags.
fn book_mode_core(dir: &std::path::Path, trading: bool, balance: Decimal) -> Core {
    let (positions, trades) = family(dir, trading);
    let mut c = Core::new(CoreConfig {
        mode: if trading { Mode::Live } else { Mode::Dry },
        trade_log_path: Some(trades.to_string_lossy().into_owned()),
        position_log_path: Some(positions.to_string_lossy().into_owned()),
        manual_ops_audit_path: Some(dir.join("manual-ops.jsonl").to_string_lossy().into_owned()),
        dry_seed_balance: balance,
        ..Default::default()
    });
    c.set_balance(balance);
    c
}

/// A closed-trade row, written the way `TradeDb::record` does, to seed
/// (or NOT seed) a book's id baseline.
fn closed_trade(id: &str) -> crate::trade_db::TradeRecord {
    crate::trade_db::TradeRecord::from_closed(&crate::position::ClosedPosition {
        id: id.into(),
        strategy: "spread_arb".into(),
        asset: "BTC".into(),
        direction: crate::model::SignalDirection::Up,
        token_id: "tok".into(),
        condition_id: "cond".into(),
        entry_price: dec!(0.4),
        exit_price: dec!(0.5),
        shares: dec!(10),
        cost_usd: dec!(4),
        was_maker_entry: true,
        was_maker_exit: true,
        entry_fee_pct: Decimal::ZERO,
        exit_fee_pct: Decimal::ZERO,
        maker_rebate_usd: Decimal::ZERO,
        pnl_usd: Decimal::ONE,
        pnl_pct: Decimal::from(25),
        net_pnl_usd: Decimal::ONE,
        net_pnl_pct: Decimal::from(25),
        high_pnl_pct: Decimal::ZERO,
        low_pnl_pct: Decimal::ZERO,
        hold_time_sec: 0,
        exit_reason: crate::model::ExitReason::TakeProfit,
        entered_at_ms: 0,
        exited_at_ms: 1,
        entry_role: crate::model::OrderRole::Maker,
        exit_role: crate::model::OrderRole::Taker,
        dust_shares: Decimal::ZERO,
        account_id: crate::model::default_account_id(),
        venue: String::new(),
    })
}

/// One minimally-valid OPEN-position line, serialized the way
/// `PositionDb::save` writes it.
fn held(id: &str) -> crate::position::OpenPosition {
    crate::position::OpenPosition {
        id: id.into(),
        strategy: "spread_arb".into(),
        asset: "BTC".into(),
        direction: crate::model::SignalDirection::Up,
        token_id: "tok".into(),
        condition_id: "cond".into(),
        entry_price: dec!(0.4),
        current_price: dec!(0.4),
        prev_price: dec!(0.4),
        shares: dec!(10),
        cost_usd: dec!(4),
        was_maker_entry: true,
        entry_fee_pct: Decimal::ZERO,
        entry_role: crate::model::OrderRole::Maker,
        exit_role: crate::model::OrderRole::Pending,
        target_exit_price: None,
        entered_at_ms: 1,
        expires_at_ms: 91_000,
        account_id: crate::model::default_account_id(),
        venue: "polymarket".into(),
        partner_token_id: String::new(),
        last_book_ts: 0,
        round_duration_sec: 0,
        round_slot: 0,
        state: crate::exit_policy::ExitState::new(dec!(0.4), 1),
        flows: crate::position::CashFlows {
            entry_cost_usd: dec!(4),
            opened_shares: dec!(10),
            ..Default::default()
        },
    }
}

/// Acceptance A: a live book that holds `hft-31` (open log) plus a trade
/// history riding to `hft-40` is INVISIBLE to the dry boot on the same
/// directory. The dry snapshot file is its own family; the dry book
/// starts with zero positions and a fresh id sequence (`hft-1`), never
/// `hft-41`/`hft-32`.
#[test]
fn a_live_to_dry_does_not_inherit_live_positions_or_ids() {
    let dir = snap_dir("a-l2d");
    let (live_book, live_trades) = family(&dir, true);

    // Live's era: one held position (hft-31) and a history reaching hft-40.
    crate::position_db::PositionDb::new(&live_book).save(&[held("hft-31")]);
    crate::trade_db::TradeDb::new(&live_trades).record(&closed_trade("hft-40"), 1);

    // The switch (process 1 stopped): the SAME directory, dry mode, no
    // explicit path flags — `main` hands the core exactly the paths
    // `default_position_log_for(Dry)` / `default_trade_log_path(Dry)`
    // derive. First dry boot: an EMPTY dry book.
    let mut dry = book_mode_core(&dir, false, Decimal::from(10_000));

    // Acceptance A, part 1: dry restores ZERO positions — live's hft-31 was
    // never in the dry snapshot file, so the same-directory dry boot starts
    // with its own (empty) book.
    assert_eq!(
        dry.restore_positions(),
        0,
        "the dry boot must not restore live's held positions"
    );
    // Acceptance A, part 2: dry's id seed reads the DRY family's max only
    // (no dry rows yet → both #942 bounds are 0), so the counter's
    // sequence is DRY'S OWN (hft-1) — live's hft-31/hft-40 never touch it.
    assert_eq!(dry.positions.next_id_for_test(), 1);

    // Acceptance E, ownership half: the LIVE snapshot on disk is
    // byte-untouched by the dry boot — no rename, no rewrite, no delete.
    let live_text = std::fs::read_to_string(&live_book).unwrap();
    assert!(live_text.contains("\"hft-31\""), "{live_text}");
    assert_eq!(
        live_text.lines().count(),
        1,
        "the dry boot must not touch the live file"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Acceptance B: dry→live. The dry era's snapshot (hft-7 under
/// `positions.dry.jsonl`) stays behind its `.dry.` name; a live boot on
/// the same directory restores only live's own book (still empty on the
/// first live boot) and mints from live's own sequence.
#[test]
fn b_dry_to_live_does_not_inherit_dry_positions_or_ids() {
    let dir = snap_dir("b-d2l");
    let (dry_book, _) = family(&dir, false);

    // The dry era: one held dry position, restorable by the dry book.
    crate::position_db::PositionDb::new(&dry_book).save(&[held("hft-7")]);
    let mut dry = book_mode_core(&dir, false, Decimal::from(10_000));
    assert_eq!(dry.restore_positions(), 1, "the dry era's own book");

    // The switch back: live boots. The dry snapshot is never read.
    let mut live = book_mode_core(&dir, true, Decimal::from(1_000));
    assert_eq!(
        live.restore_positions(),
        0,
        "dry's hft-7 is not live's book"
    );
    // Live's first mint starts live's OWN sequence (hft-1), not dry's hft-8.
    assert_eq!(live.positions.next_id_for_test(), 1);
    // One live cycle books its history into the LIVE ledger only.
    live.trade_db
        .as_mut()
        .expect("live ledger")
        .record(&closed_trade("hft-40"), 1);

    // The next live boot picks up live's OWN id baseline only.
    let mut live2 = book_mode_core(&dir, true, Decimal::from(1_000));
    assert_eq!(live2.trade_db.as_ref().map(|t| t.max_hft_tail()), Some(40));
    assert_eq!(live2.restore_positions(), 0, "still nothing held live-side");

    // The dry file survives the whole live round (acceptance E).
    let dry_text = std::fs::read_to_string(&dry_book).unwrap();
    assert!(dry_text.contains("\"hft-7\""), "{dry_text}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The A/B id half at the Core level, dry→live: live's boot scan
/// (`max_hft_tail`) reads THE LIVE LEDGER ONLY. The dry era's closed-trade
/// row sits in `trades.jsonl` and never seeds the live counter, so the
/// first live position after the switch continues LIVE's sequence, not
/// dry's (`hft-31` would have meant live minting ONTO the dry ids).
#[test]
fn live_id_counter_seeds_from_the_live_history_only() {
    let dir = snap_dir("b-ids");

    // The dry era: a closed trade with id hft-30 in the DRY ledger.
    let (_, dry_trades) = family(&dir, false);
    crate::trade_db::TradeDb::new(&dry_trades).record(&closed_trade("hft-30"), 1);

    // The switch: live boots. Its ledger path is the LIVE spelling, so the
    // boot scan finds NO hft ids in live's own history.
    let live = book_mode_core(&dir, true, Decimal::from(1_000));
    assert_eq!(
        live.trade_db.as_ref().expect("live ledger").max_hft_tail(),
        0,
        "live's boot scan reads live's ledger, not the dry one"
    );
    // And the dry era's own book still restores under the dry spellings
    // (the row sits behind the dry ledger's name, waiting for the dry
    // book's return).
    let mut dry = book_mode_core(&dir, false, Decimal::from(10_000));
    dry.set_balance(Decimal::from(10_000));
    assert_eq!(dry.trade_db.as_ref().map(|t| t.max_hft_tail()), Some(30));
    let _ = std::fs::remove_dir_all(&dir);
}

/// The default derivations the binary applies (#953) must already agree
/// with the trade-log precedent's composition rules: live's spelling is
/// byte-identical with the pre-#953 default (acceptance C), the dry file
/// lands beside it with the `.dry.` tag inside the same directory, and
/// the two books' files never share a name.
#[test]
fn the_mode_split_matches_the_trade_log_convention() {
    // C: live path byte-identical with the pre-#953 default.
    assert_eq!(
        crate::position_db::default_position_log_for(Mode::Live),
        "data/positions/positions.jsonl"
    );
    assert_eq!(
        crate::strategy_state::default_strategy_state_for(Mode::Live),
        "data/strategy-state.json"
    );
    // The dry family: the `.dry.` tag inside the same directory, the same
    // song the journals (#941) and the risk-side files sing.
    assert_eq!(
        crate::position_db::default_position_log_for(Mode::Dry),
        "data/positions/positions.dry.jsonl"
    );
    assert_eq!(
        crate::strategy_state::default_strategy_state_for(Mode::Dry),
        "data/strategy-state.dry.json"
    );
    assert_ne!(
        crate::position_db::default_position_log_for(Mode::Live),
        crate::position_db::default_position_log_for(Mode::Dry),
        "the two books never share a file"
    );
    // Same-directory sibling: only the `.dry.` tag differs.
    assert_eq!(
        crate::position_db::default_position_log_for(Mode::Dry).replace(".dry", ""),
        crate::position_db::default_position_log_for(Mode::Live),
    );
    // The trade-log precedent the family's composition agrees with — the
    // local splitting minute: live derives a new name, dry keeps the
    // historical one.
    assert_eq!(
        crate::trade_db::default_trade_log_path(Mode::Dry),
        "data/trades/trades.jsonl"
    );
    assert_eq!(
        crate::trade_db::default_trade_log_path(Mode::Live),
        "data/trades/trades-live.jsonl"
    );
    assert_eq!(
        family(std::path::Path::new(""), false).0.to_string_lossy(),
        crate::position_db::default_position_log_for(Mode::Dry),
        "the fixture table and the derivation helper are one truth"
    );
    assert_eq!(
        family(std::path::Path::new(""), true).0.to_string_lossy(),
        crate::position_db::default_position_log_for(Mode::Live),
        "the fixture table and the derivation helper are one truth"
    );
}

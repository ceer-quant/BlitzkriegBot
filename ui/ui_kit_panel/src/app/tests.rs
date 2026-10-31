#![cfg(test)]

use super::*;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn key(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}

/// A settings face with one configured account and one rule, as the
/// kernel's `list`/`get`/`history` answer them (raw wire shapes).
fn app_with_policy() -> App {
    let mut app = App::new("test.sock".into(), false);
    app.on_key(key('8'));
    // Enter the policy sub-mode the way the operator does: `p`.
    app.on_key(key('p'));
    app.policy_accounts = vec![serde_json::json!({
        "accountId": "acct-a",
        "section": {}
    })];
    // The seeded section/history below belong to acct-a: move the chip
    // cursor onto it (chip 1 = the list's first entry).
    app.on_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    app.policy_section = Some(serde_json::json!({
        "budget_ratio": "0.10",
        "min_budget_usd": "1.00",
        "max_budget_usd": "50.00",
        "min_equity_usd": "1.00",
        "max_positions_per_asset": 1,
        "rules": [{
            "name": "no_btc",
            "priority": 10,
            "enabled": true,
            "when": { "field": "symbol", "op": "eq", "value": "BTC" },
            "then": { "action": "skip" },
            "reason": "no bitcoin"
        }]
    }));
    app.policy_history = vec![serde_json::json!({
        "tsMs": 1,
        "actor": "ipc",
        "action": "set",
        "accountId": "acct-a",
        "before": { "budget_ratio": "0.20", "max_positions_per_asset": 1 },
        "after": { "budget_ratio": "0.10", "max_positions_per_asset": 1 }
    })];
    app
}

/// #364: `8` enters Settings and demands a policy refresh; ←/→ walk the
/// account chips (defaults first, then the kernel's list) and re-read.
#[test]
fn settings_tab_policy_refresh_and_account_switch() {
    let mut app = App::new("test.sock".into(), false);
    // Plain `8` does NOT open the editor (the tab keeps its own keys).
    assert!(matches!(app.on_key(key('8')), Action::None));
    assert!(!app.policy_mode);
    // `p` enters the policy sub-mode and demands a refresh.
    assert!(matches!(app.on_key(key('p')), Action::PolicyRefresh));
    assert!(app.policy_mode);
    assert_eq!(app.policy_account_id().as_deref(), Some("defaults"));
    // The kernel's list arrives: one configured account.
    app.policy_accounts = vec![serde_json::json!({ "accountId": "acct-a" })];
    // → lands on acct-a; the id rides the list.
    app.on_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    assert_eq!(app.policy_account_id().as_deref(), Some("acct-a"));
    // ← back to defaults; ← again saturates (no wraparound into nothing).
    app.on_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
    app.on_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
    assert_eq!(app.policy_account_id().as_deref(), Some("defaults"));
    // [q] returns out of the sub-mode (the spec's `[q] return`).
    app.on_key(key('q'));
    assert!(!app.policy_mode);
    assert_eq!(app.tab, Tab::Settings);
    // …and the tab's own [a] (auto-update) is reachable again: it asks.
    app.on_key(key('a'));
    assert_eq!(
        app.pending_confirmation.as_deref(),
        Some("update.auto on"),
        "outside the mode, [a] is the auto-update switch again"
    );
}

/// [e] on a rule row opens the builder seeded from that rule; [a] starts
/// a neutral draft; the draft commits to a valid `rules[]` element the
/// kernel's own parser will accept (table-form when, one-action then).
#[test]
fn rule_edit_and_draft_commit() {
    let mut app = app_with_policy();
    // Focus onto the rule row (rules start at focus 5).
    app.policy_focus = 5;
    assert!(matches!(app.on_key(key('e')), Action::None));
    let draft = app.policy_draft.clone().expect("draft opened");
    assert_eq!(draft.name, "no_btc");
    assert_eq!(draft.field(), "symbol");
    assert_eq!(draft.value, "BTC");
    assert_eq!(draft.edit_idx, Some(0));
    // A fresh draft commits to a shape the kernel validates.
    app.policy_draft = Some(PolicyDraft::neutral());
    let mut d = app.policy_draft.clone().unwrap();
    d.name = "cap_ratio".into();
    d.field_idx = PolicyDraft::FIELDS
        .iter()
        .position(|f| *f == "available_balance")
        .unwrap();
    d.op_idx = PolicyDraft::OPS.iter().position(|o| *o == "<").unwrap();
    d.value = "10".into();
    d.then_idx = PolicyDraft::THENS
        .iter()
        .position(|t| *t == "budget_ratio")
        .unwrap();
    d.then_arg = "0.05".into();
    d.edit_idx = None;
    let rule = d.to_rule_json().expect("draft renders");
    assert_eq!(rule["when"]["op"], "<");
    assert_eq!(rule["then"]["budget_ratio"], "0.05");
    assert_eq!(rule["priority"], 100);
    // A nameless draft refuses to render — the kernel would refuse it too.
    let mut bad = d.clone();
    bad.name = String::new();
    assert!(bad.to_rule_json().is_err());
}

/// [d] and [空格] must ASK first: the confirm bar names the change, `y`
/// routes it to `PolicyEdit` with the params built at ask time.
#[test]
fn delete_and_toggle_ask_before_writing() {
    let mut app = app_with_policy();
    app.policy_focus = 5;
    assert!(matches!(app.on_key(key('d')), Action::None));
    assert!(app.pending_confirmation.is_some(), "delete asks y/n");
    let (action, params, _) = app.policy_pending.clone().expect("pending armed");
    assert_eq!(action, PolicyAction::Set);
    assert_eq!(params["rules"].as_array().map(|a| a.len()), Some(0));
    // Any non-`y` cancels and clears the armed write.
    app.on_key(key('n'));
    assert!(app.pending_confirmation.is_none());
    assert!(app.policy_pending.is_none());

    // [空格] flips `enabled` in the staged params (not yet on disk).
    assert!(matches!(app.on_key(key(' ')), Action::None));
    let (_, params, _) = app.policy_pending.clone().expect("toggle armed");
    assert_eq!(params["rules"][0]["enabled"], serde_json::json!(false));
    app.on_key(key('n'));
    assert!(app.policy_pending.is_none());
}

/// [r] rollback writes the most recent `set`'s BEFORE-state back through
/// `set` — no new backend verb, the journal IS the version store.
#[test]
fn rollback_takes_the_last_set_before_state() {
    let mut app = app_with_policy();
    assert!(matches!(app.on_key(key('r')), Action::None));
    let (_, params, label) = app.policy_pending.clone().expect("rollback armed");
    assert_eq!(params["accountId"], serde_json::json!("acct-a"));
    assert_eq!(
        params["budgetRatio"],
        serde_json::json!("0.20"),
        "the before-state"
    );
    assert!(label.contains("roll back"));
    // No `set` in the journal → a refusal with a reason, never a write.
    let mut bare = app_with_policy();
    bare.policy_history.clear();
    assert!(matches!(bare.on_key(key('r')), Action::None));
    assert!(bare.pending_confirmation.is_none());
    assert!(bare
        .logs
        .last()
        .is_some_and(|l| l.contains("no prior version")));
}

/// [s] save stages the CURRENT effective view as the write (visible round
/// trip: the face re-reads after the write lands).
#[test]
fn save_stages_the_effective_view() {
    let mut app = app_with_policy();
    assert!(matches!(app.on_key(key('s')), Action::None));
    let (action, params, _) = app.policy_pending.clone().expect("save armed");
    assert_eq!(action, PolicyAction::Set);
    assert_eq!(params["accountId"], serde_json::json!("acct-a"));
    assert_eq!(params["budgetRatio"], serde_json::json!("0.10"));
    assert_eq!(params["rules"][0]["name"], serde_json::json!("no_btc"));
}

/// With no rule row under the cursor, the row keys say so instead of
/// guessing (the cursor is the only index the face trusts).
#[test]
fn row_keys_need_a_rule_row() {
    let mut app = app_with_policy();
    app.policy_focus = 0; // base-param rows, not a rule
    for c in ['e', 'd', ' '] {
        assert!(matches!(app.on_key(key(c)), Action::None));
        assert!(app.pending_confirmation.is_none());
        assert!(app.policy_draft.is_none());
    }
}

/// issue 456: the Orders tab's [F] routes through the y/n bar — a ghost
/// force-close never fires on one keypress — and the confirmed line is
/// `force-order <id>` for the row under the cursor. With nothing in the
/// book, [F] says so instead of guessing.
#[test]
fn orders_tab_force_close_asks_and_names_the_row() {
    let mut app = App::new("test.sock".into(), false);
    app.on_key(key('3')); // Orders tab
    assert_eq!(app.tab, Tab::Orders);
    // Empty book: [F] refuses, nothing is armed.
    assert!(matches!(app.on_key(key('f')), Action::None));
    assert!(app.pending_confirmation.is_none());
    assert!(app.logs.last().is_some_and(|l| l.contains("no order row")));

    // One ghost order under the cursor: [F] arms the y/n bar.
    app.snap.orders = vec![blitzkrieg_ui_kit::core::types::OrderView {
        order_id: "dry_7".into(),
        strategy: "spread_arb".into(),
        asset: "BTC_UP".into(),
        direction: "up".into(),
        token_id: "tok-1".into(),
        side: "buy".into(),
        mode: "maker".into(),
        price: 0.39,
        size: 5.0,
        filled_size: 0.0,
        status: "LIVE".into(),
        round_slot: 123,
        account_id: String::new(),
        venue: String::new(),
    }];
    assert!(matches!(app.on_key(key('f')), Action::ConfirmToggle(_)));
    // The main loop sets the bar from a ConfirmToggle when the predicate
    // demands it — simulate that handoff here.
    assert!(app.toggle_needs_confirmation("force-order dry_7"));
    app.pending_confirmation = Some("force-order dry_7".into());
    // `y` resolves the bar into the dispatch line (what the main loop runs).
    let action = app.on_key(key('y'));
    assert!(matches!(action, Action::RunCommand(c) if c == "force-order dry_7"));
}

/// 账户↔venue 解耦（本轮）：the `A` overlay — open, navigate, confirm
/// through the y/n bar, close. Switching the session default is a write
/// that always asks (`account <id>` starts with `account `).
#[test]
fn accounts_overlay_opens_selects_and_switches() {
    let mut app = App::new("/tmp/x.sock".into(), false);

    // First `A` opens the overlay and asks the main loop for the book.
    assert!(matches!(app.on_key(key('A')), Action::AccountsRefresh));
    assert!(app.accounts_visible);
    // A held list renders at once (no second fetch on reopen).
    assert!(matches!(app.on_key(key('A')), Action::None));

    app.accounts_doc = Some(serde_json::json!({
        "version": "1.1", "active": "default",
        "accounts": [
            { "id": "default", "name": "Default", "status": "active", "venue": "" },
            { "id": "kalshi_main", "name": "Kalshi", "status": "active",
              "venue": "kalshi", "strategies": ["spread_arb"] },
        ],
    }));
    // Esc closes; reopen keeps the held book.
    assert!(matches!(
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        Action::None
    ));
    assert!(!app.accounts_visible);
    app.accounts_visible = true;

    // Cursor starts at row 0 (the session default), moves down one row,
    // and stops at the last row (no wrap).
    assert_eq!(app.selected_account_id().as_deref(), Some("default"));
    app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(app.selected_account_id().as_deref(), Some("kalshi_main"));
    app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(app.selected_account_id().as_deref(), Some("kalshi_main"));
    app.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(app.selected_account_id().as_deref(), Some("default"));

    // Enter routes through the confirm bar: the main loop asks the
    // predicate, the predicate arms, `y` dispatches `account <id>`.
    assert!(
        matches!(app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), Action::ConfirmToggle(c) if c == "account default")
    );
    assert!(app.toggle_needs_confirmation("account kalshi_main"));
    app.pending_confirmation = Some("account kalshi_main".into());
    app.accounts_visible = false;
    assert!(matches!(
        app.on_key(key('y')),
        Action::RunCommand(c) if c == "account kalshi_main"
    ));

    // `q` inside the overlay keeps its global quit meaning.
    app.accounts_visible = true;
    assert!(matches!(app.on_key(key('q')), Action::Quit));
}

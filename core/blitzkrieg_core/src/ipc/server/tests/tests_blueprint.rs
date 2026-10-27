#![cfg(test)]

use super::*;

use crate::account::{AccountId, AccountStatus};
use crate::ipc::schema::*;
use crate::model::{CoreError, CoreErrorCode, Mode};
use crate::service::{Core, CoreConfig};
use blitzkrieg_market_api::net::now_ms;
use rust_decimal::Decimal;
use serde_json::Value;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex as AsyncMutex, broadcast, mpsc, oneshot};

// ── #362: the blueprint editor surface, at the wire ──────────────────────
//
// The compile arm is pure (stateless, no Core lock), the save arm is the
// one place this surface touches the filesystem, so every test here pins
// the write shape from a temp ROOT via the same entry the sessions use.

/// The spec's four-node example — the same document the blueprint module's
/// own DOG_BLUEPRINT test pins, so the wire arm and the compiler agree on
/// the happy path.
const WIRE_BLUEPRINT: &str = r#"{"version":1,"name":"wire_dog","nodes":[
    {"id":"n1","type":"data_source","params":{"field":"tick.price"}},
    {"id":"n2","type":"condition","params":{"op":"<=","value":0.25}},
    {"id":"n3","type":"condition","params":{"field":"tick.trend_confirmed","op":"==","value":true}},
    {"id":"n4","type":"action_buy","params":{"price":0.25,"budget_ratio":0.10}}],
    "edges":[{"from":"n1","to":"n2","when":true},{"from":"n2","to":"n3","when":true},
    {"from":"n3","to":"n4","when":true}]}"#;

#[tokio::test]
async fn blueprint_compile_answers_the_generated_lua() {
    let (core, registry, peer) = hot_reload_fixture().await;
    let params = serde_json::json!({ "json": WIRE_BLUEPRINT }).to_string();
    let line =
        format!(r#"{{"jsonrpc":"2.0","id":1,"method":"blueprint.compile","params":{params}}}"#);
    let reply = rpc(&core, &registry, &peer, line).await;
    assert!(reply.get("error").is_none(), "compile must answer: {reply}");
    let lua = reply["result"]["lua"].as_str().expect("lua is a string");
    assert!(lua.contains("function on_tick(tick)"), "{lua}");
    assert!(
        lua.contains("tick.price <= __param(\"n2_value\", 0.25)"),
        "{lua}"
    );
    assert!(lua.contains("side = \"buy\""), "{lua}");
}

/// Reverse acceptance: every refusal class the editor must render INLINE
/// reaches the wire with the node id (or the parse position) and nothing
/// else changed — no partial result, no silent success.
#[tokio::test]
async fn blueprint_compile_refusals_carry_the_editor_context() {
    let (core, registry, peer) = hot_reload_fixture().await;

    // Cycle: names the node on it.
    let cyclic = json_line(
        1,
        "blueprint.compile",
        &serde_json::json!({
            "json": r#"{"version":1,"name":"cyc","nodes":[
            {"id":"a1","type":"data_source","params":{"field":"tick.price"}},
            {"id":"a2","type":"condition","params":{"op":"<","value":1}},
            {"id":"a3","type":"condition","params":{"op":">","value":0}},
            {"id":"a4","type":"action_buy","params":{}}],
            "edges":[{"from":"a1","to":"a2"},{"from":"a2","to":"a3"},
            {"from":"a3","to":"a2"},{"from":"a3","to":"a4"}]}"#,
        }),
    );
    let reply = rpc(&core, &registry, &peer, cyclic).await;
    let msg = reply["error"]["message"].as_str().unwrap_or_default();
    assert!(msg.contains("cycle") && msg.contains("a2"), "{reply}");

    // No reachable action: the whole-graph failure with no node id.
    let actionless = json_line(
        2,
        "blueprint.compile",
        &serde_json::json!({
            "json": r#"{"version":1,"name":"noact","nodes":[
            {"id":"a1","type":"data_source","params":{"field":"tick.price"}},
            {"id":"a2","type":"condition","params":{"op":"<","value":1}}],
            "edges":[{"from":"a1","to":"a2"}]}"#,
        }),
    );
    let reply = rpc(&core, &registry, &peer, actionless).await;
    let msg = reply["error"]["message"].as_str().unwrap_or_default();
    assert!(msg.contains("no reachable action output"), "{reply}");

    // Non-whitelisted field: names the node AND the field.
    let bad_field = json_line(
        3,
        "blueprint.compile",
        &serde_json::json!({
            "json": r#"{"version":1,"name":"badf","nodes":[
            {"id":"a1","type":"data_source","params":{"field":"tick.close"}},
            {"id":"a2","type":"condition","params":{"op":"<","value":1}},
            {"id":"a3","type":"action_buy","params":{}}],
            "edges":[{"from":"a1","to":"a2"},{"from":"a2","to":"a3"}]}"#,
        }),
    );
    let reply = rpc(&core, &registry, &peer, bad_field).await;
    let msg = reply["error"]["message"].as_str().unwrap_or_default();
    assert!(msg.contains("a1") && msg.contains("tick.close"), "{reply}");

    // Malformed JSON: the parse error travels verbatim.
    let not_json = json_line(
        4,
        "blueprint.compile",
        &serde_json::json!({ "json": "{not json" }),
    );
    let reply = rpc(&core, &registry, &peer, not_json).await;
    let msg = reply["error"]["message"].as_str().unwrap_or_default();
    assert!(msg.contains("blueprint parse failed"), "{reply}");
}

/// Build one JSON-RPC line from a params object (the save tests' helper).
fn json_line(id: u32, method: &str, params: &serde_json::Value) -> String {
    format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{params}}}"#)
}

/// The save tests' core: the standard fixture with its `lua_strategy_dir`
/// pointed at a private temp root, so a save lands where the assertions
/// look and never in the checkout. The save arm reads the root from this
/// config snapshot — the same road `serve` takes, no process-wide knob.
async fn save_fixture(
    root: &std::path::Path,
) -> (
    Arc<AsyncMutex<Core>>,
    crate::market::registry::MarketPluginRegistry,
    PeerAuth,
) {
    use rust_decimal_macros::dec;
    let cfg = CoreConfig {
        risk: crate::risk::RiskConfig {
            max_order_notional: dec!(3),
            ..Default::default()
        },
        dry_seed_balance: dec!(100),
        lua_strategy_dir: Some(root.display().to_string()),
        trade_log_path: None,
        order_log_path: None,
        position_log_path: None,
        ..Default::default()
    };
    let mut c = Core::new(cfg.clone());
    c.set_balance(cfg.dry_seed_balance);
    cfg.install_engine(&mut c).expect("engine install");
    (
        Arc::new(AsyncMutex::new(c)),
        crate::market::registry::MarketPluginRegistry::new(),
        PeerAuth::SameUid { uid: own_uid() },
    )
}

#[tokio::test]
async fn execution_policy_surface_list_get_set_reset_history() {
    let (core, registry, peer, path) = policy_fixture().await;

    // The journal is this fixture's PRIVATE file (#386) inside a scratch
    // dir, so parallel test cores cannot interleave appends into it. Its
    // stamps are wall-clock, though — a backward step between two now_ms()
    // calls would make "records stamped after the test started" miss this
    // run's own set. So freshness is proven by COUNT, not by clock: read
    // the account's journal length first, then assert it grew by exactly
    // one — the set that answered, no audit for the refused one.
    let before_set = {
        let c = core.lock().await;
        c.execution_policy_audit()
            .iter()
            .filter(|r| r.account_id == "acct-a")
            .count()
    };

    // list: the shipped global section only, no accounts.
    let v = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"execution_policy.list","params":{}}"#.into(),
    )
    .await;
    assert!(v.get("error").is_none(), "list answers: {v}");
    assert_eq!(v["result"]["loaded"], serde_json::json!(true));
    assert_eq!(
        v["result"]["defaults"]["budgetRatio"],
        serde_json::json!("0.10")
    );
    assert!(
        v["result"]["accounts"]
            .as_array()
            .is_some_and(|a| a.is_empty())
    );

    // get: an account with no section folds over the globals (effective view).
    let v = rpc(&core, &registry, &peer,
        r#"{"jsonrpc":"2.0","id":2,"method":"execution_policy.get","params":{"accountId":"acct-a"}}"#.into())
        .await;
    assert!(v.get("error").is_none(), "get falls back to globals: {v}");
    assert_eq!(v["result"]["budgetRatio"], serde_json::json!("0.10"));

    // set: write ONE account's section; decimals arrive as strings.
    let v = rpc(&core, &registry, &peer,
        r#"{"jsonrpc":"2.0","id":3,"method":"execution_policy.set","params":{"accountId":"acct-a","budgetRatio":"0.05","maxBudgetUsd":"20"}}"#.into())
        .await;
    assert!(v.get("error").is_none(), "set answers: {v}");

    // The file now carries the section, rendered as valid TOML…
    let text = std::fs::read_to_string(&path).expect("policy file");
    assert!(text.contains("[accounts.acct-a]"), "section landed: {text}");
    // …and the reload sees it (the effective view is the account's own).
    let v = rpc(&core, &registry, &peer,
        r#"{"jsonrpc":"2.0","id":4,"method":"execution_policy.get","params":{"accountId":"acct-a"}}"#.into())
        .await;
    assert_eq!(
        v["result"]["budgetRatio"],
        serde_json::json!("0.05"),
        "set then get: {v}"
    );
    assert_eq!(v["result"]["maxBudgetUsd"], serde_json::json!("20"), "{v}");
    // The untouched global section is byte-identical in intent.
    let v = rpc(&core, &registry, &peer,
        r#"{"jsonrpc":"2.0","id":5,"method":"execution_policy.get","params":{"accountId":"someone-else"}}"#.into())
        .await;
    assert_eq!(
        v["result"]["budgetRatio"],
        serde_json::json!("0.10"),
        "cross-account untouched: {v}"
    );

    // set with an out-of-range ratio is refused on the wire (INVALID_PARAMS),
    // and the in-memory policy is untouched.
    let v = rpc(&core, &registry, &peer,
        r#"{"jsonrpc":"2.0","id":6,"method":"execution_policy.set","params":{"accountId":"acct-a","budgetRatio":"1.5"}}"#.into())
        .await;
    assert_eq!(
        v["error"]["code"],
        serde_json::json!(Failure::INVALID_PARAMS),
        "bad ratio refused: {v}"
    );
    let v = rpc(&core, &registry, &peer,
        r#"{"jsonrpc":"2.0","id":7,"method":"execution_policy.get","params":{"accountId":"acct-a"}}"#.into())
        .await;
    assert_eq!(
        v["result"]["budgetRatio"],
        serde_json::json!("0.05"),
        "refusal kept old section: {v}"
    );

    // history: this run's set landed one audit line; the refused set did
    // not. Counted against the pre-set length (clock-free freshness —
    // see the note at the top of this test).
    let this_run = rpc(&core, &registry, &peer,
        r#"{"jsonrpc":"2.0","id":8,"method":"execution_policy.history","params":{"accountId":"acct-a"}}"#.into())
        .await;
    let lines = this_run["result"].as_array().expect("history array");
    assert_eq!(
        lines.len(),
        before_set + 1,
        "one set line this run, refusals do not audit: {this_run}"
    );
    let fresh = lines.last().expect("the one fresh line");
    assert_eq!(fresh["action"], serde_json::json!("set"));
    assert_eq!(fresh["accountId"], serde_json::json!("acct-a"));
    // The `after` summary is the RELOADED state: the account's own 0.05.
    assert_eq!(
        fresh["after"]["budgetRatio"],
        serde_json::json!("0.05"),
        "{this_run}"
    );

    // reset: the section goes away; the account folds back to the globals.
    let v = rpc(&core, &registry, &peer,
        r#"{"jsonrpc":"2.0","id":9,"method":"execution_policy.reset","params":{"accountId":"acct-a"}}"#.into())
        .await;
    assert!(v.get("error").is_none(), "reset answers: {v}");
    let v = rpc(&core, &registry, &peer,
        r#"{"jsonrpc":"2.0","id":10,"method":"execution_policy.get","params":{"accountId":"acct-a"}}"#.into())
        .await;
    assert_eq!(
        v["result"]["budgetRatio"],
        serde_json::json!("0.10"),
        "reset falls back: {v}"
    );
    let text = std::fs::read_to_string(&path).expect("policy file");
    assert!(
        !text.contains("[accounts.acct-a]"),
        "section removed: {text}"
    );

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// The reset of a section that does not exist is refused by name — an
/// operator who fat-fingers an id finds out immediately, not "silently ok".
#[tokio::test]
async fn execution_policy_reset_of_unknown_section_refuses() {
    let (core, registry, peer, path) = policy_fixture().await;
    let v = rpc(&core, &registry, &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"execution_policy.reset","params":{"accountId":"never-existed"}}"#.into())
        .await;
    assert_eq!(
        v["error"]["code"],
        serde_json::json!(Failure::INVALID_PARAMS),
        "{v}"
    );
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("never-existed"),
        "the refusal names the account: {v}"
    );
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// #364 — the FRESHNESS contract at the wire: after `execution_policy.set`
/// answers, a `get` on the SAME connection (any connection — the policy
/// is core state, not session state) must return the NEW view, and
/// `history` must carry one more audit line for the account. This is the
/// testable form of "WebUI saves → TUI reads stale → red": the two faces
/// share the core's state, and this pins that a write is visible to the
/// next reader without any restart or re-login.
#[tokio::test]
async fn execution_policy_set_is_fresh_for_the_next_reader() {
    let (core, registry, peer, path) = policy_fixture().await;

    // The baseline read: the account folds over the globals (0.10).
    let v = rpc(&core, &registry, &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"execution_policy.get","params":{"accountId":"acct-fresh"}}"#.into())
        .await;
    assert_eq!(
        v["result"]["budgetRatio"],
        serde_json::json!("0.10"),
        "pre-set view: {v}"
    );
    let history_before = rpc(&core, &registry, &peer,
        r#"{"jsonrpc":"2.0","id":2,"method":"execution_policy.history","params":{"accountId":"acct-fresh"}}"#.into())
        .await;
    let before_len = history_before["result"]
        .as_array()
        .map(|a| a.len())
        .unwrap_or(0);

    // A write lands through the wire…
    let v = rpc(&core, &registry, &peer,
        r#"{"jsonrpc":"2.0","id":3,"method":"execution_policy.set","params":{"accountId":"acct-fresh","budgetRatio":"0.07","maxBudgetUsd":"25"}}"#.into())
        .await;
    assert!(v.get("error").is_none(), "set answers: {v}");

    // …and the VERY NEXT reader sees the new state, no restart, no
    // re-login, no cache to invalidate.
    let v = rpc(&core, &registry, &peer,
        r#"{"jsonrpc":"2.0","id":4,"method":"execution_policy.get","params":{"accountId":"acct-fresh"}}"#.into())
        .await;
    assert_eq!(
        v["result"]["budgetRatio"],
        serde_json::json!("0.07"),
        "post-set get must be FRESH: {v}"
    );
    assert_eq!(v["result"]["maxBudgetUsd"], serde_json::json!("25"), "{v}");

    // The audit trail grew by exactly one line for this account, and it
    // is the set that just answered.
    let history_after = rpc(&core, &registry, &peer,
        r#"{"jsonrpc":"2.0","id":5,"method":"execution_policy.history","params":{"accountId":"acct-fresh"}}"#.into())
        .await;
    let lines = history_after["result"].as_array().expect("history array");
    assert_eq!(
        lines.len(),
        before_len + 1,
        "history must grow by one per set: {history_after}"
    );
    let last = lines.last().expect("one fresh line");
    assert_eq!(last["action"], serde_json::json!("set"));
    assert_eq!(
        last["after"]["budgetRatio"],
        serde_json::json!("0.07"),
        "the after snapshot is the reloaded state"
    );

    // The preview strip rides the same freshness: the new ratio decides
    // the budget the replay reports (0.07 × the seeded balance).
    let v = rpc(&core, &registry, &peer,
        r#"{"jsonrpc":"2.0","id":6,"method":"execution_policy.preview","params":{"accountId":"acct-fresh"}}"#.into())
        .await;
    assert!(v.get("error").is_none(), "preview answers: {v}");
    assert!(
        v["result"]["considered"].is_u64(),
        "preview carries a considered count: {v}"
    );

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// #364 — the preview arm: a skip rule removes exactly the rows it names,
/// and the budget the replay reports for the survivors is the balance ×
/// ratio clamp the live order path would have handed out.
#[tokio::test]
async fn execution_policy_preview_replays_history_through_evaluate() {
    use rust_decimal_macros::dec;
    let (core, registry, peer, path) = policy_fixture_with_accounts(&["acct-prev"]).await;

    // An account section with a rule that skips BTC outright: the seed
    // trades below hit it by name. The condition rides the table form
    // ({field, op, value}) — the same spelling the file carries.
    let v = rpc(&core, &registry, &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"execution_policy.set","params":{"accountId":"acct-prev","budgetRatio":"0.10","minBudgetUsd":"1","maxBudgetUsd":"50","rules":[{"name":"no_btc","priority":10,"enabled":true,"when":{"field":"symbol","op":"==","value":"BTC"},"then":{"action":"skip"},"reason":"no bitcoin"}]}}"#.into())
        .await;
    assert!(v.get("error").is_none(), "set with rules: {v}");

    // Seed two closed trades for the account: one BTC (skipped), one ETH
    // (placed). Rows land in the position manager's own closed book —
    // the same book `trades.history` reports from — so the preview
    // replays exactly what the panel shows.
    let balance;
    {
        use crate::model::{ExitReason, OrderRole, SignalDirection};
        use crate::position::ClosedPosition;
        let mut c = core.lock().await;
        balance = dec!(100);
        c.accounts_mut()
            .get_mut(&AccountId::from("acct-prev"))
            .expect("configured ledger")
            .set_balance(balance);
        let mk = |asset: &str, price: Decimal| ClosedPosition {
            id: format!("hft-prev-{asset}"),
            strategy: "spread_arb".into(),
            asset: asset.into(),
            direction: SignalDirection::Up,
            token_id: format!("tok-{asset}"),
            condition_id: "cond".into(),
            account_id: AccountId::from("acct-prev"),
            entry_price: price,
            exit_price: price,
            shares: dec!(10),
            cost_usd: price * dec!(10),
            was_maker_entry: true,
            was_maker_exit: false,
            entry_fee_pct: Decimal::ZERO,
            exit_fee_pct: Decimal::ZERO,
            pnl_usd: Decimal::ZERO,
            pnl_pct: Decimal::ZERO,
            net_pnl_usd: Decimal::ZERO,
            net_pnl_pct: Decimal::ZERO,
            high_pnl_pct: Decimal::ZERO,
            low_pnl_pct: Decimal::ZERO,
            hold_time_sec: 60,
            exit_reason: ExitReason::TakeProfit,
            entered_at_ms: now_ms() - 60_000,
            exited_at_ms: now_ms(),
            entry_role: OrderRole::Maker,
            exit_role: OrderRole::Taker,
            dust_shares: Decimal::ZERO,
            venue: String::new(),
        };
        c.positions_mut()
            .push_closed_for_test(mk("BTC", dec!(0.40)));
        c.positions_mut()
            .push_closed_for_test(mk("ETH", dec!(0.60)));
    }
    let v = rpc(&core, &registry, &peer,
        r#"{"jsonrpc":"2.0","id":2,"method":"execution_policy.preview","params":{"accountId":"acct-prev"}}"#.into())
        .await;
    let r = &v["result"];
    assert_eq!(r["considered"], serde_json::json!(2), "{v}");
    assert_eq!(r["skipped"], serde_json::json!(1), "the BTC row skips: {v}");
    // One placed row at 10% of the seeded balance — the budget the rules
    // would have handed that entry.
    let expected = balance * dec!(0.10);
    assert_eq!(
        r["avgBudgetUsd"],
        serde_json::json!(expected.to_string()),
        "average budget = balance × ratio over the placed rows: {v}"
    );
    let rows = r["rows"].as_array().expect("rows");
    assert_eq!(rows.len(), 2, "one row per replayed entry");
    assert!(
        rows.iter()
            .any(|row| row["symbol"] == "BTC" && row["verdict"] == "skip"),
        "the BTC row is a skip: {v}"
    );
    assert!(
        rows.iter()
            .any(|row| row["symbol"] == "ETH" && row["verdict"] == "place"),
        "the ETH row places: {v}"
    );

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn blueprint_save_writes_the_package_files_and_receipt() {
    // A private strategy root through the core's OWN config — no process
    // env knob, so the two save tests cannot race each other for a shared
    // cell and nothing ever writes into the real checkout.
    let root = std::env::temp_dir().join(format!(
        "bk-ipc-bpsave-ok-{}-{}",
        std::process::id(),
        now_ms()
    ));
    let (core, registry, peer) = save_fixture(&root).await;
    let json = serde_json::json!({ "json": WIRE_BLUEPRINT, "name": "wire_dog" }).to_string();
    let line = json_line(
        1,
        "blueprint.save",
        &serde_json::from_str::<Value>(&json).expect("obj"),
    );
    let reply = rpc(&core, &registry, &peer, line).await;
    assert!(reply.get("error").is_none(), "save must land: {reply}");

    let r = &reply["result"];
    assert_eq!(r["name"], serde_json::json!("wire_dog"));
    let pkg = root.join("wire_dog");
    // The receipt's paths are the files on disk, and all three exist.
    assert_eq!(
        r["packageDir"],
        serde_json::json!(pkg.display().to_string())
    );
    let lua = std::fs::read_to_string(pkg.join("strategy.lua")).expect("strategy.lua written");
    assert!(lua.contains("function on_tick(tick)"), "{lua}");
    let bp = std::fs::read_to_string(pkg.join("blueprint.json")).expect("blueprint.json written");
    assert_eq!(bp, WIRE_BLUEPRINT, "the blueprint bytes land verbatim");
    let manifest: Value = serde_json::from_str(
        &std::fs::read_to_string(pkg.join("manifest.json")).expect("manifest written"),
    )
    .expect("manifest is JSON");
    assert_eq!(manifest["name"], "wire_dog");
    assert_eq!(manifest["entry"], "strategy.lua");
    assert_eq!(manifest["api"], "1.0");
    // The manifest's digest matches the receipt AND the file — the loader
    // verifies exactly this, so a drifted copy would refuse the package.
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(lua.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    assert_eq!(r["luaSha256"], serde_json::json!(digest));
    assert_eq!(manifest["sha256"], serde_json::json!(digest));
    // The receipt counts the bytes of all three files.
    assert_eq!(
        r["bytes"],
        serde_json::json!([
            WIRE_BLUEPRINT.len(),
            lua.len(),
            std::fs::read_to_string(pkg.join("manifest.json"))
                .unwrap()
                .len()
        ])
    );
    // The receipt names the evolvable knobs — the same set the manifest's
    // tunables object carries (numeric anchors of this graph: the
    // condition threshold plus the buy's price and budget_ratio).
    assert_eq!(
        r["tunables"],
        serde_json::json!(["n2_value", "n4_price", "n4_budget_ratio"]),
        "receipt tunables: {r}"
    );
    let manifest_tunables = manifest["tunables"].as_object().expect("tunables exported");
    assert_eq!(
        manifest_tunables.len(),
        r["tunables"].as_array().unwrap().len()
    );
    assert!(lua.contains("__param(\"n2_value\", 0.25)"), "{lua}");

    // A second save of the SAME name refuses: the package is now on the
    // loader's scan path and a silent overwrite could swap a strategy an
    // operator believes they know.
    let line2 = json_line(
        2,
        "blueprint.save",
        &serde_json::from_str::<Value>(&json).expect("obj"),
    );
    let reply = rpc(&core, &registry, &peer, line2).await;
    let msg = reply["error"]["message"].as_str().unwrap_or_default();
    assert!(msg.contains("already exists"), "{reply}");

    // With the explicit flag the SAME call lands: overwrite is possible,
    // never silent.
    let line3 = json_line(
        3,
        "blueprint.save",
        &serde_json::json!({
            "name": "wire_dog", "json": WIRE_BLUEPRINT, "overwrite": true,
        }),
    );
    let reply = rpc(&core, &registry, &peer, line3).await;
    assert!(
        reply.get("error").is_none(),
        "an explicit overwrite must land: {reply}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn blueprint_load_returns_the_saved_document() {
    // #393: the read-only mirror of save — a saved package's blueprint
    // document comes back verbatim; a hand-written (save-less) package and
    // a path-flavoured name are refused with the honest reasons.
    let root =
        std::env::temp_dir().join(format!("bk-ipc-bpload-{}-{}", std::process::id(), now_ms()));
    let (core, registry, peer) = save_fixture(&root).await;

    // Save one package first — load answers from what save wrote.
    let save_line = json_line(
        1,
        "blueprint.save",
        &serde_json::json!({ "name": "wire_dog", "json": WIRE_BLUEPRINT }),
    );
    let reply = rpc(&core, &registry, &peer, save_line).await;
    assert!(reply.get("error").is_none(), "save must land: {reply}");

    // The load returns the SAME bytes the save wrote.
    let load_line = json_line(
        2,
        "blueprint.load",
        &serde_json::json!({ "name": "wire_dog" }),
    );
    let reply = rpc(&core, &registry, &peer, load_line).await;
    assert!(reply.get("error").is_none(), "load must answer: {reply}");
    let r = &reply["result"];
    assert_eq!(r["name"], serde_json::json!("wire_dog"));
    assert_eq!(r["json"], serde_json::json!(WIRE_BLUEPRINT));
    assert_eq!(
        r["blueprintPath"],
        serde_json::json!(
            root.join("wire_dog")
                .join("blueprint.json")
                .display()
                .to_string()
        )
    );

    // A package with no blueprint document (never written by save) is an
    // explicit refusal, not an invented graph.
    let no_doc = json_line(
        3,
        "blueprint.load",
        &serde_json::json!({ "name": "no_such_pkg" }),
    );
    let reply = rpc(&core, &registry, &peer, no_doc).await;
    let msg = reply["error"]["message"].as_str().unwrap_or_default();
    assert!(msg.contains("no blueprint document"), "{reply}");

    // The load path screens the name with the save path's own rules, so
    // no caller can steer the read outside the strategy root.
    let escape = json_line(
        4,
        "blueprint.load",
        &serde_json::json!({ "name": "../escape" }),
    );
    let reply = rpc(&core, &registry, &peer, escape).await;
    let msg = reply["error"]["message"].as_str().unwrap_or_default();
    assert!(msg.contains("ASCII letters"), "{reply}");

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn blueprint_load_source_reads_a_handwritten_package() {
    // 手写包的编辑入口：没有 blueprint.json 的包不再被拒 —— 入口文件与
    // manifest 原样读回；名字闸门与 load/save 同源；不存在的包报「no
    // manifest」而不是编造内容。
    let root =
        std::env::temp_dir().join(format!("bk-ipc-bpsrc-{}-{}", std::process::id(), now_ms()));
    let (core, registry, peer) = save_fixture(&root).await;
    let pkg = root.join("hand_poked");
    std::fs::create_dir_all(&pkg).expect("package dir");
    std::fs::write(pkg.join("strategy.lua"), "function bk_evaluate() end\n").expect("entry file");
    std::fs::write(
        pkg.join("manifest.json"),
        r#"{"name":"hand_poked","version":"2.0.0","api":"1.0","entry":"strategy.lua","sha256":"deadbeef"}"#,
    )
    .expect("manifest");

    let line = json_line(
        1,
        "blueprint.loadSource",
        &serde_json::json!({ "name": "hand_poked" }),
    );
    let reply = rpc(&core, &registry, &peer, line).await;
    assert!(reply.get("error").is_none(), "read must answer: {reply}");
    let r = &reply["result"];
    assert_eq!(r["lua"], serde_json::json!("function bk_evaluate() end\n"));
    assert_eq!(r["sha256"], serde_json::json!("deadbeef"));
    assert_eq!(r["manifest"]["version"], serde_json::json!("2.0.0"));

    // A package with no manifest is refused with the honest reason.
    let line = json_line(
        2,
        "blueprint.loadSource",
        &serde_json::json!({ "name": "no_such_pkg" }),
    );
    let reply = rpc(&core, &registry, &peer, line).await;
    let msg = reply["error"]["message"].as_str().unwrap_or_default();
    assert!(msg.contains("no manifest"), "{reply}");

    // Same package-name screen as load/save: no path steering.
    let line = json_line(
        3,
        "blueprint.loadSource",
        &serde_json::json!({ "name": "../escape" }),
    );
    let reply = rpc(&core, &registry, &peer, line).await;
    let msg = reply["error"]["message"].as_str().unwrap_or_default();
    assert!(msg.contains("ASCII letters"), "{reply}");

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn blueprint_save_source_reseals_the_manifest_and_refuses_implicit_writes() {
    // 写回镜像：原样落盘 + sha256 现场重算进 manifest（装载器照常验证）；
    // 内容不同且未显式 overwrite 时拒绝写；目标包不存在时拒绝（编辑不
    // 创建）；manifest entry 带路径成分时拒绝（不写出包目录）。
    let root =
        std::env::temp_dir().join(format!("bk-ipc-bpsrcw-{}-{}", std::process::id(), now_ms()));
    let (core, registry, peer) = save_fixture(&root).await;
    let pkg = root.join("hand_poked");
    std::fs::create_dir_all(&pkg).expect("package dir");
    std::fs::write(pkg.join("strategy.lua"), "-- old\n").expect("entry file");
    std::fs::write(
        pkg.join("manifest.json"),
        r#"{"name":"hand_poked","version":"1.0.0","api":"1.0","entry":"strategy.lua","sha256":"old"}"#,
    )
    .expect("manifest");

    // Different body, no overwrite — refused, and nothing changed.
    let line = json_line(
        1,
        "blueprint.saveSource",
        &serde_json::json!({ "name": "hand_poked", "lua": "-- new\n" }),
    );
    let reply = rpc(&core, &registry, &peer, line).await;
    let msg = reply["error"]["message"].as_str().unwrap_or_default();
    assert!(msg.contains("must be explicit"), "{reply}");
    assert_eq!(
        std::fs::read_to_string(pkg.join("strategy.lua")).expect("old body intact"),
        "-- old\n"
    );

    // Explicit overwrite lands, and the manifest's sha256 now matches the
    // NEW bytes (the loader's §6.4 verification would refuse otherwise).
    let line = json_line(
        2,
        "blueprint.saveSource",
        &serde_json::json!({ "name": "hand_poked", "lua": "-- new\n", "overwrite": true }),
    );
    let reply = rpc(&core, &registry, &peer, line).await;
    assert!(reply.get("error").is_none(), "overwrite must land: {reply}");
    let r = &reply["result"];
    let new_sha = r["luaSha256"].as_str().expect("sha in receipt");
    assert_eq!(
        std::fs::read_to_string(pkg.join("strategy.lua")).expect("new body"),
        "-- new\n"
    );
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(pkg.join("manifest.json")).expect("m"))
            .expect("manifest json");
    assert_eq!(manifest["sha256"], serde_json::json!(new_sha));
    assert_eq!(
        manifest["version"],
        serde_json::json!("1.0.0"),
        "untouched fields stay"
    );

    // Identical body without overwrite is a no-op write that still answers.
    let line = json_line(
        3,
        "blueprint.saveSource",
        &serde_json::json!({ "name": "hand_poked", "lua": "-- new\n" }),
    );
    let reply = rpc(&core, &registry, &peer, line).await;
    assert!(reply.get("error").is_none(), "no-op write answers: {reply}");

    // A package with no manifest is refused — saveSource edits, never creates.
    let line = json_line(
        4,
        "blueprint.saveSource",
        &serde_json::json!({ "name": "no_such_pkg", "lua": "x", "overwrite": true }),
    );
    let reply = rpc(&core, &registry, &peer, line).await;
    let msg = reply["error"]["message"].as_str().unwrap_or_default();
    assert!(msg.contains("no manifest"), "{reply}");

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn blueprint_save_refusals_write_nothing() {
    // Same config-rooted fixture as the ok-path test.
    let root = std::env::temp_dir().join(format!(
        "bk-ipc-bpsave-bad-{}-{}",
        std::process::id(),
        now_ms()
    ));
    let (core, registry, peer) = save_fixture(&root).await;

    // A path-flavoured name is refused BEFORE any directory exists.
    let line = json_line(
        1,
        "blueprint.save",
        &serde_json::json!({
            "name": "../escape", "json": WIRE_BLUEPRINT,
        }),
    );
    let reply = rpc(&core, &registry, &peer, line).await;
    let msg = reply["error"]["message"].as_str().unwrap_or_default();
    assert!(msg.contains("ASCII letters"), "{reply}");
    assert!(!root.join("escape").exists(), "no directory was created");

    // An invalid blueprint is refused by the compiler — nothing on disk.
    let line = json_line(
        2,
        "blueprint.save",
        &serde_json::json!({
            "name": "no_action_pkg",
            "json": r#"{"version":1,"name":"noact","nodes":[
            {"id":"a1","type":"data_source","params":{"field":"tick.price"}},
            {"id":"a2","type":"condition","params":{"op":"<","value":1}}],
            "edges":[{"from":"a1","to":"a2"}]}"#,
        }),
    );
    let reply = rpc(&core, &registry, &peer, line).await;
    let msg = reply["error"]["message"].as_str().unwrap_or_default();
    assert!(msg.contains("no reachable action output"), "{reply}");
    assert!(
        !root.join("no_action_pkg").exists(),
        "a refused compile must write nothing"
    );

    // A name carrying a forbidden substring is refused (it would be
    // embedded in the generated source verbatim).
    let line = json_line(
        3,
        "blueprint.save",
        &serde_json::json!({
            "name": "debug_probe", "json": WIRE_BLUEPRINT,
        }),
    );
    let reply = rpc(&core, &registry, &peer, line).await;
    let msg = reply["error"]["message"].as_str().unwrap_or_default();
    assert!(msg.contains("refused"), "{reply}");
    assert!(
        !root.join("debug_probe").exists(),
        "no directory was created"
    );

    let _ = std::fs::remove_dir_all(&root);
}

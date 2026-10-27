#![cfg(test)]

use super::*;

use crate::account::AccountId;
use blitzkrieg_market_api::net::now_ms;
use std::sync::Arc;

// ── E28 (§12.1): the account verbs, at the wire ─────────────────────────

/// The §12.1 wire shape: `version` + the SESSION's active + one view per
/// configured account, each carrying its OWN book (§9.3) — and no
/// credential value anywhere on the wire (§9.4: `credentialsLoaded` is a
/// boolean, never a value).
#[tokio::test]
async fn account_list_reports_the_session_default_and_every_book() {
    let (core, registry, peer) = two_account_fixture().await;
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"account.list","params":{}}"#.to_string(),
    )
    .await;
    assert!(reply.get("error").is_none(), "list must answer: {reply}");
    assert_eq!(reply["result"]["version"], serde_json::json!("1.1"));
    // No session switch yet: the process-level active is what's reported.
    assert_eq!(reply["result"]["active"], serde_json::json!("default"));
    let accounts = reply["result"]["accounts"].as_array().expect("views");
    assert_eq!(accounts.len(), 2, "one view per configured account");
    assert_eq!(accounts[0]["id"], serde_json::json!("default"));
    assert_eq!(accounts[1]["id"], serde_json::json!("paper"));
    assert_eq!(accounts[0]["status"], serde_json::json!("active"));
    // Money fields are the account's OWN book: the seeded default carries
    // the dry seed; paper starts flat.
    assert!(accounts[0]["balance"].is_number());
    assert!(accounts[1]["balance"].is_number());
    assert_eq!(
        accounts[0]["credentialsLoaded"],
        serde_json::json!(false),
        "§9.4: a presence fact only"
    );
    let raw = reply.to_string();
    assert!(
        !raw.contains("apiKeyValue") && !raw.contains("secretValue"),
        "no credential VALUE may ride the wire: {raw}"
    );
}

/// §9.5: the session's default account drives the orders an un-versioned
/// client places — an omitted accountId rides the session cell (what this
/// connection switched to), an explicit one always stands.
#[tokio::test]
async fn an_omitted_account_id_rides_the_session_default_and_an_explicit_one_stands() {
    let (core, registry, peer) = two_account_fixture().await;
    let updates = Arc::new(crate::ipc::version::UpdateState::new(false, false));
    let mut session = SessionState::default();

    // Switch the session to `paper` first.
    let reply = rpc_with_updates(
        &core,
        &registry,
        &peer,
        &updates,
        &mut session,
        r#"{"jsonrpc":"2.0","id":1,"method":"account.switch","params":{"accountId":"paper"}}"#
            .to_string(),
    )
    .await;
    assert!(reply.get("error").is_none(), "switch must land: {reply}");

    // A book for `tok`, so the taker crosses.
    let feed = |id: u32| {
        format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"books.snapshot","params":{{"tokenId":"tok","bids":[{{"price":0.40,"size":100}}],"asks":[{{"price":0.41,"size":100}}]}}}}"#
        )
    };
    let slot = now_ms() / 1000 / 900;
    let buy_omitted = format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"orders.place","params":{{"tokenId":"tok","conditionId":"cond","side":"buy","mode":"taker","price":0.41,"size":2,"internalKey":"e-omitted","strategy":"s","asset":"BTC","direction":"up","roundSlot":{slot}}}}}"#
    );
    let reply = rpc_with_updates(&core, &registry, &peer, &updates, &mut session, feed(1)).await;
    assert!(reply.get("error").is_none(), "book feed must land: {reply}");
    let reply =
        rpc_with_updates(&core, &registry, &peer, &updates, &mut session, buy_omitted).await;
    assert!(
        reply.get("error").is_none(),
        "an orderless place must land: {reply}"
    );

    // The position (and the reservation) belong to `paper` — the session's
    // default, not the process-level `default` account.
    let c = core.lock().await;
    let paper = c.accounts().ledger_of(&AccountId::from("paper")).unwrap();
    assert!(
        paper.reserved() > rust_decimal::Decimal::ZERO || {
            let views = c.position_views(now_ms());
            !views.is_empty() && views.iter().all(|v| v.account_id == "paper")
        },
        "the omitted-account order must spend the SESSION's book"
    );
    let def = c
        .accounts()
        .ledger_of(&AccountId::from("default"))
        .unwrap()
        .balance();
    assert_eq!(
        def,
        rust_decimal::Decimal::from(10_000),
        "the process-level book is untouched"
    );
    drop(c);

    // An EXPLICIT `accountId: "default"` NAMES its account: it spends the
    // process-level book even though the session points at paper. A second
    // token, because the position book is one-position-per-token and the
    // first order already holds `tok`.
    let slot = now_ms() / 1000 / 900;
    let feed2 = r#"{"jsonrpc":"2.0","id":3,"method":"books.snapshot","params":{"tokenId":"tok2","bids":[{"price":0.40,"size":100}],"asks":[{"price":0.41,"size":100}]}}"#.to_string();
    let reply = rpc_with_updates(&core, &registry, &peer, &updates, &mut session, feed2).await;
    assert!(reply.get("error").is_none(), "book 2 must land: {reply}");
    let buy_explicit = format!(
        r#"{{"jsonrpc":"2.0","id":4,"method":"orders.place","params":{{"tokenId":"tok2","conditionId":"cond2","side":"buy","mode":"taker","price":0.41,"size":2,"internalKey":"e-explicit","strategy":"s","asset":"ETH","direction":"up","roundSlot":{slot},"accountId":"default"}}}}"#
    );
    let reply = rpc_with_updates(
        &core,
        &registry,
        &peer,
        &updates,
        &mut session,
        buy_explicit,
    )
    .await;
    assert!(
        reply.get("error").is_none(),
        "an explicit place must land: {reply}"
    );
    let c = core.lock().await;
    let views = c.position_views(now_ms());
    assert!(
        views.iter().any(|v| v.account_id == "default"),
        "the explicit accountId must stand: {views:?}"
    );
}

/// `account.switch` is SESSION-level (§9.5): the switched connection sees
/// the new default on its NEXT request, a second connection still sees the
/// process-level one, and Core's process active never moves.
#[tokio::test]
async fn a_switch_is_session_scoped_and_survives_across_requests() {
    let (core, registry, peer) = two_account_fixture().await;
    let updates = Arc::new(crate::ipc::version::UpdateState::new(false, false));
    let mut session_a = SessionState::default();
    let mut session_b = SessionState::default();

    let reply = rpc_with_updates(
        &core,
        &registry,
        &peer,
        &updates,
        &mut session_a,
        r#"{"jsonrpc":"2.0","id":1,"method":"account.switch","params":{"accountId":"paper"}}"#
            .to_string(),
    )
    .await;
    assert!(reply.get("error").is_none(), "switch must answer: {reply}");
    assert_eq!(reply["result"]["active"], serde_json::json!("paper"));

    // Session A now defaults to paper …
    let reply = rpc_with_updates(
        &core,
        &registry,
        &peer,
        &updates,
        &mut session_a,
        r#"{"jsonrpc":"2.0","id":2,"method":"account.list","params":{}}"#.to_string(),
    )
    .await;
    assert_eq!(
        reply["result"]["active"],
        serde_json::json!("paper"),
        "the session cell lingers across requests"
    );

    // … while session B still sees the process-level one.
    let reply = rpc_with_updates(
        &core,
        &registry,
        &peer,
        &updates,
        &mut session_b,
        r#"{"jsonrpc":"2.0","id":2,"method":"account.list","params":{}}"#.to_string(),
    )
    .await;
    assert_eq!(
        reply["result"]["active"],
        serde_json::json!("default"),
        "another connection is NOT switched"
    );

    // And the process-level active itself never moved.
    let c = core.lock().await;
    assert_eq!(c.accounts().active_id().as_str(), "default");
}

/// A switch to an account the deployment never configured is refused and
/// leaves the session cell exactly where it was (§9.3: no implicit
/// account, ever).
#[tokio::test]
async fn a_switch_to_an_unknown_account_is_refused_without_touching_the_session() {
    let (core, registry, peer) = two_account_fixture().await;
    let updates = Arc::new(crate::ipc::version::UpdateState::new(false, false));
    let mut session = SessionState::default();
    let reply = rpc_with_updates(
        &core,
        &registry,
        &peer,
        &updates,
        &mut session,
        r#"{"jsonrpc":"2.0","id":1,"method":"account.switch","params":{"accountId":"ghost"}}"#
            .to_string(),
    )
    .await;
    assert_eq!(
        reply["error"]["code"],
        serde_json::json!(Failure::INVALID_PARAMS),
        "an unknown account must fail loud: {reply}"
    );
    assert_eq!(
        reply["error"]["data"]["coreCode"],
        serde_json::json!("INVALID_PARAMS"),
        "the refusal is a client fault at heart"
    );
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("unknown account"),
        "the refusal must say WHY: {reply}"
    );
    // The session still resolves to the process default.
    let reply = rpc_with_updates(
        &core,
        &registry,
        &peer,
        &updates,
        &mut session,
        r#"{"jsonrpc":"2.0","id":2,"method":"account.list","params":{}}"#.to_string(),
    )
    .await;
    assert_eq!(reply["result"]["active"], serde_json::json!("default"));
}

/// §12.1 / §14 acceptance: a tighten lands (`active -> frozen`, then
/// `frozen -> suspended` with the flat reason), and the reverse is
/// refused with INVALID_PARAMS — an unwind one IPC call could undo is
/// not an unwind. Unfreezing is config + restart.
#[tokio::test]
async fn a_status_tighten_lands_and_a_loosening_is_refused() {
    let (core, registry, peer) = two_account_fixture().await;

    // active -> frozen: allowed, and the view carries no reason.
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"account.status","params":{"accountId":"paper","status":"frozen"}}"#
            .to_string(),
    )
    .await;
    assert!(reply.get("error").is_none(), "a tighten must land: {reply}");
    assert_eq!(reply["result"]["id"], serde_json::json!("paper"));
    assert_eq!(reply["result"]["status"], serde_json::json!("frozen"));
    assert!(
        reply["result"].get("statusReason").is_none(),
        "a freeze without a reason reports none: {reply}"
    );

    // frozen -> suspended WITH a reason: still a tighten, the flat reason
    // is carried onto the view.
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":2,"method":"account.status","params":{"accountId":"paper","status":"suspended","reason":"ops review"}}"#
            .to_string(),
    )
    .await;
    assert!(reply.get("error").is_none(), "a tighten must land: {reply}");
    assert_eq!(reply["result"]["status"], serde_json::json!("suspended"));
    assert_eq!(
        reply["result"]["statusReason"],
        serde_json::json!("ops review")
    );

    // The posture is visible in the list view, as a plain string.
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":3,"method":"account.list","params":{}}"#.to_string(),
    )
    .await;
    let accounts = reply["result"]["accounts"].as_array().expect("views");
    assert_eq!(accounts[1]["status"], serde_json::json!("suspended"));
    assert_eq!(accounts[1]["statusReason"], serde_json::json!("ops review"));

    // And the loosening is refused at the wire with INVALID_PARAMS.
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":4,"method":"account.status","params":{"accountId":"paper","status":"active"}}"#
            .to_string(),
    )
    .await;
    assert_eq!(
        reply["error"]["code"],
        serde_json::json!(Failure::INVALID_PARAMS),
        "suspended -> active must be refused with INVALID_PARAMS: {reply}"
    );
    assert_eq!(
        reply["error"]["data"]["coreCode"],
        serde_json::json!("INVALID_PARAMS"),
        "the refusal is a client fault at heart"
    );
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("LOOSEN"),
        "the refusal must say WHY: {reply}"
    );
}

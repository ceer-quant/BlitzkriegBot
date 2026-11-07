#![cfg(test)]

use super::*;

use crate::service::Core;
use blitzkrieg_market_api::net::now_ms;
use std::sync::Arc;
use tokio::sync::Mutex as AsyncMutex;

type TestCore = Arc<AsyncMutex<Core>>;
type TestRegistry = crate::market::registry::MarketPluginRegistry;

/// Seed a book and lift it, so the core holds one open position on `token`.
/// The position book is one-position-per-token, so a second token is a second
/// position (the "only this id closed" test needs two).
async fn open_position(
    core: &TestCore,
    registry: &TestRegistry,
    peer: &PeerAuth,
    token: &str,
    asset: &str,
    key: &str,
) {
    let book = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"books.snapshot","params":{{"tokenId":"{token}","bids":[{{"price":0.39,"size":100}}],"asks":[{{"price":0.40,"size":100}}]}}}}"#
    );
    let reply = rpc(core, registry, peer, book).await;
    assert!(reply.get("error").is_none(), "book for {token}: {reply}");

    let slot = now_ms() / 1000 / 900;
    let order = format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"orders.place","params":{{"tokenId":"{token}","conditionId":"cond-{token}","side":"buy","mode":"taker","price":0.40,"size":5,"internalKey":"{key}","strategy":"s","asset":"{asset}","direction":"up","roundSlot":{slot}}}}}"#
    );
    let reply = rpc(core, registry, peer, order).await;
    assert!(reply.get("error").is_none(), "place on {token}: {reply}");
    assert_eq!(
        reply["result"]["status"],
        serde_json::json!("FILLED"),
        "the probe order must fill: {reply}"
    );
}

async fn open_positions(core: &TestCore, registry: &TestRegistry, peer: &PeerAuth) -> Vec<Value> {
    let reply = rpc(
        core,
        registry,
        peer,
        r#"{"jsonrpc":"2.0","id":9,"method":"positions.list","params":{}}"#.to_string(),
    )
    .await;
    assert!(reply.get("error").is_none(), "positions.list: {reply}");
    reply["result"]["positions"]
        .as_array()
        .expect("positions.list returns an array")
        .clone()
}

async fn exit_with(
    core: &TestCore,
    registry: &TestRegistry,
    peer: &PeerAuth,
    params: &str,
) -> Value {
    rpc(
        core,
        registry,
        peer,
        format!(r#"{{"jsonrpc":"2.0","id":7,"method":"positions.exit","params":{params}}}"#),
    )
    .await
}

/// Issue 894: a params object that does not deserialize is a CLIENT error.
/// The old handler swallowed it into `position_id: None`, which `flatten`
/// treats as "every open position" — a typo flattened the whole book.
#[tokio::test]
async fn exit_params_that_do_not_deserialize_never_flatten_the_book() {
    let (core, registry, peer) = hot_reload_fixture().await;
    open_position(&core, &registry, &peer, "tok", "BTC", "exit-bad").await;
    assert_eq!(open_positions(&core, &registry, &peer).await.len(), 1);

    for (label, params) in [
        ("wrong type", r#"{"positionId":123}"#),
        ("snake-case field", r#"{"position_id":"tok"}"#),
        (
            "unknown extra field",
            r#"{"positionId":"all","force":true}"#,
        ),
    ] {
        let reply = exit_with(&core, &registry, &peer, params).await;
        assert_eq!(
            reply["error"]["code"],
            serde_json::json!(-32602),
            "{label} must be INVALID_PARAMS: {reply}"
        );
        assert_eq!(
            open_positions(&core, &registry, &peer).await.len(),
            1,
            "{label} must not close anything: {reply}"
        );
    }
}

/// Issue 895 compatibility closure: a request body with NO `params` key at
/// all. The envelope's `#[serde(default)] params` fills in `Value::Null`,
/// which `typed::<PositionExitParams>` refuses — a CLIENT error (-32602).
/// Pinned so "omitted params" can never silently decay into the historical
/// flatten-all: a malformed client must move no positions.
#[tokio::test]
async fn an_omitted_params_key_is_a_client_error_not_a_flatten() {
    let (core, registry, peer) = hot_reload_fixture().await;
    open_position(&core, &registry, &peer, "tok", "BTC", "exit-no-key").await;
    assert_eq!(open_positions(&core, &registry, &peer).await.len(), 1);

    // Not `exit_with`: that helper always spells a params value; here the
    // key itself is absent from the request body.
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":7,"method":"positions.exit"}"#.to_string(),
    )
    .await;
    assert_eq!(
        reply["error"]["code"],
        serde_json::json!(-32602),
        "an omitted params key must be INVALID_PARAMS: {reply}"
    );
    assert_eq!(
        open_positions(&core, &registry, &peer).await.len(),
        1,
        "an omitted params key must not close anything"
    );
}

/// The explicit `"params": null` spelling deserializes to the same
/// `Value::Null` as the omitted key, so both spellings must land on the
/// same -32602 — and neither falls back to flatten-all (#895).
#[tokio::test]
async fn an_explicit_null_params_is_a_client_error_not_a_flatten() {
    let (core, registry, peer) = hot_reload_fixture().await;
    open_position(&core, &registry, &peer, "tok", "BTC", "exit-null").await;
    assert_eq!(open_positions(&core, &registry, &peer).await.len(), 1);

    let reply = exit_with(&core, &registry, &peer, "null").await;
    assert_eq!(
        reply["error"]["code"],
        serde_json::json!(-32602),
        "params:null must be INVALID_PARAMS: {reply}"
    );
    assert_eq!(
        open_positions(&core, &registry, &peer).await.len(),
        1,
        "params:null must not close anything"
    );
}

/// The schema documents `"all"` as the explicit whole-book spelling. Before
/// the fix it was compared against position ids literally and closed nothing.
#[tokio::test]
async fn the_documented_all_token_flattens_every_position() {
    let (core, registry, peer) = hot_reload_fixture().await;
    open_position(&core, &registry, &peer, "tok", "BTC", "exit-all-1").await;
    open_position(&core, &registry, &peer, "tok2", "ETH", "exit-all-2").await;
    assert_eq!(open_positions(&core, &registry, &peer).await.len(), 2);

    let reply = exit_with(&core, &registry, &peer, r#"{"positionId":"all"}"#).await;
    assert_eq!(reply["result"]["closed"], serde_json::json!(2), "{reply}");
    assert!(open_positions(&core, &registry, &peer).await.is_empty());
}

#[tokio::test]
async fn a_specific_id_closes_only_that_position() {
    let (core, registry, peer) = hot_reload_fixture().await;
    open_position(&core, &registry, &peer, "tok", "BTC", "exit-one-1").await;
    open_position(&core, &registry, &peer, "tok2", "ETH", "exit-one-2").await;

    let views = open_positions(&core, &registry, &peer).await;
    assert_eq!(views.len(), 2);
    let id = views[0]["id"]
        .as_str()
        .expect("a position view carries its id")
        .to_string();

    let reply = exit_with(
        &core,
        &registry,
        &peer,
        &format!(r#"{{"positionId":"{id}"}}"#),
    )
    .await;
    assert_eq!(reply["result"]["closed"], serde_json::json!(1), "{reply}");

    let left = open_positions(&core, &registry, &peer).await;
    assert_eq!(left.len(), 1, "only the named position may close");
    assert_ne!(left[0]["id"], serde_json::json!(id));
}

/// The omitted-id spelling predates the explicit `"all"` token and kept its
/// meaning: the whole book. Pinned so a later hardening does not silently
/// turn an operator's `positions.exit {}` into a no-op.
#[tokio::test]
async fn an_omitted_id_keeps_the_historical_flatten_all() {
    let (core, registry, peer) = hot_reload_fixture().await;
    open_position(&core, &registry, &peer, "tok", "BTC", "exit-omitted").await;

    let reply = exit_with(&core, &registry, &peer, "{}").await;
    assert_eq!(reply["result"]["closed"], serde_json::json!(1), "{reply}");
    assert!(open_positions(&core, &registry, &peer).await.is_empty());
}

/// Issue 991: `positions.unverified_exposure` answers the unverified exposure
/// report over IPC. The row in the report is a REAL fallback close (the core's
/// own audited force-close path) — no mock data. A `venue` close never appears
/// in the report; the aggregates ride Decimal's default (string) serde, the
/// entry decimals the number-or-string wire rule.
#[tokio::test]
async fn unverified_exposure_query_reports_real_fallback_rows() {
    use blitzkrieg_market_api::net::now_ms;
    // A private audit path: the force-close gate refuses an unaudited close.
    let dir = std::env::temp_dir().join(format!("bk-unverified-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = CoreConfig {
        manual_ops_audit_path: Some(
            dir.join("audit/manual-ops.jsonl")
                .to_string_lossy()
                .into_owned(),
        ),
        ..Default::default()
    };
    let mut c = Core::new(cfg.clone());
    c.set_balance(cfg.dry_seed_balance);
    let core = Arc::new(AsyncMutex::new(c));
    let registry = crate::market::registry::MarketPluginRegistry::new();
    let peer = PeerAuth::SameUid { uid: own_uid() };

    // Empty book: the frozen envelope with an empty list — never `null`,
    // never METHOD_NOT_FOUND.
    let empty = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"positions.unverified_exposure","params":{}}"#
            .to_string(),
    )
    .await;
    assert!(empty.get("error").is_none(), "the query answers: {empty}");
    assert_eq!(empty["result"]["report"]["entries"], serde_json::json!([]));
    assert_eq!(
        empty["result"]["report"]["totalPositions"],
        serde_json::json!(0)
    );

    // One real fallback row: open a position on the wire, close it through the
    // core's audited force-close (source == "fallback"), then query again.
    open_position(&core, &registry, &peer, "tok", "BTC", "unverified-1").await;
    let views = open_positions(&core, &registry, &peer).await;
    let id = views[0]["id"]
        .as_str()
        .expect("position view carries id")
        .to_string();
    let closed = {
        let mut c = core.lock().await;
        // The position's round ends within the next 900 s, so a timestamp
        // ~3 h out is genuinely past expiry (gate 2 passes).
        c.force_close_expired_position(&id, "uid:op", now_ms() + 10_800_000)
            .expect("expired + audited = closed")
    };
    assert_eq!(closed.source, "fallback", "precondition: a fallback row");

    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":2,"method":"positions.unverified_exposure","params":{}}"#
            .to_string(),
    )
    .await;
    assert!(reply.get("error").is_none(), "the query answers: {reply}");
    assert_eq!(
        reply["result"]["report"]["totalPositions"],
        serde_json::json!(1),
        "the fallback row is on the report: {reply}"
    );
    assert_eq!(
        reply["result"]["report"]["entries"][0]["positionId"],
        serde_json::json!(id),
        "the entry names the real closed position: {reply}"
    );
    assert_eq!(
        reply["result"]["report"]["entries"][0]["source"],
        serde_json::json!("fallback"),
        "the entry carries its provenance: {reply}"
    );
    assert!(
        reply["result"]["report"]["entries"][0]["netPnlUsd"].is_number()
            || reply["result"]["report"]["entries"][0]["netPnlUsd"].is_string(),
        "entry decimals ride the number-or-string wire rule: {reply}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

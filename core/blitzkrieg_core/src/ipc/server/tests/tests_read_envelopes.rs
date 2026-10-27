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

// ── v0.3 Wave 0 (#329): the two read-only envelopes, at the wire ────────
//
// Both arms are ADDITIVE (two new methods; nothing existing moves), so what
// these tests pin is the envelope a consumer parses: the echoed routing
// fields and the empty list — never `null`, never METHOD_NOT_FOUND. E25 and
// E29 fill the lists later; these shapes must survive that swap.

/// `kline.history` keeps the frozen envelope (symbol/interval echoed
/// verbatim, `klines` oldest first). E29 swaps the EMPTY list for real
/// bars: with no engine data the empty list is still the truth; the
/// bar-producing path is driven with CONTROLLED timestamps through the
/// same `engine_on_data` entry the feed arm drives (the arm stamps the
/// host clock, so a wall-clock wire feed cannot pin bucketing here).
#[tokio::test]
async fn kline_history_answers_the_frozen_envelope() {
    let (core, registry, peer) = hot_reload_fixture().await;
    // Fresh core, no data: the empty envelope — never `null`, never 404.
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"kline.history","params":{"symbol":"BTC-UP","interval":"min1"}}"#
            .to_string(),
    )
    .await;
    assert!(
        reply.get("error").is_none(),
        "a Wave-0 envelope must answer, not 404: {reply}"
    );
    assert_eq!(reply["result"]["symbol"], serde_json::json!("BTC-UP"));
    assert_eq!(reply["result"]["interval"], serde_json::json!("min1"));
    assert_eq!(reply["result"]["klines"], serde_json::json!([]));

    // Two mids one minute apart: the first min1 bar closes. Driven
    // through `engine_on_data` — the same entry `books.snapshot` lands
    // in — with venue timestamps under the test's control.
    use rust_decimal_macros::dec;
    for (ts_ms, bid, ask) in [
        (1_758_888_010_000, dec!(0.40), dec!(0.41)),
        (1_758_888_070_000, dec!(0.50), dec!(0.51)),
    ] {
        core.lock().await.engine_on_data(
            crate::engine::DataEvent::Book {
                token_id: "BTC-UP".to_string(),
                bids: vec![(bid, dec!(100))],
                asks: vec![(ask, dec!(100))],
                now_ms: ts_ms,
            },
            ts_ms,
        );
    }
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":3,"method":"kline.history","params":{"symbol":"BTC-UP","interval":"min1"}}"#
            .to_string(),
    )
    .await;
    let bars = &reply["result"]["klines"];
    assert_eq!(bars.as_array().map(Vec::len), Some(2), "{reply}");
    assert_eq!(bars[0]["isClosed"], serde_json::json!(true));
    assert_eq!(bars[0]["open"], serde_json::json!(0.405));
    assert_eq!(bars[0]["close"], serde_json::json!(0.405));
    assert_eq!(bars[0]["tradeCount"], serde_json::json!(1));
    assert!(!bars[1]["isClosed"].as_bool().unwrap_or(true));
    assert_eq!(bars[1]["open"], serde_json::json!(0.505));
    // `limit` clamps the window.
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":4,"method":"kline.history","params":{"symbol":"BTC-UP","interval":"min1","limit":1}}"#
            .to_string(),
    )
    .await;
    assert_eq!(reply["result"]["klines"].as_array().map(Vec::len), Some(1));
}

/// §12.2: the subscription pair writes THE SESSION's set — all three
/// calls below share one set (as one connection's arms do), the reply
/// counts the set size AFTER the change, dedup holds (resubscribing an
/// existing pair does not grow it), and an unsubscribe carves the set
/// down. The auto-unsubscribe on disconnect is `spawn_session`'s drop of
/// this very Arc — structural, nothing to assert here beyond the set
/// arithmetic the reply reports.
#[tokio::test]
async fn kline_subscribe_and_unsubscribe_shape_the_session_set() {
    let (core, registry, peer) = hot_reload_fixture().await;
    let mut session = SessionState::default();
    let reply = rpc_with_subs(
        &core,
        &registry,
        &peer,
        &mut session,
        r#"{"jsonrpc":"2.0","id":1,"method":"kline.subscribe","params":{"symbols":["BTC-UP","BTC-DOWN"],"intervals":["min1","min5"]}}"#
            .to_string(),
    )
    .await;
    assert!(reply.get("error").is_none(), "{reply}");
    assert_eq!(reply["result"]["subscribed"], serde_json::json!(4));

    // Dedup: the same four pairs again — the count does not grow.
    let reply = rpc_with_subs(
        &core,
        &registry,
        &peer,
        &mut session,
        r#"{"jsonrpc":"2.0","id":2,"method":"kline.subscribe","params":{"symbols":["BTC-UP"],"intervals":["min1"]}}"#
            .to_string(),
    )
    .await;
    assert_eq!(reply["result"]["subscribed"], serde_json::json!(4));

    // Unsubscribe two pairs.
    let reply = rpc_with_subs(
        &core,
        &registry,
        &peer,
        &mut session,
        r#"{"jsonrpc":"2.0","id":3,"method":"kline.unsubscribe","params":{"symbols":["BTC-UP","BTC-DOWN"],"intervals":["min5"]}}"#
            .to_string(),
    )
    .await;
    assert_eq!(reply["result"]["subscribed"], serde_json::json!(2));

    // The caller-owned set holds exactly what the replies said.
    assert_eq!(session.kline_subs.lock().expect("subs").len(), 2);
    assert!(
        session
            .kline_subs
            .lock()
            .expect("subs")
            .contains(&("BTC-UP".into(), crate::kline::KlineInterval::Min1))
    );
    assert!(
        session
            .kline_subs
            .lock()
            .expect("subs")
            .contains(&("BTC-DOWN".into(), crate::kline::KlineInterval::Min1))
    );
}

/// `intent.audit.tail`: a missing log is the documented empty envelope;
/// with rows present it answers the NEWEST window in file order, and
/// `accountId` / `decision` filter BEFORE the window is taken (`decision`
/// case-insensitively against `decision.status`, the §3.4 wire shape).
/// The read is a BACKWARD tail scan (§12.3: 读尾部，不读全文件): `total`
/// reports the matches within the scanned span and is OMITTED when the
/// window filled before the file head (earlier rows were never counted —
/// a fake full count is worse than no count).
#[tokio::test]
async fn intent_audit_tail_reads_the_tail_and_filters() {
    let dir = std::path::Path::new("data/audit");
    // Remove only THIS test's log: the directory is shared with the
    // execution-policy audit, which the concurrent policy IPC test
    // reads — a directory nuke here raced its history read.
    let _ = std::fs::remove_file(dir.join("intents.jsonl"));
    let (core, registry, peer) = hot_reload_fixture().await;

    // Fresh data dir: the empty envelope, not an error.
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"intent.audit.tail","params":{}}"#.to_string(),
    )
    .await;
    assert!(reply.get("error").is_none(), "{reply}");
    assert_eq!(reply["result"]["records"], serde_json::json!([]));
    assert_eq!(reply["result"]["total"], serde_json::json!(0));

    // Three rows shaped like §3.4's IntentAuditRecord (camelCase; the
    // decision is the status-tagged enum).
    std::fs::create_dir_all(dir).expect("create data/audit");
    let rows = [
        r#"{"tsMs":1,"accountId":"default","strategy":"s","intentId":"i1","decision":{"status":"APPROVED"},"gates":[],"latencyUs":1}"#,
        r#"{"tsMs":2,"accountId":"paper","strategy":"s","intentId":"i2","decision":{"status":"REJECTED"},"gates":[],"latencyUs":1}"#,
        r#"{"tsMs":3,"accountId":"default","strategy":"s","intentId":"i3","decision":{"status":"MODIFIED"},"gates":[],"latencyUs":1}"#,
    ];
    std::fs::write(dir.join("intents.jsonl"), rows.join("\n")).expect("write rows");

    // No filter: all three, oldest → newest (file order preserved).
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"intent.audit.tail","params":{}}"#.to_string(),
    )
    .await;
    assert_eq!(reply["result"]["total"], serde_json::json!(3));
    assert_eq!(
        reply["result"]["records"][0]["intentId"],
        serde_json::json!("i1")
    );
    assert_eq!(
        reply["result"]["records"][2]["intentId"],
        serde_json::json!("i3")
    );

    // `limit` takes the NEWEST rows, still oldest → newest inside the
    // window; `total` keeps reporting the full filtered count.
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"intent.audit.tail","params":{"limit":1}}"#.to_string(),
    )
    .await;
    assert_eq!(reply["result"]["records"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        reply["result"]["records"][0]["intentId"],
        serde_json::json!("i3")
    );
    // The window filled (= limit reached) and the scan still covered the
    // whole (tiny) file, so `total` names every match: 3, not 1 — the
    // count is the scan's, independent of how many rows the window keeps.
    assert_eq!(reply["result"]["total"], serde_json::json!(3));

    // `decision` matches case-insensitively against `decision.status` …
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"intent.audit.tail","params":{"decision":"rejected"}}"#
            .to_string(),
    )
    .await;
    assert_eq!(reply["result"]["total"], serde_json::json!(1));
    assert_eq!(
        reply["result"]["records"][0]["intentId"],
        serde_json::json!("i2")
    );

    // … and `accountId` is an exact match.
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"intent.audit.tail","params":{"accountId":"default"}}"#
            .to_string(),
    )
    .await;
    assert_eq!(reply["result"]["total"], serde_json::json!(2));

    let _ = std::fs::remove_file(dir.join("intents.jsonl"));
}

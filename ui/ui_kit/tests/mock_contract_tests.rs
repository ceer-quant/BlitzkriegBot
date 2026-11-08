//! Issue #842 lane C — the mock server's contract tests, run through the REAL
//! [`IpcClient`] against a REAL socket (the client must not be able to tell
//! the mock apart from the kernel).
//!
//! Gate coverage (docs/rust-core/ISSUE-842-ACCEPTANCE.md):
//! * **B1** mock starts with zero kernel linkage (a `cargo tree` CI gate
//!   asserts no `blitzkrieg_core` edge; here: the golden family fixtures
//!   serve through the mock verbatim);
//! * **B2** the five scripted scenarios: success / stale revision /
//!   reconnect / malformed payload / RPC error — each ≥1 integration test;
//! * **B3** method coverage floor: account list, market snapshot, order
//!   lifecycle, risk/freeze state, reconnect;
//! * **B4** the same `tests/fixtures/` directory the UI-A seam test and the
//!   webUI check scripts read (one artifact, multiple consumers).

use blitzkrieg_ui_kit::core::ipc_client::{IpcClient, IpcError};
use blitzkrieg_ui_kit::core::mock_server::{family_fixture_path, Behavior, MockIpcServer};

async fn call(socket: &str, method: &str) -> Result<serde_json::Value, IpcError> {
    let mut c = IpcClient::new(socket);
    c.call(method, serde_json::json!({}))
}

// ── B1: the golden family fixtures serve through the mock ───────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b1_golden_family_fixtures_serve_through_the_mock() {
    let mock = MockIpcServer::builder().spawn().await.expect("mock binds");
    // The default surface binds a family per read method; each reply's
    // `result` must equal the fixture bytes verbatim.
    let bindings: &[(&str, u32)] = &[
        ("engine.stats", 1),
        ("risk.limits", 3),
        ("ledger.balance", 7),
        ("engine.round", 9),
        ("market.list", 11),
        ("core.feeQuote", 14),
        ("engine.books", 15),
        ("account.list", 18),
        ("account.journal", 20),
        ("intent.audit.tail", 22),
        ("positions.unverified_exposure", 23),
    ];
    for (method, family) in bindings {
        let result = call(&mock.socket_path, method)
            .await
            .unwrap_or_else(|e| panic!("{method}: {e}"));
        let fixture: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(family_fixture_path(*family))
                .unwrap_or_else(|e| panic!("family {family}: {e}")),
        )
        .unwrap();
        assert_eq!(
            result, fixture,
            "{method} must serve its family-{family} fixture verbatim"
        );
    }
    mock.shutdown().await;
}

// ── B2: the five scenarios ──────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b2_success_scenario_covers_the_b3_read_floor() {
    let mock = MockIpcServer::builder().spawn().await.expect("mock binds");
    // B3's floor rides the success sweep: account list, market snapshot,
    // order-lifecycle read, risk/freeze state.
    for method in [
        "account.list",
        "market.list",
        "orders.list",
        "risk.limits",
        "positions.list",
        "engine.stats",
        "core.ready",
        "trades.summary",
        "trades.history",
    ] {
        call(&mock.socket_path, method)
            .await
            .unwrap_or_else(|e| panic!("success scenario {method}: {e}"));
    }
    mock.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b2_stale_revision_scenario_write_refused_as_client_fault() {
    let mock = MockIpcServer::builder()
        .method(
            "account.switch",
            Behavior::InvalidParams("unknown account: gone-acc".into()),
        )
        .spawn()
        .await
        .expect("mock binds");
    let mut c = IpcClient::new(&mock.socket_path);
    let err = c
        .call(
            "account.switch",
            serde_json::json!({"accountId": "gone-acc"}),
        )
        .expect_err("a stale write must be refused");
    let IpcError::Rpc { code, message } = err else {
        panic!("expected Rpc error, got {err:?}");
    };
    assert_eq!(code, -32602, "client faults ride INVALID_PARAMS");
    assert!(message.contains("gone-acc"));
    mock.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b2_reconnect_scenario_next_call_recovers() {
    let mock = MockIpcServer::builder()
        .method("engine.stats", Behavior::HangOnce)
        .spawn()
        .await
        .expect("mock binds");
    let mut c = IpcClient::new(&mock.socket_path);
    // The default client timeout is 3 s: the hung call must die with it.
    let hung = c.call("engine.stats", serde_json::json!({}));
    assert!(hung.is_err(), "the hung call must time out");
    // The next call reconnects transparently and answers.
    let v = c
        .call("engine.stats", serde_json::json!({}))
        .expect("the next call reconnects and answers");
    assert!(v.is_object());
    mock.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b2_malformed_payload_scenario_surfaces_as_protocol_error() {
    let mock = MockIpcServer::builder()
        .method("engine.stats", Behavior::MalformedOnce)
        .spawn()
        .await
        .expect("mock binds");
    let mut c = IpcClient::new(&mock.socket_path);
    // Broken bytes → the client skips the unparsable line; the mock then
    // keeps the connection open, so the client's own read timeout is the
    // boundary — either way this must be an Err, never a fake success.
    let err = c.call("engine.stats", serde_json::json!({}));
    assert!(err.is_err(), "broken bytes must surface as an error");
    mock.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b2_rpc_error_scenario_carries_code_and_message() {
    let mock = MockIpcServer::builder()
        .method(
            "positions.list",
            Behavior::RpcError {
                code: -32000,
                message: "venue unreachable".into(),
                core_code: Some("VenueError"),
            },
        )
        .spawn()
        .await
        .expect("mock binds");
    let mut c = IpcClient::new(&mock.socket_path);
    let err = c.call("positions.list", serde_json::json!({})).unwrap_err();
    let IpcError::Rpc { code, message } = err else {
        panic!("expected Rpc");
    };
    assert_eq!(code, -32000);
    assert_eq!(message, "venue unreachable");
    mock.shutdown().await;
}

// ── B3: order-lifecycle writes + freeze state + notifications ───────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b3_order_lifecycle_and_freeze_writes_answer() {
    let mock = MockIpcServer::builder().spawn().await.expect("mock binds");
    let placed = call(&mock.socket_path, "orders.place")
        .await
        .expect("place");
    assert_eq!(placed["orderId"], "ord-mock-1");
    let cancelled = call(&mock.socket_path, "orders.cancel")
        .await
        .expect("cancel");
    assert_eq!(cancelled["cancelled"], true);
    let closed = call(&mock.socket_path, "orders.force_close")
        .await
        .expect("force close");
    assert_eq!(closed["accepted"], true);
    let frozen = call(&mock.socket_path, "account.freeze")
        .await
        .expect("freeze");
    assert_eq!(frozen["frozen"], true);
    mock.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b3_interleaved_notification_does_not_desync_the_response_pair() {
    let mock = MockIpcServer::builder()
        .on_call_notify(
            "account.list",
            r#"{"kind":"riskAlert","level":"amber","message":"mock streak"}"#,
        )
        .spawn()
        .await
        .expect("mock binds");
    let mut c = IpcClient::new(&mock.socket_path);
    // The notification arrives BEFORE the reply; the client must skip it and
    // still land the matching response id.
    let v = c
        .call("account.list", serde_json::json!({}))
        .expect("reply after a skipped notification");
    assert!(v.is_object());
    mock.shutdown().await;
}

// ── B4: one fixture directory, multiple consumers ───────────────────────────

#[test]
fn b4_shared_fixture_directory_holds_all_23_families() {
    let dir = family_fixture_path(1).parent().unwrap().to_path_buf();
    let count = std::fs::read_dir(&dir)
        .expect("shared fixture dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("family_"))
        .count();
    assert_eq!(count, 23, "all 23 golden families live in the shared dir");
}

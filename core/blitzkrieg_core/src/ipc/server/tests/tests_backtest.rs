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

// ── #353: the WebUI backtest surface, at the wire ────────────────────────

/// Like [`rpc`], but the caller holds the job registry — the six backtest
/// arms all front ONE shared registry, and a test pinning submission-time
/// validation must hand in the same instance (with its pinned dataset
/// root), not a fresh one per call.
#[allow(clippy::too_many_arguments)]
async fn rpc_jobs(
    core: &Arc<AsyncMutex<Core>>,
    registry: &crate::market::registry::MarketPluginRegistry,
    peer: &PeerAuth,
    jobs: &Arc<crate::backtest_jobs::BacktestJobs>,
    id: u32,
    method: &str,
    params: &str,
) -> Value {
    let line = format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{params}}}"#);
    let update_state = Arc::new(crate::ipc::version::UpdateState::new(false, false));
    let host: Arc<dyn blitzkrieg_market_api::MarketHost> =
        Arc::new(crate::market::host::CoreHost::new(core.clone()));
    let data_plane = DataPlaneRuntime {
        feed_ws_enabled: false,
        discovery_enabled: false,
        discovery_assets: Vec::new(),
        discovery_round_duration_sec: 0,
        discovery_mounted: std::sync::Arc::new(std::sync::Mutex::new(Default::default())),
        soft_unloaded_polling_instances: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(
            0,
        )),
        feed_reuse: None,
    };
    let mut session = SessionState::default();
    serde_json::from_str(
        &handle_line(
            core,
            registry,
            line,
            peer,
            &update_state,
            jobs,
            &host,
            &data_plane,
            &mut session,
        )
        .await,
    )
    .expect("every reply is one JSON object")
}

/// The six arms are wired to the registry through the REAL entry point:
/// list answers the envelope, the read trio name unknown jobs, run/pull
/// validate at the wire (a refusal here is a JSON-RPC error with the
/// actionable message, and NO job exists behind it).
#[tokio::test]
async fn the_backtest_surface_answers_at_the_wire() {
    let (core, registry, peer) = hot_reload_fixture().await;
    let dir = std::env::temp_dir().join(format!("bk-ipc-jobs-{}-{}", std::process::id(), now_ms()));
    std::fs::create_dir_all(&dir).expect("scratch root");
    let jobs = Arc::new(crate::backtest_jobs::BacktestJobs::with_root(dir.clone()));

    // list: an empty root is a valid, empty envelope — not an error.
    let list = rpc_jobs(
        &core,
        &registry,
        &peer,
        &jobs,
        1,
        method::BACKTEST_ONCHAIN_LIST,
        "{}",
    )
    .await;
    assert!(list.get("error").is_none(), "list answers: {list}");
    assert!(list["result"]["datasets"].is_array(), "{list}");

    // run: an archive outside the dataset root is refused BY NAME at the
    // wire — and no job was created behind the refusal.
    let run = rpc_jobs(
        &core,
        &registry,
        &peer,
        &jobs,
        2,
        method::BACKTEST_RUN,
        r#"{"archive":"/etc/passwd"}"#,
    )
    .await;
    assert!(
        run["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("not a dataset")),
        "the refusal names the boundary: {run}"
    );

    // The read trio on a job id that never existed: named JSON-RPC errors.
    for (id, m) in [
        (3, method::BACKTEST_STATUS),
        (4, method::BACKTEST_RESULT),
        (5, method::BACKTEST_EXPORT),
    ] {
        let v = rpc_jobs(&core, &registry, &peer, &jobs, id, m, r#"{"id":"bt-9999"}"#).await;
        assert!(
            v["error"]["message"]
                .as_str()
                .is_some_and(|m| m.contains("unknown job")),
            "{m} names the unknown job: {v}"
        );
    }

    // pull: a malformed wallet is refused with the actionable message.
    let pull = rpc_jobs(
        &core,
        &registry,
        &peer,
        &jobs,
        6,
        method::BACKTEST_ONCHAIN_PULL,
        r#"{"wallet":"0xabc","start":"2026-01-01","end":"2026-01-02"}"#,
    )
    .await;
    assert!(
        pull["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("0x")),
        "the refusal names the wallet shape: {pull}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

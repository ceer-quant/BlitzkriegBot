//! The #842 standard Mock IPC Server (lane C) — a deterministic, fixture-driven
//! JSON-RPC server over a real Unix socket, for UI work that must never import
//! or boot the kernel.
//!
//! Zero kernel dependency by construction: this module links `serde_json` and
//! `tokio` only. The wire behavior it mirrors (the response envelope, the
//! failure codes, the interleaved `core.event` notification stream) is copied
//! from the kernel's `ipc/server` TEXTUALLY — the same discipline as the
//! client's `method` table, which also mirrors core constants without a
//! dependency edge.
//!
//! Determinism: every reply is either a fixture file read from
//! `tests/fixtures/mock/` (the same byte-for-byte goldens the schema-family
//! tests pin, so a contract drift fails BOTH sides) or a scripted
//! [`Behavior`]. No wall-clock, no RNG, no venue I/O. The issue-#842 B2
//! scenarios (success / stale-revision / reconnect / malformed / rpc-error)
//! are TEST concerns — composed out of `Behavior` scripting, not baked in.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

// ── Failure codes — the schema freeze's own constants (mirrored textually) ──
const PARSE_ERROR: i64 = -32700;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const APPLICATION: i64 = -32000;

/// The protocol version this mock speaks (schema freeze: `PROTOCOL_VERSION`).
pub const PROTOCOL_VERSION: &str = "1.1";

/// A scripted in-flight event: this method streams this `core.event`
/// notification (the `params` value) before its reply — the interleaved-push
/// discipline the client drains in [`IpcClient::call`](crate::core::ipc_client).
#[derive(Clone)]
pub struct ScriptedEvent {
    pub method: &'static str,
    pub event: &'static str,
}

/// How a method behaves when the mock serves it.
#[derive(Clone)]
pub enum Behavior {
    /// Reply `result` = the bound fixture file's JSON (see [`fixture_family`]).
    Fixture,
    /// Reply `result` = this value (write-verb echoes).
    Value(serde_json::Value),
    /// Reply with an RPC error; `coreCode` rides `error.data.coreCode` — the
    /// kernel's structured-error envelope.
    RpcError {
        code: i64,
        message: String,
        core_code: Option<&'static str>,
    },
    /// Reply `-32602 INVALID_PARAMS` — the stale-revision posture: a UI
    /// holding a stale view (an account the kernel already removed) sends its
    /// write, and the boundary refuses it as a CLIENT fault (§12.1's
    /// `frozen -> active` refusal shape).
    InvalidParams(String),
    /// Never reply ONCE, then fall back to the builder default for later
    /// calls on the same connection — the reconnect scenario's driver: the
    /// client's read timeout fires, it drops the connection, and its next
    /// call reconnects (the client's own documented posture) and succeeds.
    HangOnce,
    /// Reply with raw broken JSON bytes ONCE, then fall back to the builder
    /// default — the malformed-payload scenario.
    MalformedOnce,
}

/// The mock server. Build with [`MockIpcServer::builder`], [`spawn`][Self::spawn]
/// it, and point a real [`crate::core::ipc_client::IpcClient`] at the returned
/// socket path — the client cannot tell it apart from the kernel.
pub struct MockIpcServer {
    behaviors: BTreeMap<&'static str, Behavior>,
    /// Set when a `*Once` behavior has fired (shared across connections —
    /// "once" means once per MOCK, not per connection: the reconnect
    /// scenario's second call lands on a NEW connection).
    once_fired: Arc<std::sync::atomic::AtomicBool>,
    events: Vec<ScriptedEvent>,
    fixtures_dir: Option<PathBuf>,
}

impl Default for MockIpcServer {
    fn default() -> Self {
        Self::builder()
    }
}

impl MockIpcServer {
    /// Fixture-driven defaults: the UI Kit client's whole read surface answers
    /// with the golden fixture of its schema family; write verbs answer with
    /// canned echoes. A client method without a mock binding fails the mock
    /// coverage test (`mock_contract_tests.rs`), which keeps this 1:1 with the
    /// client's `method` table.
    pub fn builder() -> Self {
        Self {
            behaviors: default_surface(),
            once_fired: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            events: Vec::new(),
            fixtures_dir: Some(PathBuf::from(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/mock"
            ))),
        }
    }

    /// Override or add one method's behavior.
    pub fn method(mut self, name: &'static str, behavior: Behavior) -> Self {
        self.behaviors.insert(name, behavior);
        self
    }

    /// A method streams this `core.event` notification before its reply.
    pub fn on_call_notify(mut self, method: &'static str, event: &'static str) -> Self {
        self.events.push(ScriptedEvent { method, event });
        self
    }

    /// Serve fixtures from a different directory (tests that script their own).
    pub fn fixtures_dir(mut self, dir: PathBuf) -> Self {
        self.fixtures_dir = Some(dir);
        self
    }

    /// Bind the socket and serve until the handle is dropped.
    pub async fn spawn(self) -> std::io::Result<SpawnedMock> {
        let dir = std::env::temp_dir().join(format!("bk-mock-{}-{}", std::process::id(), {
            use std::sync::atomic::{AtomicU64, Ordering};
            static SEQ: AtomicU64 = AtomicU64::new(0);
            SEQ.fetch_add(1, Ordering::Relaxed)
        },));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("mock.ipc");
        let listener = UnixListener::bind(&path)?;

        let behaviors = Arc::new(self.behaviors);
        let once_fired = self.once_fired;
        let events = Arc::new(self.events);
        let fixtures_dir = Arc::new(self.fixtures_dir);
        let handle = tokio::task::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (b, o, e, f) = (
                    behaviors.clone(),
                    once_fired.clone(),
                    events.clone(),
                    fixtures_dir.clone(),
                );
                tokio::spawn(async move {
                    serve_connection(stream, b, o, e, f).await;
                });
            }
        });

        Ok(SpawnedMock {
            socket_path: path.to_string_lossy().into_owned(),
            _task: handle,
            dir,
        })
    }
}

/// A running mock. Drop (or [`shutdown`][Self::shutdown]) to unbind.
pub struct SpawnedMock {
    pub socket_path: String,
    _task: tokio::task::JoinHandle<()>,
    dir: PathBuf,
}

impl SpawnedMock {
    pub async fn shutdown(self) {
        let _ = std::fs::remove_dir_all(&self.dir);
        self._task.abort();
    }
}

impl Drop for SpawnedMock {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
        self._task.abort();
    }
}

/// The fixture file for one golden family, inside the shared
/// `tests/fixtures/mock/` directory (B4's shared artifact).
pub fn family_fixture_path(family: u32) -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mock"))
        .join(format!("family_{family:02}.json"))
}

async fn serve_connection(
    stream: UnixStream,
    behaviors: Arc<BTreeMap<&'static str, Behavior>>,
    once_fired: Arc<std::sync::atomic::AtomicBool>,
    events: Arc<Vec<ScriptedEvent>>,
    fixtures_dir: Arc<Option<PathBuf>>,
) {
    use std::sync::atomic::Ordering;
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        // The method's scripted notification goes out BEFORE the reply (the
        // client's read loop skips non-matching ids).
        for ev in events.iter().filter(|ev| line.contains(ev.method)) {
            let notification = serde_json::json!({
                "jsonrpc": "2.0",
                "version": PROTOCOL_VERSION,
                "method": "core.event",
                "params": ev.event,
            });
            let _ = writer
                .write_all(format!("{notification}\n").as_bytes())
                .await;
        }
        // A `*Once` behavior misbehaves on the FIRST request the MOCK sees
        // for its method (any connection) and answers normally afterwards —
        // the reconnect scenario's second call lands on a new connection and
        // must get the healthy reply.
        let method = method_of(&line);
        let mut behavior = behaviors.get(method.as_str()).cloned();
        if let Some(b) = behavior.as_mut() {
            if matches!(b, Behavior::HangOnce | Behavior::MalformedOnce)
                && once_fired.swap(true, Ordering::SeqCst)
            {
                *b = Behavior::Value(serde_json::json!({ "recovered": true }));
            }
        }
        let reply = serve_line(&line, &behavior, &fixtures_dir);
        if reply.is_empty() {
            // A Hang consumed the line: hold the connection (no reply) until
            // the client times out and drops it.
            tokio::time::sleep(std::time::Duration::from_secs(300)).await;
            return;
        }
        let _ = writer.write_all(reply.as_bytes()).await;
        let _ = writer.write_all(b"\n").await;
        let _ = writer.flush().await;
    }
    // Client went away (e.g. a Hang timed it out): the accept loop above
    // covers the next connection.
}

/// The method name a request line carries ("" when unparsable — the parse
/// error path answers -32700 and needs no behavior).
fn method_of(line: &str) -> String {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|v| {
            v.get("method")
                .and_then(|m| m.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_default()
}

/// One request line → one reply line. An empty return means "hold the
/// connection" (the caller sleeps instead of writing).
fn serve_line(line: &str, behavior: &Option<Behavior>, fixtures_dir: &Option<PathBuf>) -> String {
    let req: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            return failure_json(
                serde_json::Value::Null,
                PARSE_ERROR,
                format!("parse error: {e}"),
                None,
            );
        }
    };
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let method = method_of(line);
    let Some(behavior) = behavior else {
        return failure_json(
            id,
            METHOD_NOT_FOUND,
            format!("unknown method: {method}"),
            None,
        );
    };
    match behavior {
        Behavior::Fixture => {
            let Some(dir) = fixtures_dir else {
                return failure_json(id, APPLICATION, "mock has no fixtures dir".into(), None);
            };
            let file = dir.join(fixture_family(&method));
            match std::fs::read_to_string(&file)
                .ok()
                .and_then(|b| serde_json::from_str::<Value>(&b).ok())
            {
                Some(result) => success_json(id, result),
                None => failure_json(id, APPLICATION, "mock fixture missing/invalid".into(), None),
            }
        }
        Behavior::Value(v) => success_json(id, v.clone()),
        Behavior::RpcError {
            code,
            message,
            core_code,
        } => failure_json(
            id,
            *code,
            message.clone(),
            core_code.map(|c| serde_json::json!({ "coreCode": c })),
        ),
        Behavior::InvalidParams(message) => failure_json(id, INVALID_PARAMS, message.clone(), None),
        Behavior::HangOnce => String::new(),
        Behavior::MalformedOnce => "{\"jsonrpc\":\"2.0\",\"id\":".to_string(),
    }
}

/// Method → fixture family binding (1:1 with the builder defaults). Names the
/// schema family whose golden bytes the file froze — `family_NN.json` files
/// are extracted verbatim from `schema_family_golden.rs`.
fn fixture_family(method: &str) -> &'static str {
    match method {
        // family 07 — BalanceResult
        "ledger.balance" => "family_07.json",
        // family 01 — the Request transport shape itself
        "engine.stats" => "family_01.json",
        // family 09 — RoundDurationResult
        "engine.round" => "family_09.json",
        // family 15 — BookLevel
        "engine.books" => "family_15.json",
        // family 23 — UnverifiedExposureReport (issue #991's additive family)
        "positions.unverified_exposure" => "family_23.json",
        // family 18 — AccountView
        "account.list" => "family_18.json",
        // family 03 — RiskLimitsResult (risk/freeze state)
        "risk.limits" => "family_03.json",
        // family 11 — MarketPluginReadout (market list/snapshot)
        "market.list" => "family_11.json",
        // family 22 — IntentAuditTailParams
        "intent.audit.tail" => "family_22.json",
        // family 14 — FeeQuoteResult
        "core.feeQuote" => "family_14.json",
        // family 20 — AccountJournalParams
        "account.journal" => "family_20.json",
        // family 21 — Event (the notification stream)
        "engine.markets" => "family_21.json",
        _ => "family_07.json",
    }
}

fn success_json(id: Value, result: Value) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "version": PROTOCOL_VERSION,
        "id": id,
        "result": result,
    })
    .to_string()
}

fn failure_json(id: Value, code: i64, message: String, data: Option<Value>) -> String {
    let mut err = serde_json::json!({ "code": code, "message": message });
    if let Some(d) = data {
        err["data"] = d;
    }
    serde_json::json!({
        "jsonrpc": "2.0",
        "version": PROTOCOL_VERSION,
        "id": id,
        "error": err,
    })
    .to_string()
}

/// The default method surface: read verbs → family fixtures, write verbs →
/// canned echoes. B3's coverage floor (account list, market snapshot, order
/// lifecycle, risk/freeze state, reconnect) sits inside these plus the tests.
fn default_surface() -> BTreeMap<&'static str, Behavior> {
    use Behavior::*;
    let mut b: BTreeMap<&'static str, Behavior> = BTreeMap::new();
    // ── Read verbs → family fixtures (B1: golden families on the mock) ─────
    b.insert("core.ready", Fixture); // transport shape (family 01 discipline)
    b.insert("ledger.balance", Fixture); // family 07
    b.insert("engine.round", Fixture); // family 09
    b.insert("engine.books", Fixture); // family 15
    b.insert("engine.stats", Fixture); // family 01
    b.insert("engine.markets", Fixture); // family 21 Event stream shape
    b.insert("positions.list", Fixture); // family 23 (#991 exposure family)
    b.insert("positions.unverified_exposure", Fixture); // family 23 (B1: family 23 twice)
    b.insert("orders.list", Fixture); // order lifecycle read side
    b.insert("account.list", Fixture); // family 18 (B3: account list)
    b.insert("risk.limits", Fixture); // family 03 (B3: risk/freeze state)
    b.insert("market.list", Fixture); // family 11 (B3: market snapshot)
    b.insert("intent.audit.tail", Fixture); // family 22
    b.insert("core.feeQuote", Fixture); // family 14
    b.insert("account.journal", Fixture); // family 20
    b.insert("trades.summary", Fixture); // family 07 shape (aggregates)
    b.insert("trades.history", Fixture); // family 07 shape (rows)
                                         // ── Write verbs → canned echoes (B3: order lifecycle) ──────────────────
    b.insert(
        "orders.place",
        Value(serde_json::json!({
            "orderId": "ord-mock-1",
            "status": "LIVE",
            "version": PROTOCOL_VERSION,
        })),
    );
    b.insert(
        "orders.cancel",
        Value(serde_json::json!({ "orderId": "ord-mock-1", "cancelled": true })),
    );
    b.insert(
        "positions.exit",
        Value(serde_json::json!({ "accepted": true })),
    );
    b.insert(
        "positions.force_close",
        Value(serde_json::json!({ "accepted": true })),
    );
    b.insert(
        "orders.force_close",
        Value(serde_json::json!({ "accepted": true })),
    );
    b.insert(
        "account.switch",
        Value(serde_json::json!({ "switched": "acc-1" })),
    );
    b.insert(
        "display.account.set",
        Value(serde_json::json!({ "displayAccount": "acc-1" })),
    );
    b.insert(
        "account.freeze",
        Value(serde_json::json!({ "frozen": true, "reason": "manual" })),
    );
    b.insert(
        "strategy.enable",
        Value(serde_json::json!({ "enabled": true })),
    );
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_envelope_matches_the_kernel_shape() {
        let f = failure_json(
            serde_json::json!(7),
            INVALID_PARAMS,
            "nope".to_string(),
            Some(serde_json::json!({ "coreCode": "InvalidParams" })),
        );
        let v: serde_json::Value = serde_json::from_str(&f).unwrap();
        assert_eq!(v["error"]["code"], -32602);
        assert_eq!(v["error"]["data"]["coreCode"], "InvalidParams");
        assert_eq!(v["id"], 7);
        assert_eq!(v["version"], PROTOCOL_VERSION);
    }

    #[test]
    fn success_envelope_carries_result_and_version() {
        let s = success_json(serde_json::json!(1), serde_json::json!({"ok": true}));
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["result"]["ok"], true);
        assert_eq!(v["version"], PROTOCOL_VERSION);
    }

    /// The B1 floor: every family fixture file parses, and family 23 (the
    /// #991 additive family) carries the four settlement fields the panel's
    /// banner reads.
    #[test]
    fn family_fixtures_parse_and_family_23_carries_the_991_surface() {
        for n in 1..=23u32 {
            let file = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mock"))
                .join(format!("family_{n:02}.json"));
            let bytes =
                std::fs::read_to_string(&file).unwrap_or_else(|e| panic!("fixture {file:?}: {e}"));
            serde_json::from_str::<serde_json::Value>(&bytes)
                .unwrap_or_else(|e| panic!("fixture {file:?} invalid: {e}"));
        }
        let f23: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(PathBuf::from(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/mock/family_23.json"
            )))
            .unwrap(),
        )
        .unwrap();
        let entry = &f23["entries"][0];
        assert_eq!(entry["source"], "reconciled");
        assert!(entry.get("fallbackPrice").is_some());
        assert!(entry.get("netPnlUsd").is_some());
    }
}

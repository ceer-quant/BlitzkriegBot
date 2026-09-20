//! Blocking Unix-domain-socket JSON-RPC 2.0 client for the Rust core.
//!
//! Deliberately dependency-free (`std::os::unix::net::UnixStream`) so the UI Kit
//! stays trivially portable across the web/TUI/app adapters. One request at a
//! time is enough for a render loop; notifications (`core.event`) are ignored by
//! `call` so a stray push never desyncs a request/response pair.

use crate::core::types::*;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

#[derive(Debug)]
pub enum IpcError {
    Io(std::io::Error),
    Protocol(String),
    Rpc { code: i64, message: String },
    Timeout,
}

impl std::fmt::Display for IpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IpcError::Io(e) => write!(f, "io: {e}"),
            IpcError::Protocol(s) => write!(f, "protocol: {s}"),
            IpcError::Rpc { code, message } => write!(f, "rpc {code}: {message}"),
            IpcError::Timeout => write!(f, "request timed out"),
        }
    }
}
impl std::error::Error for IpcError {}
impl From<std::io::Error> for IpcError {
    fn from(e: std::io::Error) -> Self {
        IpcError::Io(e)
    }
}

pub struct IpcClient {
    socket_path: String,
    stream: Option<UnixStream>,
    reader: Option<BufReader<UnixStream>>,
    next_id: u64,
    timeout: Duration,
}

impl IpcClient {
    pub fn new(socket_path: impl Into<String>) -> Self {
        Self {
            socket_path: socket_path.into(),
            stream: None,
            reader: None,
            next_id: 1,
            timeout: Duration::from_secs(3),
        }
    }

    pub fn with_timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }

    pub fn socket_path(&self) -> &str {
        &self.socket_path
    }

    pub fn is_connected(&self) -> bool {
        self.stream.is_some()
    }

    /// Open the connection (idempotent).
    pub fn connect(&mut self) -> Result<(), IpcError> {
        if self.stream.is_some() {
            return Ok(());
        }
        let s = UnixStream::connect(&self.socket_path)?;
        s.set_read_timeout(Some(self.timeout))?;
        s.set_write_timeout(Some(self.timeout))?;
        let r = BufReader::new(s.try_clone()?);
        self.stream = Some(s);
        self.reader = Some(r);
        Ok(())
    }

    fn disconnect(&mut self) {
        self.stream = None;
        self.reader = None;
    }

    /// Send one request and read until its matching response id arrives,
    /// skipping any interleaved notifications.
    pub fn call(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, IpcError> {
        if self.stream.is_none() {
            self.connect()?;
        }
        let id = self.next_id;
        self.next_id += 1;
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "version": "1.1",
            "id": id,
            "method": method,
            "params": params,
        });
        let mut line =
            serde_json::to_string(&req).map_err(|e| IpcError::Protocol(e.to_string()))?;
        line.push('\n');

        {
            let stream = self.stream.as_mut().ok_or(IpcError::Timeout)?;
            if let Err(e) = stream.write_all(line.as_bytes()) {
                self.disconnect();
                return Err(IpcError::Io(e));
            }
            let _ = stream.flush();
        }

        for _ in 0..64 {
            let mut buf = String::new();
            let n = {
                let reader = self.reader.as_mut().ok_or(IpcError::Timeout)?;
                match reader.read_line(&mut buf) {
                    Ok(n) => n,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut =>
                    {
                        self.disconnect();
                        return Err(IpcError::Timeout);
                    }
                    Err(e) => {
                        self.disconnect();
                        return Err(IpcError::Io(e));
                    }
                }
            };
            if n == 0 {
                self.disconnect();
                return Err(IpcError::Protocol("connection closed".into()));
            }
            let v: serde_json::Value = match serde_json::from_str(buf.trim()) {
                Ok(v) => v,
                Err(_) => continue,
            };
            // Response ids are echoed; a notification has no id or a method.
            let is_response = v
                .get("id")
                .map(|i| i == &serde_json::json!(id))
                .unwrap_or(false);
            if !is_response {
                continue;
            }
            if let Some(err) = v.get("error") {
                let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
                let message = err
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("error")
                    .to_string();
                return Err(IpcError::Rpc { code, message });
            }
            return Ok(v.get("result").cloned().unwrap_or(serde_json::Value::Null));
        }
        Err(IpcError::Protocol(
            "too many interleaved notifications".into(),
        ))
    }

    // ── Typed convenience wrappers (all read-only; the UI issues no orders) ──

    pub fn ready(&mut self) -> Result<ReadyView, IpcError> {
        serde_json::from_value(self.call("core.ready", serde_json::json!({}))?)
            .map_err(|e| IpcError::Protocol(e.to_string()))
    }
    pub fn balance(&mut self) -> Result<BalanceView, IpcError> {
        serde_json::from_value(self.call("ledger.balance", serde_json::json!({}))?)
            .map_err(|e| IpcError::Protocol(e.to_string()))
    }
    pub fn round(&mut self) -> Result<RoundView, IpcError> {
        serde_json::from_value(self.call("engine.round", serde_json::json!({}))?)
            .map_err(|e| IpcError::Protocol(e.to_string()))
    }

    /// E8-c 盘口深度: per-asset L2 depth (`engine.books`). Older cores refuse
    /// the unknown method — callers must degrade to an empty view, never fail
    /// the whole snapshot.
    pub fn books(&mut self) -> Result<Vec<AssetBooksView>, IpcError> {
        serde_json::from_value(self.call("engine.books", serde_json::json!({}))?)
            .map_err(|e| IpcError::Protocol(e.to_string()))
    }
    pub fn stats(&mut self) -> Result<EngineStatsView, IpcError> {
        serde_json::from_value(self.call("engine.stats", serde_json::json!({}))?)
            .map_err(|e| IpcError::Protocol(e.to_string()))
    }
    pub fn positions(&mut self) -> Result<PositionsView, IpcError> {
        serde_json::from_value(self.call("positions.list", serde_json::json!({}))?)
            .map_err(|e| IpcError::Protocol(e.to_string()))
    }
    pub fn orders(&mut self) -> Result<OrdersView, IpcError> {
        serde_json::from_value(self.call("orders.list", serde_json::json!({}))?)
            .map_err(|e| IpcError::Protocol(e.to_string()))
    }
    pub fn trades(&mut self, limit: usize) -> Result<TradesView, IpcError> {
        serde_json::from_value(self.call("trades.history", serde_json::json!({ "limit": limit }))?)
            .map_err(|e| IpcError::Protocol(e.to_string()))
    }

    /// All-time closed-trade totals from the persisted summary (trades.summary).
    pub fn trade_summary(&mut self) -> Result<serde_json::Value, IpcError> {
        let v = self.call("trades.summary", serde_json::json!({}))?;
        Ok(v.get("summary").cloned().unwrap_or(serde_json::Value::Null))
    }

    pub fn strategies(&mut self) -> Result<StrategyListView, IpcError> {
        serde_json::from_value(self.call("strategy.list", serde_json::json!({}))?)
            .map_err(|e| IpcError::Protocol(e.to_string()))
    }
    pub fn extensions(&mut self) -> Result<ExtensionListView, IpcError> {
        serde_json::from_value(self.call("extension.list", serde_json::json!({}))?)
            .map_err(|e| IpcError::Protocol(e.to_string()))
    }
    pub fn market_plugins(&mut self) -> Result<MarketListView, IpcError> {
        serde_json::from_value(self.call("market.list", serde_json::json!({}))?)
            .map_err(|e| IpcError::Protocol(e.to_string()))
    }

    // ── Shadow Evolution (E13): the proposal workflow ────────────────────────

    /// Full shadow-evolution status block (`shadow_evolution.status`). Older
    /// cores answer too — the E13 keys just stay absent, so the view
    /// deserialises with defaults.
    pub fn evolution_status(&mut self) -> Result<EvolutionStatusView, IpcError> {
        serde_json::from_value(self.call("shadow_evolution.status", serde_json::json!({}))?)
            .map_err(|e| IpcError::Protocol(e.to_string()))
    }

    /// Every known proposal's latest state, newest first.
    pub fn evolution_proposals(&mut self, limit: usize) -> Result<Vec<EvolutionProposalView>, IpcError> {
        let v = self.call(
            "shadow_evolution.proposals",
            serde_json::json!({ "limit": limit }),
        )?;
        let view: EvolutionProposalsView =
            serde_json::from_value(v).map_err(|e| IpcError::Protocol(e.to_string()))?;
        Ok(view.proposals)
    }

    /// The operator's verdict on one proposal (`accept` / `reject` / `defer`).
    pub fn evolution_decide(
        &mut self,
        id: &str,
        decision: &str,
    ) -> Result<serde_json::Value, IpcError> {
        self.call(
            "shadow_evolution.decide",
            serde_json::json!({ "id": id, "decision": decision }),
        )
    }

    /// The auto-evolve checkbox (persisted across restarts).
    pub fn evolution_set_auto(&mut self, on: bool) -> Result<serde_json::Value, IpcError> {
        self.call(
            "shadow_evolution.set_auto",
            serde_json::json!({ "enabled": on }),
        )
    }

    /// Roll ONE strategy back to the parameters in force before its last change.
    pub fn evolution_rollback(&mut self, strategy: &str) -> Result<serde_json::Value, IpcError> {
        self.call(
            "shadow_evolution.rollback",
            serde_json::json!({ "strategy": strategy }),
        )
    }

    pub fn strategy_enable(
        &mut self,
        name: &str,
        enabled: bool,
    ) -> Result<serde_json::Value, IpcError> {
        self.call(
            "strategy.enable",
            serde_json::json!({ "name": name, "enabled": enabled }),
        )
    }
    pub fn position_exit(&mut self, position_id: &str) -> Result<serde_json::Value, IpcError> {
        self.call(
            "positions.exit",
            serde_json::json!({ "positionId": position_id }),
        )
    }

    pub fn extension_enable(
        &mut self,
        name: &str,
        enabled: bool,
    ) -> Result<serde_json::Value, IpcError> {
        self.call(
            if enabled {
                "extension.enable"
            } else {
                "extension.disable"
            },
            serde_json::json!({ "name": name }),
        )
    }

    /// Assemble one full UI snapshot. Never errors on a single missing call —
    /// the panel should still render whatever the core answered.
    ///
    /// `connected` means "a core ANSWERED", not "we have a socket object". The
    /// distinction is load-bearing for crash reporting (E12-c): `connect()`
    /// short-circuits when a handle already exists, so after the core is killed
    /// the cached handle is still `Some` and a snapshot built on it would report
    /// a dead core as up — the panel would show a healthy engine for the poll
    /// after a crash, which is the exact failure the acceptance line is about.
    /// So the first call is what proves liveness, and `core.ready` is the one
    /// that says nothing is there.
    pub fn snapshot(&mut self, trade_limit: usize) -> UiSnapshot {
        let mut s = UiSnapshot::default();
        if let Err(e) = self.connect() {
            s.last_error = Some(e.to_string());
            return s;
        }
        match self.ready() {
            Ok(v) => {
                s.ready = Some(v);
                s.connected = true;
            }
            Err(e) => {
                // The handle outlived the core it points at. Report unreachable
                // and stop here: every later call would fail the same way, and
                // empty arrays on a `connected: true` snapshot are what a panel
                // draws as "the engine is running with no positions".
                s.last_error = Some(e.to_string());
                return s;
            }
        }
        s.balance = self.balance().ok();
        s.round = self.round().ok();
        s.stats = self.stats().ok();
        s.positions = self.positions().map(|p| p.positions).unwrap_or_default();
        s.orders = self.orders().map(|o| o.orders).unwrap_or_default();
        s.trades = self
            .trades(trade_limit)
            .map(|t| t.trades)
            .unwrap_or_default();
        // All-time closed-trade totals (persisted summary; older cores omit).
        s.trade_summary = self.trade_summary().ok();
        s.strategies = self.strategies().map(|r| r.strategies).unwrap_or_default();
        s.extensions = self.extensions().map(|r| r.extensions).unwrap_or_default();
        // E9-g: per-strategy accounting travels with every snapshot so the
        // WebUI plugins page renders counters without a second round-trip.
        s.strategy_stats = s
            .stats
            .as_ref()
            .map(|st| st.strategies.clone())
            .unwrap_or_default();
        // E8-c 盘口深度: per-asset L2 depth. Older cores refuse the method and
        // the snapshot keeps its shape — an empty vec means "no depth data",
        // never "the core said there is no book".
        s.books = self.books().unwrap_or_default();
        if let Ok(m) = self.market_plugins() {
            s.market_plugins = m.plugins;
            s.market_active = m.active.is_some();
            s.market_active_name = m.active;
        }
        // E13: the proposal workflow. Older cores refuse the methods and the
        // snapshot keeps its shape — empty pending list means "nothing held",
        // never an error.
        s.evolution_proposals = self.evolution_proposals(50).unwrap_or_default();
        s.evolution_status = self.evolution_status().ok();
        s
    }
}

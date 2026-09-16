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
    pub fn snapshot(&mut self, trade_limit: usize) -> UiSnapshot {
        let mut s = UiSnapshot::default();
        if let Err(e) = self.connect() {
            s.last_error = Some(e.to_string());
            return s;
        }
        s.connected = true;
        match self.ready() {
            Ok(v) => s.ready = Some(v),
            Err(e) => s.last_error = Some(e.to_string()),
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
        s.strategies = self.strategies().map(|r| r.strategies).unwrap_or_default();
        s.extensions = self.extensions().map(|r| r.extensions).unwrap_or_default();
        // E9-g: per-strategy accounting travels with every snapshot so the
        // WebUI plugins page renders counters without a second round-trip.
        s.strategy_stats = s.stats.as_ref().map(|st| st.strategies.clone()).unwrap_or_default();
        if let Ok(m) = self.market_plugins() {
            s.market_plugins = m.plugins;
            s.market_active = m.active.is_some();
            s.market_active_name = m.active;
        }
        s
    }
}

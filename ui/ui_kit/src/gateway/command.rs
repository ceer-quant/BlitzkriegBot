//! Command dispatch — the UI Kit's replacement for the Node `/crypto-hft`
//! command surface (`src/skills/bundled/crypto-hft/index.ts`).
//!
//! Supported verbs (identical in meaning to the Node skill's Rust-core path):
//!
//!   start [ASSETS] [--size N] [--dry-run]   spawn/adopt the core, start trading
//!   stop                                     stop the core we spawned
//!   status                                   round + stats + positions + balance
//!   positions [N]                            closed-trade history (newest first)
//!   strategies                               list strategies (name + enabled)
//!   strategy NAME on|off                     enable/disable one strategy
//!   extensions                               list extensions (name + state)
//!   extension NAME on|off                    enable/disable one extension
//!   markets                                  list market plugins
//!   help
//!
//! NO order-placing verb exists here. `stop`/`start` act on the *process*, not on
//! orders; `status`/`positions`/`strategies`/`extensions`/`markets` are reads.
//! Only `strategy NAME on|off` / `extension NAME on|off` mutate core runtime
//! state, and the interactive TUI asks for confirmation before sending them
//! when a target is currently ENABLED. All trading decisions stay in the core.

use crate::core::ipc_client::IpcClient;
use crate::core::types::UiSnapshot;
use crate::gateway::supervisor::{StartOutcome, StopOutcome, Supervisor, SupervisorConfig};
use serde::Serialize;

/// A parsed command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Start {
        assets: Vec<String>,
        size_usd: Option<String>,
        dry_run: bool,
    },
    Stop,
    Status,
    Positions {
        limit: usize,
    },
    Strategies,
    StrategySet {
        name: String,
        enabled: bool,
    },
    Extensions,
    ExtensionSet {
        name: String,
        enabled: bool,
    },
    Markets,
    Help,
}

/// Parse one command line. Unknown verbs yield `Err(message)` (never a panic).
pub fn parse_command(input: &str) -> Result<Command, String> {
    let parts: Vec<&str> = input.split_whitespace().collect();
    let Some(verb) = parts.first().map(|s| s.to_ascii_lowercase()) else {
        return Err("empty command".into());
    };
    match verb.as_str() {
        "start" => {
            let mut assets: Vec<String> = Vec::new();
            let mut size_usd = None;
            let mut dry_run = std::env::var("DRY_RUN")
                .map(|v| v != "false")
                .unwrap_or(true);
            let mut i = 1;
            while i < parts.len() {
                let p = parts[i];
                if p == "--dry-run" || p == "--dry" {
                    dry_run = true;
                } else if p == "--live" {
                    dry_run = false;
                } else if p == "--size" {
                    i += 1;
                    size_usd = parts.get(i).map(|s| s.to_string());
                } else if let Some(rest) = p.strip_prefix("--size=") {
                    size_usd = Some(rest.to_string());
                } else if p.starts_with('-') {
                    return Err(format!("unknown flag for start: {p}"));
                } else if assets.is_empty() {
                    assets = p
                        .split(',')
                        .map(|a| a.trim().to_uppercase())
                        .filter(|a| !a.is_empty())
                        .collect();
                }
                i += 1;
            }
            Ok(Command::Start {
                assets,
                size_usd,
                dry_run,
            })
        }
        "stop" => Ok(Command::Stop),
        "status" => Ok(Command::Status),
        "positions" => {
            let limit = parts
                .get(1)
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(0);
            Ok(Command::Positions { limit })
        }
        "strategies" => Ok(Command::Strategies),
        "strategy" => {
            let name = parts.get(1).copied().unwrap_or("").to_string();
            let on = parts.get(2).copied();
            if name.is_empty() || parts.len() != 3 || !matches!(on, Some("on") | Some("off")) {
                return Err("usage: strategy <name> on|off".into());
            }
            Ok(Command::StrategySet {
                name,
                enabled: on == Some("on"),
            })
        }
        "extensions" => Ok(Command::Extensions),
        "extension" => {
            let name = parts.get(1).copied().unwrap_or("").to_string();
            let on = parts.get(2).copied();
            if name.is_empty() || parts.len() != 3 || !matches!(on, Some("on") | Some("off")) {
                return Err("usage: extension <name> on|off".into());
            }
            Ok(Command::ExtensionSet {
                name,
                enabled: on == Some("on"),
            })
        }
        "markets" => Ok(Command::Markets),
        "help" | "?" => Ok(Command::Help),
        other => Err(format!("unknown command: {other}")),
    }
}

/// Structured result of a dispatched command (JSON-friendly for the gateway API).
#[derive(Debug, Clone, Serialize)]
pub struct CommandOutcome {
    pub ok: bool,
    pub command: String,
    /// started | adopted | stopped | not_owned | status | positions | help | error
    pub action: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl CommandOutcome {
    fn ok(command: &str, action: &str, message: impl Into<String>) -> Self {
        Self {
            ok: true,
            command: command.into(),
            action: action.into(),
            message: message.into(),
            data: None,
        }
    }
    fn with_data(mut self, data: serde_json::Value) -> Self {
        self.data = Some(data);
        self
    }
    fn err(command: &str, message: impl Into<String>) -> Self {
        Self {
            ok: false,
            command: command.into(),
            action: "error".into(),
            message: message.into(),
            data: None,
        }
    }
}

/// Holds the supervisor and an IPC client, and turns commands into effects.
pub struct Dispatcher {
    sup: Supervisor,
    client: IpcClient,
    /// When false the lifecycle verbs (start/stop) are refused; reads still work.
    lifecycle_enabled: bool,
}

impl Dispatcher {
    pub fn new(cfg: SupervisorConfig, lifecycle_enabled: bool) -> Self {
        let client = IpcClient::new(cfg.socket_path.clone());
        Self {
            sup: Supervisor::new(cfg),
            client,
            lifecycle_enabled,
        }
    }

    pub fn lifecycle_enabled(&self) -> bool {
        self.lifecycle_enabled
    }

    pub fn socket_path(&self) -> &str {
        self.client.socket_path()
    }

    /// Run one raw command line.
    pub fn dispatch_line(&mut self, input: &str) -> CommandOutcome {
        match parse_command(input) {
            Ok(cmd) => self.dispatch(cmd, input),
            Err(e) => CommandOutcome::err(input.trim(), e),
        }
    }

    /// Assemble a full read-only snapshot through this dispatcher's client.
    /// Exposed for the interactive panel; the web adapter uses its own client.
    pub fn snapshot(&mut self) -> UiSnapshot {
        self.client.snapshot(0)
    }

    /// Raw registry reads for the plugin-manager tab: strategies / extensions /
    /// market plugins in one round-trip each, tolerant of a missing core.
    pub fn plugins_snapshot(&mut self) -> UiSnapshot {
        let mut s = UiSnapshot::default();
        if self.client.connect().is_err() {
            s.last_error = Some(format!(
                "core not reachable on {}",
                self.client.socket_path()
            ));
            return s;
        }
        s.connected = true;
        s.strategies = self
            .client
            .strategies()
            .map(|r| r.strategies)
            .unwrap_or_default();
        s.extensions = self
            .client
            .extensions()
            .map(|r| r.extensions)
            .unwrap_or_default();
        if let Ok(m) = self.client.market_plugins() {
            s.market_plugins = m.plugins;
            s.market_active = m.active.is_some();
        }
        s
    }

    /// Enable/disable one strategy. Returns the raw RPC payload (`found` says
    /// whether the name is registered).
    pub fn set_strategy(&mut self, name: &str, enabled: bool) -> Result<serde_json::Value, String> {
        self.client
            .strategy_enable(name, enabled)
            .map_err(|e| e.to_string())
    }

    /// Enable/disable one extension.
    pub fn set_extension(
        &mut self,
        name: &str,
        enabled: bool,
    ) -> Result<serde_json::Value, String> {
        self.client
            .extension_enable(name, enabled)
            .map_err(|e| e.to_string())
    }

    /// True when this dispatcher spawned (and owns) the running core.
    pub fn managed(&self) -> bool {
        self.sup.owns()
    }

    /// PID of the core this dispatcher spawned, if any.
    pub fn pid(&self) -> Option<u32> {
        self.sup.pid()
    }

    pub fn dispatch(&mut self, cmd: Command, raw: &str) -> CommandOutcome {
        let raw = raw.trim();
        match cmd {
            Command::Start {
                assets,
                size_usd,
                dry_run,
            } => self.cmd_start(raw, assets, size_usd, dry_run),
            Command::Stop => self.cmd_stop(raw),
            Command::Status => self.cmd_status(raw),
            Command::Positions { limit } => self.cmd_positions(raw, limit),
            Command::Strategies => self.cmd_plugin_list(raw, PluginKind::Strategy),
            Command::Extensions => self.cmd_plugin_list(raw, PluginKind::Extension),
            Command::Markets => self.cmd_plugin_list(raw, PluginKind::Market),
            Command::StrategySet { name, enabled } => {
                self.cmd_plugin_set(raw, PluginKind::Strategy, &name, enabled)
            }
            Command::ExtensionSet { name, enabled } => {
                self.cmd_plugin_set(raw, PluginKind::Extension, &name, enabled)
            }
            Command::Help => CommandOutcome::ok(raw, "help", HELP)
                .with_data(serde_json::json!({ "usage": HELP })),
        }
    }

    fn cmd_start(
        &mut self,
        raw: &str,
        assets: Vec<String>,
        size_usd: Option<String>,
        dry_run: bool,
    ) -> CommandOutcome {
        if !self.lifecycle_enabled {
            return CommandOutcome::err(
                raw,
                "lifecycle control disabled; start the gateway with --manage to enable start/stop",
            );
        }
        // A core is already up → adopt it. Never re-spawn over a live core, and
        // never replace the supervisor (that would drop — and kill — an owned
        // core). Per-invocation parameters only apply to a fresh start.
        if self.sup.is_running() {
            return CommandOutcome::ok(
                raw,
                "adopted",
                "a core is already serving this socket; adopted it (not owned — stop will not kill it)",
            );
        }
        // Idle: (re)configure then spawn.
        let mut cfg = self.sup.config().clone();
        if !assets.is_empty() {
            cfg.assets = assets;
        }
        cfg.mode = if dry_run { "dry".into() } else { "live".into() };
        if let Some(s) = size_usd {
            // Keep the safety notional cap >= the requested size.
            let max_shares = cfg.max_shares as f64;
            cfg.max_order_notional = format!(
                "{:.2}",
                s.parse::<f64>().unwrap_or(0.0).max(max_shares * 0.6)
            );
        }
        self.sup.set_config(cfg.clone());

        match self.sup.start() {
            Ok(StartOutcome::Started { pid }) => CommandOutcome::ok(
                raw,
                "started",
                format!(
                    "core started (pid {pid}) mode={} assets={}",
                    cfg.mode,
                    cfg.assets.join(",")
                ),
            )
            .with_data(serde_json::json!({
                "pid": pid, "mode": cfg.mode, "assets": cfg.assets,
                "roundSec": cfg.round_sec, "shares": [cfg.min_shares, cfg.max_shares],
            })),
            Ok(StartOutcome::Adopted) => CommandOutcome::ok(
                raw,
                "adopted",
                "a core is already serving this socket; adopted it (not owned — stop will not kill it)",
            ),
            Err(e) => CommandOutcome::err(raw, e.to_string()),
        }
    }

    fn cmd_stop(&mut self, raw: &str) -> CommandOutcome {
        if !self.lifecycle_enabled {
            return CommandOutcome::err(
                raw,
                "lifecycle control disabled; start the gateway with --manage to enable start/stop",
            );
        }
        match self.sup.stop() {
            StopOutcome::Stopped { pid } => {
                CommandOutcome::ok(raw, "stopped", format!("core stopped (pid {pid})"))
            }
            StopOutcome::NotOwned => CommandOutcome::ok(
                raw,
                "not_owned",
                "no core spawned by this gateway; an adopted core is left running",
            ),
        }
    }

    fn cmd_status(&mut self, raw: &str) -> CommandOutcome {
        let snap = self.snapshot();
        if !snap.connected {
            return CommandOutcome::err(
                raw,
                format!(
                    "core not reachable on {}: {}",
                    self.client.socket_path(),
                    snap.last_error.unwrap_or_default()
                ),
            );
        }
        let msg = format_status(&snap);
        CommandOutcome::ok(raw, "status", msg).with_data(status_json(self, &snap))
    }

    fn cmd_positions(&mut self, raw: &str, limit: usize) -> CommandOutcome {
        let snap = self.snapshot();
        if !snap.connected {
            return CommandOutcome::err(raw, "core not reachable");
        }
        let n = if limit == 0 {
            snap.trades.len()
        } else {
            limit.min(snap.trades.len())
        };
        let msg = format_positions(&snap, n);
        CommandOutcome::ok(raw, "positions", msg)
            .with_data(serde_json::json!({ "count": n, "total": snap.trades.len() }))
    }

    fn cmd_plugin_list(&mut self, raw: &str, kind: PluginKind) -> CommandOutcome {
        let snap = self.plugins_snapshot();
        if !snap.connected {
            return CommandOutcome::err(
                raw,
                snap.last_error
                    .unwrap_or_else(|| "core not reachable".into()),
            );
        }
        match kind {
            PluginKind::Strategy => {
                let mut msg = format!("Strategies ({}):\n", snap.strategies.len());
                for r in &snap.strategies {
                    msg.push_str(&format!("  {} {}\n", state_mark(r.enabled), r.name));
                }
                CommandOutcome::ok(raw, "strategies", msg.trim_end().to_string())
                    .with_data(serde_json::json!({ "strategies": snap.strategies }))
            }
            PluginKind::Extension => {
                let mut msg = format!("Extensions ({}):\n", snap.extensions.len());
                for r in &snap.extensions {
                    msg.push_str(&format!("  [{:>10}] {}\n", r.state, r.name));
                }
                CommandOutcome::ok(raw, "extensions", msg.trim_end().to_string())
                    .with_data(serde_json::json!({ "extensions": snap.extensions }))
            }
            PluginKind::Market => {
                let mut msg = format!(
                    "Market plugins (active={}) ({}):\n",
                    snap.market_active,
                    snap.market_plugins.len()
                );
                for r in &snap.market_plugins {
                    let caps = [
                        r.has_data_feed.then(|| "feed"),
                        r.has_discovery.then(|| "discovery"),
                        r.has_executor.then(|| "executor"),
                    ]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join("+");
                    msg.push_str(&format!(
                        "  {} {} [{:>9}] ({}{})\n",
                        state_mark(r.active),
                        r.name,
                        r.kind,
                        caps,
                        if r.enabled { "" } else { " · disabled" }
                    ));
                }
                CommandOutcome::ok(raw, "markets", msg.trim_end().to_string())
                    .with_data(serde_json::json!({ "active": snap.market_active, "plugins": snap.market_plugins }))
            }
        }
    }

    fn cmd_plugin_set(
        &mut self,
        raw: &str,
        kind: PluginKind,
        name: &str,
        enabled: bool,
    ) -> CommandOutcome {
        let result = match kind {
            PluginKind::Strategy => self.set_strategy(name, enabled),
            PluginKind::Extension => self.set_extension(name, enabled),
            PluginKind::Market => {
                return CommandOutcome::err(
                    raw,
                    "market plugins cannot be toggled via UI; edit the core's market config",
                )
            }
        };
        match result {
            Ok(data) => CommandOutcome::ok(
                raw,
                if kind == PluginKind::Strategy {
                    "strategy_set"
                } else {
                    "extension_set"
                },
                format!(
                    "{} {} -> {}",
                    kind.label(),
                    name,
                    if enabled { "on" } else { "off" }
                ),
            )
            .with_data(data),
            Err(e) => CommandOutcome::err(raw, e),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PluginKind {
    Strategy,
    Extension,
    Market,
}

impl PluginKind {
    fn label(self) -> &'static str {
        match self {
            PluginKind::Strategy => "strategy",
            PluginKind::Extension => "extension",
            PluginKind::Market => "market",
        }
    }
}

fn state_mark(on: bool) -> &'static str {
    if on {
        "[on ]"
    } else {
        "[off]"
    }
}

pub const HELP: &str = "\
crypto-hft commands (UI Kit gateway):
  start [ASSETS] [--size N] [--dry-run]   start the core (adopts if already running)
  stop                                     stop the core this gateway spawned
  status                                   round / stats / positions / balance
  positions [N]                            closed-trade history (newest first)
  strategies                               list strategies and enabled state
  strategy <name> on|off                   enable/disable one strategy
  extensions                               list extensions and state
  extension <name> on|off                  enable/disable one extension
  markets                                  list market plugins
  help";

fn status_json(disp: &Dispatcher, s: &UiSnapshot) -> serde_json::Value {
    let round = s.round.as_ref();
    serde_json::json!({
        "connection": {
            "connected": s.connected,
            "socket": disp.socket_path(),
            "managed": disp.sup.owns(),
            "pid": disp.sup.pid(),
            "lifecycleEnabled": disp.lifecycle_enabled,
        },
        "mode": s.mode(),
        "round": round.map(|r| serde_json::json!({
            "slot": r.slot, "ageSec": r.age_sec, "timeLeftSec": r.time_left_sec,
            "markets": r.markets, "canTrade": r.can_trade,
        })),
        "balance": s.balance.as_ref().map(|b| serde_json::json!({
            "balance": b.balance, "reserved": b.reserved, "available": b.available })),
        "stats": s.stats.as_ref().map(|st| serde_json::json!({
            "books": st.books, "spots": st.spots, "signals": st.signals,
            "confirmed": st.confirmed.len(), "placeRejected": st.place_rejected,
            "blockedTiming": st.blocked.timing, "blockedMomentum": st.blocked.momentum })),
        "positions": s.positions.iter().map(|p| serde_json::json!({
            "asset": p.asset, "direction": p.direction, "entryPrice": p.entry_price,
            "currentPrice": p.current_price, "unrealizedPct": p.unrealized_pct,
            "strategy": p.strategy, "remainingSec": p.remaining_sec })).collect::<Vec<_>>(),
        "pnl": { "net": s.net_pnl(), "trades": s.trades.len(), "winRate": s.win_rate() },
    })
}

fn format_status(s: &UiSnapshot) -> String {
    let mut out = String::new();
    match &s.round {
        Some(r) => out.push_str(&format!(
            "Round #{} | {}s old | {}s left | {}\n",
            r.slot,
            r.age_sec,
            r.time_left_sec,
            if r.can_trade { "TRADING" } else { "WAITING" }
        )),
        None => out.push_str("Round: unavailable\n"),
    }
    if let Some(st) = &s.stats {
        out.push_str(&format!(
            "Feed: books={} spots={} signals={} confirmed={} rejected={}\n",
            st.books,
            st.spots,
            st.signals,
            st.confirmed.len(),
            st.place_rejected
        ));
    }
    if let Some(b) = &s.balance {
        out.push_str(&format!(
            "Balance: ${:.2} (reserved ${:.2}, avail ${:.2})\n",
            b.balance, b.reserved, b.available
        ));
    }
    out.push_str(&format!(
        "Trades: {} net {} ({}% WR)\n",
        s.trades.len(),
        fmt_usd(s.net_pnl()),
        s.win_rate().round()
    ));
    out.push_str(&format!("Open positions: {}", s.positions.len()));
    if !s.positions.is_empty() {
        out.push('\n');
        for p in &s.positions {
            out.push_str(&format!(
                "  {} {} @ {:.2} -> {:.2} ({:+.1}%) [{}] {}s left\n",
                p.asset,
                p.direction.to_uppercase(),
                p.entry_price,
                p.current_price,
                p.unrealized_pct,
                p.strategy,
                p.remaining_sec
            ));
        }
    }
    if let Some(e) = &s.last_error {
        out.push_str(&format!("\nlast error: {e}"));
    }
    out
}

fn format_positions(s: &UiSnapshot, limit: usize) -> String {
    if s.trades.is_empty() {
        return "No closed trades yet.".into();
    }
    let take = if limit == 0 {
        s.trades.len()
    } else {
        limit.min(s.trades.len())
    };
    let mut out = format!("Last {take} Trades (newest first):\n");
    for t in s.trades.iter().rev().take(take) {
        out.push_str(&format!(
            "  {} {} {} ({}) [{}] {:.2}->{:.2} {}s\n",
            t.asset,
            t.direction.to_uppercase(),
            fmt_pct(t.net_pnl_pct),
            fmt_usd(t.net_pnl_usd),
            t.strategy,
            t.entry_price,
            t.exit_price,
            t.hold_time_sec
        ));
    }
    out
}

fn fmt_usd(v: f64) -> String {
    format!("{}{:.2}", if v >= 0.0 { "+" } else { "-" }, v.abs())
}
fn fmt_pct(v: f64) -> String {
    format!("{}{:.1}%", if v >= 0.0 { "+" } else { "-" }, v.abs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_start_defaults_and_flags() {
        match parse_command("start BTC,ETH --size 5 --dry-run").unwrap() {
            Command::Start {
                assets,
                size_usd,
                dry_run,
            } => {
                assert_eq!(assets, vec!["BTC".to_string(), "ETH".to_string()]);
                assert_eq!(size_usd.as_deref(), Some("5"));
                assert!(dry_run);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn parses_positions_limit_and_status() {
        assert_eq!(
            parse_command("positions 25").unwrap(),
            Command::Positions { limit: 25 }
        );
        assert_eq!(
            parse_command("positions").unwrap(),
            Command::Positions { limit: 0 }
        );
        assert_eq!(parse_command("STATUS").unwrap(), Command::Status);
        assert_eq!(parse_command("stop").unwrap(), Command::Stop);
    }

    #[test]
    fn rejects_unknown_and_bad_flags() {
        assert!(parse_command("buy BTC").is_err());
        assert!(parse_command("start --nope").is_err());
        assert!(parse_command("").is_err());
    }

    #[test]
    fn parses_plugin_verbs() {
        assert_eq!(parse_command("strategies").unwrap(), Command::Strategies);
        assert_eq!(parse_command("extensions").unwrap(), Command::Extensions);
        assert_eq!(parse_command("markets").unwrap(), Command::Markets);
        assert_eq!(
            parse_command("strategy trend_follow on").unwrap(),
            Command::StrategySet {
                name: "trend_follow".into(),
                enabled: true
            }
        );
        assert_eq!(
            parse_command("strategy mean_reversion off").unwrap(),
            Command::StrategySet {
                name: "mean_reversion".into(),
                enabled: false
            }
        );
        assert_eq!(
            parse_command("extension my_ext off").unwrap(),
            Command::ExtensionSet {
                name: "my_ext".into(),
                enabled: false
            }
        );
    }

    #[test]
    fn rejects_malformed_plugin_verbs() {
        assert!(parse_command("strategy").is_err());
        assert!(parse_command("strategy solo").is_err());
        assert!(parse_command("strategy s maybe").is_err());
        assert!(parse_command("extension").is_err());
        assert!(parse_command("strategie x on").is_err());
    }

    #[test]
    fn start_is_refused_without_lifecycle_enabled() {
        let cfg = SupervisorConfig::from_env("/tmp/none-such.sock".into());
        let mut disp = Dispatcher::new(cfg, false);
        let out = disp.dispatch_line("start");
        assert!(!out.ok);
        assert_eq!(out.action, "error");
        assert!(out.message.contains("--manage"));
    }
}

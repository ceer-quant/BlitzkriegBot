//! Command dispatch — the UI Kit's replacement for the Node `/crypto-hft`
//! command surface (`src/skills/bundled/crypto-hft/index.ts`, deleted with the
//! rest of the source layer in `62b16c88`).
//!
//! Supported verbs (identical in meaning to the Node skill's Rust-core path,
//! which this module now owns outright) are listed exactly once — in
//! [`COMMANDS`], the table that [`help_text`], the TUI overlay and the command
//! bar all read.
//!
//! No entry-order verb exists here. `stop`/`start` act on the process, while
//! `flatten` is the explicit human-supervision exit path and delegates to the
//! core's existing `positions.exit` RPC. All trading decisions stay in the core.

use crate::core::ipc_client::IpcClient;
use crate::core::types::{NetCheckReportView, UiSnapshot};
use crate::gateway::supervisor::{
    RestartPolicy, StartOutcome, StopOutcome, Supervisor, SupervisorConfig,
};
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
    Flatten {
        position_id: String,
    },
    Markets,
    /// Network self-check: probe the paths the active venue trades over.
    NetCheck,
    Help,
    /// E13: every known evolution proposal's latest state.
    EvolutionProposals {
        limit: usize,
    },
    /// E13: the operator's verdict on one held proposal.
    EvolutionDecide {
        id: String,
        decision: String,
    },
    /// E13: the auto-evolve switch (persisted across restarts).
    EvolutionAuto {
        on: bool,
    },
    /// #249: the engine switch — whether the evaluator runs at all (persisted).
    /// Distinct from `EvolutionAuto`, which only decides who applies a change.
    EvolutionEngine {
        on: bool,
    },
    /// E13: roll ONE strategy back to its pre-change parameters.
    EvolutionRollback {
        strategy: String,
    },
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
        "flatten" | "close" => {
            if parts.len() != 2 || parts[1].is_empty() {
                return Err("usage: flatten <position_id>".into());
            }
            Ok(Command::Flatten {
                position_id: parts[1].to_string(),
            })
        }
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
        "netcheck" | "net-check" => Ok(Command::NetCheck),
        "proposals" | "evo" => Ok(Command::EvolutionProposals {
            limit: parts
                .get(1)
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(20),
        }),
        "decide" => {
            let id = parts.get(1).copied().unwrap_or("").to_string();
            let decision = parts
                .get(2)
                .map(|s| s.to_ascii_lowercase())
                .unwrap_or_default();
            if id.is_empty() || !matches!(decision.as_str(), "accept" | "reject" | "defer") {
                return Err("usage: decide <proposal_id> accept|reject|defer".into());
            }
            Ok(Command::EvolutionDecide { id, decision })
        }
        "auto-evolve" | "autoevolve" => {
            let on = parts.get(1).copied();
            if !matches!(on, Some("on") | Some("off")) || parts.len() != 2 {
                return Err("usage: auto-evolve on|off".into());
            }
            Ok(Command::EvolutionAuto {
                on: on == Some("on"),
            })
        }
        "evolve" => {
            let on = parts.get(1).copied();
            if !matches!(on, Some("on") | Some("off")) || parts.len() != 2 {
                return Err("usage: evolve on|off".into());
            }
            Ok(Command::EvolutionEngine {
                on: on == Some("on"),
            })
        }
        "rollback" => {
            let strategy = parts.get(1).copied().unwrap_or("").to_string();
            if strategy.is_empty() {
                return Err("usage: rollback <strategy>".into());
            }
            Ok(Command::EvolutionRollback { strategy })
        }
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
        let mut sup = Supervisor::new(cfg);
        // A gateway that may start the core is also the one that answers for it
        // being gone: `--manage` is the operator saying "you own this process",
        // so it gets the keep-alive policy and the panel refresh becomes the
        // heartbeat that notices a crash (E12-c). A read-only gateway keeps the
        // default (off) and only ever adopts.
        if lifecycle_enabled {
            sup.set_restart_policy(RestartPolicy::keep_alive());
        }
        Self {
            sup,
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
        // Same rule as `IpcClient::snapshot`: a cached handle is not evidence.
        // `connect()` returns Ok for a socket whose core has since been killed,
        // and reporting that as connected would put an empty plugin registry on
        // screen as if the core had answered with nothing (E12-c).
        s.connected = self.client.ready().is_ok();
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
            s.market_active_name = m.active;
        }
        // E9-g: the plugins tab also shows per-strategy counters + rejection
        // causes (engine.stats), not just the enabled/disabled registry list.
        if let Ok(st) = self.client.stats() {
            s.strategy_stats = st.strategies;
        }
        s
    }

    /// E25 (#331): the intent-audit tail (§12.3) as RAW JSON rows — the
    /// panel's decisions tab renders exactly what the audit holds, so the TUI
    /// needs no second model of a decision. Empty on any error (the tab says
    /// so itself; this is a read, never a lifecycle action).
    pub fn intent_audit_tail(&mut self, limit: usize) -> Vec<serde_json::Value> {
        self.client
            .call("intent.audit.tail", serde_json::json!({ "limit": limit }))
            .ok()
            .and_then(|v| v.get("records").cloned())
            .and_then(|r| r.as_array().cloned())
            .unwrap_or_default()
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

    /// Network self-check (`net.check`): every path the active venue trades
    /// over, walked resolver → TCP → TLS → one request each.
    ///
    /// Takes SECONDS by design (the probe dials), so the browser panel runs it
    /// on a background thread and the TUI on a worker — never on a render loop.
    pub fn net_check(&mut self) -> Result<NetCheckReportView, String> {
        self.client.net_check().map_err(|e| e.to_string())
    }

    /// True when this dispatcher spawned (and owns) the running core.
    pub fn managed(&self) -> bool {
        self.sup.owns()
    }

    /// PID of the core this dispatcher spawned, if any.
    pub fn pid(&self) -> Option<u32> {
        self.sup.pid()
    }

    /// Observe the core and apply the restart policy (E12-c). The UI calls this
    /// on its refresh tick so a core that died is noticed, reported and — when a
    /// policy says so — replaced. Returns the crash report when one happened.
    pub fn pump(&mut self) -> Option<crate::gateway::supervisor::ExitReport> {
        self.sup.pump()
    }

    /// Up-or-down plus why, for the panel's process-control view.
    pub fn health(&self) -> crate::gateway::supervisor::CoreHealth {
        self.sup.health()
    }

    /// Stop the core if owned by this dispatcher's supervisor.
    pub fn stop(&mut self) -> StopOutcome {
        self.sup.stop()
    }

    /// Access the underlying supervisor.
    pub fn supervisor(&self) -> &Supervisor {
        &self.sup
    }

    /// Mutably access the underlying supervisor.
    pub fn supervisor_mut(&mut self) -> &mut Supervisor {
        &mut self.sup
    }

    /// Enable crash replacement for a core this dispatcher owns.
    pub fn set_restart_policy(&mut self, policy: RestartPolicy) {
        self.sup.set_restart_policy(policy);
    }

    /// Whether a crashed core will be replaced instead of only reported. The
    /// panel shows this so an operator is never left assuming a dead core will
    /// come back on its own.
    pub fn restart_policy(&self) -> RestartPolicy {
        self.sup.restart_policy()
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
            Command::NetCheck => self.cmd_net_check(raw),
            Command::StrategySet { name, enabled } => {
                self.cmd_plugin_set(raw, PluginKind::Strategy, &name, enabled)
            }
            Command::ExtensionSet { name, enabled } => {
                self.cmd_plugin_set(raw, PluginKind::Extension, &name, enabled)
            }
            Command::Flatten { position_id } => self.cmd_flatten(raw, &position_id),
            Command::EvolutionProposals { limit } => self.cmd_evolution_proposals(raw, limit),
            Command::EvolutionDecide { id, decision } => {
                self.cmd_evolution_decide(raw, &id, &decision)
            }
            Command::EvolutionAuto { on } => self.cmd_evolution_auto(raw, on),
            Command::EvolutionEngine { on } => self.cmd_evolution_engine(raw, on),
            Command::EvolutionRollback { strategy } => self.cmd_evolution_rollback(raw, &strategy),
            Command::Help => CommandOutcome::ok(raw, "help", help_text())
                .with_data(serde_json::json!({ "usage": help_text() })),
        }
    }

    /// E13: the proposal list — pending ones first, then the recent history.
    fn cmd_evolution_proposals(&mut self, raw: &str, limit: usize) -> CommandOutcome {
        let proposals = match self.client.evolution_proposals(limit) {
            Ok(p) => p,
            Err(e) => return CommandOutcome::err(raw, e.to_string()),
        };
        let pending: Vec<&crate::core::types::EvolutionProposalView> =
            proposals.iter().filter(|p| p.is_pending()).collect();
        let mut msg = format!(
            "Evolution proposals ({} pending / {} total):\n",
            pending.len(),
            proposals.len()
        );
        for p in &pending {
            msg.push_str(&format!(
                "  [{}] {} · {} · {}\n",
                p.state, p.id, p.strategy, p.reason
            ));
        }
        if pending.is_empty() {
            msg.push_str("  (nothing held — no decision waiting)\n");
        }
        CommandOutcome::ok(raw, "proposals", msg.trim_end().to_string())
            .with_data(serde_json::json!({ "proposals": serde_json::to_value(&proposals).unwrap_or_default() }))
    }

    /// E13: one operator verdict. The core re-runs the full guard chain, so an
    /// RPC error here is a REAL refusal (world moved / decided already).
    fn cmd_evolution_decide(&mut self, raw: &str, id: &str, decision: &str) -> CommandOutcome {
        match self.client.evolution_decide(id, decision) {
            Ok(v) => CommandOutcome::ok(raw, "decide", format!("{decision} recorded for {id}"))
                .with_data(v),
            Err(e) => CommandOutcome::err(raw, e.to_string()),
        }
    }

    /// E13: flip the auto-evolve switch.
    fn cmd_evolution_auto(&mut self, raw: &str, on: bool) -> CommandOutcome {
        match self.client.evolution_set_auto(on) {
            Ok(v) => CommandOutcome::ok(
                raw,
                "auto-evolve",
                if on {
                    "auto-evolve ON — the engine now applies qualifying variants itself"
                } else {
                    "auto-evolve OFF — proposals wait for a decision"
                },
            )
            .with_data(v),
            Err(e) => CommandOutcome::err(raw, e.to_string()),
        }
    }

    /// #249: the engine switch. Turning it ON is what makes anything evolve at
    /// all; the auto switch only decides who applies what qualifies.
    fn cmd_evolution_engine(&mut self, raw: &str, on: bool) -> CommandOutcome {
        match self.client.evolution_set_enabled(on) {
            Ok(v) => CommandOutcome::ok(
                raw,
                "evolve",
                if on {
                    "evolution engine ON — evaluating, and its twin sets are built"
                } else {
                    "evolution engine OFF — nothing is evaluated, held or applied"
                },
            )
            .with_data(v),
            Err(e) => CommandOutcome::err(raw, e.to_string()),
        }
    }

    /// E13: one strategy's one-click rollback.
    fn cmd_evolution_rollback(&mut self, raw: &str, strategy: &str) -> CommandOutcome {
        match self.client.evolution_rollback(strategy) {
            Ok(v) => CommandOutcome::ok(
                raw,
                "rollback",
                format!("{strategy} rolled back to its previous parameters"),
            )
            .with_data(v),
            Err(e) => CommandOutcome::err(raw, e.to_string()),
        }
    }

    /// Network self-check: probe every path the venue trades over and hand back
    /// the same table the TUI overlay and `blitzkrieg net-check` print, so the
    /// command line, the panel and the CLI cannot describe one report three
    /// ways. Takes seconds — the probe dials.
    fn cmd_net_check(&mut self, raw: &str) -> CommandOutcome {
        let report = match self.client.net_check() {
            Ok(r) => r,
            Err(e) => return CommandOutcome::err(raw, e.to_string()),
        };
        let (passed, total) = report.passed();
        let message = crate::core::net_check::render_text(&report)
            .trim_end()
            .to_string();
        CommandOutcome::ok(raw, "netcheck", message).with_data(serde_json::json!({
            "passed": passed,
            "total": total,
            "report": serde_json::to_value(&report).unwrap_or_default(),
        }))
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

    fn cmd_flatten(&mut self, raw: &str, position_id: &str) -> CommandOutcome {
        if !self.lifecycle_enabled {
            return CommandOutcome::err(
                raw,
                "manual flatten disabled; start the gateway with --manage to enable operator controls",
            );
        }
        match self.client.position_exit(position_id) {
            Ok(data) => {
                let closed = data.get("closed").and_then(|v| v.as_u64()).unwrap_or(0);
                CommandOutcome::ok(
                    raw,
                    "flattened",
                    format!("manual flatten requested for {position_id} (closed={closed})"),
                )
                .with_data(data)
            }
            Err(e) => CommandOutcome::err(raw, e.to_string()),
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
                        r.has_data_feed.then_some("feed"),
                        r.has_discovery.then_some("discovery"),
                        r.has_executor.then_some("executor"),
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

/// The gateway's command surface, `(usage, summary)` per command in the order
/// help prints them. The verb is the usage's first token, so a row cannot
/// disagree with itself.
///
/// This table is the single source: [`help_text`] renders it, the TUI's `?`
/// overlay and `--help` list it, and the command bar completes against it. A
/// verb the parser does not dispatch is therefore impossible to offer to a user,
/// and a verb the parser does dispatch is impossible to leave out of the help.
pub const COMMANDS: &[(&str, &str)] = &[
    (
        "start [ASSETS] [--size N] [--dry-run]",
        "start the core (adopts if already running)",
    ),
    ("stop", "stop the core this gateway spawned"),
    ("status", "round / stats / positions / balance"),
    ("positions [N]", "closed-trade history (newest first)"),
    ("strategies", "list strategies and enabled state"),
    ("strategy <name> on|off", "enable/disable one strategy"),
    ("extensions", "list extensions and state"),
    ("extension <name> on|off", "enable/disable one extension"),
    ("flatten <position_id>", "force-close one open position"),
    ("markets", "list market plugins"),
    ("netcheck", "probe the network paths the venue uses"),
    ("proposals [N]", "evolution proposals (pending first)"),
    (
        "decide <id> accept|reject|defer",
        "vote on one evolution proposal",
    ),
    ("auto-evolve on|off", "unattended mode switch (persisted)"),
    ("evolve on|off", "evolution engine switch (persisted)"),
    ("rollback <strategy>", "undo one strategy's last evolution"),
    ("help", ""),
];

/// The verb a usage dispatches: its first token.
fn verb_of(usage: &'static str) -> &'static str {
    usage.split(' ').next().unwrap_or("")
}

/// Every verb the gateway dispatches, in help order — what the command bar
/// offers as completion candidates.
pub fn command_verbs() -> impl Iterator<Item = &'static str> {
    COMMANDS.iter().map(|&(usage, _)| verb_of(usage))
}

/// One line per command, `usage` and `summary` in columns: the single renderer
/// behind [`help_text`], the TUI overlay and the panel's `--help`.
pub fn command_lines() -> impl Iterator<Item = String> {
    let width = COMMANDS
        .iter()
        .map(|(usage, _)| usage.len())
        .max()
        .unwrap_or(0);
    COMMANDS.iter().map(move |&(usage, summary)| {
        if summary.is_empty() {
            format!("  {usage}")
        } else {
            format!("  {usage:<width$}   {summary}")
        }
    })
}

/// The `help` text.
pub fn help_text() -> String {
    let mut out = String::from("crypto-hft commands (UI Kit gateway):");
    for line in command_lines() {
        out.push('\n');
        out.push_str(&line);
    }
    out
}

fn status_json(disp: &Dispatcher, s: &UiSnapshot) -> serde_json::Value {
    let round = s.round.as_ref();
    let health = disp.health();
    serde_json::json!({
        "connection": {
            "connected": s.connected,
            "socket": disp.socket_path(),
            "managed": health.managed,
            "pid": health.pid,
            "lifecycleEnabled": disp.lifecycle_enabled,
            // E12-c: crash-recovery state. `restarts` counts replacements of cores
            // this dispatcher owned; `lastExit` says how the last one ended
            // (`kind` is "crash" or "clean"), so a panel can distinguish "it died"
            // from "we stopped it" without guessing from a missing pid.
            "restarts": health.restarts,
            "restartGivenUp": health.restart_given_up,
            "lastExit": health.last_exit.as_ref().map(|e| serde_json::json!({
                "pid": e.pid,
                "kind": match e.kind {
                    crate::gateway::supervisor::ExitKind::Clean => "clean",
                    crate::gateway::supervisor::ExitKind::Crashed => "crash",
                },
                "code": e.code,
                "signal": e.signal,
                "description": e.describe(),
            })),
        },
        "mode": s.mode(),
        "round": round.map(|r| serde_json::json!({
            "slot": r.slot, "ageSec": r.age_sec, "timeLeftSec": r.time_left_sec,
            "markets": r.markets, "canTrade": r.can_trade,
        })),
        "balance": s.balance.as_ref().map(|b| serde_json::json!({
            "balance": b.balance, "reserved": b.reserved, "available": b.available,
            // Starting principal (dry only), so the text UI can show the same
            // `principal + realized net` reconciliation the panel does.
            "seed": b.seed })),
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
    fn parses_manual_flatten_with_stable_position_id() {
        assert_eq!(
            parse_command("flatten pos-123").unwrap(),
            Command::Flatten {
                position_id: "pos-123".into()
            }
        );
        assert_eq!(
            parse_command("close pos-123").unwrap(),
            Command::Flatten {
                position_id: "pos-123".into()
            }
        );
        assert!(parse_command("flatten").is_err());
        assert!(parse_command("flatten pos-1 extra").is_err());
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

    #[test]
    fn only_the_gateway_that_may_start_the_core_may_replace_it() {
        // E12-c: the pairing matters. A read-only gateway only ever adopts, so a
        // keep-alive policy there would be dead code that reads like a safety
        // net; and a lifecycle gateway with no policy would report a crash and
        // then leave the operator staring at a dead core it was entitled to fix.
        let cfg = SupervisorConfig::from_env("/tmp/none-such.sock".into());
        let readonly = Dispatcher::new(cfg.clone(), false);
        assert!(
            !readonly.restart_policy().enabled,
            "a read-only gateway must not hold a restart policy"
        );

        let managing = Dispatcher::new(cfg, true);
        assert!(
            managing.restart_policy().enabled,
            "--manage must imply crash replacement, or E12(c) is report-only"
        );
    }

    /// Every row of [`COMMANDS`] must dispatch, and to the command its own usage
    /// claims. The match in `variant` is the tripwire: a new `Command` variant
    /// stops compiling here until its row is added to the table (the drift that
    /// left a phantom `risk` in the panel's completion and hid four real verbs).
    #[test]
    fn every_command_row_dispatches() {
        fn variant(c: &Command) -> &'static str {
            match c {
                Command::Start { .. } => "start",
                Command::Stop => "stop",
                Command::Status => "status",
                Command::Positions { .. } => "positions",
                Command::Strategies => "strategies",
                Command::StrategySet { .. } => "strategy",
                Command::Extensions => "extensions",
                Command::ExtensionSet { .. } => "extension",
                Command::Flatten { .. } => "flatten",
                Command::Markets => "markets",
                Command::NetCheck => "netcheck",
                Command::Help => "help",
                Command::EvolutionProposals { .. } => "proposals",
                Command::EvolutionDecide { .. } => "decide",
                Command::EvolutionAuto { .. } => "auto-evolve",
                Command::EvolutionEngine { .. } => "evolve",
                Command::EvolutionRollback { .. } => "rollback",
            }
        }
        // Rows whose usage carries placeholders need a filled-in line to parse.
        const FILLED: &[(&str, &str)] = &[
            ("strategy", "strategy alpha on"),
            ("extension", "extension market_polymarket on"),
            ("flatten", "flatten pos-1"),
            ("decide", "decide prop-1 accept"),
            ("rollback", "rollback alpha"),
            ("auto-evolve", "auto-evolve on"),
            ("evolve", "evolve on"),
        ];
        for &(usage, _) in COMMANDS {
            let verb = verb_of(usage);
            let line = FILLED
                .iter()
                .find(|(v, _)| *v == verb)
                .map(|(_, line)| *line)
                .unwrap_or(verb);
            let parsed = parse_command(line)
                .unwrap_or_else(|e| panic!("COMMANDS row `{verb}` (`{line}`) does not parse: {e}"));
            assert_eq!(
                variant(&parsed),
                verb,
                "COMMANDS row `{verb}` parses as a different command"
            );
        }
    }
}

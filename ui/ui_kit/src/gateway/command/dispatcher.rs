//! Command execution: the Dispatcher turns parsed commands into supervisor and IPC effects, and renders CommandOutcome results.

use super::*;

use crate::core::ipc_client::IpcClient;
use crate::core::types::{NetCheckReportView, UiSnapshot};
use crate::gateway::supervisor::{
    RestartPolicy, StartOutcome, StopOutcome, Supervisor, SupervisorConfig,
};
use crate::socket_served;
use serde::Serialize;

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
            // 显示账户配置化: same ride-along as `IpcClient::snapshot`.
            s.display_account = m.display_account;
            s.display_venue = m.display_venue;
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
            Command::Restart => self.cmd_restart(raw),
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
            Command::ForceOrder { order_id } => self.cmd_force_order(raw, &order_id),
            Command::EvolutionProposals { limit } => self.cmd_evolution_proposals(raw, limit),
            Command::EvolutionDecide { id, decision } => {
                self.cmd_evolution_decide(raw, &id, &decision)
            }
            Command::EvolutionAuto { on } => self.cmd_evolution_auto(raw, on),
            Command::EvolutionEngine { on } => self.cmd_evolution_engine(raw, on),
            Command::EvolutionRollback { strategy } => self.cmd_evolution_rollback(raw, &strategy),
            Command::Accounts => self.cmd_accounts(raw),
            Command::AccountSwitch { account_id } => self.cmd_account_switch(raw, &account_id),
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

    /// E28: the account book, verbatim. The core's `account.list` reply IS the
    /// panel's model (id / status / balance / credentialsLoaded); the gateway
    /// adds no second rendering of it — same discipline as the decisions tab.
    fn cmd_accounts(&mut self, raw: &str) -> CommandOutcome {
        match self.client.account_list() {
            Ok(v) => {
                let active = v
                    .get("active")
                    .and_then(|a| a.as_str())
                    .unwrap_or("?")
                    .to_string();
                let n = v
                    .get("accounts")
                    .and_then(|a| a.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                CommandOutcome::ok(
                    raw,
                    "accounts",
                    format!("{n} account(s), default: {active}"),
                )
                .with_data(v)
            }
            Err(e) => CommandOutcome::err(raw, e.to_string()),
        }
    }

    /// E28: switch THIS connection's session default (§9.5 — the web adapter
    /// serves every browser session through its own long-lived IPC client, so
    /// the switch is panel-process-scoped, exactly what a single operator's
    /// panel means by "my default"). The core validates the id; the reply's
    /// `active` is echoed so the panel re-fetches nothing to know it landed.
    ///
    /// 切户残留窗口（issue 552 审计）：切户成功后不重拉任何读数 —— 下一次
    /// `status`/`positions` 前屏幕上躺着的还是旧账户的数字。切成功立即重取
    /// 插件面（market_active_name 是 TUI 身份行的唯一来源），best-effort：
    /// 读数失败不影响切换本身已成功的事实，回执仍带 `active`。
    fn cmd_account_switch(&mut self, raw: &str, account_id: &str) -> CommandOutcome {
        match self.client.account_switch(account_id) {
            Ok(v) => {
                let active = v
                    .get("active")
                    .and_then(|a| a.as_str())
                    .unwrap_or(account_id)
                    .to_string();
                let mut out = CommandOutcome::ok(
                    raw,
                    "account",
                    format!("this connection's default account: {active}"),
                );
                // Merge the switch reply with the re-pulled plugin identity —
                // with_data REPLACES, and the switch reply (`active` +
                // validation facts) must survive beside the fresh readout.
                let mut data = v;
                if let Ok(m) = self.client.market_plugins() {
                    if let Some(obj) = data.as_object_mut() {
                        obj.insert("activePlugin".into(), serde_json::json!(m.active));
                    }
                }
                out = out.with_data(data);
                out
            }
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

    /// issue 488: stop → wait for the socket to actually go quiet → start again.
    ///
    /// Synchronous on purpose: the caller (panel button) is owed a definite
    /// answer, and the two halves are quick — TERM_GRACE at most for the exit,
    /// STARTUP_DEADLINE at most for the boot. The socket-wait between them is
    /// what makes this safe: spawning while the old core still holds the UDS
    /// would make the new core adopt-or-abort against a dying socket instead
    /// of binding it.
    fn cmd_restart(&mut self, raw: &str) -> CommandOutcome {
        if !self.lifecycle_enabled {
            return CommandOutcome::err(
                raw,
                "lifecycle control disabled; start the gateway with --manage to enable start/stop/restart",
            );
        }
        let was_running = self.sup.is_running();
        if was_running {
            match self.sup.stop() {
                StopOutcome::Stopped { .. } => {
                    // Wait until the socket is free (bounded, well past TERM_GRACE)
                    // so the replacement bind cannot race the dying process.
                    let socket = self.sup.config().socket_path.clone();
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                    while socket_served(&socket) && std::time::Instant::now() < deadline {
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                }
                StopOutcome::NotOwned => {
                    return CommandOutcome::ok(
                        raw,
                        "not_owned",
                        "no core spawned by this gateway; an adopted core is left running (restart not applicable — stop it with `blitzkrieg stop` and start fresh)",
                    );
                }
            }
        }
        match self.sup.start() {
            Ok(StartOutcome::Started { pid }) => {
                let note = if was_running {
                    "core restarted (next-session settings now applied)"
                } else {
                    "core started (was not running)"
                };
                CommandOutcome::ok(raw, "restarted", format!("{note} (pid {pid})"))
                    .with_data(serde_json::json!({ "restarted": was_running, "pid": pid }))
            }
            Ok(StartOutcome::Adopted) => CommandOutcome::ok(
                raw,
                "adopted",
                "a core is already serving this socket; adopted it (not owned)",
            ),
            Err(e) => CommandOutcome::err(raw, e.to_string()),
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

    /// issue 456: force-close one ghost order. Same operator gate as flatten
    /// (manual verbs need `--manage`); the kernel owns the verdict — an order
    /// whose round is still live comes back refused, verbatim.
    fn cmd_force_order(&mut self, raw: &str, order_id: &str) -> CommandOutcome {
        if !self.lifecycle_enabled {
            return CommandOutcome::err(
                raw,
                "manual force-close disabled; start the gateway with --manage to enable operator controls",
            );
        }
        match self.client.force_close_order(order_id) {
            Ok(_) => CommandOutcome::ok(
                raw,
                "force_closed",
                format!("order {order_id} marked ForceClosed (audited)"),
            )
            .with_data(serde_json::json!({ "orderId": order_id })),
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
            // issue 550 / issue 559 显示面口径（TUI status 面漏网之一）：全局
            // 标量块是各 venue 最早到期的保守钟，多族并存时 TUI status 面只
            // 看得到它。这里把 snapshot 已类型化携带的 per-venue 轮次钟行原
            // 样下发（与 web/mod.rs 快照下发的 venueRounds 同一形状）—— 新
            // 面板按行选钟，老客户端忽略新键。老内核 venue_rounds 为空表 →
            // 下发空数组，feature-detect 回落现行为，单族逐字节不变。
            "venueRounds": r.venue_rounds.iter().map(|v| serde_json::json!({
                "venue": v.venue, "durationSec": v.duration_sec, "slot": v.slot,
                "ageSec": v.age_sec, "timeLeftSec": v.time_left_sec }))
                .collect::<Vec<_>>(),
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

/// issue 550 / issue 559 显示面口径（TUI status 面漏网）：`status_json`
/// must forward the snapshot's typed per-venue round rows as a
/// `round.venueRounds` array beside the legacy global scalar block — the
/// TUI status surface reads this payload and had no venue rows at all. An
/// old core's empty `venue_rounds` degrades to an empty array (the key is
/// still written; legacy clients ignore it, the scalar block is untouched).
#[test]
fn status_json_carries_the_per_venue_round_rows() {
    use crate::core::types::{RoundView, VenueRoundView};
    use crate::gateway::supervisor::SupervisorConfig;

    let disp = Dispatcher::new(
        SupervisorConfig::from_env("/tmp/none-such.sock".into()),
        false,
    );
    let round = |rows: Vec<VenueRoundView>| UiSnapshot {
        round: Some(RoundView {
            slot: 7,
            age_sec: 1,
            time_left_sec: 299,
            markets: 2,
            can_trade: true,
            venue_rounds: rows,
            ..RoundView::default()
        }),
        ..UiSnapshot::default()
    };

    // Multi-family: both rows travel verbatim, (venue, durationSec) intact.
    let doc = status_json(
        &disp,
        &round(vec![
            VenueRoundView {
                venue: "polymarket".into(),
                duration_sec: 900,
                slot: 7,
                age_sec: 1,
                time_left_sec: 299,
            },
            VenueRoundView {
                venue: "kalshi".into(),
                duration_sec: 3600,
                slot: 3,
                age_sec: 5,
                time_left_sec: 295,
            },
        ]),
    );
    assert_eq!(
        doc["round"]["slot"],
        serde_json::json!(7),
        "the legacy global scalar block stays exactly where it was"
    );
    let vr = doc["round"]["venueRounds"].as_array().expect("rows array");
    assert_eq!(vr.len(), 2, "one row per spoken (venue, family)");
    assert_eq!(vr[0]["venue"], serde_json::json!("polymarket"));
    assert_eq!(vr[0]["durationSec"], serde_json::json!(900));
    assert_eq!(vr[1]["venue"], serde_json::json!("kalshi"));
    assert_eq!(vr[1]["slot"], serde_json::json!(3));
    assert_eq!(vr[1]["timeLeftSec"], serde_json::json!(295));

    // Backward compatibility: a pre-issue-550 core's empty venue_rounds
    // serialises as an EMPTY array — every legacy scalar field unchanged.
    let doc = status_json(&disp, &round(Vec::new()));
    assert_eq!(doc["round"]["slot"], serde_json::json!(7));
    assert_eq!(doc["round"]["ageSec"], serde_json::json!(1));
    assert_eq!(doc["round"]["timeLeftSec"], serde_json::json!(299));
    assert_eq!(doc["round"]["markets"], serde_json::json!(2));
    assert_eq!(doc["round"]["canTrade"], serde_json::json!(true));
    assert_eq!(
        doc["round"]["venueRounds"],
        serde_json::json!([]),
        "no rows → empty array, never a missing key"
    );
}

//! Market seam — how the market-agnostic core talks to a concrete venue.
//!
//! The core (OME, risk, ledger, strategy engine) speaks only market-api boundary
//! types. A `MarketPlugin` supplies a `DataFeed`, `MarketDiscovery` and
//! `OrderExecutor`, all driving the core through `MarketHost`. Concrete venues
//! live in their own crates (see `extensions/*`); this module only hosts the
//! registry and the feature-gated registration seam.

pub mod compat;
pub mod host;
pub mod registry;

/// Register the market plugins compiled into this build.
///
/// This is the ONE place the core names a concrete market. Each entry is behind
/// a feature so the core can be built market-free (`--no-default-features`) and
/// so a different market is a Cargo feature rather than a code change.
pub fn register_builtin_markets(reg: &registry::MarketPluginRegistry) {
    #[cfg(feature = "polymarket")]
    reg.register(Box::new(polymarket_extension::PolymarketPlugin::new()));
    #[cfg(feature = "kalshi")]
    reg.register(Box::new(kalshi_extension::KalshiPlugin::new()));
    #[cfg(feature = "predictfun")]
    reg.register(Box::new(predictfun_extension::PredictFunPlugin::new()));
    #[cfg(feature = "polysimulator")]
    reg.register(Box::new(polysimulator_extension::PolySimulatorPlugin::new()));
    #[cfg(not(feature = "polymarket"))]
    #[cfg(not(feature = "kalshi"))]
    #[cfg(not(feature = "predictfun"))]
    #[cfg(not(feature = "polysimulator"))]
    let _ = reg; // market-free build: nothing to register
}

/// Select the operative market plugin for this process.
///
/// `preferred` is the configured plugin name (`--market-plugin`). When it is
/// absent or not registered we fall back to the first registered plugin, so a
/// stray flag can never brick startup. The chosen plugin is marked active in the
/// registry (surfaced by `market.list`).
pub fn active_market_plugin(
    registry: &registry::MarketPluginRegistry,
    preferred: Option<&str>,
) -> std::sync::Arc<dyn blitzkrieg_market_api::MarketPlugin> {
    if let Some(name) = preferred {
        if let Some(p) = registry.get(name) {
            registry.set_enabled(name, true);
            registry.set_active(name);
            return p;
        }
        eprintln!(
            "blitzkrieg-core: requested market plugin '{name}' not registered — using first available"
        );
    }
    for name in registry.names() {
        if let Some(p) = registry.get(&name) {
            registry.set_enabled(&name, true);
            registry.set_active(&name);
            return p;
        }
    }
    // No plugin registered: a do-nothing placeholder keeps the server booting
    // (dry runs with market data pushed over IPC still work).
    std::sync::Arc::new(NoopMarketPlugin)
}

/// Split a `--market-plugin` value (or the persisted `marketPlugin` cell) into
/// plugin names. Comma-separated multi-value — `"polymarket,kalshi"` runs both
/// data planes — with a single value being the historical form. Whitespace is
/// trimmed; empty items are dropped. An empty result means "not configured".
pub fn parse_plugin_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Select EVERY operative market plugin for this process (multi-plugin data
/// plane). `preferred` is the configured value (`--market-plugin` / the
/// persisted cell) — a comma-separated list; each registered name is enabled
/// and returned in configuration order. Names that are not registered are
/// warned and skipped. When nothing resolves (no preference, or no name
/// registered) the fallback is the FIRST registered plugin — the historical
/// single-plugin behaviour. The registry's `active` slot keeps pointing at the
/// FIRST selected plugin (the executor still routes through it this batch;
/// order/withdraw/settlement routing per plugin is the next batch).
pub fn active_market_plugins(
    registry: &registry::MarketPluginRegistry,
    preferred: Option<&str>,
) -> Vec<std::sync::Arc<dyn blitzkrieg_market_api::MarketPlugin>> {
    if let Some(spec) = preferred {
        let names = parse_plugin_list(spec);
        let picked: Vec<std::sync::Arc<dyn blitzkrieg_market_api::MarketPlugin>> = names
            .iter()
            .filter_map(|name| match registry.get(name) {
                Some(p) => {
                    registry.set_enabled(name, true);
                    Some(p)
                }
                None => {
                    eprintln!(
                        "blitzkrieg-core: requested market plugin '{name}' not registered — skipped"
                    );
                    None
                }
            })
            .collect();
        if let Some(first) = picked.first() {
            registry.set_active(first.name());
            return picked;
        }
        eprintln!(
            "blitzkrieg-core: none of the requested market plugins {names:?} is registered — \
             using first available"
        );
    }
    let fallback = active_market_plugin(registry, None);
    vec![fallback]
}

/// Fallback plugin used when no market is compiled in / registered. Visible to
/// the IPC layer so `net.check` can still answer with a report (its default
/// probe reports `unsupported`) instead of an error.
pub(crate) struct NoopMarketPlugin;

impl blitzkrieg_market_api::MarketPlugin for NoopMarketPlugin {
    fn name(&self) -> &str {
        "none"
    }
    fn market_type(&self) -> blitzkrieg_market_api::MarketType {
        blitzkrieg_market_api::MarketType::Prediction
    }
}

/// `market.plugin.get` readout (#485): the running session's active plugin,
/// the compiled-in registry, and what a restart would select. The registry
/// lives in the server's shared context (NOT inside Core — the core is
/// market-agnostic by the seam's whole point), so this assembles from both.
/// `flag_pinned` is the config's own `market_plugin: Some(_)` — the boot
/// only reaches the cell when no explicit flag spoke, so the readout can
/// honestly say a write cannot move a flag-pinned session.
pub fn plugin_readout(
    registry: &registry::MarketPluginRegistry,
    core: &crate::service::Core,
) -> crate::ipc::schema::MarketPluginReadout {
    use crate::ipc::schema::{MarketPluginOption, MarketPluginReadout};
    let list = registry.list();
    let active = registry
        .active()
        .unwrap_or_else(|| list.first().map(|p| p.name.clone()).unwrap_or_default());
    let registered: Vec<MarketPluginOption> = list
        .iter()
        .map(|p| MarketPluginOption {
            name: p.name.clone(),
            kind: format!("{:?}", p.market_type).to_lowercase(),
            active: p.name == active,
        })
        .collect();
    // The persisted cell is only ARMED when EVERY name it lists is a
    // registered plugin — the boot refuses a cell naming anything uncompiled,
    // and the readout must report the same next session the boot would
    // actually pick (no silently-differs). Multi-value ("polymarket,kalshi")
    // is the same rule, per name.
    let cell = core
        .config()
        .runtime_config_path
        .as_deref()
        .and_then(|p| crate::ipc::runtime_config::load_market_plugin_from(std::path::Path::new(p)))
        .filter(|c| {
            parse_plugin_list(&c.plugin)
                .iter()
                .all(|n| registry.get(n).is_some())
        });
    MarketPluginReadout {
        next_session_plugin: cell
            .as_ref()
            .map(|c| c.plugin.clone())
            .unwrap_or_else(|| active.clone()),
        persisted: cell.is_some(),
        flag_pinned: core.config().market_plugin.is_some(),
        active_plugin: active,
        registered,
    }
}

/// `market.plugin.set` write path (#485): next_session ONLY — plan-first
/// (an unregistered name is refused before anything touches the disk; the
/// registry is compiled in, so a typo must fail now, not as a broken boot
/// later), then an atomic persist-cell write. The running session's
/// feed/discovery/executor were all built around the plugin chosen at ITS
/// boot and are never moved by this call.
pub async fn apply_plugin_set(
    registry: &registry::MarketPluginRegistry,
    core: &tokio::sync::Mutex<crate::service::Core>,
    patch: &crate::ipc::schema::SetMarketPluginParams,
    actor: &str,
    now_ms: i64,
) -> Result<crate::ipc::schema::MarketPluginUpdate, crate::model::CoreError> {
    use crate::ipc::schema::{MARKET_PLUGIN_SET_NOTE, MarketPluginUpdate};
    use crate::model::{CoreError, CoreErrorCode};

    if patch.reset && patch.name.is_some() {
        return Err(CoreError::new(
            CoreErrorCode::InvalidParams,
            "market.plugin.set: give name or reset, not both".to_string(),
        ));
    }
    let name = patch
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if !patch.reset && name.is_none() {
        return Err(CoreError::new(
            CoreErrorCode::InvalidParams,
            "market.plugin.set needs name (a registered plugin) or reset: true".to_string(),
        ));
    }

    // The fallback a reset arms is what THIS session actually runs (the
    // registry's active name): a session that booted on the cell re-arms the
    // same plugin on reset — clearing the cell removes the override, not the
    // value it last resolved to (the round-duration reset carries the same
    // semantics, and the e2e gate asserts both).
    let active = registry
        .active()
        .or_else(|| registry.names().first().cloned())
        .unwrap_or_default();

    // reset 路径：清格，下次会话回落启动解析（显式旗标 > 首个注册者）。
    if patch.reset {
        persist_cell(core, None, actor, now_ms).await?;
        tracing::info!(
            target: "risk",
            actor = %actor,
            reason = patch.reason.as_deref().unwrap_or(""),
            "market plugin cell cleared — next session re-resolves from \
             the boot flag or first registered plugin ({MARKET_PLUGIN_SET_NOTE})"
        );
        return Ok(MarketPluginUpdate {
            next_session_plugin: active,
            reset: true,
            at_ms: now_ms,
            actor: actor.to_string(),
            reason: patch.reason.clone(),
            note: MARKET_PLUGIN_SET_NOTE,
        });
    }

    // 写路径：plan-first（未注册名拒绝，格不动），然后原子写。
    let Some(name) = name else {
        unreachable!("the both-given/none-given paths returned above");
    };
    if registry.get(name).is_none() {
        let known = registry.names();
        let known: Vec<&str> = known.iter().map(|s| s.as_str()).collect();
        return Err(CoreError::new(
            CoreErrorCode::InvalidParams,
            format!(
                "market.plugin.set: '{name}' is not a registered plugin — the \
                 switchable set is {known:?}"
            ),
        ));
    }
    persist_cell(core, Some(name), actor, now_ms).await?;
    tracing::info!(
        target: "risk",
        actor = %actor,
        reason = patch.reason.as_deref().unwrap_or(""),
        plugin = %name,
        "market plugin cell written — next session selects it at boot \
         ({MARKET_PLUGIN_SET_NOTE})"
    );
    Ok(MarketPluginUpdate {
        next_session_plugin: name.to_string(),
        reset: false,
        at_ms: now_ms,
        actor: actor.to_string(),
        reason: patch.reason.clone(),
        note: MARKET_PLUGIN_SET_NOTE,
    })
}

/// One persist step, shared by both arms. The cell is cwd-relative
/// (`data/runtime/config.json`); a core without a runtime config path
/// (backtester, in-process tests) refuses the write and says so — the same
/// posture as `round_duration.set`, so a test can never write into whatever
/// checkout it happens to run in.
async fn persist_cell(
    core: &tokio::sync::Mutex<crate::service::Core>,
    plugin: Option<&str>,
    actor: &str,
    now_ms: i64,
) -> Result<(), crate::model::CoreError> {
    use crate::model::{CoreError, CoreErrorCode};
    let path = core
        .lock()
        .await
        .config()
        .runtime_config_path
        .clone()
        .ok_or_else(|| {
            CoreError::new(
                CoreErrorCode::InvalidParams,
                "market.plugin.set: no runtime config path configured — \
                 persistence is disabled for this core (backtests and \
                 in-process tests); nothing was applied"
                    .to_string(),
            )
        })?;
    crate::ipc::runtime_config::persist_market_plugin_to(
        std::path::Path::new(&path),
        plugin,
        actor,
        now_ms,
    )
    .map_err(|e| {
        CoreError::new(
            CoreErrorCode::InvalidParams,
            format!("market.plugin.set: persist failed, nothing changed: {e}"),
        )
    })
}

// ── 行情插件凭据（API 配置 UI 的内核侧）─────────────────────────────────────

/// The env vars each market plugin's executor reads at boot, in the order the
/// plugins themselves read them. ONE table: `market.credentials.status`
/// reports it, `market.credentials.set` validates names against it, and the
/// UI renders it. A plugin missing from the table has no executor credentials
/// (its data plane is credential-free).
///
/// Kept in the CORE (not the plugin crates) because the status readout must
/// answer for every registered plugin, including ones whose executor never
/// starts in this session. A new credential-bearing plugin adds its row here
/// AND reads the vars in its own executor — the gate test fails when the two
/// drift.
pub fn plugin_credential_vars(plugin: &str) -> &'static [&'static str] {
    match plugin {
        "polymarket" => &["POLYMARKET_PRIVATE_KEY", "POLYMARKET_FUNDER_ADDRESS"],
        "kalshi" => &["KALSHI_API_KEY_ID", "KALSHI_RSA_PRIVATE_KEY"],
        "predictfun" => &["PREDICT_API_KEY", "PREDICT_PRIVATE_KEY"],
        "polysimulator" => &["POLYSIM_API_KEY"],
        _ => &[],
    }
}

/// `market.credentials.status`: for every registered plugin, which of its
/// required vars will resolve for the NEXT session — environment first, the
/// persisted cell second (the cell never outranks the environment, matching
/// .env posture). Values never leave the process; names and presence only.
pub fn credentials_status(
    registry: &registry::MarketPluginRegistry,
    core: &crate::service::Core,
) -> crate::ipc::schema::CredentialsStatus {
    use crate::ipc::schema::{CredentialsStatus, PluginCredentialRow};
    let cell = core
        .config()
        .runtime_config_path
        .as_deref()
        .map(std::path::Path::new)
        .map(|p| crate::ipc::runtime_config::load_credentials_from(p, None))
        .unwrap_or_default();
    let rows = registry
        .list()
        .iter()
        .map(|p| {
            let required: Vec<String> = plugin_credential_vars(&p.name)
                .iter()
                .map(|s| s.to_string())
                .collect();
            let resolved: Vec<String> = required
                .iter()
                .filter(|v| {
                    std::env::var_os(v).is_some_and(|s| !s.is_empty())
                        || cell
                            .get(&p.name)
                            .is_some_and(|m| m.get(*v).is_some_and(|s| !s.is_empty()))
                })
                .cloned()
                .collect();
            let ready = !required.is_empty() && resolved.len() == required.len();
            PluginCredentialRow {
                plugin: p.name.clone(),
                required_vars: required,
                resolved_vars: resolved,
                ready,
            }
        })
        .collect();
    CredentialsStatus { rows }
}

/// `market.credentials.set`: validate every var name against the plugin's
/// requirement table (a typo must fail now, not as a silently inert
/// executor), then persist atomically. next_session only — the executor
/// reads the environment once at boot.
pub async fn apply_credentials_set(
    registry: &registry::MarketPluginRegistry,
    core: &tokio::sync::Mutex<crate::service::Core>,
    patch: &crate::ipc::schema::SetCredentialsParams,
    actor: &str,
    now_ms: i64,
) -> Result<crate::ipc::schema::CredentialsUpdate, crate::model::CoreError> {
    use crate::ipc::schema::{CREDENTIALS_SET_NOTE, CredentialsUpdate};
    use crate::model::{CoreError, CoreErrorCode};

    let plugin = patch.plugin.trim();
    if registry.get(plugin).is_none() {
        let known = registry.names();
        let known: Vec<&str> = known.iter().map(|s| s.as_str()).collect();
        return Err(CoreError::new(
            CoreErrorCode::InvalidParams,
            format!(
                "market.credentials.set: '{plugin}' is not a registered plugin — the switchable set is {known:?}"
            ),
        ));
    }
    let allowed = plugin_credential_vars(plugin);
    let path = core
        .lock()
        .await
        .config()
        .runtime_config_path
        .clone()
        .ok_or_else(|| {
            CoreError::new(
                CoreErrorCode::InvalidParams,
                "market.credentials.set: no runtime config path configured — \
                 persistence is disabled for this core (backtests and \
                 in-process tests); nothing was applied"
                    .to_string(),
            )
        })?;

    let mut vars = patch.vars.clone();
    for name in vars.keys() {
        if !allowed.contains(&name.as_str()) {
            let known: Vec<&str> = allowed.to_vec();
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "market.credentials.set: '{name}' is not a credential {plugin} reads — it needs {known:?}"
                ),
            ));
        }
    }

    let written: Vec<String> = if patch.reset {
        allowed.iter().map(|s| s.to_string()).collect()
    } else {
        vars.keys().cloned().collect()
    };
    if patch.reset {
        vars.clear();
    }
    crate::ipc::runtime_config::persist_credentials_to(
        std::path::Path::new(&path),
        plugin,
        Some(&vars),
    )
    .map_err(|e| {
        CoreError::new(
            CoreErrorCode::InvalidParams,
            format!("market.credentials.set: persist failed, nothing changed: {e}"),
        )
    })?;
    tracing::info!(
        target: "risk",
        actor = %actor,
        reason = patch.reason.as_deref().unwrap_or(""),
        plugin = %plugin,
        vars = ?written,
        "credential cell written — next session injects it at boot \
         ({CREDENTIALS_SET_NOTE})"
    );
    Ok(CredentialsUpdate {
        plugin: plugin.to_string(),
        written,
        reset: patch.reset,
        at_ms: now_ms,
        actor: actor.to_string(),
        reason: patch.reason.clone(),
        note: CREDENTIALS_SET_NOTE,
    })
}

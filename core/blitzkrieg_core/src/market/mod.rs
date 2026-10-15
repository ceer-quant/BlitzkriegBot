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
    // The persisted cell is only ARMED when it names a registered plugin —
    // the boot refuses anything else, and the readout must report the same
    // next session the boot would actually pick (no silently-differs).
    let cell = core
        .config()
        .runtime_config_path
        .as_deref()
        .and_then(|p| crate::ipc::runtime_config::load_market_plugin_from(std::path::Path::new(p)))
        .filter(|c| registry.get(&c.plugin).is_some());
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

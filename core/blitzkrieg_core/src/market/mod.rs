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

/// Where a session's plugin ENABLEMENT set came from — the boot resolution's
/// four-layer chain, made honest on the wire (`market.plugin.get`'s `source`).
/// The layers rank exactly in this order; the first non-empty layer wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginEnableSource {
    /// An explicit `--market-plugin` flag spoke — everything else is ignored.
    Flag,
    /// The account book's venue bindings derived the set (绑定即启用: an API
    /// account bound to a venue IS that venue's plugin being enabled).
    Accounts,
    /// The persisted `marketPlugin` cell (the old `market.plugin.set` write).
    Persisted,
    /// Nothing spoke — the historical first-registered fallback.
    Fallback,
}

impl PluginEnableSource {
    /// The wire spelling (`market.plugin.get` → `source`), a stable contract
    /// the panel renders verbatim.
    pub fn as_str(self) -> &'static str {
        match self {
            PluginEnableSource::Flag => "flag",
            PluginEnableSource::Accounts => "accounts",
            PluginEnableSource::Persisted => "persisted",
            PluginEnableSource::Fallback => "fallback",
        }
    }
}

/// The venue bindings every account carries, deduplicated — the 绑定即启用
/// derivation input. A NON-EMPTY `Account.venue` is a binding (empty =
/// unbound, the historical "trades any venue"); several accounts on one venue
/// is the deployment model, not a conflict. Order is registration order of the
/// book's ids (stable), then first-seen venue — derivation only feeds sets, so
/// order is cosmetic; stability keeps logs reproducible.
pub fn derived_venue_set(core: &crate::service::Core) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for a in core.accounts().account_list() {
        if !a.venue.is_empty() && seen.insert(a.venue.clone()) {
            out.push(a.venue.clone());
        }
    }
    out
}

/// Resolve this boot's plugin enablement under the four-layer chain:
///
/// 1. explicit `--market-plugin` flag — outranks everything (replays and the
///    corpus baselines depend on it), returned as `Some(spec)` untouched;
/// 2. the account book's venue bindings — ANY non-empty derived set wins and
///    the persisted cell is retired (migration: clear it, one INFO line);
/// 3. the persisted cell (the old `market.plugin.set` write) — only when no
///    binding speaks;
/// 4. first-registered — the historical fallback, inside
///    `active_market_plugins`.
///
/// `cell` is the already-loaded persisted cell (the boot only reaches the cell
/// when no flag spoke). Returns the SPEC for `active_market_plugins` plus
/// WHERE the enablement came from, so the readout can disclose it and the
/// `market.plugin.set` guard can arm.
///
/// The Accounts layer (绑定即启用) returns the DERIVED SET as a
/// comma-joined spec — `active_market_plugins` mounts feed/discovery for
/// EVERY name in it. An earlier revision returned `None` here and left the
/// fallback resolution to re-derive: that fell to first-registered, so a
/// multi-venue binding {polymarket, kalshi} mounted only polymarket's data
/// plane and every other venue starved — the exact「切账户后非 polymarket
/// 永远加载中」defect. The spec keeps single-venue boots byte-identical
/// (`parse_plugin_list("polymarket") == ["polymarket"]`).
pub fn boot_enablement(
    core: &crate::service::Core,
    registry: &registry::MarketPluginRegistry,
    flag: Option<&str>,
    cell: Option<&crate::ipc::runtime_config::MarketPluginCell>,
) -> (Option<String>, PluginEnableSource) {
    // Layer 1: an explicit flag outranks everything, verbatim (a flag is also
    // how the replay corpus pins its plugin — never touched here).
    if let Some(spec) = flag {
        return (Some(spec.to_string()), PluginEnableSource::Flag);
    }
    // Layer 2: 绑定即启用 — the accounts' venue set. A derived set wins even
    // when the persisted cell is non-empty: the bindings are the NEWER source
    // (the panel now writes bindings, not cells), and leaving the cell armed
    // would make the next boot disagree with this one. Migration: the first
    // boot where both speak clears the cell once and says so (no migration
    // script — this IS the migration).
    let derived = derived_venue_set(core);
    if !derived.is_empty() {
        if let Some(path) = core.config().runtime_config_path.as_deref() {
            // Only a cell that would actually boot (every name registered —
            // the same filter the readout arms with) is worth clearing; a
            // stale/unregistered cell is already ignored by the boot.
            let stale = cell
                .filter(|c| {
                    parse_plugin_list(&c.plugin)
                        .iter()
                        .all(|n| registry.get(n).is_some())
                })
                .is_some();
            if stale {
                match crate::ipc::runtime_config::persist_market_plugin_to(
                    std::path::Path::new(path),
                    None,
                    "system:venue-derive-migration",
                    blitzkrieg_market_api::net::now_ms(),
                ) {
                    Ok(()) => tracing::info!(
                        target: "risk",
                        venues = ?derived,
                        "插件启用来源切换为账户绑定派生 —— 持久 marketPlugin 格已清 \
                         （market.plugin.set 在绑定派生生效期间被拒；编辑账户绑定即可）"
                    ),
                    Err(e) => {
                        // The derivation still runs (memory wins this boot);
                        // the log says the cell will need one more boot.
                        tracing::warn!(
                            target: "risk",
                            error = %e,
                            "账户绑定派生生效，但持久 marketPlugin 格清除失败 — \
                             下一会话仍会先清格再派生"
                        );
                    }
                }
            }
        }
        // The spec IS the derived set: every bound venue's plugin mounts at
        // boot (feed/discovery per name in `active_market_plugins`). Returning
        // `None` here once made a multi-venue book boot half its data plane.
        return (Some(derived.join(",")), PluginEnableSource::Accounts);
    }
    // Layer 3: the persisted cell, only when no binding spoke.
    if let Some(c) = cell {
        return (Some(c.plugin.clone()), PluginEnableSource::Persisted);
    }
    // Layer 4: nothing spoke — the boot falls to first-registered inside
    // active_market_plugins.
    (None, PluginEnableSource::Fallback)
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

/// `net.check` 的插件解析（网络诊断跟随显示账户）：探针走的是操作员正在看的
/// venue——display account 的 venue 绑定（#561/#562 显示账户配置化，与行情页/
/// 引擎计数同一身份）。未绑定（空串）保持旧语义 active 席位 → 首个注册名；
/// 绑定了但注册表里没有该插件（编译面缺插件/名字错位）交给 no-op 插件——它的
/// 默认探针报告 `unsupported`，诚实且报文形状每个 UI 都认识，绝不去探另一个
/// venue 冒充这个账户的网络。
pub(crate) fn net_check_plugin(
    registry: &registry::MarketPluginRegistry,
    display_venue: &str,
) -> std::sync::Arc<dyn blitzkrieg_market_api::MarketPlugin> {
    if display_venue.is_empty() {
        return registry
            .active()
            .and_then(|name| registry.get(&name))
            .or_else(|| registry.names().first().and_then(|n| registry.get(n)))
            .unwrap_or_else(|| std::sync::Arc::new(NoopMarketPlugin));
    }
    registry
        .get(display_venue)
        .unwrap_or_else(|| std::sync::Arc::new(NoopMarketPlugin))
}

/// `market.plugin.get` readout (#485): the running session's active plugin,
/// the compiled-in registry, and what a restart would select. The registry
/// lives in the server's shared context (NOT inside Core — the core is
/// market-agnostic by the seam's whole point), so this assembles from both.
/// `flag_pinned` is the config's own `market_plugin: Some(_)` — the boot
/// only reaches the cell when no explicit flag spoke, so the readout can
/// honestly say a write cannot move a flag-pinned session.
///
/// `source` discloses where enablement came from this boot — the same
/// four-layer chain `boot_enablement` resolves (flag > accounts > persisted >
/// fallback), re-derived from the SAME inputs the boot saw, so the panel can
/// say "绑定即启用" honestly: the derivation is a pure function of the account
/// book, the cell and the flag, all frozen at boot.
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
    // Re-derive the enablement chain from the boot's own inputs. A flag in the
    // config IS the boot's layer-1 (the config is the flag's only channel);
    // the armed cell is the boot's layer-3.
    let (_, source) = boot_enablement(
        core,
        registry,
        core.config().market_plugin.as_deref(),
        cell.as_ref(),
    );
    MarketPluginReadout {
        next_session_plugin: cell
            .as_ref()
            .map(|c| c.plugin.clone())
            .unwrap_or_else(|| active.clone()),
        persisted: cell.is_some(),
        flag_pinned: core.config().market_plugin.is_some(),
        active_plugin: active,
        registered,
        source: source.as_str(),
        derived_venues: derived_venue_set(core),
    }
}

/// `market.plugin.set` write path (#485): next_session ONLY — plan-first
/// (an unregistered name is refused before anything touches the disk; the
/// registry is compiled in, so a typo must fail now, not as a broken boot
/// later), then an atomic persist-cell write. The running session's
/// feed/discovery/executor were all built around the plugin chosen at ITS
/// boot and are never moved by this call.
///
/// The persisted cell is cwd-relative (`data/runtime/config.json`); a core
/// without a runtime config path (backtester, in-process tests) refuses the
/// write and says so — the same posture as `round_duration.set`, so a test
/// can never write into whatever checkout it happens to run in.
pub(crate) async fn apply_plugin_set(
    registry: &registry::MarketPluginRegistry,
    core: &tokio::sync::Mutex<crate::service::Core>,
    patch: &crate::ipc::schema::SetMarketPluginParams,
    actor: &str,
    now_ms: i64,
) -> Result<crate::ipc::schema::MarketPluginUpdate, crate::model::CoreError> {
    use crate::ipc::schema::{MARKET_PLUGIN_SET_NOTE, MarketPluginUpdate};
    use crate::model::{CoreError, CoreErrorCode};

    // 绑定即启用（fail-closed guard）：当 THIS session 的启用集合由账户
    // venue 绑定派生时，持久格是死格 —— boot 的派生层 outrank 它，写入一个
    // 永不生效的值是谎报（"next session selects it" 不成立）。整个写路径
    // （写与 reset 都在内）在派生生效期间拒绝，错误点名真正的编辑面：账户
    // 绑定。派生集合为空时（回落旧格）guard 不参与 —— 旧行为逐字不变。
    // （旗标钉住的会话本来就无法被写入移动 —— 格只是陪跑 —— 无需新拒绝面。）
    // readout 的 `source` 是同一个解析，面板据此禁用写入控件。cell 参数传
    // `None`：guard 只回答「派生层是否压过持久格」，持久格的值与判定无关。
    {
        let c = core.lock().await;
        let (_, source) = boot_enablement(&c, registry, c.config().market_plugin.as_deref(), None);
        if source == PluginEnableSource::Accounts {
            let derived = derived_venue_set(&c);
            let venues = derived.join(",");
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "market.plugin.set: 插件启用已由账户绑定派生（绑定即启用）—— \
                     启用集合 {{{venues}}} 由账户的 venue 绑定决定，持久格不参与 \
                     启动解析；请编辑账户绑定（account.venue.set），不要写持久格"
                ),
            ));
        }
    }
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
    // `name` 是一个逗号列表（`market.plugin.set` 与 boot/read 路径共用
    // `parse_plugin_list` 的同一语法）：拆开逐名验证注册 —— plan-first
    // 语义是全有或全无，任一名未注册则整个写拒绝、格不动，绝不写一个
    // boot 会拒绝的半合法格（读路径对格的整体过滤规则是逐名 all-registered，
    // 写路径必须先于它满足同一条规则）。单值字符串走同一条路，行为逐字
    // 不变（parse_plugin_list("polymarket") == ["polymarket"]）。
    let Some(name) = name else {
        unreachable!("the both-given/none-given paths returned above");
    };
    let names = parse_plugin_list(name);
    if names.is_empty() {
        // 拆完为空（全逗号/空白）：与「needs name or reset」同一拒绝面 ——
        // 这个字符串从不携带可验证的插件名。
        return Err(CoreError::new(
            CoreErrorCode::InvalidParams,
            "market.plugin.set needs name (a registered plugin) or reset: true".to_string(),
        ));
    }
    if let Some(bad) = names.iter().find(|n| registry.get(n).is_none()) {
        let known = registry.names();
        let known: Vec<&str> = known.iter().map(|s| s.as_str()).collect();
        return Err(CoreError::new(
            CoreErrorCode::InvalidParams,
            format!(
                "market.plugin.set: '{bad}' is not a registered plugin — the \
                 switchable set is {known:?}"
            ),
        ));
    }
    let cell = names.join(",");
    persist_cell(core, Some(&cell), actor, now_ms).await?;
    tracing::info!(
        target: "risk",
        actor = %actor,
        reason = patch.reason.as_deref().unwrap_or(""),
        plugin = %cell,
        "market plugin cell written — next session selects it at boot \
         ({MARKET_PLUGIN_SET_NOTE})"
    );
    Ok(MarketPluginUpdate {
        next_session_plugin: cell,
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

/// RFC #541 方案 2a-i 挂钩 B（原型）的凭证谓词：`plugin` 的全部必需凭证变量
/// 是否都能解析——与 [`credentials_status`] 逐变量同一谓词（env `var_os`
/// 非空，或 `cell` 里该插件该变量非空），同一持久格（`runtime_config_path`
/// load 的产物直接传入，避免本函数再读一次盘）。需要的插件变量表为空 ⇒
/// 视为就绪（数据面无凭证的插件没有可失败的维度——`credentials_status` 的
/// `ready = !required.is_empty()` 语义只属于面板行，这里不能把一个无凭证
/// 插件的 venue 判成永远未就绪）。
pub fn venue_credentials_ready(
    plugin: &str,
    cell: &std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>,
) -> bool {
    venue_credentials_missing(plugin, cell).is_empty()
}

/// The refusal-diagnosis half of [`venue_credentials_ready`]: WHICH of the
/// plugin's required vars do not resolve (env empty/absent AND the persisted
/// cell empty), in the plugin's declared order. RFC #541 边界三（拒绝可见性）：
/// an operator must be able to fix a wholesale batch refusal from the log line
/// alone — "kalshi 缺 KALSHI_RSA_PRIVATE_KEY"，不是笼统的"批校验失败"。
pub fn venue_credentials_missing(
    plugin: &str,
    cell: &std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>,
) -> Vec<&'static str> {
    plugin_credential_vars(plugin)
        .iter()
        .copied()
        .filter(|v| {
            !(std::env::var_os(v).is_some_and(|s| !s.is_empty())
                || cell
                    .get(plugin)
                    .is_some_and(|m| m.get(*v).is_some_and(|s| !s.is_empty())))
        })
        .collect()
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

#[cfg(test)]
mod set_tests {
    use super::*;
    use crate::ipc::schema::SetMarketPluginParams;
    use crate::model::CoreErrorCode;

    /// A tiny registered plugin — the registry is compiled in, so a test that
    /// wants a controllable switchable set registers its own two names.
    struct Stub {
        name: String,
    }
    impl blitzkrieg_market_api::MarketPlugin for Stub {
        fn name(&self) -> &str {
            &self.name
        }
        fn market_type(&self) -> blitzkrieg_market_api::MarketType {
            blitzkrieg_market_api::MarketType::Prediction
        }
    }

    fn registry_with(names: &[&str]) -> registry::MarketPluginRegistry {
        let reg = registry::MarketPluginRegistry::new();
        for n in names {
            reg.register(Box::new(Stub {
                name: (*n).to_string(),
            }));
        }
        reg
    }

    // ── net.check 插件解析（网络诊断跟随显示账户） ─────────────────────────

    #[test]
    fn net_check_probes_the_display_venues_plugin() {
        let reg = registry_with(&["alpha", "beta"]);
        assert_eq!(net_check_plugin(&reg, "beta").name(), "beta");
        assert_eq!(net_check_plugin(&reg, "alpha").name(), "alpha");
    }

    #[test]
    fn net_check_falls_back_to_active_then_first_when_display_unbound() {
        let reg = registry_with(&["alpha", "beta"]);
        // 未绑定：保持旧语义——active 席位（无 active 时首个注册名）。
        assert_eq!(net_check_plugin(&reg, "").name(), "alpha");
        reg.set_active("beta");
        assert_eq!(net_check_plugin(&reg, "").name(), "beta");
    }

    #[test]
    fn net_check_refuses_to_probe_another_venue_for_a_bound_unknown() {
        // 绑定了 ghost venue 而注册表没有它：no-op（默认探针报 unsupported）
        // ——绝不拿 alpha 的网络冒充 ghost 的。
        let reg = registry_with(&["alpha"]);
        assert_eq!(net_check_plugin(&reg, "ghost").name(), "none");
    }

    #[test]
    fn net_check_empty_registry_still_answers_via_noop() {
        let reg = registry_with(&[]);
        assert_eq!(net_check_plugin(&reg, "").name(), "none");
    }

    fn core_with_cell(path: Option<String>) -> tokio::sync::Mutex<crate::service::Core> {
        let cfg = crate::service::CoreConfig {
            runtime_config_path: path,
            trade_log_path: None,
            order_log_path: None,
            position_log_path: None,
            ..Default::default()
        };
        tokio::sync::Mutex::new(crate::service::Core::new(cfg))
    }

    fn cell_in(dir: &std::path::Path) -> String {
        dir.join("config.json").to_string_lossy().into_owned()
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("bk-plugin-set-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("scratch dir");
        d
    }

    fn patch(name: Option<&str>, reset: bool) -> SetMarketPluginParams {
        SetMarketPluginParams {
            name: name.map(str::to_string),
            reset,
            reason: None,
        }
    }

    #[tokio::test]
    async fn a_single_value_round_trips_verbatim() {
        let dir = scratch("single");
        let core = core_with_cell(Some(cell_in(&dir)));
        let reg = registry_with(&["alpha"]);
        let update = apply_plugin_set(&reg, &core, &patch(Some("alpha"), false), "uid:test", 1)
            .await
            .expect("a single registered name writes");
        // Single-value spelling is EXACTLY the historical shape — the reply
        // carries the value verbatim (no commas, no reordering).
        assert_eq!(update.next_session_plugin, "alpha");
        let cell = crate::ipc::runtime_config::load_market_plugin_from(std::path::Path::new(
            &cell_in(&dir),
        ))
        .expect("the cell must hold the write");
        assert_eq!(cell.plugin, "alpha");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_multi_value_write_lands_and_reads_back_as_a_list() {
        let dir = scratch("multi");
        let core = core_with_cell(Some(cell_in(&dir)));
        let reg = registry_with(&["alpha", "beta"]);
        let update = apply_plugin_set(
            &reg,
            &core,
            &patch(Some("alpha,beta"), false),
            "uid:test",
            1,
        )
        .await
        .expect("both names are registered — the write lands");
        assert_eq!(update.next_session_plugin, "alpha,beta");
        assert!(!update.reset);
        // The cell carries the whole comma string; the READ path
        // (parse_plugin_list, what the boot actually runs) re-derives both
        // names from it in order.
        let cell = crate::ipc::runtime_config::load_market_plugin_from(std::path::Path::new(
            &cell_in(&dir),
        ))
        .expect("the cell must hold the write");
        assert_eq!(parse_plugin_list(&cell.plugin), vec!["alpha", "beta"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn whitespace_and_empty_segments_are_trimmed_like_the_read_path() {
        let dir = scratch("trim");
        let core = core_with_cell(Some(cell_in(&dir)));
        let reg = registry_with(&["alpha", "beta"]);
        let update = apply_plugin_set(
            &reg,
            &core,
            &patch(Some(" alpha , ,beta, "), false),
            "uid:test",
            1,
        )
        .await
        .expect("empty segments are dropped, names trimmed");
        // The same normalisation the boot applies — the wire and the disk
        // disagree only in whitespace, never in names.
        assert_eq!(parse_plugin_list(&update.next_session_plugin).len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn one_unregistered_name_refuses_the_whole_write() {
        let dir = scratch("mixed");
        let cell = cell_in(&dir);
        // Pre-seed a good cell: a refused write must leave it untouched
        // (plan-first — the validation happens before anything touches disk).
        crate::ipc::runtime_config::persist_market_plugin_to(
            std::path::Path::new(&cell),
            Some("alpha"),
            "seed",
            0,
        )
        .expect("seed the cell");
        let core = core_with_cell(Some(cell.clone()));
        let reg = registry_with(&["alpha", "beta"]);
        let err = apply_plugin_set(
            &reg,
            &core,
            &patch(Some("alpha,ghost"), false),
            "uid:test",
            1,
        )
        .await
        .expect_err("one bad name poisons the whole list");
        assert_eq!(err.code, CoreErrorCode::InvalidParams);
        assert!(
            err.message.contains("'ghost' is not a registered plugin"),
            "the refusal names the offender: {}",
            err.message
        );
        assert!(
            err.message.contains("alpha"),
            "the refusal lists the switchable set: {}",
            err.message
        );
        // And the cell is exactly as it was — nothing was half-written.
        let after =
            crate::ipc::runtime_config::load_market_plugin_from(std::path::Path::new(&cell))
                .expect("the seeded cell survived");
        assert_eq!(after.plugin, "alpha");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_list_that_trims_to_empty_is_the_needs_name_refusal() {
        let core = core_with_cell(None);
        let reg = registry_with(&["alpha"]);
        for junk in [",,,", "  "] {
            let err = apply_plugin_set(&reg, &core, &patch(Some(junk), false), "uid:test", 1)
                .await
                .expect_err("a name that parses to nothing is not a name");
            assert_eq!(err.code, CoreErrorCode::InvalidParams);
            assert!(
                err.message.contains("needs name"),
                "junk {junk:?} refuses as needs-name: {}",
                err.message
            );
        }
    }

    #[tokio::test]
    async fn reset_semantics_are_unchanged() {
        let dir = scratch("reset");
        let cell = cell_in(&dir);
        let core = core_with_cell(Some(cell.clone()));
        let reg = registry_with(&["alpha", "beta"]);
        reg.set_active("alpha");
        // A write first, so the reset has something to clear.
        apply_plugin_set(
            &reg,
            &core,
            &patch(Some("alpha,beta"), false),
            "uid:test",
            1,
        )
        .await
        .expect("multi-value write");
        let update = apply_plugin_set(&reg, &core, &patch(None, true), "uid:test", 2)
            .await
            .expect("reset clears the cell");
        assert!(update.reset);
        // The fallback a reset arms is the registry's ACTIVE name — the
        // session's own boot resolution, not a cell value.
        assert_eq!(update.next_session_plugin, "alpha");
        assert!(
            crate::ipc::runtime_config::load_market_plugin_from(std::path::Path::new(&cell))
                .is_none(),
            "the cell holds nothing after a reset"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_pathless_core_refuses_the_multi_value_write_too() {
        let core = core_with_cell(None);
        let reg = registry_with(&["alpha", "beta"]);
        let err = apply_plugin_set(
            &reg,
            &core,
            &patch(Some("alpha,beta"), false),
            "uid:test",
            1,
        )
        .await
        .expect_err("no runtime config path, no write");
        assert!(
            err.message.contains("persistence is disabled"),
            "the persist refusal carries the round_duration.set posture: {}",
            err.message
        );
    }
}

#[cfg(test)]
mod derive_tests {
    //! 绑定即启用（boot 从账户 venue 绑定派生插件启用集合）的内核半：
    //! 四层解析链（旗标 > 账户绑定 > 旧持久格 > 首个注册者）、迁移（首个
    //! 绑定翻转启用来源时清旧格 + INFO）、market.plugin.set 在派生生效
    //! 期间的 fail-closed 拒绝、readout 的 source 披露。
    use super::*;
    use crate::ipc::schema::SetMarketPluginParams;
    use crate::model::CoreErrorCode;
    use crate::service::{Core, CoreConfig};

    /// A tiny registered plugin — same trick as `set_tests`: the registry is
    /// compiled in, so a test that wants a controllable switchable set
    /// registers its own names (the venue names double as plugin names).
    struct Stub {
        name: String,
    }
    impl blitzkrieg_market_api::MarketPlugin for Stub {
        fn name(&self) -> &str {
            &self.name
        }
        fn market_type(&self) -> blitzkrieg_market_api::MarketType {
            blitzkrieg_market_api::MarketType::Prediction
        }
    }

    fn registry_with(names: &[&str]) -> registry::MarketPluginRegistry {
        let reg = registry::MarketPluginRegistry::new();
        for n in names {
            reg.register(Box::new(Stub {
                name: (*n).to_string(),
            }));
        }
        reg
    }

    /// A core whose book is bound to the given (account_id, venue) pairs —
    /// the derivation input, built the way `AccountLedgers::new` builds it.
    /// Venues are STUB PLUGIN names so the set the derivation returns can be
    /// resolved against a registry this test also controls.
    fn core_with_bindings(bindings: &[(&str, &str)], runtime_config_path: Option<String>) -> Core {
        let now = crate::ipc::schema::ACCOUNT_VENUE_SET_NOTE.len() as i64; // any constant now
        let accounts: Vec<crate::account::Account> = bindings
            .iter()
            .map(|(id, venue)| crate::account::Account {
                id: crate::account::AccountId::from(*id),
                name: (*id).to_string(),
                market_type: blitzkrieg_market_api::MarketType::Prediction,
                status: crate::account::AccountStatus::Active,
                credential_keys: crate::account::CredentialKeys::default(),
                venue: (*venue).to_string(),
                trading_enabled: true,
                strategies: Vec::new(),
                rounds: Vec::new(),
                updated_at_ms: now,
            })
            .collect();
        let book = crate::account::AccountLedgers::new(
            accounts,
            crate::account::AccountId::from(
                bindings.first().map(|(id, _)| *id).unwrap_or("default"),
            ),
        );
        let cfg = CoreConfig {
            runtime_config_path,
            trade_log_path: None,
            order_log_path: None,
            position_log_path: None,
            ..Default::default()
        };
        Core::with_account_book(cfg, book)
    }

    fn patch(name: Option<&str>, reset: bool) -> SetMarketPluginParams {
        SetMarketPluginParams {
            name: name.map(str::to_string),
            reset,
            reason: None,
        }
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("bk-derive-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("scratch dir");
        d
    }

    fn cell_in(dir: &std::path::Path) -> String {
        dir.join("config.json").to_string_lossy().into_owned()
    }

    /// 第一层：旗标在场 > 一切（逐字不动 —— 回放/语料基线靠它）。账户绑定
    /// 同时在场也压不过旗标。
    #[test]
    fn flag_outranks_account_bindings() {
        let reg = registry_with(&["alpha", "beta"]);
        let core = core_with_bindings(&[("a", "beta")], None);
        let (resolved, source) = boot_enablement(&core, &reg, Some("alpha"), None);
        assert_eq!(
            resolved.as_deref(),
            Some("alpha"),
            "the flag passes through verbatim"
        );
        assert_eq!(source, PluginEnableSource::Flag);
        // Even a persisted cell cannot outrank the flag.
        let cell = crate::ipc::runtime_config::MarketPluginCell {
            plugin: "beta".to_string(),
        };
        let (resolved, source) = boot_enablement(&core, &reg, Some("alpha"), Some(&cell));
        assert_eq!(resolved.as_deref(), Some("alpha"));
        assert_eq!(source, PluginEnableSource::Flag);
    }

    /// 第二层：无旗标、两账户绑定 kalshi/polymarket（stub 同名插件）——
    /// 启用集合 = 去重后的绑定集合；旧格非空被清（迁移），boot 日志明示；
    /// source = accounts。派生出的集合交给 active_market_plugins 后真的把
    /// 两个插件都启用（feed/discovery 双数据面的前提）。
    #[test]
    fn bindings_derive_the_enablement_set_and_migrate_the_old_cell() {
        let dir = scratch("derive");
        let cell = cell_in(&dir);
        // 旧持久格：上一次 UI 写的 alpha —— 现在账户绑定了 beta/gamma，派生
        // 集合非空，旧格必须退休。
        crate::ipc::runtime_config::persist_market_plugin_to(
            std::path::Path::new(&cell),
            Some("alpha"),
            "uid:old",
            0,
        )
        .expect("seed the old cell");
        let loaded =
            crate::ipc::runtime_config::load_market_plugin_from(std::path::Path::new(&cell));
        assert!(loaded.is_some(), "the old cell is on disk");
        let reg = registry_with(&["alpha", "beta", "gamma"]);
        let core = core_with_bindings(
            &[("a", "beta"), ("b", "gamma"), ("c", "beta")],
            Some(cell.clone()),
        );

        let (resolved, source) = boot_enablement(&core, &reg, None, loaded.as_ref());
        assert_eq!(source, PluginEnableSource::Accounts);
        // The resolution RETURNS the derived set as the spec: the boot hands
        // it straight to `active_market_plugins` — no second derivation, no
        // fallback. (An earlier revision returned None and fell to
        // first-registered, mounting half a data plane.)
        assert_eq!(
            resolved.as_deref(),
            Some("beta,gamma"),
            "the derived set IS the spec active_market_plugins mounts"
        );
        // 迁移：旧格已清 —— 下次会话不会再从旧格解析。
        assert!(
            crate::ipc::runtime_config::load_market_plugin_from(std::path::Path::new(&cell))
                .is_none(),
            "the retired cell is gone from disk"
        );

        // 派生集合本身：非空 venue 去重（c 的 beta 与 a 重复，只出现一次）。
        let derived = derived_venue_set(&core);
        assert_eq!(derived, vec!["beta", "gamma"]);

        // 派生集合真的启用插件：把集合交给解析器（boot 的做法）。
        let spec = derived.join(",");
        let actives = active_market_plugins(&reg, Some(&spec));
        let names: Vec<&str> = actives.iter().map(|p| p.name()).collect();
        assert_eq!(
            names,
            vec!["beta", "gamma"],
            "both bound venues are enabled"
        );
        for n in ["beta", "gamma"] {
            let list = reg.list();
            let info = list.iter().find(|p| p.name == n).expect("registered");
            assert!(info.enabled, "{n} must be enabled");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 第三层：无旗标无绑定 → 回落旧格（source=persisted）；旧格也空 →
    /// 首个注册者（source=fallback）。绑定全空（venue="" 是解绑，不是绑定）
    /// 不触发派生 —— 旧部署逐字不变。
    #[test]
    fn no_bindings_falls_back_to_cell_then_first_registered() {
        let reg = registry_with(&["alpha", "beta"]);
        // 旧格在场：回落旧格。
        let cell = crate::ipc::runtime_config::MarketPluginCell {
            plugin: "alpha,beta".to_string(),
        };
        let core = core_with_bindings(&[("a", "")], None);
        let (resolved, source) = boot_enablement(&core, &reg, None, Some(&cell));
        assert_eq!(source, PluginEnableSource::Persisted);
        assert_eq!(resolved.as_deref(), Some("alpha,beta"));
        // 旧格也没有：fallback。
        let (resolved, source) = boot_enablement(&core, &reg, None, None);
        assert_eq!(source, PluginEnableSource::Fallback);
        assert_eq!(resolved, None);
        // 没有任何账户（空簿的边缘）同样回落 —— 派生集合为空。
        let empty = core_with_bindings(&[], None);
        assert!(derived_venue_set(&empty).is_empty());
    }

    /// 第四层的守门：派生生效期间 market.plugin.set 被拒（写与 reset 都是
    /// ），错误点名「账户绑定派生」与派生集合；回落旧格时行为逐字不变
    /// （写照常落格、reset 照常清格）。
    #[tokio::test]
    async fn plugin_set_is_refused_while_bindings_derive_the_set() {
        let dir = scratch("guard");
        let cell = cell_in(&dir);
        let reg = registry_with(&["alpha", "beta", "gamma"]);
        let core = tokio::sync::Mutex::new(core_with_bindings(
            &[("a", "beta"), ("b", "gamma")],
            Some(cell.clone()),
        ));

        // 派生生效：写被拒，错误点名账户绑定。
        let err = apply_plugin_set(&reg, &core, &patch(Some("alpha"), false), "uid:test", 1)
            .await
            .expect_err("the derived enablement refuses the write");
        assert_eq!(err.code, CoreErrorCode::InvalidParams);
        assert!(
            err.message.contains("账户绑定派生"),
            "the refusal names the derivation: {}",
            err.message
        );
        assert!(
            err.message.contains("beta") && err.message.contains("gamma"),
            "the refusal names the derived set: {}",
            err.message
        );
        assert!(
            err.message.contains("account.venue.set"),
            "the refusal points at the real edit surface: {}",
            err.message
        );
        // reset 同拒：派生生效期间没有「清格回落」可言 —— 格已死。
        let err = apply_plugin_set(&reg, &core, &patch(None, true), "uid:test", 2)
            .await
            .expect_err("reset is refused under the same guard");
        assert!(err.message.contains("账户绑定派生"), "{}", err.message);
        // 什么都不落盘。
        assert!(
            crate::ipc::runtime_config::load_market_plugin_from(std::path::Path::new(&cell))
                .is_none(),
            "nothing was written under the guard"
        );

        // 回落旧格（无绑定）：写与 reset 照常 —— 旧行为逐字不变。
        let plain = tokio::sync::Mutex::new(core_with_bindings(&[("a", "")], Some(cell.clone())));
        let update = apply_plugin_set(
            &reg,
            &plain,
            &patch(Some("alpha,beta"), false),
            "uid:test",
            3,
        )
        .await
        .expect("no bindings — the write lands as before");
        assert_eq!(update.next_session_plugin, "alpha,beta");
        let update = apply_plugin_set(&reg, &plain, &patch(None, true), "uid:test", 4)
            .await
            .expect("no bindings — the reset lands as before");
        assert!(update.reset);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// readout 的披露：source 随解析链走（accounts/persisted/fallback），
    /// derived_venues 只在 accounts 层非空。旗标层由 flag_pinned 承担，
    /// source 同步报 flag。
    #[test]
    fn readout_discloses_the_enablement_source() {
        let reg = registry_with(&["alpha", "beta"]);
        // accounts 层。
        let core = core_with_bindings(&[("a", "beta")], None);
        let ro = plugin_readout(&reg, &core);
        assert_eq!(ro.source, "accounts");
        assert_eq!(ro.derived_venues, vec!["beta"]);
        // persisted 层。
        // plugin_readout 自己从 config 读格 —— config 没有 runtime_config_path
        // 时 cell 是 None，直接落 fallback；persisted 层用带路径的核验证。
        let dir = scratch("readout");
        let path = cell_in(&dir);
        crate::ipc::runtime_config::persist_market_plugin_to(
            std::path::Path::new(&path),
            Some("alpha"),
            "uid:old",
            0,
        )
        .expect("seed the cell");
        let core = core_with_bindings(&[], Some(path));
        let ro = plugin_readout(&reg, &core);
        assert_eq!(ro.source, "persisted");
        assert_eq!(ro.next_session_plugin, "alpha");
        assert!(ro.persisted);
        assert!(ro.derived_venues.is_empty());
        // fallback 层。
        let core = core_with_bindings(&[], None);
        let ro = plugin_readout(&reg, &core);
        assert_eq!(ro.source, "fallback");
        assert_eq!(ro.next_session_plugin, "alpha", "first-registered");
        assert!(ro.derived_venues.is_empty());
        // flag 层：旗标在 config 里（boot 的第一层通道）。
        let mut core = core_with_bindings(&[("a", "beta")], None);
        core.config_mut().market_plugin = Some("alpha".to_string());
        let ro = plugin_readout(&reg, &core);
        assert_eq!(ro.source, "flag");
        assert!(ro.flag_pinned);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

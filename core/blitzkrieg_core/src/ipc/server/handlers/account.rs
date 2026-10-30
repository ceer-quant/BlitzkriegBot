use super::super::{
    DataPlaneRuntime, PeerAuth, SessionState, client_err, effective_default_account,
    enable_venue_at_runtime, typed,
};
use super::prelude::*;
use crate::ipc::schema::*;

// The `account` family verbs, moved verbatim out of `handle_line`'s dispatch match (issue 699).
//
// Pure code motion: arm bodies, error codes, audit lines and reply bytes are exactly
// the pre-split ones; the shared skeleton (READONLY gate, version gate, reply
// encoding) stays in `server.rs`.

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_account(
    method: &str,
    core: &Arc<AsyncMutex<Core>>,
    registry: &crate::market::registry::MarketPluginRegistry,
    peer: &PeerAuth,
    host: &std::sync::Arc<dyn blitzkrieg_market_api::MarketHost>,
    data_plane: &DataPlaneRuntime,
    session: &mut SessionState,
    params: Value,
) -> Result<Value, (i32, String, Option<ErrorData>)> {
    match method {
        method::ACCOUNT_LIST => {
            let c = core.lock().await;
            // issue 561: `active` 字段同样走统一缺省链（session → display →
            // 进程 default）—— 它是快照账户语义的报文面，必须与页面消费的
            // display 源一致；进程 default 只在链条尾部落位。
            let active = effective_default_account(session, c.accounts());
            let mut body = serde_json::to_value(AccountListResult {
                version: PROTOCOL_VERSION.to_string(),
                active: active.as_str().to_string(),
                accounts: c.account_views(now_ms()),
            })
            .unwrap_or(Value::Null);
            // 显示账户配置化: the DISPLAY pointer rides the same answer
            // (`displayAccount` + the pointed-at account's venue binding,
            // `displayVenue` — empty string = unbound, the binding's own
            // absence spelling). Top-level keys beside `active`, so every
            // snapshot consumer (webui's account switcher, the TUI identity
            // row) reads them from the one account.list round-trip it
            // already makes; older clients ignore the extra keys.
            if let Some(obj) = body.as_object_mut() {
                obj.insert(
                    "displayAccount".to_string(),
                    serde_json::json!(c.accounts().display_id()),
                );
                obj.insert(
                    "displayVenue".to_string(),
                    serde_json::json!(c.accounts().display_venue()),
                );
            }
            Ok(body)
        }
        method::ACCOUNT_SWITCH => {
            match serde_json::from_value::<AccountSwitchParams>(params) {
                Ok(p) => {
                    let id = AccountId::from(p.account_id.as_str());
                    let c = core.lock().await;
                    // Existence only: switching TO a frozen account to LOOK at
                    // it is legal — the posture gates the orders, not the view.
                    if let Err(e) = c.accounts().require(&id) {
                        Err(client_err(e))
                    } else {
                        session.session_active = Some(id);
                        Ok(serde_json::json!({ "active": p.account_id }))
                    }
                }
                Err(e) => Err((Failure::INVALID_PARAMS, e.to_string(), None)),
            }
        }
        method::ACCOUNT_STATUS => {
            match serde_json::from_value::<AccountStatusParams>(params) {
                Ok(p) => {
                    // The flat wire `reason` overrides the placeholder a
                    // bare-string `suspended` would carry (§12.1); a map-form
                    // reason survives when no flat one is sent.
                    let status = match (p.status, p.reason) {
                        (AccountStatus::Suspended { .. }, Some(r)) => {
                            AccountStatus::Suspended { reason: r }
                        }
                        (s, _) => s,
                    };
                    let status_tag = status.wire_tag().to_string();
                    let status_reason = match &status {
                        AccountStatus::Suspended { reason } => Some(reason.clone()),
                        _ => None,
                    };
                    let id = AccountId::from(p.account_id.as_str());
                    let mut c = core.lock().await;
                    match c.tighten_account_status(&id, status, now_ms()) {
                        Ok(()) => Ok(serde_json::to_value(AccountStatusResult {
                            id: p.account_id,
                            status: status_tag,
                            status_reason,
                        })
                        .unwrap_or(Value::Null)),
                        Err(e) => Err(client_err(e)),
                    }
                }
                Err(e) => Err((Failure::INVALID_PARAMS, e.to_string(), None)),
            }
        }
        method::ACCOUNT_VENUE_GET => {
            let p: AccountVenueGetParams = serde_json::from_value(params)
                .unwrap_or(AccountVenueGetParams { account_id: None });
            let c = core.lock().await;
            // issue 561: 缺省链与 account.list 同源（session → display →
            // 进程 default），两份报文的 active 语义不再分叉。
            let active = effective_default_account(session, c.accounts());
            let rows: Vec<AccountVenueRow> = c
                .account_views(now_ms())
                .iter()
                .filter(|v| {
                    p.account_id
                        .as_deref()
                        .map(|id| !id.trim().is_empty() && id == v.id)
                        .unwrap_or(true)
                })
                .map(|v| AccountVenueRow {
                    account_id: v.id.clone(),
                    venue: v.venue.clone(),
                    updated_at_ms: v.updated_at_ms,
                    // A row the panel has never re-bound carries the boot's
                    // provenance — the write actor is only known to the
                    // runtime write path (it lands in the risk log).
                    actor: "boot".to_string(),
                })
                .collect();
            Ok(serde_json::to_value(AccountVenueGetResult {
                version: PROTOCOL_VERSION.to_string(),
                venues: rows,
                active: active.as_str().to_string(),
            })
            .unwrap_or(Value::Null))
        }
        method::ACCOUNT_VENUE_SET => {
            typed(params, |p: AccountVenueSetParams| {
                let core = core.clone();
                async move {
                    let actor = match peer {
                        PeerAuth::SameUid { uid } => format!("uid:{uid}"),
                        _ => "uid:unknown".to_string(),
                    };
                    let venue = p.venue.trim();
                    // Plan-first, before anything touches memory or disk: a
                    // NON-empty venue must name a REGISTERED plugin (the
                    // registry is compiled in — a typo must fail now, not as
                    // a silently inert binding the next boot refuses). The
                    // empty string is the legal UNBIND and needs no name.
                    if !venue.is_empty() && registry.get(venue).is_none() {
                        let known = registry.names();
                        let known: Vec<&str> = known.iter().map(|s| s.as_str()).collect();
                        return Err(CoreError::new(
                            CoreErrorCode::InvalidParams,
                            format!(
                                "account.venue.set: '{venue}' is not a registered plugin — \
                                 the bindable set is {known:?}"
                            ),
                        ));
                    }
                    let id = AccountId::from(p.account_id.as_str());
                    let mut c = core.lock().await;
                    // One transaction in the method layer: persist FIRST
                    // (fail leaves both stores untouched), then memory. The
                    // method layer's refusals are all client faults
                    // (unknown id, no persist path), so they surface as
                    // INVALID_PARAMS through the arm's CoreError tail.
                    c.set_account_venue_persisting(
                        &id,
                        venue,
                        &actor,
                        p.reason.as_deref().unwrap_or(""),
                        now_ms(),
                    )?;
                    // The lock guard is dropped BEFORE the runtime mount
                    // below: mounting re-enters the core THROUGH the host
                    // (the feed start installs its subscription control, the
                    // discovery feeds its round markets) — holding the guard
                    // across that await would deadlock the session against
                    // itself.
                    drop(c);
                    // 运行时半边（绑定即启用 at runtime）：绑到一个本会话从未
                    // 挂载的 venue 时，把该 venue 的 feed/discovery 挂进来 ——
                    // 与 boot 对每个选中插件做的挂载是同一条路（复用
                    // `enable_venue_at_runtime`），不发明新机制。已挂载的
                    // venue（boot 启用 / 更早的运行时绑定）幂等跳过：重复
                    // start 会顶掉在跑的订阅控制句柄。挂载旗标是 boot 解析的
                    // 快照（config 是旗标的唯一通道，会话内不变）；executor
                    // 不在运行时挂 —— may_trade 是 boot 冻结的承诺，见该函数
                    // 的文档。
                    let mut runtime_enabled = false;
                    if !venue.is_empty()
                        && (data_plane.feed_ws_enabled || data_plane.discovery_enabled)
                        && !data_plane.venue_mounted(venue)
                        && let Some(plugin) = registry.get(venue)
                    {
                        // 轮次多族: the mount walks every (venue, duration)
                        // family this venue must run — the boot's global
                        // family plus each family the (possibly re-bound)
                        // account's `rounds` declares. Pairs already mounted
                        // are filtered out here (the idempotence guard).
                        let mut fams =
                            vec![(venue.to_string(), data_plane.discovery_round_duration_sec)];
                        for a in core.lock().await.accounts().account_list() {
                            if a.venue == venue {
                                for &dur in &a.rounds {
                                    let pair = (venue.to_string(), dur);
                                    if !fams.contains(&pair) {
                                        fams.push(pair);
                                    }
                                }
                            }
                        }
                        fams.retain(|(v, d)| v != venue || !data_plane.discovery_mounted_at(v, *d));
                        if !fams.is_empty() {
                            enable_venue_at_runtime(
                                plugin.as_ref(),
                                host.clone(),
                                data_plane.feed_ws_enabled,
                                data_plane.discovery_enabled,
                                &data_plane.discovery_assets,
                                data_plane.discovery_round_duration_sec,
                                &fams,
                                registry,
                                data_plane,
                            )
                            .await;
                            // A feed_source consumer owns NO components: the
                            // mounts inside `enable_venue_at_runtime` were the
                            // SOURCE's (keyed by the source's name). Marking
                            // the consumer's name here would fabricate a data
                            // plane it does not have — its re-bind idempotence
                            // lives entirely in the source-side guards.
                            if plugin.feed_source().is_none() {
                                for (_, d) in &fams {
                                    data_plane.mark_discovery_mounted(venue, *d);
                                }
                                // The feed side keeps its per-venue guard: mark the
                                // venue mounted even when every family was already
                                // live (a re-bind must not restart the feed).
                                data_plane.mark_mounted(venue);
                            }
                            runtime_enabled = true;
                        }
                    }
                    Ok(serde_json::to_value(AccountVenueSetResult {
                        account_id: id.as_str().to_string(),
                        venue: venue.to_string(),
                        persisted: true,
                        at_ms: now_ms(),
                        actor,
                        reason: p.reason,
                        note: ACCOUNT_VENUE_SET_NOTE,
                        runtime_enabled,
                    })
                    .unwrap_or(Value::Null))
                }
            })
            .await
        }
        method::ACCOUNT_ROUNDS_SET => {
            typed(params, |p: AccountRoundsSetParams| {
                let core = core.clone();
                async move {
                    let actor = match peer {
                        PeerAuth::SameUid { uid } => format!("uid:{uid}"),
                        _ => "uid:unknown".to_string(),
                    };
                    let id = AccountId::from(p.account_id.as_str());
                    // Validate FIRST (the parser's own union+dedup), so a
                    // bad value refuses before anything is computed or
                    // written. The union comes from the core's config (the
                    // boot's registry report). The validated set is the one
                    // every later step sees.
                    let allowed_round_secs = core.lock().await.config().allowed_round_secs.clone();
                    let new_rounds = crate::account::config::validate_rounds(
                        Some(p.rounds.clone()),
                        &allowed_round_secs,
                    )?;
                    let mut c = core.lock().await;
                    // The REMOVED families: old set minus new set (the
                    // arithmetic runs on the validated new set). Added =
                    // new minus old, for 增族即挂.
                    let old_rounds: Vec<i64> = c
                        .accounts()
                        .account(&id)
                        .map(|a| a.rounds.clone())
                        // require() inside the write would also refuse an
                        // unknown id, but the removed/added arithmetic and
                        // the exposure guard need the account NOW — refuse
                        // here with the standard unknown-account error.
                        .ok_or_else(|| c.accounts().require(&id).unwrap_err())?;
                    let removed: Vec<i64> = old_rounds
                        .iter()
                        .filter(|d| !new_rounds.contains(d))
                        .copied()
                        .collect();
                    let added: Vec<i64> = new_rounds
                        .iter()
                        .filter(|d| !old_rounds.contains(d))
                        .copied()
                        .collect();
                    // 移除守卫（整体拒绝）：any open position of THIS
                    // account whose venue sits in one of the removed
                    // families' venue dimension blocks the WHOLE write —
                    // memory and disk stay exactly as they were. ponytail:
                    // the guard is venue-scoped, not duration-exact — a
                    // position on the removed family's VENUE refuses even
                    // when its own round length differs (the position book
                    // stamps `round_duration_sec`, but a 0 = legacy row
                    // would slip a duration-exact check; the conservative
                    // reading can never strand an exit. Refinement path:
                    // compare `effective_round_duration_sec` per position
                    // once every live row carries a non-zero stamp).
                    if !removed.is_empty() {
                        let removed_venues: std::collections::HashSet<String> = {
                            let mut set = std::collections::HashSet::new();
                            for a in c.accounts().account_list() {
                                if !a.venue.is_empty() && a.rounds.iter().any(|d| removed.contains(d))
                                {
                                    set.insert(a.venue.clone());
                                }
                            }
                            // An unbound account's removed family speaks
                            // for every venue it trades: conservative
                            // union with THIS account's own binding.
                            set.extend(
                                c.accounts()
                                    .account_list()
                                    .iter()
                                    .filter(|a| !a.venue.is_empty() && a.id == id)
                                    .map(|a| a.venue.clone()),
                            );
                            set
                        };
                        let blocking: Vec<String> = c
                            .positions()
                            .open_positions()
                            .iter()
                            .filter(|pos| pos.account_id == id)
                            .filter(|pos| {
                                pos.venue.is_empty() || removed_venues.contains(&pos.venue)
                            })
                            .map(|pos| pos.id.clone())
                            .collect();
                        if !blocking.is_empty() {
                            return Err(CoreError::new(
                                CoreErrorCode::InvalidParams,
                                format!(
                                    "account.rounds.set: removing families {removed:?} is refused — \
                                     the account holds {} open position(s) ({}) in the removed \
                                     families' venues; flatten them (or wait for expiry) before \
                                     removing the family, exits are unaffected either way",
                                    blocking.len(),
                                    blocking.join(", ")
                                ),
                            ));
                        }
                    }
                    // One transaction: persist FIRST (fail leaves both
                    // stores untouched), then memory. Method-layer refusals
                    // are all client faults, surfaced as INVALID_PARAMS
                    // through the typed tail.
                    c.set_account_rounds_persisting(
                        &id,
                        new_rounds.clone(),
                        &actor,
                        p.reason.as_deref().unwrap_or(""),
                        now_ms(),
                    )?;
                    // The lock guard is dropped BEFORE the runtime mount
                    // below: mounting re-enters the core THROUGH the host
                    // (the same deadlock the venue.set arm documents).
                    drop(c);
                    // 运行时半边（增族即挂）：对每个 added 族组装挂载对 ——
                    // 账户绑定 venue → (该 venue, dur)；未绑定 → 只挂已在
                    // data_plane 挂载的 venue 上的 (v, dur)（运行时无 boot
                    // 资产不能裸挂未启用 venue）。ponytail 顶棚：未启用
                    // venue 的族要走 account.venue.set 的绑定流程（绑定即启
                    // 用），这里不复制那半边。已挂对幂等跳过。
                    let mut runtime_enabled = false;
                    let account = if !added.is_empty() && data_plane.discovery_enabled {
                        // The guard is dropped BEFORE the mount below reads
                        // the account back — the mount re-enters the core
                        // THROUGH the host (discovery.start feeds its round
                        // markets on the same await), so holding the lock
                        // across that await would deadlock the session
                        // against itself (the venue.set arm's documented
                        // hazard).
                        core.lock().await.accounts().account(&id).cloned()
                    } else {
                        None
                    };
                    if let Some(account) = account {
                        let mounted: std::collections::BTreeSet<(String, i64)> = data_plane
                            .discovery_mounted
                            .lock()
                            .expect("data plane mounted set poisoned")
                            .clone();
                        let mut fams: Vec<(String, i64)> = Vec::new();
                        for &dur in &added {
                            let targets: Vec<String> = if account.venue.is_empty() {
                                // Unbound: only venues this session
                                // ALREADY runs (the runtime ceiling above).
                                mounted
                                    .iter()
                                    .map(|(v, _)| v.clone())
                                    .collect::<std::collections::BTreeSet<String>>()
                                    .into_iter()
                                    .collect()
                            } else {
                                vec![account.venue.clone()]
                            };
                            for v in targets {
                                let pair = (v, dur);
                                if !fams.contains(&pair) {
                                    fams.push(pair);
                                }
                            }
                        }
                        fams.retain(|(v, d)| {
                            !data_plane.discovery_mounted_at(v, *d)
                        });
                        if !fams.is_empty() {
                            for (v, _) in &fams {
                                if let Some(plugin) = registry.get(v) {
                                    enable_venue_at_runtime(
                                        plugin.as_ref(),
                                        host.clone(),
                                        data_plane.feed_ws_enabled,
                                        data_plane.discovery_enabled,
                                        &data_plane.discovery_assets,
                                        data_plane.discovery_round_duration_sec,
                                        &fams
                                            .iter()
                                            .filter(|(fv, _)| fv == v)
                                            .cloned()
                                            .collect::<Vec<_>>(),
                                        registry,
                                        data_plane,
                                    )
                                    .await;
                                    // A feed_source consumer owns NO components
                                    // (same posture as the venue.set arm): the
                                    // mounts above were keyed by the SOURCE's
                                    // name inside `enable_venue_at_runtime`, so
                                    // the consumer's name is never marked.
                                    if plugin.feed_source().is_none() {
                                        for (_, d) in fams.iter().filter(|(fv, _)| fv == v) {
                                            data_plane.mark_discovery_mounted(v, *d);
                                        }
                                        // The feed side keeps its per-venue
                                        // guard (a re-declared family must not
                                        // restart the feed).
                                        data_plane.mark_mounted(v);
                                    }
                                    runtime_enabled = true;
                                }
                            }
                        }
                    }
                    Ok(serde_json::to_value(AccountRoundsSetResult {
                        account_id: id.as_str().to_string(),
                        rounds: new_rounds,
                        persisted: true,
                        at_ms: now_ms(),
                        actor,
                        reason: p.reason,
                        note: ACCOUNT_ROUNDS_SET_NOTE,
                        runtime_enabled,
                    })
                    .unwrap_or(Value::Null))
                }
            })
            .await
        }
        method::ACCOUNT_TRADING_SET => {
            typed(params, |p: AccountTradingSetParams| {
                let core = core.clone();
                async move {
                    let id = AccountId::from(p.account_id.as_str());
                    let mut c = core.lock().await;
                    c.set_account_trading_persisting(
                        &id,
                        p.enabled,
                        "panel",
                        p.reason.as_deref().unwrap_or(""),
                        now_ms(),
                    )?;
                    Ok(serde_json::to_value(AccountTradingSetResult {
                        account_id: id.as_str().to_string(),
                        enabled: p.enabled,
                        persisted: true,
                        at_ms: now_ms(),
                        reason: p.reason,
                        note: ACCOUNT_TRADING_SET_NOTE,
                    })
                    .unwrap_or(Value::Null))
                }
            })
            .await
        }
        method::ACCOUNT_FREEZE => {
            typed(params, |p: AccountFreezeParams| {
                let core = core.clone();
                async move {
                    let id = AccountId::from(p.account_id.as_str());
                    let mut c = core.lock().await;
                    c.accounts_mut().require(&id)?;
                    if p.frozen {
                        c.freeze_account(
                            id.as_str(),
                            p.reason
                                .clone()
                                .unwrap_or_else(|| "manual freeze".to_string()),
                        );
                    } else {
                        c.resume_account(id.as_str());
                    }
                    Ok(serde_json::to_value(AccountFreezeResult {
                        account_id: id.as_str().to_string(),
                        frozen: p.frozen,
                        reason: c.account_freeze_reason(id.as_str()).map(String::from),
                        note: ACCOUNT_FREEZE_NOTE,
                    })
                    .unwrap_or(Value::Null))
                }
            })
            .await
        }
        method::ACCOUNT_STRATEGIES_SET => {
            typed(params, |p: AccountStrategiesSetParams| {
                let core = core.clone();
                async move {
                    let id = AccountId::from(p.account_id.as_str());
                    let mut c = core.lock().await;
                    // The method layer adds nothing to the transaction: the
                    // service plan-first checks names against the REGISTERED
                    // set, persists, flips the book, rebuilds the engine
                    // scopes (exits-only safety net included), and returns
                    // the kept-exits-only list for the reply.
                    let kept_exits_only = c.set_account_strategies_persisting(
                        &id,
                        p.strategies.clone(),
                        "panel",
                        p.reason.as_deref().unwrap_or(""),
                        now_ms(),
                    )?;
                    Ok(serde_json::to_value(AccountStrategiesSetResult {
                        account_id: id.as_str().to_string(),
                        strategies: p.strategies,
                        kept_exits_only,
                        persisted: true,
                        at_ms: now_ms(),
                        reason: p.reason,
                        note: ACCOUNT_STRATEGIES_SET_NOTE,
                    })
                    .unwrap_or(Value::Null))
                }
            })
            .await
        }
        method::DISPLAY_ACCOUNT_SET => {
            typed(params, |p: DisplayAccountSetParams| {
                let core = core.clone();
                async move {
                    let id = AccountId::from(p.account_id.as_str());
                    let mut c = core.lock().await;
                    c.set_display_account_persisting(
                        &id,
                        "panel",
                        p.reason.as_deref().unwrap_or(""),
                    )?;
                    let display_venue = c.accounts().display_venue().to_string();
                    Ok(serde_json::to_value(DisplayAccountSetResult {
                        account_id: id.as_str().to_string(),
                        display_venue,
                        persisted: true,
                        at_ms: now_ms(),
                        reason: p.reason,
                        note: DISPLAY_ACCOUNT_SET_NOTE,
                    })
                    .unwrap_or(Value::Null))
                }
            })
            .await
        }
        method::ACCOUNT_CREATE => {
            typed(params, |p: AccountCreateParams| {
                let core = core.clone();
                async move {
                    let actor = p.actor.clone().unwrap_or_else(|| match peer {
                        PeerAuth::SameUid { uid } => format!("uid:{uid}"),
                        _ => "uid:unknown".to_string(),
                    });
                    // Plan-first: a NON-empty venue name must be REGISTERED.
                    let venue = p.venue.as_deref().map(str::trim).filter(|v| !v.is_empty());
                    if let Some(v) = venue
                        && registry.get(v).is_none()
                    {
                        let known = registry.names();
                        let known: Vec<&str> = known.iter().map(|s| s.as_str()).collect();
                        return Err(CoreError::new(
                            CoreErrorCode::InvalidParams,
                            format!(
                                "account.create: '{v}' is not a registered plugin — \
                                 the bindable set is {known:?}"
                            ),
                        ));
                    }
                    let id = AccountId::from(p.id.as_str());
                    let credential_keys = venue
                        .and_then(|v| registry.get(v))
                        .map(|plugin| {
                            crate::account::CredentialKeys::from_plugin_env(
                                plugin.required_credential_env(),
                            )
                        })
                        .unwrap_or_default();
                    let mut c = core.lock().await;
                    c.create_account_persisting(
                        &id,
                        p.name.as_deref().map(str::trim).filter(|n| !n.is_empty()),
                        venue,
                        credential_keys,
                        &actor,
                        p.reason.as_deref().unwrap_or(""),
                        now_ms(),
                    )?;
                    // The lock guard is dropped BEFORE the runtime mount
                    // below — the same re-entry shape as account.venue.set:
                    // mounting re-enters the core THROUGH the host (the feed
                    // start installs its subscription control, the discovery
                    // feeds its round markets), and holding the guard across
                    // that await would deadlock the session against itself.
                    drop(c);
                    // 运行时半边（绑定即启用 at runtime）：建户即绑定 —— 一个
                    // 带着非空 venue 的建户请求与「建户 + venue.set」是同一个
                    // 语义，必须同样把该 venue 的 feed/discovery 挂进本会话，
                    // 否则新建账户的行情永远停在「首行数据到达后渲染」
                    // （#535 用户实录）。挂载条件与 venue.set 逐字相同：
                    // boot 解析的旗标门 + 幂等（已挂载跳过）+ 注册表组件在。
                    // executor 同样不在运行时挂（may_trade 是 boot 冻结的
                    // 承诺，见 enable_venue_at_runtime 的文档）。
                    let mut runtime_enabled = false;
                    if let Some(v) = venue
                        && (data_plane.feed_ws_enabled || data_plane.discovery_enabled)
                        && !data_plane.venue_mounted(v)
                        && let Some(plugin) = registry.get(v)
                    {
                        // 轮次多族: the same family walk venue.set runs —
                        // the boot's global family plus the new account's
                        // `rounds` rows (idempotent per pair; the venue is
                        // fresh, so in practice nothing is filtered).
                        let mut fams =
                            vec![(v.to_string(), data_plane.discovery_round_duration_sec)];
                        if let Ok(c) = core.try_lock() {
                            for a in c.accounts().account_list() {
                                if a.venue == v {
                                    for &dur in &a.rounds {
                                        let pair = (v.to_string(), dur);
                                        if !fams.contains(&pair) {
                                            fams.push(pair);
                                        }
                                    }
                                }
                            }
                        }
                        fams.retain(|(vv, d)| vv != v || !data_plane.discovery_mounted_at(vv, *d));
                        if !fams.is_empty() {
                            enable_venue_at_runtime(
                                plugin.as_ref(),
                                host.clone(),
                                data_plane.feed_ws_enabled,
                                data_plane.discovery_enabled,
                                &data_plane.discovery_assets,
                                data_plane.discovery_round_duration_sec,
                                &fams,
                                registry,
                                data_plane,
                            )
                            .await;
                            // A feed_source consumer owns NO components (same
                            // posture as the venue.set arm): the mounts above
                            // were keyed by the SOURCE's name inside
                            // `enable_venue_at_runtime`, so the consumer's name
                            // is never marked.
                            if plugin.feed_source().is_none() {
                                for (_, d) in &fams {
                                    data_plane.mark_discovery_mounted(v, *d);
                                }
                                // The feed side keeps its per-venue guard (a
                                // re-bind must not restart the feed).
                                data_plane.mark_mounted(v);
                            }
                            runtime_enabled = true;
                        }
                    }
                    Ok(serde_json::to_value(AccountCreateResult {
                        id: id.as_str().to_string(),
                        persisted: true,
                        at_ms: now_ms(),
                        actor,
                        reason: p.reason,
                        note: ACCOUNT_CREATE_NOTE,
                        runtime_enabled,
                    })
                    .unwrap_or(Value::Null))
                }
            })
            .await
        }
        method::ACCOUNT_REMOVE => {
            typed(params, |p: AccountRemoveParams| {
                let core = core.clone();
                async move {
                    let actor = p.actor.clone().unwrap_or_else(|| match peer {
                        PeerAuth::SameUid { uid } => format!("uid:{uid}"),
                        _ => "uid:unknown".to_string(),
                    });
                    let id = AccountId::from(p.id.as_str());
                    let mut c = core.lock().await;
                    c.remove_account_persisting(&id, &actor, p.reason.as_deref().unwrap_or(""))?;
                    Ok(serde_json::to_value(AccountRemoveResult {
                        id: id.as_str().to_string(),
                        persisted: true,
                        at_ms: now_ms(),
                        actor,
                        reason: p.reason,
                        note: ACCOUNT_REMOVE_NOTE,
                    })
                    .unwrap_or(Value::Null))
                }
            })
            .await
        }
        method::ACCOUNT_JOURNAL => {
            typed(params, |p: AccountJournalParams| {
                let core = core.clone();
                async move {
                    let limit = p.limit.unwrap_or(100).clamp(1, 500);
                    let c = core.lock().await;
                    let id = match p.account_id.as_deref() {
                        Some(id) if !id.trim().is_empty() => AccountId::from(id),
                        _ => effective_default_account(session, c.accounts()),
                    };
                    let rows = c.accounts().ledger_of(&id)?.journal_snapshot();
                    let total = rows.len();
                    let tail: Vec<AccountJournalRow> = rows
                        .into_iter()
                        .skip(total.saturating_sub(limit))
                        .map(|e| AccountJournalRow {
                            at_ms: e.at_ms,
                            kind: e.kind,
                            amount: e.amount,
                            balance_after: e.balance_after,
                            r#ref: e.ref_,
                        })
                        .collect();
                    Ok::<_, CoreError>(
                        serde_json::to_value(AccountJournalResult {
                            account_id: id.as_str().to_string(),
                            truncated: total > tail.len(),
                            rows: tail,
                        })
                        .unwrap_or(Value::Null),
                    )
                }
            })
            .await
        }
        // Routed here only from `handle_line`'s dispatch, which sends exactly
        // this family's verbs; `&str` matching needs the exhaustive arm.
        _ => unreachable!("family handler called with a verb of another family"),
    }
}

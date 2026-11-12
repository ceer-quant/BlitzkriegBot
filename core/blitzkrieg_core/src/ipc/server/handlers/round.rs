use super::super::{DataPlaneRuntime, PeerAuth, enable_venue_at_runtime, typed};
use super::prelude::*;
use crate::schema::*;

// The `round` family verbs, moved verbatim out of `handle_line`'s dispatch match (issue 699).
//
// Pure code motion: arm bodies, error codes, audit lines and reply bytes are exactly
// the pre-split ones; the shared skeleton (READONLY gate, version gate, reply
// encoding) stays in `server.rs`.

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_round(
    method: &str,
    core: &Arc<AsyncMutex<Core>>,
    peer: &PeerAuth,
    host: &std::sync::Arc<dyn blitzkrieg_market_api::MarketHost>,
    registry: &crate::market::registry::MarketPluginRegistry,
    data_plane: &DataPlaneRuntime,
    params: Value,
) -> Result<Value, (i32, String, Option<ErrorData>)> {
    match method {
        method::ROUND_DURATION_GET => {
            let c = core.lock().await;
            Ok(
                serde_json::to_value(crate::service::round_duration_readout(&c))
                    .unwrap_or(Value::Null),
            )
        }
        method::ROUND_DURATION_SET => {
            typed(params, |p: SetRoundDurationParams| {
                let core = core.clone();
                let host = host.clone();
                async move {
                    let actor = match peer {
                        PeerAuth::SameUid { uid } => format!("uid:{uid}"),
                        _ => "uid:unknown".to_string(),
                    };
                    let update =
                        core.lock()
                            .await
                            .apply_round_duration_set(&p, &actor, now_ms())?;
                    // #1165 (L2/L3 契约): the grid write alone starves the
                    // account — see `ensure_round_family_mounted`.
                    ensure_round_family_mounted(&core, &host, registry, data_plane, &update).await;
                    Ok(serde_json::to_value(update).unwrap_or(Value::Null))
                }
            })
            .await
        }
        // Routed here only from `handle_line`'s dispatch, which sends exactly
        // this family's verbs; `&str` matching needs the exhaustive arm.
        _ => unreachable!("family handler called with a verb of another family"),
    }
}

/// #1165 (L2/L3 契约): a runtime grid write must reach the DATA PLANE, not
/// only the scanner. The family the account's scanner now runs
/// (`update.next_session_sec` — the override on a set, the default grid on a
/// reset) needs a discovery instance polling it; without one the feed never
/// carries that family's tokens, the scanner (grid_override = 选市权威)
/// rejects every declared market of the stale families, and the account
/// starves with "no live PairBook entry" — the exact live incident #1165
/// closes. Reuses `enable_venue_at_runtime` verbatim (no second mechanism):
/// its per-(venue, family) idempotence guard skips every family that
/// already runs, and its feed mount now carries the venue-mounted guard
/// inside, so a family addition on a live venue never restarts the feed.
async fn ensure_round_family_mounted(
    core: &Arc<AsyncMutex<Core>>,
    host: &std::sync::Arc<dyn blitzkrieg_market_api::MarketHost>,
    registry: &crate::market::registry::MarketPluginRegistry,
    data_plane: &DataPlaneRuntime,
    update: &crate::schema::RoundDurationUpdate,
) {
    if !data_plane.discovery_enabled {
        return;
    }
    let family = update.next_session_sec;
    // Venue dimension — the boot's same reading of a declaration: a bound
    // account speaks for ITS venue, an unbound one for every venue the
    // session actually runs.
    let venues: Vec<String> = {
        let c = core.lock().await;
        let bound = c
            .accounts()
            .account_list()
            .into_iter()
            .find(|a| a.id.as_str() == update.account_id)
            .map(|a| a.venue.clone())
            .unwrap_or_default();
        if !bound.is_empty() {
            vec![bound]
        } else {
            registry
                .names()
                .into_iter()
                .filter(|v| data_plane.venue_mounted(v))
                .collect()
        }
    };
    for venue in venues {
        if data_plane.discovery_mounted_at(&venue, family) {
            continue; // already running — nothing to mount
        }
        let Some(plugin) = registry.get(&venue) else {
            continue;
        };
        // #1165 (L3 自覆盖): a venue whose discovery walks its own declared
        // family table (`supported_round_secs`) feeds the target family from
        // its ONE running instance already — the rows land on the next poll
        // and the soft-unload gate admits them (the active set carries the
        // override). Mounting a second per-family instance would double every
        // family's Gamma queries and batches. Record coverage, mount nothing.
        // A family OUTSIDE the table (kalshi's [900], a plugin with no table)
        // still takes the per-family instance below — the B1d pool.
        if plugin
            .supported_round_secs()
            .is_some_and(|t| t.contains(&family))
        {
            data_plane.mark_discovery_mounted(&venue, family);
            data_plane.mark_mounted(&venue);
            continue;
        }
        // The core guard is NOT held across the mount (it re-enters the core
        // through the host) — the venue.set arm's same deadlock discipline.
        enable_venue_at_runtime(
            plugin.as_ref(),
            host.clone(),
            data_plane.feed_ws_enabled,
            data_plane.discovery_enabled,
            &data_plane.discovery_assets,
            data_plane.discovery_round_duration_sec,
            &[(venue.clone(), family)],
            registry,
            data_plane,
        )
        .await;
        // Same mark discipline as the venue.set arm: a feed_source consumer
        // owns no data plane of its own (the reuse arm records the source's
        // families internally); a direct mount records its own family — and
        // the venue mark, for the case where this family addition was the
        // venue's very first mount.
        if plugin.feed_source().is_none() {
            data_plane.mark_discovery_mounted(&venue, family);
            data_plane.mark_mounted(&venue);
        }
    }
}

use super::super::{PeerAuth, typed};
use super::prelude::*;
use crate::schema::*;

// The `market` family verbs, moved verbatim out of `handle_line`'s dispatch match (issue 699).
//
// Pure code motion: arm bodies, error codes, audit lines and reply bytes are exactly
// the pre-split ones; the shared skeleton (READONLY gate, version gate, reply
// encoding) stays in `server.rs`.

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_market(
    method: &str,
    core: &Arc<AsyncMutex<Core>>,
    registry: &crate::market::registry::MarketPluginRegistry,
    peer: &PeerAuth,
    params: Value,
) -> Result<Value, (i32, String, Option<ErrorData>)> {
    match method {
        method::MARKET_PLUGIN_GET => {
            let c = core.lock().await;
            Ok(
                serde_json::to_value(crate::market::plugin_readout(registry, &c))
                    .unwrap_or(Value::Null),
            )
        }
        method::MARKET_PLUGIN_SET => {
            typed(params, |p: SetMarketPluginParams| {
                let core = core.clone();
                async move {
                    let actor = match peer {
                        PeerAuth::SameUid { uid } => format!("uid:{uid}"),
                        _ => "uid:unknown".to_string(),
                    };
                    let update =
                        crate::market::apply_plugin_set(registry, &core, &p, &actor, now_ms())
                            .await?;
                    Ok(serde_json::to_value(update).unwrap_or(Value::Null))
                }
            })
            .await
        }
        method::MARKET_CREDENTIALS_STATUS => Ok(serde_json::to_value(
            crate::market::credentials_status(registry),
        )
        .unwrap_or(Value::Null)),
        method::MARKET_CREDENTIALS_SET => {
            typed(params, |p: SetCredentialsParams| {
                let core = core.clone();
                async move {
                    let actor = match peer {
                        PeerAuth::SameUid { uid } => format!("uid:{uid}"),
                        _ => "uid:unknown".to_string(),
                    };
                    let update =
                        crate::market::apply_credentials_set(registry, &core, &p, &actor, now_ms())
                            .await?;
                    Ok(serde_json::to_value(update).unwrap_or(Value::Null))
                }
            })
            .await
        }
        method::MARKET_LIST => {
            let plugins: Vec<serde_json::Value> = registry
                .list()
                .into_iter()
                .map(|p| {
                    // E27 (§8.3): the row carries the plugin's declared
                    // structure and the union of its capability bits — with
                    // the readable names beside the raw value so the modes
                    // gate can push the two against each other in both
                    // directions.
                    let caps = blitzkrieg_market_api::modes::capabilities_to_names(p.capabilities);
                    // The plugin's own round-family declaration (supported_
                    // round_secs): omitted entirely when undeclared — the
                    // panel filters nothing for such a plugin. Omitting the
                    // KEY (not null) keeps the wire shape for undeclared
                    // plugins byte-identical to the pre-declaration core.
                    let mut row = serde_json::json!({
                        "name": p.name,
                        "type": p.market_type,
                        "structure": p.structure,
                        "capabilities": caps,
                        "capabilitiesBits": p.capabilities.0,
                        "hasDataFeed": p.has_data_feed,
                        "hasDiscovery": p.has_discovery,
                        "hasExecutor": p.has_executor,
                        "enabled": p.enabled,
                        "active": p.active,
                    });
                    if let Some(secs) = p.supported_round_secs {
                        row["supportedRoundSecs"] = serde_json::json!(secs);
                    }
                    // RFC-661 M2: the declared feed reuse — the probe's
                    // 「consumer components ⊆ native venues」 assertion reads
                    // it, humans read it as「这个 venue 行情寄生在谁家」.
                    // Omitted entirely when undeclared — the wire shape for
                    // non-consumer plugins stays byte-identical.
                    if let Some(fs) = &p.feed_source {
                        row["feedSource"] = serde_json::json!(fs);
                    }
                    row
                })
                .collect();
            // 显示账户配置化: the display pointer rides on market.list so
            // the panel's 行情页 can name the account (and its venue
            // binding) it mirrors WITHOUT a second round-trip — the
            // 「账户交易所 ≠ 行情页交易所」 fix. Pure presentation: an
            // older client that ignores these keys loses nothing.
            let (display_account, display_venue) = {
                let c = core.lock().await;
                (
                    c.accounts().display_id().to_string(),
                    c.accounts().display_venue().to_string(),
                )
            };
            Ok(serde_json::json!({
                "version": PROTOCOL_VERSION,
                "active": registry.active(),
                "displayAccount": display_account,
                "displayVenue": display_venue,
                "plugins": plugins,
            }))
        }
        // Routed here only from `handle_line`'s dispatch, which sends exactly
        // this family's verbs; `&str` matching needs the exhaustive arm.
        _ => unreachable!("family handler called with a verb of another family"),
    }
}

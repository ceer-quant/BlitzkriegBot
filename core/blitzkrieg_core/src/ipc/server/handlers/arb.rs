use super::prelude::*;
use crate::ipc::schema::*;

// The `arb` family verbs, moved verbatim out of `handle_line`'s dispatch match (issue 699).
//
// Pure code motion: arm bodies, error codes, audit lines and reply bytes are exactly
// the pre-split ones; the shared skeleton (READONLY gate, version gate, reply
// encoding) stays in `server.rs`.

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_arb(
    method: &str,
    core: &Arc<AsyncMutex<Core>>,
    params: Value,
) -> Result<Value, (i32, String, Option<ErrorData>)> {
    match method {
        method::ARB_CONFIG_SET => {
            let params: crate::ipc::schema::ArbConfigSetParams =
                match serde_json::from_value(params) {
                    Ok(p) => p,
                    Err(e) => {
                        return Err((
                            Failure::PARSE_ERROR,
                            format!("invalid arb.config.set params: {e}"),
                            None,
                        ));
                    }
                };
            if params.version != 1 {
                Err((
                    Failure::APPLICATION,
                    format!(
                        "arb.config.set: `version` must be 1 (got {})",
                        params.version
                    ),
                    None,
                ))
            } else {
                let cfg = crate::account::CrossVenueArbConfig {
                    leg_a_venue: params.leg_a_venue,
                    leg_a_account: params.leg_a_account,
                    leg_b_venue: params.leg_b_venue,
                    leg_b_account: params.leg_b_account,
                    pairs: params.pairs,
                };
                let actor = params
                    .actor
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("panel")
                    .to_string();
                let mut c = core.lock().await;
                match c.arb_config_set_persisting(&cfg, &actor, "arb.config.set", now_ms()) {
                    Ok(()) => Ok(serde_json::json!({
                        "version": crate::ipc::schema::PROTOCOL_VERSION,
                        "configured": true,
                    })),
                    Err(e) => Err((Failure::APPLICATION, e.to_string(), None)),
                }
            }
        }
        method::ARB_CONFIG_GET => {
            let c = core.lock().await;
            let payload = match c.arb_leg_config() {
                Some(cfg) => serde_json::json!({
                    "version": crate::ipc::schema::PROTOCOL_VERSION,
                    "configured": true,
                    "legAVenue": cfg.leg_a_venue,
                    "legAAccount": cfg.leg_a_account,
                    "legBVenue": cfg.leg_b_venue,
                    "legBAccount": cfg.leg_b_account,
                    "pairs": cfg.pairs,
                }),
                None => serde_json::json!({
                    "version": crate::ipc::schema::PROTOCOL_VERSION,
                    "configured": false,
                    "legAVenue": null,
                    "legAAccount": null,
                    "legBVenue": null,
                    "legBAccount": null,
                    "pairs": [],
                }),
            };
            Ok(payload)
        }
        // Routed here only from `handle_line`'s dispatch, which sends exactly
        // this family's verbs; `&str` matching needs the exhaustive arm.
        _ => unreachable!("family handler called with a verb of another family"),
    }
}

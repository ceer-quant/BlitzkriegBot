use super::prelude::*;
use crate::schema::*;

// The `unified` family verbs, moved verbatim out of `handle_line`'s dispatch match (issue 699).
//
// Pure code motion: arm bodies, error codes, audit lines and reply bytes are exactly
// the pre-split ones; the shared skeleton (READONLY gate, version gate, reply
// encoding) stays in `server.rs`.

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_unified(
    method: &str,
    core: &Arc<AsyncMutex<Core>>,
    params: Value,
) -> Result<Value, (i32, String, Option<ErrorData>)> {
    match method {
        method::UNIFIED_LIST => {
            let c = core.lock().await;
            Ok(serde_json::json!({
                "version": crate::schema::PROTOCOL_VERSION,
                "events": c.unified_list(),
            }))
        }
        method::UNIFIED_CANDIDATES => {
            let c = core.lock().await;
            Ok(serde_json::json!({
                "version": crate::schema::PROTOCOL_VERSION,
                "candidates": c.unified_candidates(),
            }))
        }
        method::UNIFIED_CONFIRM => {
            let params: crate::schema::UnifiedConfirmParams = match serde_json::from_value(params) {
                Ok(p) => p,
                Err(e) => {
                    return Err((
                        Failure::PARSE_ERROR,
                        format!("invalid unified.confirm params: {e}"),
                        None,
                    ));
                }
            };
            let title = params.title.trim().to_string();
            let actor = params
                .actor
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("panel")
                .to_string();
            if title.is_empty() {
                Err((
                    Failure::APPLICATION,
                    "unified.confirm requires a non-empty `title`".to_string(),
                    None,
                ))
            } else if params.listings.len() < 2 {
                Err((
                    Failure::APPLICATION,
                    "unified.confirm requires at least two listings".to_string(),
                    None,
                ))
            } else {
                let mut c = core.lock().await;
                match c.unified_confirm(&title, &actor, params.listings, now_ms()) {
                    Ok(event) => Ok(serde_json::json!({
                        "version": crate::schema::PROTOCOL_VERSION,
                        "event": {
                            "id": event.id,
                            "title": event.title,
                            "status": event.status,
                            "venues": event.listings.values().map(|l| l.venue.as_str()).collect::<Vec<_>>(),
                        },
                    })),
                    Err(e) => Err((Failure::APPLICATION, e, None)),
                }
            }
        }
        method::UNIFIED_REVOKE => {
            let params: crate::schema::UnifiedRevokeParams = match serde_json::from_value(params) {
                Ok(p) => p,
                Err(e) => {
                    return Err((
                        Failure::PARSE_ERROR,
                        format!("invalid unified.revoke params: {e}"),
                        None,
                    ));
                }
            };
            let event_id = params.event_id.trim().to_string();
            let actor = params
                .actor
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("panel")
                .to_string();
            if event_id.is_empty() {
                Err((
                    Failure::APPLICATION,
                    "unified.revoke requires a non-empty `eventId`".to_string(),
                    None,
                ))
            } else {
                let mut c = core.lock().await;
                match c.unified_revoke(&event_id, &actor, now_ms()) {
                    Ok(()) => Ok(serde_json::json!({
                        "version": crate::schema::PROTOCOL_VERSION,
                        "revoked": event_id,
                    })),
                    Err(e) => Err((Failure::APPLICATION, e, None)),
                }
            }
        }
        // Routed here only from `handle_line`'s dispatch, which sends exactly
        // this family's verbs; `&str` matching needs the exhaustive arm.
        _ => unreachable!("family handler called with a verb of another family"),
    }
}

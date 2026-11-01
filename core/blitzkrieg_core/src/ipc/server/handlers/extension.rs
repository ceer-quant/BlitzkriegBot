use super::prelude::*;
use crate::schema::*;

// The `extension` family verbs, moved verbatim out of `handle_line`'s dispatch match (issue 699).
//
// Pure code motion: arm bodies, error codes, audit lines and reply bytes are exactly
// the pre-split ones; the shared skeleton (READONLY gate, version gate, reply
// encoding) stays in `server.rs`.

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_extension(
    method: &str,
    core: &Arc<AsyncMutex<Core>>,
    params: Value,
) -> Result<Value, (i32, String, Option<ErrorData>)> {
    match method {
        method::EXTENSION_LIST => {
            let c = core.lock().await;
            let list: Vec<_> = c
                .extension_list()
                .into_iter()
                .map(|(name, ty, state)| serde_json::json!({ "name": name, "type": ty, "state": state }))
                .collect();
            Ok(serde_json::json!({
                "version": crate::schema::PROTOCOL_VERSION,
                "extensions": list,
            }))
        }
        method::EXTENSION_ENABLE => {
            let name = params
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if name.is_empty() {
                Err((Failure::INVALID_PARAMS, "name required".into(), None))
            } else {
                match core.lock().await.enable_extension(&name).await {
                    Ok(()) => Ok(serde_json::json!({ "name": name, "enabled": true })),
                    Err(e) => Err((
                        Failure::APPLICATION,
                        e,
                        Some(ErrorData {
                            core_code: crate::model::CoreErrorCode::InvalidParams,
                            raw: None,
                        }),
                    )),
                }
            }
        }
        method::EXTENSION_DISABLE => {
            let name = params
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if name.is_empty() {
                Err((Failure::INVALID_PARAMS, "name required".into(), None))
            } else {
                match core.lock().await.disable_extension(&name).await {
                    Ok(()) => Ok(serde_json::json!({ "name": name, "enabled": false })),
                    Err(e) => Err((
                        Failure::APPLICATION,
                        e,
                        Some(ErrorData {
                            core_code: crate::model::CoreErrorCode::InvalidParams,
                            raw: None,
                        }),
                    )),
                }
            }
        }
        // Routed here only from `handle_line`'s dispatch, which sends exactly
        // this family's verbs; `&str` matching needs the exhaustive arm.
        _ => unreachable!("family handler called with a verb of another family"),
    }
}

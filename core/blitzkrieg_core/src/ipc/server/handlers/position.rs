use super::super::{PeerAuth, typed};
use super::prelude::*;
use crate::schema::*;

// The `position` family verbs, moved verbatim out of `handle_line`'s dispatch match (issue 699).
//
// Pure code motion: arm bodies, error codes, audit lines and reply bytes are exactly
// the pre-split ones; the shared skeleton (READONLY gate, version gate, reply
// encoding) stays in `server.rs`.

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_position(
    method: &str,
    core: &Arc<AsyncMutex<Core>>,
    peer: &PeerAuth,
    params: Value,
) -> Result<Value, (i32, String, Option<ErrorData>)> {
    match method {
        method::POSITIONS_LIST => {
            let views = core.lock().await.position_views(now_ms());
            Ok(serde_json::json!({ "positions": views }))
        }
        method::POSITION_EXIT => {
            typed(params, |p: PositionExitParams| {
                let core = core.clone();
                async move {
                    // `"all"` is the documented explicit spelling for the
                    // whole book; an omitted `positionId` keeps its historical
                    // meaning. A param set that does not deserialize is a
                    // client error — it must never decay into "flatten all"
                    // (issue 894).
                    let target = p.position_id.as_deref().filter(|id| *id != "all");
                    let closed = core.lock().await.flatten(target, now_ms())?;
                    Ok::<_, CoreError>(
                        serde_json::to_value(PositionExitResult { closed }).unwrap_or(Value::Null),
                    )
                }
            })
            .await
        }
        method::POSITION_FORCE_CLOSE => {
            typed(params, |p: PositionForceCloseParams| {
                let core = core.clone();
                async move {
                    let actor = match peer {
                        PeerAuth::SameUid { uid } => format!("uid:{uid}"),
                        _ => "uid:unknown".to_string(),
                    };
                    let closed = core.lock().await.force_close_expired_position(
                        &p.position_id,
                        &actor,
                        now_ms(),
                    )?;
                    Ok::<_, CoreError>(
                        serde_json::to_value(PositionForceCloseResult {
                            position_id: closed.id,
                            price: closed.exit_price,
                        })
                        .unwrap_or(Value::Null),
                    )
                }
            })
            .await
        }
        // Routed here only from `handle_line`'s dispatch, which sends exactly
        // this family's verbs; `&str` matching needs the exhaustive arm.
        // Issue 991: the unverified exposure report over IPC — every closed
        // position whose exit price is still a bookkeeping estimate
        // (`fallback` / `reconciled`), plus aggregates. Read-only; the
        // `UnverifiedExposureReport` type is the schema (serde camelCase,
        // decimal-as-number-or-string, the same wire rules every view wears).
        method::POSITIONS_UNVERIFIED_EXPOSURE => {
            let report = core.lock().await.unverified_exposure_report();
            Ok(serde_json::json!({
                "version": crate::schema::PROTOCOL_VERSION,
                "report": serde_json::to_value(report).unwrap_or(Value::Null),
            }))
        }
        _ => unreachable!("family handler called with a verb of another family"),
    }
}

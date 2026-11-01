use super::super::{PeerAuth, core_err, typed};
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
            let p: PositionExitParams = serde_json::from_value(params.clone())
                .unwrap_or(PositionExitParams { position_id: None });
            match core
                .lock()
                .await
                .flatten(p.position_id.as_deref(), now_ms())
            {
                Ok(n) => {
                    Ok(serde_json::to_value(PositionExitResult { closed: n })
                        .unwrap_or(Value::Null))
                }
                Err(e) => Err(core_err(e)),
            }
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
        _ => unreachable!("family handler called with a verb of another family"),
    }
}

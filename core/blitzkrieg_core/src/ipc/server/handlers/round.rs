use super::super::{PeerAuth, typed};
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
                async move {
                    let actor = match peer {
                        PeerAuth::SameUid { uid } => format!("uid:{uid}"),
                        _ => "uid:unknown".to_string(),
                    };
                    let update =
                        core.lock()
                            .await
                            .apply_round_duration_set(&p, &actor, now_ms())?;
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

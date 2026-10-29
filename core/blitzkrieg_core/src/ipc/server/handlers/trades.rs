use super::prelude::*;
use crate::ipc::schema::*;

// The `trades` family verbs, moved verbatim out of `handle_line`'s dispatch match (issue 699).
//
// Pure code motion: arm bodies, error codes, audit lines and reply bytes are exactly
// the pre-split ones; the shared skeleton (READONLY gate, version gate, reply
// encoding) stays in `server.rs`.

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_trades(
    method: &str,
    core: &Arc<AsyncMutex<Core>>,
    params: Value,
) -> Result<Value, (i32, String, Option<ErrorData>)> {
    match method {
        method::TRADES_HISTORY => {
            let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
            let trades = core.lock().await.recent_trades(limit);
            Ok(serde_json::json!({
                "version": crate::ipc::schema::PROTOCOL_VERSION,
                "trades": trades,
            }))
        }
        method::TRADES_SUMMARY => {
            let account = params.get("accountId").and_then(|v| v.as_str());
            let summary = core.lock().await.trade_summary(account);
            Ok(serde_json::json!({
                "version": crate::ipc::schema::PROTOCOL_VERSION,
                "summary": summary,
            }))
        }
        // Routed here only from `handle_line`'s dispatch, which sends exactly
        // this family's verbs; `&str` matching needs the exhaustive arm.
        _ => unreachable!("family handler called with a verb of another family"),
    }
}

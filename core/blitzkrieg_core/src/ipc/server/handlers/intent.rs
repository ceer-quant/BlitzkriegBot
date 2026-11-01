use super::super::typed;
use super::prelude::*;
use crate::schema::*;

// The `intent` family verbs, moved verbatim out of `handle_line`'s dispatch match (issue 699).
//
// Pure code motion: arm bodies, error codes, audit lines and reply bytes are exactly
// the pre-split ones; the shared skeleton (READONLY gate, version gate, reply
// encoding) stays in `server.rs`.

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_intent(
    method: &str,
    params: Value,
) -> Result<Value, (i32, String, Option<ErrorData>)> {
    match method {
        method::INTENT_AUDIT_TAIL => {
            typed(params, |p: IntentAuditTailParams| async move {
                // §12.3: 「返回最新的 N 条（读尾部，不读全文件）」—— the
                // backward tail scan, not the full `read_to_string` this arm
                // once ran: a long-lived panel polls this every 4 s, and on a
                // multi-hundred-MB audit log that read blew the gateway's IPC
                // read budget (`request timed out` on the decisions page) and
                // pinned a tokio worker each time. Filters run on the scanned
                // span; `total` names the span's matches and is omitted when
                // the span is not the whole file (no fake full counts).
                let limit = p.limit.unwrap_or(50);
                let account_id = p.account_id.clone();
                let strategy = p.strategy.clone();
                let decision = p.decision.clone();
                let (mut records, matched, whole_file) = tokio::task::spawn_blocking(move || {
                    crate::jsonl::tail_filtered::<Value>(
                        std::path::Path::new("data/audit/intents.jsonl"),
                        "intent audit log: skipped unparseable lines",
                        limit,
                        |rec: &Value| {
                            if let Some(f) = account_id.as_deref()
                                && rec.get("accountId").and_then(Value::as_str) != Some(f)
                            {
                                return false;
                            }
                            if let Some(f) = strategy.as_deref()
                                && rec.get("strategy").and_then(Value::as_str) != Some(f)
                            {
                                return false;
                            }
                            if let Some(f) = decision.as_deref()
                                && !rec
                                    .get("decision")
                                    .and_then(|d| d.get("status"))
                                    .and_then(Value::as_str)
                                    .is_some_and(|s| s.eq_ignore_ascii_case(f))
                            {
                                return false;
                            }
                            true
                        },
                    )
                })
                .await
                .unwrap_or((Vec::new(), 0, false));
                let total = whole_file.then_some(matched);
                records.truncate(limit);
                Ok::<_, CoreError>(
                    serde_json::to_value(IntentAuditTailResult { records, total })
                        .unwrap_or(Value::Null),
                )
            })
            .await
        }
        // Routed here only from `handle_line`'s dispatch, which sends exactly
        // this family's verbs; `&str` matching needs the exhaustive arm.
        _ => unreachable!("family handler called with a verb of another family"),
    }
}

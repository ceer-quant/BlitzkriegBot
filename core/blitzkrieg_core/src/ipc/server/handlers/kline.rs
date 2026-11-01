use super::super::{SessionState, typed};
use super::prelude::*;
use crate::schema::*;

// The `kline` family verbs, moved verbatim out of `handle_line`'s dispatch match (issue 699).
//
// Pure code motion: arm bodies, error codes, audit lines and reply bytes are exactly
// the pre-split ones; the shared skeleton (READONLY gate, version gate, reply
// encoding) stays in `server.rs`.

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_kline(
    method: &str,
    core: &Arc<AsyncMutex<Core>>,
    session: &mut SessionState,
    params: Value,
) -> Result<Value, (i32, String, Option<ErrorData>)> {
    match method {
        method::KLINE_HISTORY => {
            typed(params, |p: KlineHistoryParams| {
                let core = core.clone();
                async move {
                    let limit = p.limit.unwrap_or(200).clamp(1, 1000);
                    let c = core.lock().await;
                    let klines = c.kline_history(&p.symbol, p.interval, limit);
                    Ok::<_, CoreError>(
                        serde_json::to_value(KlineHistoryResult {
                            symbol: p.symbol,
                            interval: p.interval,
                            klines,
                        })
                        .unwrap_or(Value::Null),
                    )
                }
            })
            .await
        }
        method::KLINE_SUBSCRIBE => {
            typed(params, |p: KlineSubscribeParams| {
                let session_subs = session.kline_subs.clone();
                async move {
                    // A poisoned lock can only mean a peer task panicked while
                    // holding the set; refuse the call rather than fabricate a
                    // count — the subscription state is then unknowable.
                    let mut subs = session_subs.lock().map_err(|_| {
                        CoreError::new(
                            CoreErrorCode::Internal,
                            "kline subscription state is poisoned (peer task panicked)",
                        )
                    })?;
                    for s in &p.symbols {
                        for iv in &p.intervals {
                            subs.insert((s.clone(), *iv));
                        }
                    }
                    Ok::<_, CoreError>(
                        serde_json::to_value(KlineSubscribeResult {
                            subscribed: subs.len(),
                        })
                        .unwrap_or(Value::Null),
                    )
                }
            })
            .await
        }
        method::KLINE_UNSUBSCRIBE => {
            typed(params, |p: KlineSubscribeParams| {
                let session_subs = session.kline_subs.clone();
                async move {
                    let mut subs = session_subs.lock().map_err(|_| {
                        CoreError::new(
                            CoreErrorCode::Internal,
                            "kline subscription state is poisoned (peer task panicked)",
                        )
                    })?;
                    for s in &p.symbols {
                        for iv in &p.intervals {
                            subs.remove(&(s.clone(), *iv));
                        }
                    }
                    Ok::<_, CoreError>(
                        serde_json::to_value(KlineSubscribeResult {
                            subscribed: subs.len(),
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

use super::super::typed;
use super::prelude::*;
use crate::schema::*;

// The `backtest` family verbs, moved verbatim out of `handle_line`'s dispatch match (issue 699).
//
// Pure code motion: arm bodies, error codes, audit lines and reply bytes are exactly
// the pre-split ones; the shared skeleton (READONLY gate, version gate, reply
// encoding) stays in `server.rs`.

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_backtest(
    method: &str,
    core: &Arc<AsyncMutex<Core>>,
    jobs: &Arc<crate::backtest_jobs::BacktestJobs>,
    params: Value,
) -> Result<Value, (i32, String, Option<ErrorData>)> {
    match method {
        method::BACKTEST_ONCHAIN_PULL => {
            typed(params, |p: crate::backtest_jobs::OnchainPullParams| {
                let jobs = jobs.clone();
                async move {
                    let ids = jobs
                        .start_pull(p)
                        .map_err(|e| CoreError::new(CoreErrorCode::InvalidParams, e))?;
                    Ok(serde_json::json!({ "jobIds": ids }))
                }
            })
            .await
        }
        method::BACKTEST_ONCHAIN_LIST => {
            typed(params, |_: crate::backtest_jobs::OnchainListParams| {
                let jobs = jobs.clone();
                async move {
                    jobs.list_datasets()
                        .map_err(|e| CoreError::new(CoreErrorCode::Internal, e))
                }
            })
            .await
        }
        method::BACKTEST_RUN => {
            typed(params, |p: crate::backtest_jobs::BacktestRunParams| {
                let core = core.clone();
                let jobs = jobs.clone();
                async move {
                    // The replay needs a config SNAPSHOT, not the live core:
                    // trading keeps running while the job drives its own
                    // replay instance (no #199 ledger-dir contention — the
                    // backtester forces the no-log replay subset).
                    let cfg = {
                        let c = core.lock().await;
                        c.config().clone()
                    };
                    let id = jobs
                        .start_backtest(&cfg, p)
                        .map_err(|e| CoreError::new(CoreErrorCode::InvalidParams, e))?;
                    Ok(serde_json::json!({ "jobId": id }))
                }
            })
            .await
        }
        method::BACKTEST_STATUS => {
            typed(params, |p: crate::backtest_jobs::JobIdParams| {
                let jobs = jobs.clone();
                async move {
                    match jobs.status(&p.id) {
                        Some((view, result)) => {
                            let mut v = serde_json::to_value(&view).unwrap_or(Value::Null);
                            if let Some(r) = result
                                && let Some(obj) = v.as_object_mut()
                            {
                                obj.insert("result".into(), r);
                            }
                            Ok(v)
                        }
                        None => Err(CoreError::new(
                            CoreErrorCode::InvalidParams,
                            format!("unknown job {}", p.id),
                        )),
                    }
                }
            })
            .await
        }
        method::BACKTEST_RESULT => {
            typed(params, |p: crate::backtest_jobs::JobIdParams| {
                let jobs = jobs.clone();
                async move {
                    jobs.result(&p.id)
                        .map_err(|e| CoreError::new(CoreErrorCode::InvalidParams, e))
                }
            })
            .await
        }
        method::BACKTEST_EXPORT => {
            typed(params, |p: crate::backtest_jobs::JobIdParams| {
                let jobs = jobs.clone();
                async move {
                    jobs.export(&p.id)
                        .map_err(|e| CoreError::new(CoreErrorCode::InvalidParams, e))
                }
            })
            .await
        }
        // Routed here only from `handle_line`'s dispatch, which sends exactly
        // this family's verbs; `&str` matching needs the exhaustive arm.
        _ => unreachable!("family handler called with a verb of another family"),
    }
}

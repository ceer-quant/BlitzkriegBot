use super::super::{DataPlaneRuntime, PeerAuth, SOCKET_MODE, typed};
use super::prelude::*;
use crate::schema::*;

// The `misc` family verbs, moved verbatim out of `handle_line`'s dispatch match (issue 699).
//
// Pure code motion: arm bodies, error codes, audit lines and reply bytes are exactly
// the pre-split ones; the shared skeleton (READONLY gate, version gate, reply
// encoding) stays in `server.rs`.

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_misc(
    method: &str,
    core: &Arc<AsyncMutex<Core>>,
    registry: &crate::market::registry::MarketPluginRegistry,
    peer: &PeerAuth,
    data_plane: &DataPlaneRuntime,
    params: Value,
) -> Result<Value, (i32, String, Option<ErrorData>)> {
    match method {
        method::PING => Ok(serde_json::json!({ "pong": true, "ts": now_ms() })),
        method::READY => {
            let c = core.lock().await;
            Ok(serde_json::json!({
                "version": crate::CORE_VERSION,
                "mode": c.mode(),
                // WHICH PROCESS is answering (issue #200): an out-of-process
                // audit writes one line per poll, and two cores can be alive on
                // the same host (a fixture, a supervisor restart, the production
                // one). `commit` alone cannot tell a fresh dry core from the one
                // it replaced — the revision is the same — so the identity that
                // travels with every audit line needs the pid and the session
                // clock as well.
                "pid": std::process::id(),
                "startedAtMs": c.started_at_ms(),
                // Venue-side authentication (wallet signer/funder), NOT the IPC
                // peer check — that one is `peerVerified` below. Kept `false`
                // here so existing clients keep their meaning for this field.
                "authenticated": false,
                "signer": Value::Null,
                "funder": Value::Null,
                // Build provenance (#179): the revision this binary was compiled
                // from, so a client can state WHICH code answered it without
                // spawning `--version` beside the running core (#172).
                "build": crate::ipc::build_info::version_string(),
                "commit": crate::ipc::build_info::GIT_SHA,
                "dirty": crate::ipc::build_info::is_dirty(),
                // #187: the IPC trust boundary, as observed rather than assumed.
                "peerVerified": matches!(peer, PeerAuth::SameUid { .. }),
                "peerUid": peer.uid(),
                "peerAuth": peer.describe(),
                "socketMode": format!("{SOCKET_MODE:04o}"),
                // B2 (轮次多族 软卸): discovery-poll batches dropped because
                // their (venue, duration) family was soft-unloaded — the
                // operator's 「软卸还在漏喂吗」 readout (an incrementing
                // number here means a live instance is polling a family no
                // account declares). Incremental field: old clients ignore
                // it.
                "softUnloadedPollingInstances": data_plane.soft_unloaded_count(),
            }))
        }
        method::CORE_FEE_QUOTE => {
            let p: FeeQuoteParams =
                serde_json::from_value(params.clone()).unwrap_or(FeeQuoteParams { price: None });
            let quote = core.lock().await.fee_quote(p.price);
            Ok(serde_json::to_value(quote).unwrap_or(Value::Null))
        }
        method::LEDGER_BALANCE => {
            let c = core.lock().await;
            // The dry seed is only a principal while DRY: a live core's opening
            // cash is the venue's number, so it reports no principal at all
            // rather than a config value that never applied.
            let seed = match c.mode() {
                Mode::Dry => Some(c.config().dry_seed_balance),
                Mode::Live => None,
                // ReadOnly never reaches a venue, so like Dry its cash is the
                // seed — reporting none would misdescribe a funded simulation.
                Mode::ReadOnly => Some(c.config().dry_seed_balance),
            };
            // issue 787 + issue 749: same projection as `account_views` — a
            // live core whose ACTIVE book no venue has ever reported has no
            // venue number behind it, so whatever sits there is leftover (a
            // replayed dry journal, a stale seed). It must not cross the wire
            // as "the balance".
            //
            // The gate keys on the venue-report fact, NOT on
            // `CredentialKeys::loaded` (issue 749): a plugin that signs with
            // its own env declares nothing here, and an account whose keys did
            // load still has no number until the venue actually answers. The
            // first venue report re-establishes it after every boot.
            let unfunded_live = c.mode() == Mode::Live && !c.active_ledger_venue_reported();
            let (balance, reserved, available) = if unfunded_live {
                (Decimal::ZERO, Decimal::ZERO, Decimal::ZERO)
            } else {
                (
                    c.ledger().balance(),
                    c.ledger().reserved(),
                    c.ledger().available(),
                )
            };
            Ok(serde_json::to_value(BalanceResult {
                balance,
                reserved,
                available,
                seed,
            })
            .unwrap_or(Value::Null))
        }
        method::BOOK_SNAPSHOT => {
            typed(params, |p: BookSnapshotParams| {
                let core = core.clone();
                async move {
                    let bids: Vec<(Decimal, Decimal)> =
                        p.bids.into_iter().map(|l| (l.price, l.size)).collect();
                    let asks: Vec<(Decimal, Decimal)> =
                        p.asks.into_iter().map(|l| (l.price, l.size)).collect();
                    let now = now_ms();
                    let mut c = core.lock().await;
                    c.book_snapshot(&p.token_id, bids.clone(), asks.clone(), now);
                    // Feed the self-driving engine (inert if disabled).
                    if c.has_engine() {
                        c.engine_on_data(
                            crate::engine::DataEvent::Book {
                                token_id: p.token_id.clone(),
                                bids,
                                asks,
                                now_ms: now,
                            },
                            now,
                        );
                    }
                    Ok::<_, CoreError>(serde_json::json!({ "ok": true }))
                }
            })
            .await
        }
        method::TOP_OF_BOOK => {
            typed(params, |p: TopOfBookParams| {
                let core = core.clone();
                async move {
                    let now = now_ms();
                    let mut c = core.lock().await;
                    if let Some(b) = p.best_bid
                        && let Some(a) = p.best_ask
                    {
                        c.book_snapshot(
                            &p.token_id,
                            vec![(b, Decimal::ONE)],
                            vec![(a, Decimal::ONE)],
                            now,
                        );
                    }
                    if c.has_engine() {
                        c.engine_on_data(
                            crate::engine::DataEvent::TopOfBook {
                                token_id: p.token_id.clone(),
                                best_bid: p.best_bid,
                                best_ask: p.best_ask,
                                now_ms: now,
                            },
                            now,
                        );
                    }
                    Ok::<_, CoreError>(serde_json::json!({ "ok": true }))
                }
            })
            .await
        }
        method::SPOT_PRICE => {
            typed(params, |p: SpotPriceParams| {
                let core = core.clone();
                async move {
                    let now = now_ms();
                    let mut c = core.lock().await;
                    if c.has_engine() {
                        c.engine_on_data(
                            crate::engine::DataEvent::Spot {
                                asset: p.asset,
                                price: p.price,
                                now_ms: now,
                            },
                            now,
                        );
                    }
                    Ok::<_, CoreError>(serde_json::json!({ "ok": true }))
                }
            })
            .await
        }
        method::NET_CHECK => {
            // The probe follows the DISPLAY account (显示账户)：操作员读到的是
            // 他正在看的那个 venue 的网络判定——选择逻辑（含未绑定的旧语义
            // 回落）收在 `net_check_plugin`，那里有按名字的单测。
            let display_venue = {
                let c = core.lock().await;
                c.accounts().display_venue().to_string()
            };
            let plugin = crate::market::net_check_plugin(registry, display_venue.as_str());
            // The registry hands out an `Arc`, so the handle outlives the
            // borrow; the probe's real network I/O runs lock-free.
            let report = plugin.net_check().await;
            serde_json::to_value(report).map_err(|e| {
                (
                    Failure::APPLICATION,
                    format!("net.check report is not serializable: {e}"),
                    None,
                )
            })
        }
        // Routed here only from `handle_line`'s dispatch, which sends exactly
        // this family's verbs; `&str` matching needs the exhaustive arm.
        _ => unreachable!("family handler called with a verb of another family"),
    }
}

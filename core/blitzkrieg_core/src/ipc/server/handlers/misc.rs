use super::super::{
    DataPlaneRuntime, PeerAuth, SOCKET_MODE, SessionState, effective_default_account, typed,
};
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
    session: &SessionState,
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
            // The balance answers for the SESSION's default account (§9.5),
            // not the process default: `account.switch` on this connection
            // must move this read, or the panel's balance card stops
            // following the account switcher. Same resolution chain
            // account.list uses (session → display → process default).
            let active = effective_default_account(session, c.accounts());
            // The chain only names existing accounts (session ids pass
            // `require` at switch time; the display pointer's invariant
            // covers the tail), so the fallback is unreachable in practice —
            // but an unknown id must never 500 a read, hence the fallback.
            let ledger = c.accounts().ledger_of(&active).unwrap_or(c.ledger());
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
            // issue 1046: opening equity — the principal the cumulative net PnL
            // is measured against. DRY/ReadOnly: the configured seed. LIVE: the
            // venue's first reported equity, persisted to the account file exactly
            // once (never overwritten) and read back here. `None` until known.
            let opening_equity = match c.mode() {
                Mode::Dry | Mode::ReadOnly => seed,
                Mode::Live => c.accounts().account(&active).and_then(|a| a.opening_equity),
            };
            eprintln!("[DEBUG misc] opening_equity={:?} balance={:?}", opening_equity, ledger.balance());
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
            let unfunded_live = c.mode() == Mode::Live && !ledger.venue_reported();
            let (balance, reserved, available) = if unfunded_live {
                (Decimal::ZERO, Decimal::ZERO, Decimal::ZERO)
            } else {
                (ledger.balance(), ledger.reserved(), ledger.available())
            };
            // Cumulative net PnL = balance − opening_equity (the single ledger
            // truth, issue 1046 / #1063 fix). `balance` is venue-reported cash
            // gross of local reservations, so `reserved` must NOT be added back:
            // balance already equals available + reserved, and cumulative net
            // PnL measures equity against opening equity, not collateral.
            // None when opening_equity is unknown — never derived from trade folding.
            // `cumulative_net_pnl` is a pure function of (balance, opening_equity):
            // `reserved` does not appear in its signature, so it structurally
            // cannot influence the result.
            let cumulative_net_pnl = cumulative_net_pnl(balance, opening_equity);
            Ok(serde_json::to_value(BalanceResult {
                balance,
                reserved,
                available,
                seed,
                opening_equity,
                cumulative_net_pnl,
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

/// Cumulative net PnL = balance − opening_equity (issue 1046 / #1063).
///
/// `balance` is venue-reported cash gross of local reservations
/// (`available + reserved == balance`), so `reserved` must NOT be added back.
/// `reserved` does not appear in this function's signature at all, so it
/// structurally cannot inflate the reported net PnL — the double-count from
/// `balance + reserved − oe` is impossible here.
/// Returns `None` when `opening_equity` is unknown — never derived from
/// trade folding.
pub(crate) fn cumulative_net_pnl(
    balance: Decimal,
    opening_equity: Option<Decimal>,
) -> Option<Decimal> {
    opening_equity.map(|oe| balance - oe)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn cumulative_net_pnl_excludes_reserved_by_signature() {
        // #1063 regression: `reserved` is not a parameter of this pure
        // function, so it structurally cannot inflate the reported net PnL.
        // A resting BUY collateral of 50 must not change cumulative net PnL
        // versus 0 when balance and opening equity match.
        assert_eq!(
            cumulative_net_pnl(dec!(100), Some(dec!(80))),
            Some(dec!(20))
        );
        assert_eq!(cumulative_net_pnl(dec!(100), None), None);
        assert_eq!(cumulative_net_pnl(dec!(50), Some(dec!(50))), Some(dec!(0)));
    }
}

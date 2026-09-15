//! Unix-domain-socket JSON-RPC 2.0 server.
//!
//! One JSON object per line. Multiple Node sessions may connect; each receives
//! the full event stream. Requests are dispatched against the shared Core under
//! an async lock; a writer task per connection serialises responses and pushed
//! notifications onto the socket.

use crate::ipc::schema::*;
use crate::model::{CoreError, Mode};
use crate::service::{Core, CoreConfig};
use rust_decimal::Decimal;
use serde_json::Value;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex as AsyncMutex, broadcast, mpsc, oneshot};

/// Run the UDS server until the shutdown oneshot fires or a signal arrives.
pub async fn run(
    socket_path: String,
    config: CoreConfig,
    tick_ms: u64,
    shutdown: oneshot::Receiver<()>,
) -> anyhow::Result<()> {
    // Event plumbing: Core -> mpsc -> broadcast -> every session.
    let (evt_tx, mut evt_rx) = mpsc::unbounded_channel::<Event>();
    let (bus_tx, _) = broadcast::channel::<Event>(512);
    {
        let bus_tx = bus_tx.clone();
        tokio::spawn(async move {
            while let Some(ev) = evt_rx.recv().await {
                let _ = bus_tx.send(ev);
            }
        });
    }

    let mut core = Core::new(config.clone());
    core.set_event_sink(evt_tx);
    // Crash recovery FIRST: rebuild tracked orders from the durable log so any
    // order still resting at the venue is known (and thus cancellable) rather
    // than becoming an orphan. Done before the engine/feeds start.
    let recovered = core.restore_orders();
    if recovered > 0 {
        eprintln!("blitzkrieg-core: restored {recovered} live order(s) from the order log");
    }
    // Then rebuild the OPEN position book, so a restart keeps valuing and
    // exit-managing positions that were already filled before the restart.
    let recovered_pos = core.restore_positions();
    if recovered_pos > 0 {
        eprintln!(
            "blitzkrieg-core: restored {recovered_pos} open position(s) from the position log"
        );
    }
    if config.mode == Mode::Dry {
        core.set_balance(config.dry_seed_balance);
    }
    if config.engine_enabled {
        // One mapping shared with the backtester (`CoreConfig::install_engine`).
        config.install_engine(&mut core);
        eprintln!("blitzkrieg-core: self-driving engine enabled");
    }
    let core = Arc::new(AsyncMutex::new(core));

    // Market plugins: the concrete venue is compiled in via a feature and
    // registered here; the core drives its components through the market seam.
    let registry = crate::market::registry::MarketPluginRegistry::new();
    crate::market::register_builtin_markets(&registry);
    let active = crate::market::active_market_plugin(&registry, config.market_plugin.as_deref());
    let host: Arc<dyn blitzkrieg_market_api::MarketHost> =
        Arc::new(crate::market::host::CoreHost::new(core.clone()));

    // P4: Rust-native feeds. Start Binance spot now (symbols known at boot); the
    // orderbook subscriptions are added when `engine.markets` arrives (from Node)
    // or from Rust-native round discovery below.
    if config.feed_ws_enabled {
        if let Some(feed) = active.data_feed() {
            let cfg = blitzkrieg_market_api::DataFeedConfig {
                spot_assets: config.binance_assets.clone(),
                ws_url: None,
            };
            if let Err(e) = feed.start(host.clone(), cfg, Vec::new()).await {
                eprintln!("blitzkrieg-core: data feed start failed: {e}");
            }
            eprintln!(
                "blitzkrieg-core: rust-native feeds enabled (binance spot; poly on first round)"
            );
        }
    }

    // P5: Rust-native round discovery — the plugin finds its own markets and feeds
    // the engine + orderbook subscriptions, so Node is out of the data path.
    if config.engine_enabled && config.discovery_enabled {
        if let Some(disc) = active.discovery() {
            let cfg = blitzkrieg_market_api::DiscoveryConfig {
                assets: config.assets.clone(),
                round_duration_sec: config.round_duration_sec,
                poll_sec: 5,
            };
            if let Err(e) = disc.start(host.clone(), cfg).await {
                eprintln!("blitzkrieg-core: discovery start failed: {e}");
            }
            eprintln!("blitzkrieg-core: rust-native round discovery enabled");
        }
    }

    // Live mode: start the market's order executor (CLOB bridge: submits orders,
    // ingests user-WS fills, periodic REST reconciliation). The executor reads
    // credentials from the environment and stays inert when they are absent.
    if config.mode == Mode::Live {
        if let Some(exec) = active.executor() {
            let cfg = blitzkrieg_market_api::ExecutorConfig {
                markets: config.markets.clone(),
            };
            match exec.start(host.clone(), cfg).await {
                Ok(()) => eprintln!("blitzkrieg-core: live order executor started"),
                Err(e) => eprintln!("blitzkrieg-core: live executor failed to start: {e}"),
            }
        }
    }

    // Refuse to start if another core is already listening: the socket is a
    // shared singleton. Without this probe a second process would unlink the
    // live socket and bind its own, silently orphaning a healthy core (or
    // failing with EADDRINUSE under a concurrent-start race).
    if std::path::Path::new(&socket_path).exists() {
        if std::os::unix::net::UnixStream::connect(&socket_path).is_ok() {
            anyhow::bail!("another blitzkrieg-core is already listening on {socket_path}");
        }
        // Stale socket from a crashed process: safe to remove.
    }
    let _ = std::fs::remove_file(&socket_path);
    if let Some(parent) = std::path::Path::new(&socket_path).parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let listener = UnixListener::bind(&socket_path)?;
    eprintln!(
        "blitzkrieg-core listening on {socket_path} mode={:?} version={}",
        config.mode,
        crate::CORE_VERSION
    );
    // Log the EFFECTIVE risk/exit tuning at boot: a stale binary silently ran the
    // old wide stop for a whole soak; this makes the deployed parameters visible
    // in the run log without trusting that the running file matches current source.
    let ex = &config.positions.exit;
    eprintln!(
        "exit tuning: stop_loss={}% take_profit={}% trail_min={}% trail_arm={}% min_time_left={}s force_exit={}s maker_timeout={}ms",
        ex.stop_loss_pct,
        ex.take_profit_pct,
        ex.min_trail_pct,
        ex.trailing_min_high_pct,
        ex.min_time_left_sec,
        ex.force_exit_sec,
        config.default_maker_timeout_ms
    );

    // Maintenance loop: maker→taker escalation + pending-fill retry, and (when
    // the self-driving engine is enabled) a periodic evaluate→place cycle.
    {
        let core = core.clone();
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(std::time::Duration::from_millis(tick_ms.max(10)));
            loop {
                interval.tick().await;
                let mut c = core.lock().await;
                if let Err(e) = c.tick(now_ms()) {
                    tracing::warn!(error = %e, "core tick failed");
                }
                if c.has_engine() {
                    let n = c.engine_evaluate(now_ms());
                    if n > 0 {
                        tracing::info!(placed = n, "engine placed entries");
                    }
                }
            }
        });
    }

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;

    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            accept = listener.accept() => match accept {
                Ok((stream, _)) => spawn_session(stream, core.clone(), bus_tx.subscribe(), registry.clone()),
                Err(e) => tracing::warn!(error = %e, "accept failed"),
            },
            _ = sigterm.recv() => break,
            _ = sigint.recv() => break,
            _ = &mut shutdown => break,
        }
    }
    // Persist any pending near-miss records before exiting.
    core.lock().await.flush_near_misses();
    let _ = std::fs::remove_file(&socket_path);
    Ok(())
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn spawn_session(
    stream: UnixStream,
    core: Arc<AsyncMutex<Core>>,
    events: broadcast::Receiver<Event>,
    registry: crate::market::registry::MarketPluginRegistry,
) {
    tokio::spawn(async move {
        let (read_half, write_half) = stream.into_split();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();

        let writer = tokio::spawn(async move {
            let mut w = write_half;
            while let Some(line) = out_rx.recv().await {
                if w.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
            }
        });

        // Server -> client notifications.
        {
            let out_tx = out_tx.clone();
            tokio::spawn(async move {
                let mut events = events;
                loop {
                    match events.recv().await {
                        Ok(ev) => {
                            if let Ok(json) = serde_json::to_string(&Notification::new(ev)) {
                                if out_tx.send(format!("{json}\n")).is_err() {
                                    break;
                                }
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
        }

        let mut lines = BufReader::new(read_half).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) if !line.trim().is_empty() => {
                    let response = handle_line(&core, &registry, line).await;
                    if out_tx.send(format!("{response}\n")).is_err() {
                        break;
                    }
                }
                Ok(_) => break,
                Err(_) => break,
            }
        }
        drop(out_tx);
        let _ = writer.await;
    });
}

async fn handle_line(
    core: &Arc<AsyncMutex<Core>>,
    registry: &crate::market::registry::MarketPluginRegistry,
    line: String,
) -> String {
    let req: Request = match serde_json::from_str(&line) {
        Ok(r) => r,
        Err(e) => {
            return fail(
                Value::Null,
                Failure::PARSE_ERROR,
                format!("parse error: {e}"),
                None,
            );
        }
    };
    let id = req.id;
    let method = req.method.as_str();
    let params = req.params;

    let reply: Result<Value, (i32, String, Option<ErrorData>)> = match method {
        method::PING => Ok(serde_json::json!({ "pong": true, "ts": now_ms() })),

        method::READY => {
            let c = core.lock().await;
            Ok(serde_json::json!({
                "version": crate::CORE_VERSION,
                "mode": c.mode(),
                "authenticated": false,
                "signer": Value::Null,
                "funder": Value::Null,
            }))
        }

        method::RISK_KILL => {
            let reason = params
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("manual")
                .to_string();
            core.lock().await.kill(reason);
            Ok(serde_json::json!({ "killed": true }))
        }
        method::RISK_RESUME => {
            core.lock().await.resume();
            Ok(serde_json::json!({ "killed": false }))
        }

        method::ORDER_PLACE => {
            typed(params, |p: PlaceParams| {
                let core = core.clone();
                async move {
                    let mut c = core.lock().await;
                    let (order_id, status) = c.place(p.order, p.maker_timeout_ms, now_ms())?;
                    Ok::<_, CoreError>(
                        serde_json::to_value(PlaceResult { order_id, status })
                            .unwrap_or(Value::Null),
                    )
                }
            })
            .await
        }

        method::ORDER_CANCEL => {
            typed(params, |p: CancelParams| {
                let core = core.clone();
                async move {
                    core.lock().await.cancel(&p.order_id, now_ms())?;
                    Ok::<_, CoreError>(serde_json::json!({ "success": true }))
                }
            })
            .await
        }

        method::ORDER_CANCEL_ALL => {
            let p: CancelAllParams = serde_json::from_value(params.clone()).unwrap_or_default();
            match core
                .lock()
                .await
                .cancel_all(p.token_id.as_deref(), now_ms())
            {
                Ok(n) => {
                    Ok(serde_json::to_value(CancelAllResult { cancelled: n })
                        .unwrap_or(Value::Null))
                }
                Err(e) => Err(core_err(e)),
            }
        }

        method::ORDER_LIST => {
            let orders = core.lock().await.list_orders();
            Ok(serde_json::json!({ "orders": orders }))
        }

        method::LEDGER_BALANCE => {
            let c = core.lock().await;
            Ok(serde_json::to_value(BalanceResult {
                balance: c.ledger().balance(),
                reserved: c.ledger().reserved(),
                available: c.ledger().available(),
            })
            .unwrap_or(Value::Null))
        }

        method::ORDER_RECONCILE => {
            typed(params, |p: ReconcileParams| {
                let core = core.clone();
                async move {
                    let snap = crate::reconcile::VenueSnapshot {
                        open_order_ids: p.open_order_ids,
                        trades: p
                            .trades
                            .into_iter()
                            .map(|t| crate::reconcile::VenueTrade {
                                venue_order_id: t.venue_order_id,
                                trade_id: t.trade_id,
                                token_id: t.token_id,
                                side: t.side,
                                size: t.size,
                                price: t.price,
                                ts_ms: t.ts_ms,
                                tx_hash: None,
                            })
                            .collect(),
                        now_ms: now_ms(),
                    };
                    let r = core.lock().await.reconcile(snap)?;
                    Ok::<_, CoreError>(
                        serde_json::to_value(ReconcileResultView {
                            filled: r
                                .actions
                                .iter()
                                .filter(|a| {
                                    matches!(a, crate::reconcile::ReconcileAction::FilledGap { .. })
                                })
                                .count(),
                            marked_filled: r
                                .actions
                                .iter()
                                .filter(|a| {
                                    matches!(
                                        a,
                                        crate::reconcile::ReconcileAction::MarkedFilled { .. }
                                    )
                                })
                                .count(),
                            marked_cancelled: r
                                .actions
                                .iter()
                                .filter(|a| {
                                    matches!(
                                        a,
                                        crate::reconcile::ReconcileAction::MarkedCancelled { .. }
                                    )
                                })
                                .count(),
                            ghost_ids: r.suspect_ghost_ids,
                        })
                        .unwrap_or(Value::Null),
                    )
                }
            })
            .await
        }

        method::POSITIONS_LIST => {
            let views = core.lock().await.position_views(now_ms());
            Ok(serde_json::json!({ "positions": views }))
        }

        method::TRADES_HISTORY => {
            let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
            let trades = core.lock().await.recent_trades(limit);
            Ok(serde_json::json!({
                "version": crate::ipc::schema::PROTOCOL_VERSION,
                "trades": trades,
            }))
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

        method::ENGINE_BOOK => {
            typed(params, |p: BookSnapshotParams| {
                let core = core.clone();
                async move {
                    let bids: Vec<(Decimal, Decimal)> =
                        p.bids.into_iter().map(|l| (l.price, l.size)).collect();
                    let asks: Vec<(Decimal, Decimal)> =
                        p.asks.into_iter().map(|l| (l.price, l.size)).collect();
                    let now = now_ms();
                    let mut c = core.lock().await;
                    // Engine feed only: no `book_snapshot`, so no DRY maker-fill
                    // simulation. This is the path the Rust-native feed drives, and
                    // the one the archive/replay pair must agree with.
                    if c.has_engine() {
                        c.engine_on_data(
                            crate::engine::DataEvent::Book {
                                token_id: p.token_id,
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
                    if let Some(b) = p.best_bid {
                        if let Some(a) = p.best_ask {
                            c.book_snapshot(
                                &p.token_id,
                                vec![(b, Decimal::ONE)],
                                vec![(a, Decimal::ONE)],
                                now,
                            );
                        }
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

        method::ENGINE_STATS => Ok(core.lock().await.engine_stats()),

        method::STRATEGY_LIST => {
            let c = core.lock().await;
            let enabled = c.enabled_strategy_names();
            let strategies: Vec<_> = c
                .strategy_names()
                .into_iter()
                .map(|name| {
                    let is_on = enabled.contains(&name);
                    serde_json::json!({ "name": name, "enabled": is_on })
                })
                .collect();
            Ok(serde_json::json!({
                "version": crate::ipc::schema::PROTOCOL_VERSION,
                "strategies": strategies,
            }))
        }

        method::STRATEGY_ENABLE => {
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let enabled = params
                .get("enabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let found = core.lock().await.set_strategy_enabled(name, enabled);
            Ok(serde_json::json!({ "name": name, "enabled": enabled, "found": found }))
        }

        method::STRATEGY_LOAD => {
            let path = params.get("path").and_then(|v| v.as_str()).unwrap_or("");
            if path.is_empty() {
                Err((Failure::INVALID_PARAMS, "path required".into(), None))
            } else {
                let outcome = core.lock().await.load_strategy_lib(path);
                Ok(serde_json::to_value(outcome).unwrap_or(Value::Null))
            }
        }

        method::EXTENSION_LIST => {
            let c = core.lock().await;
            let list: Vec<_> = c
                .extension_list()
                .into_iter()
                .map(|(name, ty, state)| serde_json::json!({ "name": name, "type": ty, "state": state }))
                .collect();
            Ok(serde_json::json!({
                "version": crate::ipc::schema::PROTOCOL_VERSION,
                "extensions": list,
            }))
        }

        method::MARKET_LIST => {
            let plugins: Vec<serde_json::Value> = registry
                .list()
                .into_iter()
                .map(|p| {
                    serde_json::json!({
                        "name": p.name,
                        "type": p.market_type,
                        "hasDataFeed": p.has_data_feed,
                        "hasDiscovery": p.has_discovery,
                        "hasExecutor": p.has_executor,
                        "enabled": p.enabled,
                        "active": p.active,
                    })
                })
                .collect();
            Ok(serde_json::json!({
                "version": PROTOCOL_VERSION,
                "active": registry.active(),
                "plugins": plugins,
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

        method::SE_ENABLE => {
            let now = now_ms();
            core.lock().await.shadow_evolution_enable(now);
            Ok(serde_json::json!({ "enabled": true }))
        }
        method::SE_DISABLE => {
            core.lock().await.shadow_evolution_disable();
            Ok(serde_json::json!({ "enabled": false }))
        }
        method::SE_STATUS => {
            let now = now_ms();
            let mut c = core.lock().await;
            let status = c.shadow_evolution_status(now);
            let params = c.shadow_evolution().current_params();
            let variants = c.shadow_evolution_variants(now);
            let variant_count = c.shadow_evolution().variant_count();
            // Per-strategy blocks. Isolation is only observable if the caller can
            // see each strategy's own counters and knob values side by side.
            let strategies: Vec<serde_json::Value> = c
                .shadow_evolution()
                .strategy_names()
                .into_iter()
                .map(|name| {
                    let st = c.shadow_evolution_status_for(&name, now);
                    serde_json::json!({
                        "strategy": name,
                        "status": st,
                        "params": c.shadow_evolution().params_for(&name),
                        "knobs": c.shadow_evolution().declared_knobs(&name),
                        "evolutionsApplied": c.shadow_evolution().evolution_count(&name),
                        "evolutionsRejected": c.shadow_evolution().rejected_count(&name),
                        "secondsSinceLastEvolution": c
                            .shadow_evolution()
                            .seconds_since_last_evolution(&name, now),
                    })
                })
                .collect();
            let evolved: u64 = c
                .shadow_evolution()
                .strategy_names()
                .iter()
                .map(|n| c.shadow_evolution().evolution_count(n))
                .sum();
            let rejected: u64 = c
                .shadow_evolution()
                .strategy_names()
                .iter()
                .map(|n| c.shadow_evolution().rejected_count(n))
                .sum();
            let since = c
                .shadow_evolution()
                .strategy_names()
                .iter()
                .map(|n| c.shadow_evolution().seconds_since_last_evolution(n, now))
                .filter(|s| *s >= 0)
                .min()
                .unwrap_or(-1);
            Ok(serde_json::json!({
                "version": crate::ipc::schema::PROTOCOL_VERSION,
                // Aggregate keys kept unchanged so an existing Node consumer keeps
                // working; `strategies` carries the per-strategy detail.
                "status": status,
                "currentParams": params,
                "variantCount": variant_count,
                "variants": variants,
                "evolutionsApplied": evolved,
                "evolutionsRejected": rejected,
                "secondsSinceLastEvolution": since,
                "strategies": strategies,
            }))
        }
        method::SE_HISTORY => {
            let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
            let strategy = params
                .get("strategy")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let records = core
                .lock()
                .await
                .shadow_evolution_history(strategy.as_deref(), limit);
            Ok(serde_json::json!({
                "version": crate::ipc::schema::PROTOCOL_VERSION,
                "strategy": strategy,
                "history": records,
            }))
        }
        method::SE_ROLLBACK => {
            let now = now_ms();
            match params
                .get("strategy")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                None => Err((Failure::INVALID_PARAMS, "strategy required".into(), None)),
                Some(strategy) => {
                    match core.lock().await.shadow_evolution_rollback(strategy, now) {
                        Ok(_) => {
                            Ok(serde_json::json!({ "rolledBack": true, "strategy": strategy }))
                        }
                        Err(e) => Err((Failure::APPLICATION, e, None)),
                    }
                }
            }
        }
        method::SE_APPLY => match params
            .get("strategy")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            // Manual override for ONE strategy. Shape: {strategy, params:{knob:value}},
            // validated against that strategy's own declaration + domain + gradient.
            None => Err((Failure::INVALID_PARAMS, "strategy required".into(), None)),
            Some(strategy) => {
                let bag = params
                    .get("params")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                match serde_json::from_value::<crate::shadow_evolution::StrategyParams>(bag) {
                    Err(e) => Err((Failure::INVALID_PARAMS, format!("params: {e}"), None)),
                    Ok(typed) => {
                        let mut bag = crate::shadow_evolution::MutableParams::new();
                        bag.set_strategy(strategy, typed);
                        match core.lock().await.shadow_evolution_apply(bag, now_ms()) {
                            Ok(()) => {
                                Ok(serde_json::json!({ "applied": true, "strategy": strategy }))
                            }
                            Err(e) => Err((Failure::APPLICATION, e, None)),
                        }
                    }
                }
            }
        },

        method::ENGINE_ROUND => {
            let view = core.lock().await.round_view();
            Ok(serde_json::to_value(view).unwrap_or(Value::Null))
        }

        method::ENGINE_MARKETS => {
            typed(params, |p: EngineMarketsParams| {
                let core = core.clone();
                async move {
                    let now = now_ms();
                    let tokens: Vec<String> = p
                        .markets
                        .iter()
                        .flat_map(|m| [m.up_token_id.clone(), m.down_token_id.clone()])
                        .collect();
                    let mut c = core.lock().await;
                    if c.has_engine() {
                        c.engine_on_data(
                            crate::engine::DataEvent::RoundMarkets {
                                markets: p.markets,
                                now_ms: now,
                            },
                            now,
                        );
                    }
                    // P4: if Rust-native feeds are on, subscribe the round's tokens so
                    // Node no longer pushes books for them.
                    c.subscribe_feed_tokens(tokens).await;
                    Ok::<_, CoreError>(serde_json::json!({ "ok": true }))
                }
            })
            .await
        }

        other => Err((
            Failure::METHOD_NOT_FOUND,
            format!("unknown method: {other}"),
            None,
        )),
    };

    match reply {
        Ok(v) => serde_json::to_string(&Success::new(id, v)).unwrap_or_default(),
        Err((code, msg, data)) => fail(id, code, msg, data),
    }
}

/// Deserialize typed params then run the (async) handler.
async fn typed<T, F, Fut>(params: Value, f: F) -> Result<Value, (i32, String, Option<ErrorData>)>
where
    T: serde::de::DeserializeOwned,
    F: FnOnce(T) -> Fut,
    Fut: std::future::Future<Output = Result<Value, CoreError>>,
{
    let p: T = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => return Err((Failure::INVALID_PARAMS, e.to_string(), None)),
    };
    f(p).await.map_err(|e| {
        (
            Failure::APPLICATION,
            format!("{e}"),
            Some(ErrorData {
                core_code: e.code,
                raw: e.raw,
            }),
        )
    })
}

fn fail(id: Value, code: i32, message: String, data: Option<ErrorData>) -> String {
    serde_json::to_string(&Failure::new(id, code, message, data)).unwrap_or_default()
}

fn core_err(e: crate::model::CoreError) -> (i32, String, Option<ErrorData>) {
    let msg = format!("{e}");
    (
        Failure::APPLICATION,
        msg,
        Some(ErrorData {
            core_code: e.code,
            raw: e.raw,
        }),
    )
}

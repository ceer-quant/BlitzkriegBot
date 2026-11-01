//! Accept loop and per-connection session wiring (issue #780: split out of
//! the parent module; pure move).

use super::*;

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
    // Seed the simulated principal in every mode that settles locally.
    //
    // This site is why `Mode` is an enum and not a `readonly: bool`: as an `==`
    // comparison against `Dry` it was invisible to the compiler, so ReadOnly
    // silently started with a zero balance, reported a seed it did not have
    // (`BalanceResult::seed` was updated separately), and rejected every entry
    // with INSUFFICIENT_FUNDS. Behaviourally that made read-only look like a
    // broken dry mode rather than a promise — the local-settlement guarantee was
    // in place, the ledger it settles into was not.
    //
    // SKIPPED when a durable cash journal replayed inside `Core::new`
    // (`config.ledger_journal_dir` set and at least one journal file on
    // disk): the boot balance is the replayed trail then, and re-seeding
    // here would overwrite it — the exact fault the journal exists to fix
    // (a restart must not vaporize the盈亏 history).
    if config.mode.settles_locally() && !core.boot_replayed_journal() {
        core.set_balance(config.dry_seed_balance);
    }

    // Market plugins: the concrete venue is compiled in via a feature and
    // registered here; the core drives its components through the market seam.
    // E27 (§8.2): this MUST run before `install_engine`, which loads and
    // enables strategies — the startup `--enable-strategy` request and both
    // IPC handshake sites judge against the same declaration snapshot.
    // `None` would leave the handshake inert.
    let registry = crate::market::registry::MarketPluginRegistry::new();
    crate::market::register_builtin_markets(&registry);
    // The enablement chain resolves here — four layers, first non-empty wins:
    // 1. an explicit `--market-plugin` flag — everything else is ignored
    //    (replays and the corpus baselines pin their plugin this way);
    // 2. 绑定即启用: the account book's venue bindings — adding a venue's API
    //    account IS enabling that venue's plugin; ANY derived set outranks the
    //    persisted cell, which the boot then retires (migration, one INFO
    //    line, inside `boot_enablement`);
    // 3. the persisted cell (the old market.plugin.set write) — kept alive
    //    only while NO binding speaks;
    // 4. first-registered — inside `active_market_plugins`, the historical
    //    fallback.
    let mut plugin = config.market_plugin.clone();
    if plugin.is_none() {
        // Multi-plugin data plane: the value is a comma-separated list
        // ("polymarket,kalshi"; a single name is the historical form). A cell
        // naming ANY plugin that is not compiled in is refused as a whole
        // (same posture as the single-value cell — a stale entry must not
        // silently boot half a data plane), falling back to the next layer.
        let cell = config
            .runtime_config_path
            .as_deref()
            .and_then(|p| crate::runtime_config::load_market_plugin_from(std::path::Path::new(p)))
            .filter(|cell| {
                let names = crate::market::parse_plugin_list(&cell.plugin);
                !names.is_empty() && names.iter().all(|n| registry.get(n).is_some())
            });
        if let Some(cell) = &cell {
            eprintln!(
                "blitzkrieg-core: market plugin '{}' (persisted UI setting, \
                 data/runtime/config.json; an explicit --market-plugin and the \
                 accounts' venue bindings outrank it)",
                cell.plugin
            );
        } else {
            // A cell that loaded but did not arm (all-commas junk or an
            // unregistered name) keeps its historical refusal warning — the
            // operator wrote SOMETHING and must hear why it did not apply.
            if let Some(raw) = config
                .runtime_config_path
                .as_deref()
                .and_then(|p| {
                    crate::runtime_config::load_market_plugin_from(std::path::Path::new(p))
                })
                .map(|c| c.plugin)
                .filter(|v| !crate::market::parse_plugin_list(v).is_empty())
            {
                eprintln!(
                    "blitzkrieg-core: persisted market plugin '{}' is not fully compiled in — \
                     the cell is ignored (falling back to the flag/bindings/first-registered)",
                    raw
                );
            }
        }
        // The chain resolves through the shared `boot_enablement` — the same
        // function the readout re-derives with, so `market.plugin.get` can
        // never disagree with this boot: it returns the cell value (layer 3),
        // the DERIVED BINDING SET (layer 2 — the spec every venue's data
        // plane mounts from), or lets the fallback resolution run (layer 4).
        let (resolved, source) =
            crate::market::boot_enablement(&core, &registry, None, cell.as_ref());
        plugin = resolved;
        if source == crate::market::PluginEnableSource::Accounts {
            let venues = crate::market::derived_venue_set(&core);
            eprintln!(
                "blitzkrieg-core: market plugins {venues:?} enabled from the accounts' \
                 venue bindings (绑定即启用) — market.plugin.set is refused while this \
                 derivation is live"
            );
        }
    }
    let actives = crate::market::active_market_plugins(&registry, plugin.as_deref());
    // Strategy handshake, multi-plugin: EVERY selected plugin's declaration is
    // snapshotted and a strategy loads when ANY ONE active plugin can serve
    // it — a venue the strategy cannot serve must not block the venues it
    // can. An empty list (backtester, market-free builds) leaves the
    // handshake inert.
    core.set_plugin_modes(
        actives
            .iter()
            .map(|p| crate::compat::PluginModes::for_plugin(p.as_ref()))
            .collect(),
    );

    if config.engine_enabled {
        // A refusal here (the #265 startup self-check: an explicit
        // `--enable-strategy` that resolved to nothing) ends the boot BEFORE
        // the socket is bound or a feed is started — the failure mode is a
        // kernel that looks healthy and trades none of what it was told to run.
        if let Err(why) = config.install_engine(&mut core) {
            anyhow::bail!("{why}");
        }
        // The accounts file's strategy assignments, applied at boot: every
        // account with a non-empty `strategies` key gets those scopes
        // instantiated AND enabled (assignment IS enablement). The one-time
        // migration materializes the default account's implicit enabled set
        // into accounts.toml, after which the file is the only truth
        // (empty = the account runs nothing). Production-only: the replay/
        // backtest boot goes through `install_engine` alone and its
        // single-default book stays byte-identical (the exit-economics
        // baseline).
        let migrated = core.apply_account_strategy_assignments_at_boot();
        if !migrated.is_empty() {
            eprintln!(
                "blitzkrieg-core: strategy assignment migration — the default account now \
                 explicitly runs {:?} (accounts.toml; empty assignments mean a strategy-less \
                 account, there is no inherit)",
                migrated
            );
        }
        eprintln!("blitzkrieg-core: self-driving engine enabled");
    }
    let core = Arc::new(AsyncMutex::new(core));

    // 任务二 1-3: the NTP clock-offset probe. PRODUCTION-ONLY (the same
    // structural posture as `apply_account_strategy_assignments_at_boot`):
    // this block lives in `server::run`, which the replay/backtest driver
    // NEVER enters — a replay touches no NTP socket. The empty host (the
    // documented `--ntp-host ""` spelling) spawns ZERO tasks via
    // `should_spawn_ntp`; interval 0 likewise. The measurement loop runs on
    // its own thread (blocking UDP, 1.5 s timeout worst case) and only takes
    // the core lock for the instant it reports one result.
    {
        let ntp_host = config.ntp_host.clone();
        let ntp_interval = config.ntp_interval_secs;
        if crate::probe::should_spawn_ntp(&ntp_host) && ntp_interval > 0 {
            let core = core.clone();
            // The probe thread has no reactor: capture the Handle HERE (we are
            // inside the async runtime) and spawn through it from the closure.
            let rt = tokio::runtime::Handle::current();
            crate::probe::spawn_loop(
                ntp_host,
                ntp_interval,
                std::sync::Arc::new(move |offset| {
                    // Fire-and-forget: the probe never blocks trading on NTP.
                    let core = core.clone();
                    rt.spawn(async move {
                        let mut c = core.lock().await;
                        c.note_ntp_offset(offset, now_ms());
                    });
                }),
            );
            eprintln!("blitzkrieg-core: NTP clock-offset probe started (every {ntp_interval}s)");
        }
    }

    // The data plane's runtime handle: feeds/discovery mount THROUGH this
    // host, at boot AND later (account.venue.set re-binding an account to a
    // venue whose plugin never booted mounts that venue's components here —
    // the runtime half of 绑定即启用; see `enable_venue_at_runtime`).
    // The boot-resolved data-plane shape the venue.set write consults: the
    // mount flags and the already-mounted (venue, duration) discovery pairs
    // (idempotence guard) — the global family per boot plugin, plus every
    // account `rounds` row (轮次多族's boot instance pool).
    let account_families: Vec<crate::account::Account> = core
        .lock()
        .await
        .accounts()
        .account_list()
        .into_iter()
        .cloned()
        .collect();
    let data_plane = DataPlaneRuntime::at_boot(&config, &actives, &account_families, &registry);

    // RFC-661 M2: the feed_source reuse plane. The cell lives on the data
    // plane (`at_boot` seeded it with the boot-ACTIVE consumers' declared
    // edges — an inactive plugin registers nothing); the concrete host gets
    // an Arc-shared clone below so its `on_round_markets` fan-out reads the
    // same edges the mount path writes. `mounts` counts every REUSED-stream
    // mount attempt — the falsifiable claim「上游连接一份」reads it (one per
    // source stream, N consumers notwithstanding). A later runtime bind
    // appends its edge through `enable_venue_at_runtime` on the SAME cell.
    {
        let feed_reuse = data_plane.feed_reuse().expect("boot installs the cell");
        let declared_edges = DataPlaneRuntime::declared_feed_edges(&actives, &registry);
        if !declared_edges.is_empty() {
            let edges = feed_reuse.edges.read().expect("feed reuse edges").clone();
            eprintln!(
                "blitzkrieg-core: feed_source reuse active — upstream stream(s) of {:?} \
                 also feed(s) {:?}",
                edges
                    .iter()
                    .map(|e| e.source.as_str())
                    .collect::<std::collections::BTreeSet<_>>(),
                edges
                    .iter()
                    .map(|e| e.consumer.as_str())
                    .collect::<Vec<_>>(),
            );
        }
    }

    // B2 (轮次多族 软卸): the discovery poll-output gate, installed on the
    // CONCRETE CoreHost before it erases into the trait object. Every
    // extension discovery funnels through `CoreHost::on_round_markets`; from
    // here on, a batch whose (venue, duration) family no account declares
    // (and the global family does not cover) is dropped at the host — the
    // soft-unloaded instance keeps polling, its output never reaches the
    // engine. The closure carries the data plane's gauge + active-family
    // computation and the Core handle.
    let concrete_host = {
        let h = crate::market::host::CoreHost::new(core.clone());
        // Untagged round lists (legacy corpora / Node bridge) route their
        // orderbook subscription to the active plugin — the historical
        // single-plugin behaviour, resolved at assembly instead of hardcoded.
        h.set_active_plugin(actives[0].name());
        {
            let gate_core = core.clone();
            let gate_plane = data_plane.clone();
            h.set_poll_admission(std::sync::Arc::new(move |markets: Vec<_>| {
                let core = gate_core.clone();
                // The gate runs on the plugin's poll task; the admission
                // check is async (it takes the Core lock). The forward the
                // gate precedes blocks on that same lock anyway, so blocking
                // here changes no deadlock surface: the runtime is
                // multi-thread (`rt-multi-thread` in Cargo.toml), so
                // `block_in_place` moves the poll task off the shared
                // workers while it waits. A runtime-less caller (none
                // today) admits by default — the historical shape.
                match tokio::runtime::Handle::try_current() {
                    Ok(rt) => tokio::task::block_in_place(|| {
                        rt.block_on(gate_plane.admit_poll_batch(&core, &markets))
                    }),
                    Err(_) => true,
                }
            }));
        }
        h
    };
    // The trait-erased handle every feed/discovery/session share. It carries
    // no own-plugin identity ("" = legacy all-take on the scoped settlement
    // drains) but re-sees the SHARED active-plugin cell set above — the
    // untagged-order owner — through the Arc; the executor loop below
    // additionally names each handle with `for_plugin`.
    let host: Arc<dyn blitzkrieg_market_api::MarketHost> = Arc::new(concrete_host.clone());

    // RFC-661 M2: the fan-out rides the CONCRETE host (every clone shares the
    // cell — the same Arc shape as poll_admission), so the upstream feed's
    // pushes and every later runtime mount re-see the same edges. The cell is
    // the DATA PLANE's (the runtime mount path writes through the same
    // Arc-shared edges; this clone makes the host's fan-out read them).
    concrete_host.set_feed_reuse(
        data_plane
            .feed_reuse()
            .expect("boot installs the cell")
            .clone(),
    );

    // P4: Rust-native feeds. Start Binance spot now (symbols known at boot); the
    // orderbook subscriptions are added when `engine.markets` arrives (from Node)
    // or from Rust-native round discovery below.
    //
    // Multi-plugin data plane: EVERY selected plugin's feed starts (each on the
    // shared CoreHost — the Core lock just queues the pushes). Tokens stay
    // per-plugin: each feed only ever receives the tokens its own venue reports.
    //
    // RFC-661 M2 (feed_source 复用管道): a plugin that declares `feed_source`
    // owns NO upstream of its own — its feed/discovery never start (the host's
    // fan-out re-registers the source's batches under its venue identity). The
    // SOURCE's own feed start bumps the reuse mount counter: the falsifiable
    // claim「上游连接一份」reads this counter (one mount per upstream stream,
    // no matter how many consumers enable). A consumer-only enablement (source
    // inactive at boot) mounts the source's DATA plane through the same loop
    // — the source's executor stays out of `actives`, so execution never
    // starts (数据消费与执行启用解耦).
    // RFC-661 M2: the source venues this boot's actives declare as their feed
    // upstream (compiled-in only) — the streams the reuse pipeline must mount
    // EXACTLY once across both the feed and the discovery walks below. Read
    // once here so the two walks agree on what "reused" means.
    let reused_sources: std::collections::BTreeSet<String> = actives
        .iter()
        .filter_map(|p| p.feed_source())
        .filter(|src| registry.get(src).is_some())
        .map(str::to_string)
        .collect();
    if config.feed_ws_enabled {
        let cfg = blitzkrieg_market_api::DataFeedConfig {
            spot_assets: config.binance_assets.clone(),
        };
        // RFC-661 M2: a consumer's own feed NEVER starts (its upstream is the
        // source's stream — a second feed would be the second connection the
        // reuse pipeline exists to prevent). The mounts this boot owes: every
        // active plugin's own feed (consumers skipped), PLUS the source's feed
        // when NO consumer's source is among the actives (consumer-only boot —
        // the data plane is self-sufficient, the executor is not invited).
        // The source is resolved through the DECLARED edges only — a plugin
        // whose feed_source names nothing compiled-in keeps its own feed.
        let mut feeds_to_start: Vec<std::sync::Arc<dyn blitzkrieg_market_api::MarketPlugin>> =
            Vec::new();
        for p in &actives {
            if p.feed_source()
                .filter(|src| registry.get(src).is_some())
                .is_some()
            {
                continue;
            }
            feeds_to_start.push(std::sync::Arc::clone(p));
        }
        for src in reused_sources
            .iter()
            .filter(|src| !actives.iter().any(|a| a.name() == src.as_str()))
        {
            let Some(sp) = registry.get(src) else {
                continue;
            };
            eprintln!(
                "blitzkrieg-core: {src} consumers — mounting {src}'s data feed (no executor)"
            );
            feeds_to_start.push(sp);
        }
        // Mount-count discipline: every mount attempt of a REUSED stream
        // (a source some consumer declares) counts once — N consumers of the
        // same source still read 1. The consumer's own (skipped) start never
        // counts: it must not exist.
        let reuse = data_plane.feed_reuse().expect("boot installs the cell");
        for p in &feeds_to_start {
            if reused_sources.contains(p.name()) {
                reuse
                    .mounts
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            if let Some(feed) = p.data_feed() {
                if let Err(e) = feed.start(host.clone(), cfg.clone(), Vec::new()).await {
                    eprintln!(
                        "blitzkrieg-core: data feed start failed for {}: {e}",
                        p.name()
                    );
                } else {
                    eprintln!("blitzkrieg-core: feed started for {}", p.name());
                }
            }
        }
        eprintln!("blitzkrieg-core: rust-native feeds enabled (binance spot; poly on first round)");
    }

    // P5: Rust-native round discovery — the plugin finds its own markets and feeds
    // the engine + orderbook subscriptions, so Node is out of the data path.
    // Per-plugin loops: each discovery reports ONLY its own venue's markets and
    // subscribes only its own tokens.
    //
    // B1d (轮次多族): the loop walks the DECLARED (venue, duration) pairs —
    // the boot's global family on every plugin, plus every account `rounds`
    // row — instead of one instance per plugin. Each pair starts its own
    // discovery with that duration baked into the config (the plugin picks
    // its series/slug from it); the pairs were pre-recorded in
    // `data_plane.discovery_mounted` by `at_boot`, and the set walk here is
    // the same information. A single-family deployment declares nothing but
    // the global value, so the walk degenerates to exactly the historical
    // one-instance-per-plugin shape — the replay/corpus byte-identity
    // posture.
    if config.engine_enabled && config.discovery_enabled {
        let pairs: Vec<(String, i64)> = data_plane
            .discovery_mounted
            .lock()
            .expect("data plane mounted set poisoned")
            .iter()
            .cloned()
            .collect();
        // RFC-661 M2: a consumer's (venue, duration) family rides the SOURCE
        // plugin's discovery (one upstream stream). Translate consumer pairs
        // to their source before mounting; the consumer's own plugin supplies
        // no discovery instance (its registrar is the host's fan-out). The
        // translation runs through the DECLARED edges only — a declaration
        // naming nothing compiled-in keeps its own family.
        let resolve_source = |venue: &str| -> String {
            actives
                .iter()
                .find(|p| p.name() == venue)
                .and_then(|p| p.feed_source())
                .filter(|src| registry.get(src).is_some())
                .map(str::to_string)
                .unwrap_or_else(|| venue.to_string())
        };
        let mut pairs: Vec<(String, i64)> = pairs
            .into_iter()
            .map(|(v, d)| (resolve_source(&v), d))
            .collect();
        pairs.sort();
        pairs.dedup();
        let reuse = data_plane.feed_reuse().expect("boot installs the cell");
        for (venue, duration) in &pairs {
            let plugin: std::sync::Arc<dyn blitzkrieg_market_api::MarketPlugin> =
                match actives.iter().find(|p| p.name() == *venue) {
                    Some(p) => std::sync::Arc::clone(p),
                    None => {
                        // Consumer-only enablement: the source plugin is not in
                        // `actives` (its executor must NOT start) but its data
                        // components are the upstream every consumer eats —
                        // mount them from the registry (data-plane
                        // self-sufficiency). `resolve_source` already mapped
                        // the consumer venue to its source, so the miss here
                        // IS the consumer-only shape.
                        match registry.get(venue) {
                            Some(p) => p,
                            None => continue,
                        }
                    }
                };
            let Some(disc) = plugin.discovery() else {
                continue;
            };
            let cfg = blitzkrieg_market_api::DiscoveryConfig {
                assets: config.assets.clone(),
                round_duration_sec: *duration,
                poll_sec: 5,
            };
            // The mount-count discipline mirrors the feed loop above: a REUSED
            // stream's discovery mount (a source some consumer declares)
            // counts once per (source, family).
            if reused_sources.contains(venue) {
                reuse
                    .mounts
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            if let Err(e) = disc.start(host.clone(), cfg).await {
                eprintln!("blitzkrieg-core: discovery start failed for {venue}@{duration}s: {e}");
            } else if *duration == config.round_duration_sec {
                eprintln!("blitzkrieg-core: discovery started for {venue}");
            } else {
                eprintln!("blitzkrieg-core: discovery started for {venue} @ {duration}s family");
            }
        }
        eprintln!("blitzkrieg-core: rust-native round discovery enabled");
    }

    // Live mode: start the market's order executor (CLOB bridge: submits orders,
    // ingests user-WS fills, periodic REST reconciliation). The executor reads
    // credentials from the environment and stays inert when they are absent.
    //
    // This block IS the structural guarantee (E12-e). Dry never places a real
    // order because the egress object is never constructed — there is no
    // `LiveVenue`, no actor loop, and no consumer of `take_pending_orders`, so a
    // pending order simply accumulates in the OME. `--readonly` is therefore not
    // a second guard bolted on top: it refuses entry to this same block, which is
    // why `may_trade()` is the single predicate rather than a chain of conditions
    // each future edit could widen back open.
    if config.mode.may_trade() {
        // Multi-plugin: EVERY selected plugin with an executor starts its own
        // instance. Orders/cancels drain venue-routed by their `plugin`
        // argument; the settlement drains carry no caller name (frozen ABI),
        // so the executor gets a `for_plugin` host handle whose `own_plugin`
        // names ITSELF (#647) — the redemption/merge queues inside the host
        // are venue-bucketed and this handle pulls only its own venue's
        // claims (plus untagged ones, the single-plugin fallback). The shared
        // `active_plugin` cell is NOT touched (#775): it stays the assembly's
        // `actives[0]`, the one owner untagged ORDERS fall back to. Data-plane
        // handles keep the empty own-plugin (legacy all-take); only the
        // executor is named.
        for p in &actives {
            if let Some(exec) = p.executor() {
                // `markets` is the operator's global `--market` condition-id
                // subscription list, not a per-venue allocation — it gates the
                // user-WS subscribe set and is passed through untouched.
                let cfg = blitzkrieg_market_api::ExecutorConfig {
                    markets: config.markets.clone(),
                };
                match exec
                    .start(Arc::new(concrete_host.for_plugin(p.name())), cfg)
                    .await
                {
                    Ok(()) => {
                        eprintln!(
                            "blitzkrieg-core: live order executor started for {}",
                            p.name()
                        );
                    }
                    Err(e) => {
                        eprintln!(
                            "blitzkrieg-core: live executor failed to start for {}: {e}",
                            p.name()
                        )
                    }
                }
            }
        }
    } else if config.mode.readonly() {
        eprintln!(
            "blitzkrieg-core: READ-ONLY — venue order egress is not constructed; \
             this process cannot place an order"
        );
    }

    claim_socket_path(&socket_path)?;
    let listener = UnixListener::bind(&socket_path)?;
    // Owner-only, immediately after bind (#187). The mode is reported at boot
    // because "the socket exists" says nothing about who may connect to it — the
    // audit found `srwxr-xr-x`, i.e. the full trading control surface readable
    // and writable by every local user.
    let mode = match restrict_socket(&socket_path) {
        Ok(m) => format!("{m:04o}"),
        Err(e) => {
            // Fail LOUD but keep serving: a filesystem that refuses chmod (some
            // network mounts) still gets the peer-uid check below.
            eprintln!(
                "blitzkrieg-core: WARNING could not restrict {socket_path} to {:04o}: {e} — \
                 the socket may be connectable by other local users",
                SOCKET_MODE
            );
            "unknown".to_string()
        }
    };
    eprintln!(
        "blitzkrieg-core listening on {socket_path} mode={:?} version={} socketMode={mode} peerAuth={} logLevel={}",
        config.mode,
        crate::CORE_VERSION,
        peer_auth_support(),
        // #184: the level this process actually records, on the same banner as the
        // socket mode — "was this run quiet by design or by accident?" is
        // answerable from the run log alone.
        crate::logging::effective_level()
    );
    // Log the EFFECTIVE risk/exit tuning at boot: a stale binary silently ran the
    // old wide stop for a whole soak; this makes the deployed parameters visible
    // in the run log without trusting that the running file matches current source.
    let ex = &config.positions.exit;
    eprintln!(
        "exit tuning: stop_loss={}% take_profit={}% trailing={} trail_min={}% trail_arm={}% min_time_left={}s force_exit={}s maker_timeout={}ms",
        ex.stop_loss_pct,
        ex.take_profit_pct,
        ex.trailing_enabled,
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

    // Process-level update state (VERSIONING.md §5.5): deliberately outside
    // `Core`, so version questions never wait behind the trading lock. The
    // initial switches come from the resolved boot config (CLI > env > TOML;
    // built-in default off); the runtime cell persisted in
    // `data/update/state.json` outranks all three and is restored first. The
    // startup check spawns ONLY if the (restored) switch is on — with the
    // shipped defaults this mount is a no-op and the stack makes zero
    // outbound connections.
    let update_state = Arc::new(crate::ipc::version::UpdateState::new(
        config.update_check_enabled,
        config.update_auto,
    ));
    // #379: the automatic-check clock is boot configuration (never written by
    // the UI switches); it rides the same state cell so the scheduler reads it.
    update_state.set_auto_check_interval_secs(config.update_interval_secs);
    crate::ipc::version::load_into(&update_state);
    let github_token = std::env::var("BLITZKRIEG_GITHUB_TOKEN")
        .ok()
        .filter(|t| !t.trim().is_empty());
    crate::ipc::version::spawn_startup_check(
        update_state.clone(),
        crate::ipc::build_info::BUILD_INFO.version.to_string(),
        github_token.clone(),
    );
    // #379 (§7.4): the automatic-check scheduler. A no-op task-less return
    // unless the interval is configured AND the check switch is on — with the
    // shipped defaults (interval 0) nothing is spawned and nothing dials.
    // No shutdown wiring on purpose: the task lives inside this runtime, so
    // it ends when the server's process ends — an extra channel would only
    // widen the surface for a lost wakeup.
    let _auto_check = crate::ipc::version::spawn_auto_check_scheduler(
        update_state.clone(),
        crate::ipc::build_info::BUILD_INFO.version.to_string(),
        github_token.clone(),
        std::future::pending(),
    );
    // The `risk.limits` readout (§4.4) is assembled per answer from the
    // LIVE risk config: `risk.setLimits`/`risk.setSystemic` hot-write the
    // matrix, so the arm takes the Core lock and reads it back rather than
    // serving a boot snapshot that a hot edit would make a lie.

    // #353: the backtest job registry — the WebUI's 拉数据→配置→回测→看结果
    // flow runs as tokio tasks INSIDE this process, addressed over IPC. One
    // registry for the whole server; every session's arms share it.
    let jobs = Arc::new(crate::backtest_jobs::BacktestJobs::new());

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;

    tokio::pin!(shutdown);
    // Report an unavailable peer check once, not per connection.
    let mut peer_warning_logged = false;
    // P0 #670: accept() 的 Err 臂限速 + 指数退避状态。EMFILE 等错误让
    // accept 立刻返回、select 立刻再就绪 → 紧自旋刷日志（外部用户实测
    // 4 行/11μs，journald+syslog 无上限轮转 7 天写满 50G）。三者都在
    // Ok 臂复位；sleep 只在 Err 臂，shutdown 最多多等一个封顶 2s 的退避。
    let mut accept_err = AcceptErrState::default();
    loop {
        tokio::select! {
            accept = listener.accept() => match accept {
                Ok((stream, _)) => {
                    accept_err.reset_ok();
                    // Authenticate the PEER before serving it (#187): the socket
                    // mode is the file-system control, this is the identity one.
                    let peer = classify_peer(&stream);
                    if !peer.accepted() {
                        // Refused without a single byte read or written, so a
                        // foreign caller learns nothing — not even the protocol.
                        eprintln!(
                            "blitzkrieg-core: refusing IPC connection from {} (this core runs as uid {})",
                            peer.describe(),
                            own_uid()
                        );
                        continue;
                    }
                    if matches!(peer, PeerAuth::Unavailable { .. }) && !peer_warning_logged {
                        peer_warning_logged = true;
                        eprintln!(
                            "blitzkrieg-core: WARNING {} — falling back to the socket file mode alone",
                            peer.describe()
                        );
                    }
                    spawn_session(
                        stream,
                        peer,
                        core.clone(),
                        bus_tx.subscribe(),
                        registry.clone(),
                        update_state.clone(),
                        jobs.clone(),
                        host.clone(),
                        data_plane.clone(),
                    )
                }
                Err(e) => {
                    let now = std::time::Instant::now();
                    let kind = e.kind();
                    let (should_log, suppressed) = accept_err.step(kind, now);
                    if should_log {
                        tracing::warn!(error = %e, suppressed, "accept failed");
                    }
                    // P0 #670: 命中 EMFILE 才广播 RiskAlert，且只在限速窗
                    // 放行的那次（首命中必放行；其后 5s 内静默，绝不给自旋
                    // 期间的每次失败各发一条）。
                    if e.raw_os_error() == Some(ACCEPT_EMFILE_ERRNO) && should_log {
                        let c = core.lock().await;
                        c.emit_risk_alert(
                            CoreErrorCode::Internal,
                            format!(
                                "IPC accept hit EMFILE (fd exhaustion): open as many files as \
                                 allowed? suppressed {suppressed} repeat error(s) before this \
                                 alert; details in the log (issue #670)"
                            ),
                        );
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(
                        accept_err.backoff_ms,
                    ))
                    .await;
                }
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

#[allow(clippy::too_many_arguments)] // session wiring — each param has one job
fn spawn_session(
    stream: UnixStream,
    peer: PeerAuth,
    core: Arc<AsyncMutex<Core>>,
    events: broadcast::Receiver<Event>,
    registry: crate::market::registry::MarketPluginRegistry,
    update_state: Arc<crate::ipc::version::UpdateState>,
    jobs: Arc<crate::backtest_jobs::BacktestJobs>,
    host: std::sync::Arc<dyn blitzkrieg_market_api::MarketHost>,
    data_plane: DataPlaneRuntime,
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

        // The connection-scoped state (§9.5 + §12.2): the session's default
        // account (`account.switch` writes THIS cell — never Core's
        // process-level one, so two connections can look at two accounts side
        // by side) and the session's K-line subscription set (§12.2) —
        // written by this connection's `kline.subscribe`/`kline.unsubscribe`
        // arms, read by the event-forwarding task below, and dropped with the
        // connection, which IS the auto-unsubscribe (a closed panel cannot
        // keep receiving market pushes; a reconnect starts clean, so no
        // double-push).
        let mut session = SessionState::default();

        // Server -> client notifications.
        {
            let out_tx = out_tx.clone();
            let session_subs = session.kline_subs.clone();
            tokio::spawn(async move {
                let mut events = events;
                loop {
                    match events.recv().await {
                        Ok(ev) => {
                            // KLINE_UPDATE is the one event a session must
                            // OPT INTO (§12.2): an unfiltered broadcast would
                            // push every bucket's preview to every client —
                            // nine intervals × every symbol at 1/s each.
                            if let Event::KlineUpdate { kline } = &ev {
                                let subscribed = session_subs
                                    .lock()
                                    .map(|subs| {
                                        subs.contains(&(kline.symbol.clone(), kline.interval))
                                    })
                                    .unwrap_or(false);
                                if !subscribed {
                                    continue;
                                }
                            }
                            if let Ok(json) = serde_json::to_string(&Notification::new(ev))
                                && out_tx.send(format!("{json}\n")).is_err()
                            {
                                break;
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
                    let response = handle_line(
                        &core,
                        &registry,
                        line,
                        &peer,
                        &update_state,
                        &jobs,
                        &host,
                        &data_plane,
                        &mut session,
                    )
                    .await;
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

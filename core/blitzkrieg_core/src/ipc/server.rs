//! Unix-domain-socket JSON-RPC 2.0 server.
//!
//! One JSON object per line. Multiple Node sessions may connect; each receives
//! the full event stream. Requests are dispatched against the shared Core under
//! an async lock; a writer task per connection serialises responses and pushed
//! notifications onto the socket.

use crate::account::{AccountId, AccountStatus};
use crate::ipc::schema::*;
use crate::model::{CoreError, CoreErrorCode, Mode};
use crate::service::{Core, CoreConfig};
use blitzkrieg_market_api::net::now_ms;
use rust_decimal::Decimal;
use serde_json::Value;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex as AsyncMutex, broadcast, mpsc, oneshot};

/// Permission bits the IPC socket file carries: owner read/write only.
///
/// `UnixListener::bind` creates the node with `0777 & !umask`, which on a
/// default umask is `srwxr-xr-x` — connectable by every local user. This channel
/// can place and cancel orders, close positions and trip the kill switch, so it
/// is an owner-only surface (#187).
const SOCKET_MODE: u32 = 0o600;

/// The uid the kernel compares a connecting peer against.
fn own_uid() -> u32 {
    // SAFETY: `getuid` is always safe to call and cannot fail.
    unsafe { libc::getuid() }
}

/// What we know about the process on the other end of an accepted socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerAuth {
    /// The peer runs as the same uid as this core. Accepted.
    SameUid { uid: u32 },
    /// The peer runs as somebody else. Refused before a single byte is read.
    OtherUid { uid: u32 },
    /// The platform cannot report peer credentials (or the syscall failed).
    /// The socket's 0600 mode is then the only control — see [`SOCKET_MODE`].
    Unavailable { reason: String },
}

impl PeerAuth {
    /// Whether a session may be spawned for this peer.
    fn accepted(&self) -> bool {
        matches!(
            self,
            PeerAuth::SameUid { .. } | PeerAuth::Unavailable { .. }
        )
    }

    /// uid for the ready handshake, when known.
    fn uid(&self) -> Option<u32> {
        match self {
            PeerAuth::SameUid { uid } | PeerAuth::OtherUid { uid } => Some(*uid),
            PeerAuth::Unavailable { .. } => None,
        }
    }

    /// One line for the ready handshake / logs.
    fn describe(&self) -> String {
        match self {
            PeerAuth::SameUid { uid } => format!("same-uid peer (uid {uid})"),
            PeerAuth::OtherUid { uid } => format!("foreign peer (uid {uid})"),
            PeerAuth::Unavailable { reason } => format!("peer credentials unavailable: {reason}"),
        }
    }
}

/// Peer uid of a connected Unix-domain socket.
///
/// macOS and Linux expose this through different syscalls (`LOCAL_PEERCRED`
/// versus `SO_PEERCRED`) and different structs, so each has its own branch.
/// Both report the *kernel-recorded* identity of the peer at connect time, so a
/// peer cannot claim another uid.
#[cfg(target_os = "macos")]
fn peer_uid(fd: RawFd) -> Result<u32, String> {
    // `struct xucred` from <sys/ucred.h>. `libc` does not expose it for Apple
    // targets, so the layout is declared here; `XUCRED_VERSION` is 0 and the
    // kernel rejects any other value, which is what makes this safe to parse.
    #[repr(C)]
    struct Xucred {
        cr_version: u32,
        cr_uid: u32,
        cr_ngroups: i16,
        cr_groups: [u32; 16],
    }
    // <sys/un.h>: SOL_LOCAL is 0 and LOCAL_PEERCRED is 1 on the BSD socket API.
    // They are not part of libc's Apple bindings, so they are spelled out here.
    const SOL_LOCAL: libc::c_int = 0;
    const LOCAL_PEERCRED: libc::c_int = 1;
    const XUCRED_VERSION: u32 = 0;

    let mut cred: Xucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<Xucred>() as libc::socklen_t;
    // SAFETY: `fd` is a connected socket owned by the caller, the option writes
    // exactly `len` bytes into `cred`, and both are valid for the call.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            SOL_LOCAL,
            LOCAL_PEERCRED,
            &mut cred as *mut Xucred as *mut libc::c_void,
            &mut len,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    if cred.cr_version != XUCRED_VERSION {
        return Err(format!("unexpected xucred version {}", cred.cr_version));
    }
    Ok(cred.cr_uid)
}

/// Peer uid on Linux, via `SO_PEERCRED` (see [`peer_uid`]).
#[cfg(target_os = "linux")]
fn peer_uid(fd: RawFd) -> Result<u32, String> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: as in the macOS branch — connected socket, matching struct size.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut libc::ucred as *mut libc::c_void,
            &mut len,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(cred.uid)
}

/// Other unix: peer credentials are not implemented. Degrade to the file mode
/// rather than refusing every connection — an unsupported platform must not look
/// like a broken core.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn peer_uid(_fd: RawFd) -> Result<u32, String> {
    Err("peer credentials not implemented on this platform".to_string())
}

/// Classify an accepted connection by its kernel-reported peer uid.
fn classify_peer(stream: &UnixStream) -> PeerAuth {
    let ours = own_uid();
    match peer_uid(stream.as_raw_fd()) {
        Ok(uid) if uid == ours => PeerAuth::SameUid { uid },
        Ok(uid) => PeerAuth::OtherUid { uid },
        Err(reason) => PeerAuth::Unavailable { reason },
    }
}

/// What the boot banner should say about peer authentication on this platform.
fn peer_auth_support() -> &'static str {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        "enforced (same-uid only)"
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        "unavailable (file mode 0600 only)"
    }
}

/// Restrict the socket node to its owner and report the mode actually applied.
///
/// The order matters: bind, then chmod, then serve. `bind` cannot create the node
/// with a chosen mode and this process cannot safely change its own umask (it is
/// process-wide and the runtime is already multi-threaded), so the window before
/// the chmod is unavoidable — it is also harmless, because the accept loop
/// refuses a foreign uid at connect time before reading anything.
fn restrict_socket(path: &str) -> std::io::Result<u32> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(SOCKET_MODE))?;
    let mode = std::fs::metadata(path)?.permissions().mode() & 0o777;
    Ok(mode)
}

/// Claim `socket_path` for this process, or explain why it must not.
///
/// The socket is a shared singleton, so three questions are asked in this order —
/// each one protects the premise of the next:
///
/// 1. **Is a core already serving this path?** A live listener means a healthy
///    kernel owns the name. A second core that unlinked and re-bound it would
///    orphan the first silently: its clients would talk to the new process while
///    the old one kept trading. Refuse, and do not touch the node.
/// 2. **Is the leftover node ours?** A node owned by another uid is either a core
///    this process cannot probe successfully, or a path someone else prepared;
///    unlinking it tears that core off the wire (or hands the name to whoever
///    binds next). Refuse.
/// 3. Only a stale node of our own uid is safe to remove, so `bind` can succeed.
///
/// Split out of `run` because `run` starts feeds, discovery and the executor on
/// the way to `bind`: the decision that protects a live core has to be testable
/// without a second kernel trading on the test host.
fn claim_socket_path(socket_path: &str) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let path = std::path::Path::new(socket_path);
    if path.exists() && std::os::unix::net::UnixStream::connect(socket_path).is_ok() {
        anyhow::bail!("another blitzkrieg-core is already listening on {socket_path}");
    }
    if let Ok(meta) = std::fs::metadata(socket_path)
        && meta.uid() != own_uid()
    {
        anyhow::bail!(
            "refusing to remove {socket_path}: it is owned by uid {} (this core runs as uid {})",
            meta.uid(),
            own_uid()
        );
    }
    let _ = std::fs::remove_file(socket_path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    Ok(())
}

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
    if config.mode.settles_locally() {
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
    let active = crate::market::active_market_plugin(&registry, config.market_plugin.as_deref());
    core.set_plugin_modes(Some(crate::market::compat::PluginModes::for_plugin(
        active.as_ref(),
    )));

    if config.engine_enabled {
        // One mapping shared with the backtester (`CoreConfig::install_engine`).
        // A refusal here (the #265 startup self-check: an explicit
        // `--enable-strategy` that resolved to nothing) ends the boot BEFORE the
        // socket is bound or a feed is started — the failure mode is a kernel
        // that looks healthy and trades none of what it was told to run.
        if let Err(why) = config.install_engine(&mut core) {
            anyhow::bail!("{why}");
        }
        eprintln!("blitzkrieg-core: self-driving engine enabled");
    }
    let core = Arc::new(AsyncMutex::new(core));

    let host: Arc<dyn blitzkrieg_market_api::MarketHost> =
        Arc::new(crate::market::host::CoreHost::new(core.clone()));

    // P4: Rust-native feeds. Start Binance spot now (symbols known at boot); the
    // orderbook subscriptions are added when `engine.markets` arrives (from Node)
    // or from Rust-native round discovery below.
    if config.feed_ws_enabled
        && let Some(feed) = active.data_feed()
    {
        let cfg = blitzkrieg_market_api::DataFeedConfig {
            spot_assets: config.binance_assets.clone(),
        };
        if let Err(e) = feed.start(host.clone(), cfg, Vec::new()).await {
            eprintln!("blitzkrieg-core: data feed start failed: {e}");
        }
        eprintln!("blitzkrieg-core: rust-native feeds enabled (binance spot; poly on first round)");
    }

    // P5: Rust-native round discovery — the plugin finds its own markets and feeds
    // the engine + orderbook subscriptions, so Node is out of the data path.
    if config.engine_enabled
        && config.discovery_enabled
        && let Some(disc) = active.discovery()
    {
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
        if let Some(exec) = active.executor() {
            let cfg = blitzkrieg_market_api::ExecutorConfig {
                markets: config.markets.clone(),
            };
            match exec.start(host.clone(), cfg).await {
                Ok(()) => eprintln!("blitzkrieg-core: live order executor started"),
                Err(e) => eprintln!("blitzkrieg-core: live executor failed to start: {e}"),
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
    loop {
        tokio::select! {
            accept = listener.accept() => match accept {
                Ok((stream, _)) => {
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
                    )
                }
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

#[allow(clippy::too_many_arguments)] // session wiring — each param has one job
fn spawn_session(
    stream: UnixStream,
    peer: PeerAuth,
    core: Arc<AsyncMutex<Core>>,
    events: broadcast::Receiver<Event>,
    registry: crate::market::registry::MarketPluginRegistry,
    update_state: Arc<crate::ipc::version::UpdateState>,
    jobs: Arc<crate::backtest_jobs::BacktestJobs>,
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

/// The state a connection accumulates across its requests (§9.5's session
/// account; §12.2's K-line subscription set shares the same scope and dies
/// with the same connection — the Arc lets the event-forwarding task read
/// the set concurrently while `handle_line` mutates it).
#[derive(Default)]
struct SessionState {
    session_active: Option<AccountId>,
    kline_subs: std::sync::Arc<
        std::sync::Mutex<std::collections::HashSet<(String, crate::kline::KlineInterval)>>,
    >,
}

#[allow(clippy::too_many_arguments)] // one shared-context param per subsystem arm
async fn handle_line(
    core: &Arc<AsyncMutex<Core>>,
    registry: &crate::market::registry::MarketPluginRegistry,
    line: String,
    peer: &PeerAuth,
    update_state: &Arc<crate::ipc::version::UpdateState>,
    jobs: &Arc<crate::backtest_jobs::BacktestJobs>,
    session: &mut SessionState,
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
            }))
        }

        // VERSIONING.md §5: version + build provenance + update state.
        // Read-only, zero side effects, NO Core lock — it must answer while the
        // data lock is still starting ("which build is running" cannot wait for
        // startup), which is why the update state is a separate cell.
        method::SYSTEM_VERSION => Ok(serde_json::to_value(
            crate::ipc::version::system_version_payload(
                crate::ipc::build_info::BUILD_INFO,
                &update_state.snapshot(),
            ),
        )
        .unwrap_or(Value::Null)),

        // E26 (§4.4): the systemic-risk readout — WHAT each limit is and
        // WHERE it came from, plus the exit triple Gate 4 binds. Read-only,
        // zero side effects, but NOT lock-free any more: `risk.setLimits`
        // and `risk.setSystemic` hot-write the matrix, so the answer is
        // assembled per call from the live risk config — serving a boot
        // snapshot after a hot edit would show limits that are no longer in
        // force, which is worse than waiting a moment for the lock.
        method::RISK_LIMITS => {
            let c = core.lock().await;
            Ok(
                serde_json::to_value(crate::ipc::schema::RiskLimitsResult::snapshot(
                    &c.risk_config().systemic,
                    &c.config().positions.exit,
                ))
                .unwrap_or(Value::Null),
            )
        }

        // VERSIONING.md §7.4: the update switches. A write lands an audit
        // record AND must persist — a switch that silently reverts on restart
        // is a switch the operator believed was on.
        method::SYSTEM_UPDATE_CONFIGURE => {
            match serde_json::from_value::<crate::ipc::version::UpdateConfigureParams>(
                params.clone(),
            ) {
                Err(e) => Err((Failure::INVALID_PARAMS, format!("{e}"), None)),
                Ok(req) => {
                    // Audit: who turned "may auto-replace binaries" on. A
                    // security-relevant state change, recorded at risk.kill's
                    // level; the actor is the kernel-recorded peer uid, not a
                    // caller's claim.
                    let actor = match peer {
                        PeerAuth::SameUid { uid } => format!("uid:{uid}"),
                        _ => "uid:unknown".to_string(),
                    };
                    update_state.set_enabled(req.check_enabled, req.auto_update);
                    match crate::ipc::version::persist(update_state) {
                        // A switch that did not land on disk silently reverts on
                        // restart while the operator believes it was on — so the
                        // caller MUST hear about it.
                        Err(e) => Err((
                            Failure::APPLICATION,
                            format!("update state not persisted: {e}"),
                            None,
                        )),
                        Ok(()) => {
                            let s = update_state.snapshot();
                            crate::ipc::version::audit_event(&serde_json::json!({
                                "ts": now_ms(),
                                "event": "update.configure",
                                "actor": actor,
                                "checkEnabled": s.check_enabled,
                                "autoUpdate": s.auto_update,
                            }));
                            Ok(serde_json::json!({
                                "checkEnabled": s.check_enabled,
                                "autoUpdate": s.auto_update,
                            }))
                        }
                    }
                }
            }
        }

        method::SYSTEM_UPDATE_CHECK => {
            // The manual check is ALSO bound by check_enabled: the switch means
            // "may go out at all", not "may go out automatically". A disabled
            // button answers clearly instead of sneaking a request out.
            if !update_state.snapshot().check_enabled {
                Err((
                    Failure::APPLICATION,
                    "update checking is disabled (checkEnabled=false); enable it in \
                     user_layer/configs/update.toml or via system.update.configure"
                        .to_string(),
                    None,
                ))
            } else {
                let actor = match peer {
                    PeerAuth::SameUid { uid } => format!("uid:{uid}"),
                    _ => "uid:unknown".to_string(),
                };
                let local = crate::ipc::build_info::BUILD_INFO.version.to_string();
                let token = std::env::var("BLITZKRIEG_GITHUB_TOKEN")
                    .ok()
                    .filter(|t| !t.trim().is_empty());
                // Ok and Err are both values, never escapes: the fetch result is
                // consumed by check_once, which writes whatever happened into the
                // state cell.
                crate::ipc::version::check_once(
                    update_state,
                    &local,
                    now_ms() as u64,
                    crate::ipc::version::fetch_latest(token),
                )
                .await;
                let s = update_state.snapshot();
                crate::ipc::version::audit_event(&serde_json::json!({
                    "ts": now_ms(),
                    "event": "update.check",
                    "actor": actor,
                    "outcome": s.outcome.token(),
                    "latest": s.latest,
                }));
                Ok(
                    serde_json::to_value(crate::ipc::version::system_version_payload(
                        crate::ipc::build_info::BUILD_INFO,
                        &s,
                    ))
                    .unwrap_or(Value::Null),
                )
            }
        }

        // #379 (§7.5): download + verify the newer release into
        // `data/update/staging/`. GUARDED by `autoUpdate` — the switch means
        // "may a newer release be fetched and staged", for the scheduler and
        // the manual call alike. The reply is the staging state machine; the
        // bytes are the launcher's to install, and this arm never touches a
        // running binary (§7.5's iron rule — see the PR description).
        method::SYSTEM_UPDATE_STAGE => {
            if !crate::ipc::version::stage_enabled(update_state) {
                Err((
                    Failure::APPLICATION,
                    "update staging is disabled (autoUpdate=false); enable it via \
                     system.update.configure"
                        .to_string(),
                    None,
                ))
            } else {
                let actor = match peer {
                    PeerAuth::SameUid { uid } => format!("uid:{uid}"),
                    _ => "uid:unknown".to_string(),
                };
                let local_target = crate::ipc::build_info::BUILD_INFO.target.to_string();
                let token = std::env::var("BLITZKRIEG_GITHUB_TOKEN")
                    .ok()
                    .filter(|t| !t.trim().is_empty());
                let staging = crate::ipc::version::staging_dir();
                let fetch_target = local_target.clone();
                crate::ipc::version::stage_once(
                    update_state,
                    &local_target,
                    now_ms() as u64,
                    &staging,
                    move |tag: String, version: String| async move {
                        crate::ipc::version::fetch_release_assets(
                            &tag,
                            &version,
                            &fetch_target,
                            token,
                        )
                        .await
                    },
                )
                .await;
                let stage = update_state.stage_snapshot();
                crate::ipc::version::audit_event(&serde_json::json!({
                    "ts": now_ms(),
                    "event": "update.stage",
                    "actor": actor,
                    "phase": stage.phase.token(),
                    "version": stage.version,
                    "asset": stage.asset,
                }));
                Ok(serde_json::to_value(&stage).unwrap_or(Value::Null))
            }
        }

        // Read-only fee schedule (#182). Stateless: no lock on the order book, no
        // mutation, no venue. Absent/blank price means "quote the neutral 0.5".
        method::CORE_FEE_QUOTE => {
            let p: FeeQuoteParams =
                serde_json::from_value(params.clone()).unwrap_or(FeeQuoteParams { price: None });
            let quote = core.lock().await.fee_quote(p.price);
            Ok(serde_json::to_value(quote).unwrap_or(Value::Null))
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

        // #191: limited hot reload of the ENTRY limits. Named refusals come
        // FIRST: an operator who tries to hot-change a breaker, an exit threshold
        // or a credential is told WHY and what a restart would do, instead of
        // getting a bare deserialization error. Either way the WHOLE request is
        // refused — nothing is applied partially.
        method::RISK_SET_LIMITS => {
            if let Some((key, why)) = params.as_object().and_then(|obj| {
                obj.keys()
                    .find_map(|k| crate::risk::hot_reload_refusal(k).map(|why| (k.as_str(), why)))
            }) {
                Err((
                    Failure::INVALID_PARAMS,
                    format!(
                        "{key} is not hot-reloadable: {why}; a restart applies it. \
                         hot-reloadable: {}",
                        crate::risk::HOT_RELOADABLE.join(", ")
                    ),
                    None,
                ))
            } else {
                match serde_json::from_value::<SetRiskLimitsParams>(params.clone()) {
                    Err(e) => Err((
                        Failure::INVALID_PARAMS,
                        format!(
                            "{e} — nothing was applied; hot-reloadable: {}",
                            crate::risk::HOT_RELOADABLE.join(", ")
                        ),
                        None,
                    )),
                    Ok(p) => {
                        // The actor is the peer's KERNEL-recorded uid, not a claim
                        // by the caller: the audit's "who" has to survive a client
                        // that would say anything it liked.
                        let actor = match peer {
                            PeerAuth::SameUid { uid } => format!("uid:{uid}"),
                            _ => "uid:unknown".to_string(),
                        };
                        match core.lock().await.apply_risk_limits(&p, &actor, now_ms()) {
                            Ok(update) => Ok(serde_json::to_value(update).unwrap_or(Value::Null)),
                            Err(e) => Err(core_err(e)),
                        }
                    }
                }
            }
        }

        // 生效风控可编辑（用户裁决：单笔最大亏损、当日最大回撤等必须都可以
        // 被编辑）：九项系统性限额的运行时写路径。六项热生效、回撤预算与
        // 连亏熔断对按「下次会话」语义 —— 回执逐项带 effect，不静默。
        method::RISK_SET_SYSTEMIC => {
            match serde_json::from_value::<SetSystemicLimitsParams>(params.clone()) {
                Err(e) => Err((
                    Failure::INVALID_PARAMS,
                    format!(
                        "{e} — nothing was applied; the nine systemic fields are \
                         maxSingleLossUsd, maxDailyDrawdownUsd, maxPositionSize, \
                         maxConsecutiveLosses, cooldownMinutes, maxTotalPosition, \
                         maxTotalExposureUsd, maxCorrelationUsd, globalKillSwitchLossUsd"
                    ),
                    None,
                )),
                Ok(p) => {
                    let actor = match peer {
                        PeerAuth::SameUid { uid } => format!("uid:{uid}"),
                        _ => "uid:unknown".to_string(),
                    };
                    match core
                        .lock()
                        .await
                        .apply_systemic_limits(&p, &actor, now_ms())
                    {
                        Ok(update) => Ok(serde_json::to_value(update).unwrap_or(Value::Null)),
                        Err(e) => Err(core_err(e)),
                    }
                }
            }
        }

        // 退出纪律可编辑（用户裁决：Gate 4 绑定的止损/止盈/强平必须可以改）：
        // 三项退出纪律的运行时写路径。对每笔新入场立即生效（Gate 4 每意图
        // 现场绑定），退出扫描同步跟随 —— 回执逐项带 effect，不静默。
        method::RISK_SET_EXIT => match serde_json::from_value::<SetExitParams>(params.clone()) {
            Err(e) => Err((
                Failure::INVALID_PARAMS,
                format!(
                    "{e} — nothing was applied; the three exit fields are \
                         stopLossPct (percent of entry price, > 0), takeProfitPct \
                         (percent of entry price, > 0), forceExitSec (seconds, \
                         0 = the guillotine is OFF)"
                ),
                None,
            )),
            Ok(p) => {
                let actor = match peer {
                    PeerAuth::SameUid { uid } => format!("uid:{uid}"),
                    _ => "uid:unknown".to_string(),
                };
                match core.lock().await.apply_exit_set(&p, &actor, now_ms()) {
                    Ok(update) => Ok(serde_json::to_value(update).unwrap_or(Value::Null)),
                    Err(e) => Err(core_err(e)),
                }
            }
        },

        method::ORDER_PLACE => {
            // §9.5: an order with NO accountId of its own spends this
            // connection's default — the SESSION's active account (what
            // `account.switch` set), else the process-level one. The check is
            // on the RAW params, BEFORE the serde default fills the gap: an
            // omitted accountId is "this connection's default", but an
            // EXPLICIT `accountId: "default"` NAMES that account and must
            // stand — an explicit id is never re-routed by a session cell.
            let explicit_account = params.get("accountId").is_some();
            typed(params, |p: PlaceParams| {
                let core = core.clone();
                async move {
                    let mut c = core.lock().await;
                    let mut order = p.order;
                    if !explicit_account {
                        order.account_id = session
                            .session_active
                            .clone()
                            .unwrap_or_else(|| c.accounts().active_id().clone());
                    }
                    // `place_outcome`, not `place`: when the kernel refuses the leg
                    // itself (a dry taker the book cannot fill) the result carries
                    // the structured reason instead of a bare REJECTED (#180).
                    let outcome = c.place_outcome(order, p.maker_timeout_ms, now_ms())?;
                    let (code, message) = match outcome.rejection {
                        Some(err) => (Some(err.code), Some(err.message)),
                        None => (None, None),
                    };
                    Ok::<_, CoreError>(
                        serde_json::to_value(PlaceResult {
                            order_id: outcome.order_id,
                            status: outcome.status,
                            code,
                            message,
                        })
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

        method::ORDER_CANCEL_REMAINING => {
            typed(params, |p: CancelRemainingParams| {
                let core = core.clone();
                async move {
                    core.lock().await.cancel_remaining(&p.order_id, now_ms())?;
                    Ok::<_, CoreError>(serde_json::json!({ "success": true }))
                }
            })
            .await
        }

        method::ORDER_CLOSE_FILLED => {
            typed(params, |p: CloseFilledParams| {
                let core = core.clone();
                async move {
                    let closed = core.lock().await.close_filled(&p.order_id, now_ms())?;
                    Ok::<_, CoreError>(
                        serde_json::to_value(CloseFilledResult { closed }).unwrap_or(Value::Null),
                    )
                }
            })
            .await
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
            Ok(serde_json::to_value(BalanceResult {
                balance: c.ledger().balance(),
                reserved: c.ledger().reserved(),
                available: c.ledger().available(),
                seed,
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
                                maker: t.maker,
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

        // Cumulative closed-trade summary straight from the persisted
        // `summary.json` (all-time totals, unlike the windowed history rows).
        method::TRADES_SUMMARY => {
            let summary = core.lock().await.trade_summary();
            Ok(serde_json::json!({
                "version": crate::ipc::schema::PROTOCOL_VERSION,
                "summary": summary,
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

        method::ENGINE_STATS => Ok(core.lock().await.engine_stats()),

        method::STRATEGY_LIST => {
            let c = core.lock().await;
            let enabled = c.enabled_strategy_names();
            let strategies: Vec<_> = c
                .strategy_names()
                .into_iter()
                .map(|name| {
                    let is_on = enabled.contains(&name);
                    // E27 (§8.3): the row carries the strategy's declared modes
                    // as the §2.3 wire objects (`null` = undeclared), the
                    // compatible flag, and the refusal reason when the
                    // handshake would refuse it. Every 0.2 strategy reads as
                    // `modes: null, compatible: true, incompatibleReason: null`.
                    let (modes, compatible, incompatible_reason) = c.strategy_compat_view(&name);
                    serde_json::json!({
                        "name": name,
                        "enabled": is_on,
                        "modes": modes,
                        "compatible": compatible,
                        "incompatibleReason": incompatible_reason,
                    })
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
            // E27 (§8.2): the enable runs through the handshake. A disabling
            // toggle is never gated (standing down is always allowed); an
            // enable the active plugin cannot serve is REFUSED with the §8.2
            // diagnostic — `{ name, enabled: false, found: true, reason }` —
            // and the dispatch keeps the strategy off.
            let (found, applied, reason) = core
                .lock()
                .await
                .set_strategy_enabled_checked(name, enabled);
            Ok(serde_json::json!({
                "name": name,
                "enabled": if reason.is_some() { false } else { enabled && applied },
                "found": found,
                "applied": applied,
                "reason": reason,
            }))
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

        method::STRATEGY_UNLOAD => {
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if name.is_empty() {
                Err((Failure::INVALID_PARAMS, "name required".into(), None))
            } else {
                let outcome = core.lock().await.unload_strategy(name);
                Ok(serde_json::to_value(outcome).unwrap_or(Value::Null))
            }
        }

        method::STRATEGY_RELOAD => {
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let path = params.get("path").and_then(|v| v.as_str()).unwrap_or("");
            if name.is_empty() || path.is_empty() {
                Err((
                    Failure::INVALID_PARAMS,
                    "name and path required".into(),
                    None,
                ))
            } else {
                let outcome = core.lock().await.reload_strategy(name, path);
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
                    // E27 (§8.3): the row carries the plugin's declared
                    // structure and the union of its capability bits — with
                    // the readable names beside the raw value so the modes
                    // gate can push the two against each other in both
                    // directions.
                    let caps = blitzkrieg_market_api::modes::capabilities_to_names(p.capabilities);
                    serde_json::json!({
                        "name": p.name,
                        "type": p.market_type,
                        "structure": p.structure,
                        "capabilities": caps,
                        "capabilitiesBits": p.capabilities.0,
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

        method::NET_CHECK => {
            // Resolve the plugin and drop the registry lock BEFORE awaiting: the
            // probe does real network I/O with multi-second timeouts, and
            // holding a lock the maintenance tick needs would turn a diagnostic
            // into a stall. The registry hands out an `Arc`, so the handle
            // outlives the borrow.
            let plugin = registry
                .active()
                .and_then(|name| registry.get(&name))
                .or_else(|| registry.names().first().and_then(|n| registry.get(n)))
                // A market-free build still answers: the no-op plugin's default
                // probe reports `unsupported`, which is honest and, unlike an
                // error, keeps the report shape every UI reads.
                .unwrap_or_else(|| std::sync::Arc::new(crate::market::NoopMarketPlugin));
            let report = plugin.net_check().await;
            serde_json::to_value(report).map_err(|e| {
                (
                    Failure::APPLICATION,
                    format!("net.check report is not serializable: {e}"),
                    None,
                )
            })
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
                // E13: the unattended switch + the 72h deep clock, so a UI can
                // render the checkbox and the next-round time from one call.
                "autoEvolve": c.shadow_evolution().auto_evolve(),
                "lastCycleMs": c.shadow_evolution().cycle_info().0,
                "nextCycleAtMs": c.shadow_evolution().next_cycle_at_ms(now),
                "pendingProposals": c.shadow_evolution().pending_proposal_count(),
                // #249: the ENGINE switch is a different thing from the auto
                // switch, and without it on the wire a UI cannot tell "nothing
                // qualified yet" from "nothing is even running" — the confusion
                // that made an auto-evolve label look like a lie.
                "enabled": c.shadow_evolution().is_enabled(),
                "cycleSeq": c.shadow_evolution().cycle_info().1,
                "cycleSecs": c.shadow_evolution().cycle_secs(),
                // #269: where `state.json` and the config file disagreed at
                // startup. The runtime switch wins by design, so the file alone
                // can read "off" while the engine is on — a panel that shows
                // only the two booleans cannot explain that, and an operator
                // reading the file gets the wrong answer.
                "switchConflicts": c.shadow_evolution().switch_conflicts(),
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

        // E13 proposal workflow. `proposals` is read-only (the UIs' 对比表);
        // `decide` carries the operator's verdict on ONE held proposal; `set_auto`
        // flips the unattended mode (persisted — a restart keeps the chosen mode).
        method::SE_PROPOSALS => {
            let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(100) as usize;
            let proposals = core.lock().await.shadow_evolution_proposals(limit);
            Ok(serde_json::json!({
                "version": crate::ipc::schema::PROTOCOL_VERSION,
                "proposals": proposals,
            }))
        }
        method::SE_DECIDE => match (
            params
                .get("id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty()),
            params
                .get("decision")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty()),
        ) {
            (None, _) => Err((Failure::INVALID_PARAMS, "id required".into(), None)),
            (_, None) => Err((
                Failure::INVALID_PARAMS,
                "decision required (accept|reject|defer)".into(),
                None,
            )),
            (Some(id), Some(decision)) => {
                let now = now_ms();
                match core.lock().await.shadow_evolution_decide(id, decision, now) {
                    Ok(result) => Ok(match result {
                        crate::shadow_evolution::DecisionResult::Accepted { proposal } => {
                            serde_json::json!({ "decision": "accepted", "proposal": proposal })
                        }
                        crate::shadow_evolution::DecisionResult::Rejected { proposal } => {
                            serde_json::json!({ "decision": "rejected", "proposal": proposal })
                        }
                        crate::shadow_evolution::DecisionResult::Deferred { proposal } => {
                            serde_json::json!({ "decision": "deferred", "proposal": proposal })
                        }
                    }),
                    Err(e) => Err((Failure::APPLICATION, e, None)),
                }
            }
        },
        method::SE_SET_AUTO => match params.get("enabled").and_then(|v| v.as_bool()) {
            None => Err((
                Failure::INVALID_PARAMS,
                "enabled (bool) required".into(),
                None,
            )),
            Some(on) => {
                let auto = core.lock().await.shadow_evolution_set_auto(on);
                Ok(serde_json::json!({ "autoEvolve": auto }))
            }
        },

        method::ENGINE_ROUND => {
            let view = core.lock().await.round_view();
            Ok(serde_json::to_value(view).unwrap_or(Value::Null))
        }

        // Read-only depth view for the UI's 盘口深度 chart (E8-c). Bounded on
        // both axes: one entry per round asset, 15 levels per token side — the
        // ladder the chart draws, not the whole book.
        method::ENGINE_BOOKS => {
            let view = core.lock().await.books_view(15);
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

        // ── v0.3 Wave 0 (#329) / E29: the read-only envelope freezes ─────────
        // Both answer and state today's truth with today's data — no stubs:
        //
        // * `intent.audit.tail` reads whatever `data/audit/intents.jsonl`
        //   holds (E25 landed the writer; a missing file is the documented
        //   empty envelope, and the JSONL rules skip a torn tail line).
        // * `kline.history` (E29): the aggregator's closed tail plus the
        //   still-growing bar as the last element. Takes the Core lock —
        //   the bars live in the engine, and unlike `risk.limits` nothing
        //   here is boot-frozen. `limit` default 200, ceiling 1000 = the
        //   aggregator's own closed-tail cap, so a request can never want
        //   bars the kernel dropped.
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

        // ── E29 (§12.2): the session-scoped subscription pair ────────────────
        // The set lives in the SESSION (see `spawn_session`): these arms only
        // mutate/remove that connection's own set, so two panels subscribe
        // independently, and dropping the connection IS the auto-unsubscribe.
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

        method::INTENT_AUDIT_TAIL => {
            typed(params, |p: IntentAuditTailParams| async move {
                let all = crate::jsonl::load::<Value>(
                    std::path::Path::new("data/audit/intents.jsonl"),
                    "intent audit log: skipped unparseable lines",
                );
                let keep = |rec: &Value| {
                    if let Some(f) = p.account_id.as_deref()
                        && rec.get("accountId").and_then(Value::as_str) != Some(f)
                    {
                        return false;
                    }
                    if let Some(f) = p.strategy.as_deref()
                        && rec.get("strategy").and_then(Value::as_str) != Some(f)
                    {
                        return false;
                    }
                    if let Some(f) = p.decision.as_deref()
                        && !rec
                            .get("decision")
                            .and_then(|d| d.get("status"))
                            .and_then(Value::as_str)
                            .is_some_and(|s| s.eq_ignore_ascii_case(f))
                    {
                        return false;
                    }
                    true
                };
                let matched: Vec<Value> = all.into_iter().filter(keep).collect();
                let total = matched.len();
                let limit = p.limit.unwrap_or(50);
                let records: Vec<Value> = matched
                    .into_iter()
                    .skip(total.saturating_sub(limit))
                    .collect();
                Ok::<_, CoreError>(
                    serde_json::to_value(IntentAuditTailResult {
                        records,
                        total: Some(total),
                    })
                    .unwrap_or(Value::Null),
                )
            })
            .await
        }

        // ── E28 (§12.1) — the account verbs ──────────────────────────────
        // All three read/write the SESSION cell (§9.5): `account.switch`
        // changes THIS connection's default only, `account.list` reports it,
        // and Core's process-level active account is never touched from the
        // wire. Two panels can therefore look at two accounts side by side.
        method::ACCOUNT_LIST => {
            let c = core.lock().await;
            let active = session
                .session_active
                .clone()
                .unwrap_or_else(|| c.accounts().active_id().clone());
            Ok(serde_json::to_value(AccountListResult {
                version: PROTOCOL_VERSION.to_string(),
                active: active.as_str().to_string(),
                accounts: c.account_views(now_ms()),
            })
            .unwrap_or(Value::Null))
        }

        method::ACCOUNT_SWITCH => {
            match serde_json::from_value::<AccountSwitchParams>(params) {
                Ok(p) => {
                    let id = AccountId::from(p.account_id.as_str());
                    let c = core.lock().await;
                    // Existence only: switching TO a frozen account to LOOK at
                    // it is legal — the posture gates the orders, not the view.
                    if let Err(e) = c.accounts().require(&id) {
                        Err(client_err(e))
                    } else {
                        session.session_active = Some(id);
                        Ok(serde_json::json!({ "active": p.account_id }))
                    }
                }
                Err(e) => Err((Failure::INVALID_PARAMS, e.to_string(), None)),
            }
        }

        method::ACCOUNT_STATUS => {
            match serde_json::from_value::<AccountStatusParams>(params) {
                Ok(p) => {
                    // The flat wire `reason` overrides the placeholder a
                    // bare-string `suspended` would carry (§12.1); a map-form
                    // reason survives when no flat one is sent.
                    let status = match (p.status, p.reason) {
                        (AccountStatus::Suspended { .. }, Some(r)) => {
                            AccountStatus::Suspended { reason: r }
                        }
                        (s, _) => s,
                    };
                    let status_tag = status.wire_tag().to_string();
                    let status_reason = match &status {
                        AccountStatus::Suspended { reason } => Some(reason.clone()),
                        _ => None,
                    };
                    let id = AccountId::from(p.account_id.as_str());
                    let mut c = core.lock().await;
                    match c.tighten_account_status(&id, status, now_ms()) {
                        Ok(()) => Ok(serde_json::to_value(AccountStatusResult {
                            id: p.account_id,
                            status: status_tag,
                            status_reason,
                        })
                        .unwrap_or(Value::Null)),
                        Err(e) => Err(client_err(e)),
                    }
                }
                Err(e) => Err((Failure::INVALID_PARAMS, e.to_string(), None)),
            }
        }

        // ── #363: execution policy surface ─────────────────────────────────
        // The two write arms go through one path: serialize the section to
        // TOML, rewrite the file, full-reload in memory, land one audit line.
        // A file that will not re-parse keeps the LAST GOOD policy in memory
        // (fail-closed: a broken write never trades unguarded) and answers
        // INVALID_PARAMS; the on-disk file is left as the write produced it
        // so the operator can see and fix what they broke.
        method::EXECUTION_POLICY_LIST => {
            let c = core.lock().await;
            let defaults = c
                .execution_policy_section_view("*")
                .map(|v| serde_json::to_value(&v).unwrap_or(Value::Null))
                .unwrap_or(Value::Null);
            let accounts: Vec<serde_json::Value> = c
                .execution_policy_account_ids()
                .into_iter()
                .filter_map(|id| {
                    c.execution_policy_section_view(&id).map(|view| {
                        serde_json::json!({
                            "accountId": id,
                            "section": serde_json::to_value(&view).unwrap_or(Value::Null),
                        })
                    })
                })
                .collect();
            Ok(serde_json::json!({
                "loaded": c.execution_policy_loaded(),
                "defaults": defaults,
                "accounts": accounts,
            }))
        }

        method::EXECUTION_POLICY_GET => {
            match serde_json::from_value::<ExecutionPolicyGetParams>(params) {
                Ok(p) => {
                    let c = core.lock().await;
                    match c.execution_policy_section_view(&p.account_id) {
                        Some(view) => Ok(serde_json::to_value(view).unwrap_or(Value::Null)),
                        None => Err((
                            Failure::INVALID_PARAMS,
                            format!(
                                "execution policy: no section for account `{}` and no policy loaded",
                                p.account_id
                            ),
                            None,
                        )),
                    }
                }
                Err(e) => Err((Failure::INVALID_PARAMS, e.to_string(), None)),
            }
        }

        method::EXECUTION_POLICY_SET => {
            match serde_json::from_value::<ExecutionPolicySetParams>(params) {
                Ok(p) => execution_policy_set(core, p).await,
                Err(e) => Err((Failure::INVALID_PARAMS, e.to_string(), None)),
            }
        }

        method::EXECUTION_POLICY_RESET => {
            match serde_json::from_value::<ExecutionPolicyGetParams>(params) {
                Ok(p) => execution_policy_reset(core, p).await,
                Err(e) => Err((Failure::INVALID_PARAMS, e.to_string(), None)),
            }
        }

        method::EXECUTION_POLICY_HISTORY => {
            match serde_json::from_value::<ExecutionPolicyHistoryParams>(params) {
                Ok(p) => {
                    let c = core.lock().await;
                    let records: Vec<crate::execution_policy::PolicyAuditRecord> = c
                        .execution_policy_audit()
                        .into_iter()
                        .filter(|r| {
                            p.account_id
                                .as_ref()
                                .map(|id| &r.account_id == id)
                                .unwrap_or(true)
                        })
                        .collect();
                    let views: Vec<crate::ipc::schema::ExecutionPolicyAuditView> = records
                        .iter()
                        .map(|r| crate::ipc::schema::ExecutionPolicyAuditView {
                            ts_ms: r.ts_ms,
                            actor: r.actor.clone(),
                            action: r.action.clone(),
                            account_id: r.account_id.clone(),
                            before: r.before.clone(),
                            after: r.after.clone(),
                            error: r.error.clone(),
                        })
                        .collect();
                    Ok(serde_json::to_value(views).unwrap_or(Value::Null))
                }
                Err(e) => Err((Failure::INVALID_PARAMS, e.to_string(), None)),
            }
        }

        // #364: the CURRENT section replayed over the recent closed trades —
        // the read-only fact behind the UIs' preview strip ("these rules
        // would skip N of the last M"). Judged by `evaluate` itself, so the
        // preview cannot drift from the kernel.
        method::EXECUTION_POLICY_PREVIEW => {
            match serde_json::from_value::<ExecutionPolicyPreviewParams>(params) {
                Ok(p) => {
                    let c = core.lock().await;
                    Ok(
                        serde_json::to_value(c.execution_policy_preview(&p.account_id, 100))
                            .unwrap_or(Value::Null),
                    )
                }
                Err(e) => Err((Failure::INVALID_PARAMS, e.to_string(), None)),
            }
        }

        // ── #353: the WebUI backtest surface (拉数据→配置→回测→看结果) ────────
        // Every arm fronts the in-kernel job registry: validation is
        // synchronous and fail-closed, the heavy work runs as tokio tasks in
        // THIS process, and the WebUI names datasets/assets/strategy names —
        // never files outside the dataset root, never executable code.
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

        // ── #362: the blueprint editor surface (compile + save) ──────────────
        // The WebUI canvas authors blueprint JSON and asks the KERNEL for every
        // side effect. `compile` is stateless and read-only (the preview pane's
        // only data source); `save` is the one arm that touches the filesystem,
        // and it does so under the loader's OWN package contract: the name is
        // validated before anything exists, the blueprint is compiled FRESH
        // here (never a caller-supplied lua), and the package lands as
        // `blueprint.json` + `strategy.lua` + `manifest.json` under the same
        // strategy root `--lua-strategy-dir` points at — the directory the
        // kernel already scans, so a saved package is discovered on the next
        // start exactly like a hand-written one.
        method::BLUEPRINT_COMPILE => {
            typed(params, |p: BlueprintCompileParams| async move {
                // Compile errors carry the node id by construction (#361);
                // InvalidParams is the wire class for "your document is wrong".
                let lua = crate::blueprint::compile(&p.json)
                    .map_err(|e| CoreError::new(CoreErrorCode::InvalidParams, e))?;
                Ok::<_, CoreError>(
                    serde_json::to_value(BlueprintCompileResult { lua }).unwrap_or(Value::Null),
                )
            })
            .await
        }

        method::BLUEPRINT_SAVE => {
            // The save root is configuration: snapshot the core's config before
            // the async move (a sync field clone, no lock held across await).
            let core_cfg = core.lock().await.config().clone();
            typed(params, |p: BlueprintSaveParams| async move {
                let receipt = blueprint_save(&p, &core_cfg)
                    .map_err(|e| CoreError::new(CoreErrorCode::InvalidParams, e))?;
                Ok::<_, CoreError>(serde_json::to_value(receipt).unwrap_or(Value::Null))
            })
            .await
        }

        // #393: the read-only mirror of `blueprint.save` — hand a saved
        // package's blueprint document back so the editor can preload it. No
        // lock, no write: it answers from the strategy root, and the name is
        // screened with the save path's own rules so no caller can steer the
        // read outside that root.
        method::BLUEPRINT_LOAD => {
            let core_cfg = core.lock().await.config().clone();
            typed(params, |p: BlueprintLoadParams| async move {
                let doc = blueprint_load(&p, &core_cfg)
                    .map_err(|e| CoreError::new(CoreErrorCode::InvalidParams, e))?;
                Ok::<_, CoreError>(serde_json::to_value(doc).unwrap_or(Value::Null))
            })
            .await
        }

        // 手写包的编辑入口（用户裁决：编辑功能必须能打开所有 Lua 包）：读回
        // 入口文件 + manifest，编辑器的源码模式展示、改写后经
        // `blueprint.saveSource` 落盘。与 `blueprint.load` 同一名字闸门与
        // 只读姿态 —— 路径永远落在策略根内的被点名包上。
        method::BLUEPRINT_LOAD_SOURCE => {
            let core_cfg = core.lock().await.config().clone();
            typed(params, |p: BlueprintLoadSourceParams| async move {
                let doc = blueprint_load_source(&p, &core_cfg)
                    .map_err(|e| CoreError::new(CoreErrorCode::InvalidParams, e))?;
                Ok::<_, CoreError>(serde_json::to_value(doc).unwrap_or(Value::Null))
            })
            .await
        }

        // 读回镜像的写侧：把手写包的入口文件原样写回，sha256 现场重算进
        // manifest（装载器照常验证字节）。目标包必须已存在（编辑不创建），
        // 覆盖必须显式 —— 与 `blueprint.save` 同一裁决。
        method::BLUEPRINT_SAVE_SOURCE => {
            let core_cfg = core.lock().await.config().clone();
            typed(params, |p: BlueprintSaveSourceParams| async move {
                let receipt = blueprint_save_source(&p, &core_cfg)
                    .map_err(|e| CoreError::new(CoreErrorCode::InvalidParams, e))?;
                Ok::<_, CoreError>(serde_json::to_value(receipt).unwrap_or(Value::Null))
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

/// #363: the `execution_policy.set` flow — validate params, write the ONE
/// named account's section, full-reload in memory, land one audit line.
/// Separate from `handle_line` so the early returns are plain `?`/`return`
/// on a real `Result` (a dispatch arm's tail cannot `return` from the match
/// without skipping the reply encoding).
async fn execution_policy_set(
    core: &Arc<AsyncMutex<Core>>,
    p: ExecutionPolicySetParams,
) -> Result<Value, (i32, String, Option<ErrorData>)> {
    let path = {
        let c = core.lock().await;
        match c.config().execution_policy_path.clone() {
            Some(p) => p,
            None => {
                return Err((
                    Failure::INVALID_PARAMS,
                    "execution policy disabled (no config path)".to_string(),
                    None,
                ));
            }
        }
    };
    let before = {
        let c = core.lock().await;
        c.execution_policy_section_view(&p.account_id)
            .map(|v| serde_json::to_value(&v).unwrap_or(Value::Null))
    };
    let section = section_from_set_params(&p).map_err(|e| (Failure::INVALID_PARAMS, e, None))?;
    policy_section_write(&path, &p.account_id, Some(section))
        .map_err(|e| (Failure::INVALID_PARAMS, e, None))?;
    let mut c = core.lock().await;
    c.reload_execution_policy()
        .map_err(|e| (Failure::INVALID_PARAMS, e, None))?;
    // `after` is the RELOADED view — the state the kernel will actually
    // judge under, not the in-memory state the write replaced.
    let after = c
        .execution_policy_section_view(&p.account_id)
        .map(|v| serde_json::to_value(&v).unwrap_or(Value::Null));
    // #386: this core's OWN journal — a test core's records never land in
    // the checkout's `data/audit/…` file another parallel test counts on.
    c.execution_policy_append_audit(&crate::execution_policy::PolicyAuditRecord {
        ts_ms: now_ms(),
        actor: crate::execution_policy::ACTOR_IPC.to_string(),
        action: "set".to_string(),
        account_id: p.account_id.clone(),
        before,
        after,
        error: None,
    });
    Ok(serde_json::json!({ "accountId": p.account_id }))
}

/// #363: the `execution_policy.reset` flow — remove the account's section
/// (it falls back to the globals), full-reload, audit.
async fn execution_policy_reset(
    core: &Arc<AsyncMutex<Core>>,
    p: ExecutionPolicyGetParams,
) -> Result<Value, (i32, String, Option<ErrorData>)> {
    let path = {
        let c = core.lock().await;
        match c.config().execution_policy_path.clone() {
            Some(p) => p,
            None => {
                return Err((
                    Failure::INVALID_PARAMS,
                    "execution policy disabled (no config path)".to_string(),
                    None,
                ));
            }
        }
    };
    let before = {
        let c = core.lock().await;
        c.execution_policy_section_view(&p.account_id)
            .map(|v| serde_json::to_value(&v).unwrap_or(Value::Null))
    };
    policy_section_write(&path, &p.account_id, None)
        .map_err(|e| (Failure::INVALID_PARAMS, e, None))?;
    let mut c = core.lock().await;
    c.reload_execution_policy()
        .map_err(|e| (Failure::INVALID_PARAMS, e, None))?;
    // `after` is the RELOADED view (see the set arm).
    let after = c
        .execution_policy_section_view(&p.account_id)
        .map(|v| serde_json::to_value(&v).unwrap_or(Value::Null));
    // #386: this core's OWN journal (see the set arm).
    c.execution_policy_append_audit(&crate::execution_policy::PolicyAuditRecord {
        ts_ms: now_ms(),
        actor: crate::execution_policy::ACTOR_IPC.to_string(),
        action: "reset".to_string(),
        account_id: p.account_id.clone(),
        before,
        after,
        error: None,
    });
    Ok(serde_json::json!({ "accountId": p.account_id }))
}

/// #363: turn `execution_policy.set` params into the section that lands in
/// the TOML. Decimals arrive as strings so the shortest-decimal rule (0.10
/// is the decimal 0.10, not a binary expansion) survives the JSON hop; they
/// are validated HERE by round-tripping through the real parser before any
/// file is touched — a bad ratio is refused on the wire, not discovered by a
/// later boot refusal.
fn section_from_set_params(
    p: &ExecutionPolicySetParams,
) -> Result<crate::execution_policy::SectionFields, String> {
    use crate::execution_policy::SectionFields;
    let mut s = SectionFields::default();
    if let Some(v) = &p.budget_ratio {
        s.budget_ratio = Some(
            rust_decimal::Decimal::from_str_exact(v)
                .map_err(|_| format!("budgetRatio `{v}` is not a decimal"))?,
        );
    }
    if let Some(v) = &p.min_budget_usd {
        s.min_budget_usd = Some(
            rust_decimal::Decimal::from_str_exact(v)
                .map_err(|_| format!("minBudgetUsd `{v}` is not a decimal"))?,
        );
    }
    if let Some(v) = &p.max_budget_usd {
        s.max_budget_usd = Some(
            rust_decimal::Decimal::from_str_exact(v)
                .map_err(|_| format!("maxBudgetUsd `{v}` is not a decimal"))?,
        );
    }
    if let Some(v) = &p.min_equity_usd {
        s.min_equity_usd = Some(
            rust_decimal::Decimal::from_str_exact(v)
                .map_err(|_| format!("minEquityUsd `{v}` is not a decimal"))?,
        );
    }
    s.max_positions_per_asset = p.max_positions_per_asset;
    if let Some(rules) = &p.rules {
        let mut out = Vec::with_capacity(rules.len());
        for r in rules {
            out.push(
                serde_json::from_value::<crate::execution_policy::Rule>(r.clone())
                    .map_err(|e| format!("rules entry does not parse: {e}"))?,
            );
        }
        s.rules = Some(out);
    }
    // The section must validate as the file would see it: round-trip it the
    // way `policy_section_write` actually writes — insert into a typed file,
    // render the WHOLE file, parse it back. Same parser, same refusals.
    // (Rendering the bare section under a hand-typed header is NOT the same
    // thing: `toml::to_string_pretty` spells the rules array `[[rules]]`
    // without the `[accounts.<id>]` prefix, so a section with rules could
    // never pass that probe — #364 fixed the validator to match the writer.)
    let mut probe_file = crate::execution_policy::PolicyFile::default();
    probe_file
        .accounts
        .insert("__probe__".to_string(), s.clone());
    let rendered =
        toml::to_string_pretty(&probe_file).map_err(|e| format!("section does not render: {e}"))?;
    crate::execution_policy::Policy::load_from_str(&rendered)
        .map_err(|e| format!("section rejected: {e}"))?;
    Ok(s)
}

/// #363: rewrite ONE account section inside the policy TOML (read file →
/// swap the map entry → write back). The section is REPLACED wholesale —
/// `set` never touches another account's lines, and `reset` (section =
/// None) removes the entry so the account falls back to the globals.
/// Cross-account safety is structural: the caller names exactly one
/// account id and this function writes exactly that map key.
fn policy_section_write(
    path: &str,
    account_id: &str,
    section: Option<crate::execution_policy::SectionFields>,
) -> Result<(), String> {
    if account_id.trim().is_empty() {
        return Err("account id must not be empty".to_string());
    }
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut file: crate::execution_policy::PolicyFile = toml::from_str(&text)
        .map_err(|e| format!("existing file does not parse; refusing to edit: {e}"))?;
    match section {
        Some(s) => {
            file.accounts.insert(account_id.to_string(), s);
        }
        None => {
            if file.accounts.remove(account_id).is_none() {
                return Err(format!(
                    "execution policy: no section for account `{account_id}` to reset"
                ));
            }
        }
    }
    let out =
        toml::to_string_pretty(&file).map_err(|e| format!("policy file does not render: {e}"))?;
    std::fs::write(path, out).map_err(|e| format!("policy file write failed: {e}"))?;
    Ok(())
}

/// The strategy-package root `blueprint.save` writes into.
///
/// The kernel's OWN discovery rule decides the path (not the spec's draft
/// wording): Lua packages live where `--lua-strategy-dir` scans, which is
/// `user_layer/strategies_lua` by default (`main::default_lua_strategy_dir`)
/// — `user_layer/strategies` is the DYLIB fixture tree and must never receive
/// a Lua package (docs/DEV_V0_3.md §1901). `BLITZKRIEG_LUA_STRATEGY_DIR`
/// overrides the default — the same deployment knob idea as the CLI flag,
/// kept as an env var so a host operator can redirect package writes without
/// an IPC-side path parameter (no caller may ever name a directory).
/// #362: validate a save request and write the strategy package.
///
/// Order is the safety property, mirroring the compile chain: the NAME is
/// screened first (it becomes a directory name and a `place_order`-adjacent
/// literal — the same `os.`/`io.`/`debug` wall the compiler applies to the
/// blueprint name applies here, plus a package must not be named like a path),
/// then the blueprint compiles fresh (a compile refusal means nothing was
/// written), then an EXISTING package of the same name refuses the save unless
/// the caller explicitly overwrote (a silent overwrite could replace a
/// strategy an operator has enabled), and only then do the three files land.
/// The save root, read from the core's OWN config (falling back to the
/// deployment default). An env-var read here would be a shared-process side
/// channel that races test parallelism for no production benefit: the kernel's
/// strategy root is configuration, so it is read as configuration.
fn blueprint_strategy_root(core_cfg: &crate::service::CoreConfig) -> String {
    core_cfg
        .lua_strategy_dir
        .clone()
        .unwrap_or_else(|| "user_layer/strategies_lua".to_string())
}

/// The package-name screen shared by `blueprint.save` and `blueprint.load`
/// (#393): the name becomes a directory name and a `place_order`-adjacent
/// literal — the same `os.`/`io.`/`debug` wall the compiler applies to the
/// blueprint name applies here, plus a package must not be named like a path.
/// The load path needs the SAME screen so no caller can steer its read
/// outside the strategy root.
fn validate_package_name(raw: &str) -> Result<String, String> {
    const FORBIDDEN: [&str; 3] = ["os.", "io.", "debug"];
    let name = raw.trim();
    if name.is_empty() {
        return Err("blueprint name must not be empty".to_string());
    }
    if name.len() > 64 {
        return Err(format!(
            "blueprint name is {} characters; the ceiling is 64",
            name.len()
        ));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(format!(
            "blueprint name {name:?} may only hold ASCII letters, digits, '_' and '-' — \
             it becomes a strategy-package directory name"
        ));
    }
    if let Some(bad) = FORBIDDEN.iter().find(|bad| name.contains(**bad)) {
        return Err(format!(
            "blueprint name {bad:?} substring is refused: the name is embedded in the \
             generated source verbatim"
        ));
    }
    Ok(name.to_string())
}

/// #393: read one saved package's blueprint document back — the read-only
/// mirror of [`blueprint_save`], feeding the strategy page's 编辑策略 entry.
/// Hand-written packages carry no `blueprint.json` (they were never compiled
/// from one), and that fact is the ERROR — the caller says so, it does not
/// synthesize a graph the strategy never had.
fn blueprint_load(
    p: &BlueprintLoadParams,
    core_cfg: &crate::service::CoreConfig,
) -> Result<BlueprintLoadResult, String> {
    let name = validate_package_name(&p.name)?;
    let root = blueprint_strategy_root(core_cfg);
    let path = std::path::Path::new(&root)
        .join(&name)
        .join("blueprint.json");
    let json = std::fs::read_to_string(&path).map_err(|_| {
        format!(
            "no blueprint document for `{name}` under {root} — the package was not \
             written by blueprint.save, so there is no graph to preload"
        )
    })?;
    Ok(BlueprintLoadResult {
        blueprint_path: format!("{root}/{name}/blueprint.json"),
        json,
        name,
    })
}

/// 手写包的读回（`blueprint.loadSource`）：`blueprint.load` 在包没有
/// blueprint.json 时会拒绝，而编辑入口必须能打开**所有** Lua 包 —— 这里读
/// 回入口文件与 manifest 本身，编辑器的源码模式展示它们。同一名字闸门；
/// 入口文件按 manifest 的 `entry` 字段定位（缺省 `strategy.lua`），manifest
/// 缺失或字段缺失按原样报错 —— 不编造元数据。
fn blueprint_load_source(
    p: &BlueprintLoadSourceParams,
    core_cfg: &crate::service::CoreConfig,
) -> Result<BlueprintLoadSourceResult, String> {
    let name = validate_package_name(&p.name)?;
    let root = blueprint_strategy_root(core_cfg);
    let pkg = std::path::Path::new(&root).join(&name);
    let manifest_raw = std::fs::read_to_string(pkg.join("manifest.json")).map_err(|_| {
        format!(
            "no manifest for `{name}` under {root} — not a strategy package the loader \
             would scan"
        )
    })?;
    let manifest: Value = serde_json::from_str(&manifest_raw)
        .map_err(|e| format!("manifest for `{name}` is not valid JSON: {e}"))?;
    let entry = manifest
        .get("entry")
        .and_then(|v| v.as_str())
        .unwrap_or("strategy.lua")
        .to_string();
    if entry.contains("..") || entry.contains('/') || entry.contains('\\') {
        return Err(format!(
            "manifest entry {entry:?} for `{name}` must be a bare file name — refusing to \
             read outside the package directory"
        ));
    }
    let lua = std::fs::read_to_string(pkg.join(&entry))
        .map_err(|e| format!("cannot read {root}/{name}/{entry}: {e}"))?;
    let sha256 = manifest
        .get("sha256")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    Ok(BlueprintLoadSourceResult {
        package_dir: pkg.display().to_string(),
        lua_path: format!("{root}/{name}/{entry}"),
        manifest_path: format!("{root}/{name}/manifest.json"),
        lua,
        sha256,
        manifest,
        name,
    })
}

/// 手写包的写回（`blueprint.saveSource`）：`blueprint.loadSource` 的镜像 ——
/// 入口文件原样落盘，sha256 现场重算进 manifest（§6.4：装载器验证字节，漂移
/// 即拒载，所以 manifest 里的摘要必须跟随新字节）。目标包必须已存在（这个
/// 动词编辑、不创建），入口文件名沿用读回时的同一闸门。
fn blueprint_save_source(
    p: &BlueprintSaveSourceParams,
    core_cfg: &crate::service::CoreConfig,
) -> Result<BlueprintSaveSourceResult, String> {
    use sha2::Digest;

    let name = validate_package_name(&p.name)?;
    let root = blueprint_strategy_root(core_cfg);
    let pkg = std::path::Path::new(&root).join(&name);
    let manifest_path = pkg.join("manifest.json");
    if !manifest_path.is_file() {
        return Err(format!(
            "no manifest for `{name}` under {root} — saveSource edits an EXISTING package; \
             create one with blueprint.save first"
        ));
    }
    let manifest_raw = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("cannot read {root}/{name}/manifest.json: {e}"))?;
    let mut manifest: Value = serde_json::from_str(&manifest_raw)
        .map_err(|e| format!("manifest for `{name}` is not valid JSON: {e}"))?;
    let entry = manifest
        .get("entry")
        .and_then(|v| v.as_str())
        .unwrap_or("strategy.lua")
        .to_string();
    if entry.contains("..") || entry.contains('/') || entry.contains('\\') {
        return Err(format!(
            "manifest entry {entry:?} for `{name}` must be a bare file name — refusing to \
             write outside the package directory"
        ));
    }
    let lua_path = pkg.join(&entry);
    if !p.overwrite {
        let existing = std::fs::read_to_string(&lua_path).unwrap_or_default();
        if existing != p.lua {
            return Err(format!(
                "strategy package {root}/{name} exists with different source — writing over \
                 it must be explicit (overwrite: true)"
            ));
        }
    }
    std::fs::write(&lua_path, p.lua.as_bytes())
        .map_err(|e| format!("cannot write {root}/{name}/{entry}: {e}"))?;

    let mut hasher = sha2::Sha256::new();
    hasher.update(p.lua.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    manifest["sha256"] = Value::String(digest.clone());
    let manifest_body = format!(
        "{}\n",
        serde_json::to_string_pretty(&manifest)
            .map_err(|e| format!("manifest serialization failed: {e}"))?
    );
    std::fs::write(&manifest_path, manifest_body.as_bytes())
        .map_err(|e| format!("cannot write {root}/{name}/manifest.json: {e}"))?;

    Ok(BlueprintSaveSourceResult {
        package_dir: pkg.display().to_string(),
        lua_path: lua_path.display().to_string(),
        manifest_path: format!("{root}/{name}/manifest.json"),
        lua_sha256: digest,
        bytes: [p.lua.len() as u64, manifest_body.len() as u64],
        name,
    })
}

fn blueprint_save(
    p: &BlueprintSaveParams,
    core_cfg: &crate::service::CoreConfig,
) -> Result<BlueprintSaveResult, String> {
    use sha2::Digest;

    let name = validate_package_name(&p.name)?;

    // Compile FRESH — the receipt vouches for what THIS call produced, and a
    // structurally invalid blueprint writes nothing. The tunables ride along:
    // the numeric anchors the compiled source reads through `__param` are
    // exported into the manifest (with ±half-magnitude domains), which is
    // what registers the package as EVOLVABLE (#393 shape: the loader's
    // specs_from_tunables needs an explicit domain to declare a knob).
    let (lua, tunables) = crate::blueprint::compile_with_tunables(&p.json)?;

    let root = blueprint_strategy_root(core_cfg);
    let pkg = std::path::Path::new(&root).join(&name);
    if pkg.join("manifest.json").is_file() && !p.overwrite {
        return Err(format!(
            "strategy package {root}/{name} already exists — saving over a package the \
             loader scans must be explicit (overwrite: true)"
        ));
    }
    std::fs::create_dir_all(&pkg).map_err(|e| format!("cannot create {root}/{name}: {e}"))?;

    let blueprint_path = pkg.join("blueprint.json");
    let lua_path = pkg.join("strategy.lua");
    std::fs::write(&blueprint_path, p.json.as_bytes())
        .map_err(|e| format!("cannot write {root}/{name}/blueprint.json: {e}"))?;
    std::fs::write(&lua_path, lua.as_bytes())
        .map_err(|e| format!("cannot write {root}/{name}/strategy.lua: {e}"))?;

    // The manifest follows §6.4 exactly: name = directory name (one identity),
    // sha256 = the digest of the entry file we just wrote (the loader verifies
    // it and REFUSES the package on any drift), modes = the fixed declaration
    // the codegen emits inside declare_modes(). The tunables export (用户裁决：
    // 蓝图生产的策略也要支持进化) carries one entry per numeric anchor the
    // generated source reads through `__param` — type decimal (they are all
    // numbers by construction) + default + the ±half-magnitude domain the
    // compiler computed, so shadow evolution can register and walk them.
    let mut hasher = sha2::Sha256::new();
    hasher.update(lua.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    let mut tunables_map = serde_json::Map::new();
    for t in &tunables {
        tunables_map.insert(
            t.name.clone(),
            serde_json::json!({
                "type": "decimal",
                "default": t.default,
                "min": t.min,
                "max": t.max,
            }),
        );
    }
    let mut manifest = serde_json::json!({
        "name": name,
        "version": "1.0.0",
        "api": "1.0",
        "entry": "strategy.lua",
        "sha256": digest,
        "author": "BlitzkriegBot",
        "description": format!(
            "Blueprint-compiled strategy from blueprint.json (name {name:?}); every \
             decision is the whitelisted node graph it declares — the kernel \
             adjudicates, sizes and gates every intent."
        ),
        "modes": [
            {
                "market_type": "prediction",
                "structure": "binary_outcome_wheel",
                "capabilities": ["websocket_feed", "level2_snapshot"]
            }
        ],
    });
    if !tunables_map.is_empty() {
        manifest["tunables"] = Value::Object(tunables_map);
    }
    let manifest_body = format!(
        "{}\n",
        serde_json::to_string_pretty(&manifest)
            .map_err(|e| format!("manifest serialization failed: {e}"))?
    );
    let manifest_path = pkg.join("manifest.json");
    std::fs::write(&manifest_path, manifest_body.as_bytes())
        .map_err(|e| format!("cannot write {root}/{name}/manifest.json: {e}"))?;

    Ok(BlueprintSaveResult {
        name: name.to_string(),
        package_dir: pkg.display().to_string(),
        blueprint_path: blueprint_path.display().to_string(),
        lua_path: lua_path.display().to_string(),
        manifest_path: manifest_path.display().to_string(),
        lua_sha256: digest,
        bytes: [
            p.json.len() as u64,
            lua.len() as u64,
            manifest_body.len() as u64,
        ],
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

/// A core error that is a CLIENT fault — an unknown account id, a tighten-only
/// refusal — surfaces as the wire-level INVALID_PARAMS the spec shows
/// (§12.1: `frozen -> active` must be refused with INVALID_PARAMS), not as a
/// generic application error. Anything else stays APPLICATION.
fn client_err(e: crate::model::CoreError) -> (i32, String, Option<ErrorData>) {
    let msg = format!("{e}");
    let code = if e.code == CoreErrorCode::InvalidParams {
        Failure::INVALID_PARAMS
    } else {
        Failure::APPLICATION
    };
    (
        code,
        msg,
        Some(ErrorData {
            core_code: e.code,
            raw: e.raw,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kernel must be able to read a peer's uid on a real connected socket:
    /// a helper that always errors would silently downgrade every deployment to
    /// "file mode only" without anyone noticing.
    #[test]
    fn peer_uid_of_a_real_socketpair_is_our_own_uid() {
        let (a, _b) = std::os::unix::net::UnixStream::pair().expect("socketpair");
        let got = peer_uid(a.as_raw_fd());
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            assert_eq!(
                got.expect("peer credentials on this platform"),
                own_uid(),
                "a socketpair peer is this process, so its uid must be ours"
            );
        }
    }

    /// Same uid → accepted; different uid → refused. This is the predicate the
    /// accept loop applies, and the whole point of #187: the socket mode is not
    /// the only control.
    #[test]
    fn a_foreign_uid_is_refused_and_our_own_is_accepted() {
        assert!(PeerAuth::SameUid { uid: 501 }.accepted());
        assert!(!PeerAuth::OtherUid { uid: 502 }.accepted());
        // Unsupported platforms degrade instead of refusing every session.
        assert!(
            PeerAuth::Unavailable {
                reason: "unsupported".into()
            }
            .accepted()
        );
    }

    /// `classify_peer` on a real loopback socketpair must land on SameUid — the
    /// shape the accept loop sees for a legitimate client (the kernel, the panel
    /// and the gate scripts all run as the core's own user).
    #[test]
    fn classify_peer_accepts_our_own_uid() {
        if !cfg!(any(target_os = "linux", target_os = "macos")) {
            return;
        }
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().expect("socketpair");
        // Wrapping the std socket in a tokio one needs a reactor, and it must be
        // the same `UnixStream` type the accept loop hands to `classify_peer`.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        theirs.set_nonblocking(true).expect("nonblocking");
        let peer = rt.block_on(async {
            let tokio_end = UnixStream::from_std(theirs).expect("tokio wrap");
            classify_peer(&tokio_end)
        });
        assert_eq!(peer, PeerAuth::SameUid { uid: own_uid() });
        drop(ours);
    }

    /// The socket node must end up owner-only, whatever mode it started from.
    ///
    /// Every assertion here is written against the LITERAL `0o600` (and the
    /// bit-level forms `applied & 0o077 == 0` / `applied & 0o700 == 0o600`),
    /// never against `SOCKET_MODE`. Asserting `applied == SOCKET_MODE` makes the
    /// test self-referential: it moves with the very constant it exists to
    /// police, so widening `SOCKET_MODE` to `0o666` — precisely the exposure
    /// #187 fixed — would leave it green. Verified by mutation: with
    /// `SOCKET_MODE = 0o666` the old form passed and this form fails.
    ///
    /// The starting modes are set explicitly rather than inherited from the
    /// umask, so "already narrow stays narrow" and "loose is narrowed" are both
    /// exercised on every machine instead of only where `bind` happens to land
    /// on a loose mode.
    #[test]
    fn restrict_socket_leaves_the_node_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("bk-ipc-sock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");

        for before in [0o600, 0o666, 0o777] {
            let path = dir.join(format!("probe-{before:o}.sock"));
            let _listener = rt.block_on(async { UnixListener::bind(&path).expect("bind") });
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(before))
                .expect("seed mode");
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                before,
                "precondition: the node starts at {before:04o}"
            );

            let applied = restrict_socket(path.to_str().unwrap()).expect("chmod");
            assert_eq!(
                applied, 0o600,
                "socket must be owner-only after starting at {before:04o}"
            );
            assert_eq!(
                applied & 0o077,
                0,
                "no group/other bit may survive {before:04o}: got {applied:04o}"
            );
            assert_eq!(
                applied & 0o700,
                0o600,
                "the owner keeps read+write and gains nothing: got {applied:04o}"
            );
            // The reported mode must be the mode on disk — a helper that chmod'ed
            // nothing but returned 0600 would satisfy the checks above.
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600,
                "the mode on disk must match the mode reported for {before:04o}"
            );
        }

        // And the mode `bind` itself produces, which is umask-dependent (0755 on
        // a default umask, 0700 under a hardened one). The property that must
        // hold either way is the one asserted — no group/other bit survives —
        // rather than a specific starting mode, so this cannot turn into a
        // precondition that silently skips the check.
        let path = dir.join("probe-bound.sock");
        let _listener = rt.block_on(async { UnixListener::bind(&path).expect("bind") });
        let created = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        let applied = restrict_socket(path.to_str().unwrap()).expect("chmod");
        assert_eq!(
            applied, 0o600,
            "socket must be owner-only after bind (bind created {created:04o})"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A second core must never steal a live socket. "Refused" alone is not the
    /// property that matters: the theft is the UNLINK, because the old kernel
    /// keeps trading while every client (panel, gate scripts) reconnects to
    /// whoever binds next. So this pins three things — a readable refusal, a
    /// node that survives with the same inode, and the original listener still
    /// answering — and then the opposite case: a stale node of our own uid is
    /// still reclaimable, so the guard protects a live service and not the name.
    #[test]
    fn a_live_socket_is_not_stolen_by_a_second_core() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::net::{UnixListener as StdListener, UnixStream as StdStream};

        // A private directory under $TMPDIR — never the production socket path.
        let dir = std::env::temp_dir().join(format!("bk-ipc-live-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("live.sock");
        let path_s = path.to_str().expect("utf-8 path").to_string();

        // The first core: a real listener that answers, which is exactly what
        // the guard's probe sees for a healthy kernel.
        let listener = StdListener::bind(&path).expect("bind the live socket");
        let ino = std::fs::metadata(&path).expect("stat").ino();
        std::thread::spawn(move || {
            for mut s in listener.incoming().flatten() {
                let _ = s.write_all(b"alive\n");
            }
        });

        // Precondition: the probe reaches the live core, so a passing guard below
        // is not passing because nothing was listening.
        let mut probe = StdStream::connect(&path_s).expect("the live socket must accept");
        let mut line = String::new();
        BufReader::new(&mut probe)
            .read_line(&mut line)
            .expect("read the greeting");
        assert_eq!(line.trim(), "alive", "precondition: a live core answers");
        drop(probe);

        // The second core claiming the same path.
        let err = claim_socket_path(&path_s).expect_err("a live socket must be refused");
        let msg = err.to_string();
        assert!(
            msg.contains("already listening"),
            "the refusal must be readable: {msg}"
        );
        assert!(msg.contains(&path_s), "the refusal must name it: {msg}");

        // The node survived, unchanged, and the original listener is still the
        // one serving it — an unlinked node could not answer at all.
        assert!(path.exists(), "the live socket node must not be unlinked");
        assert_eq!(
            std::fs::metadata(&path).expect("stat").ino(),
            ino,
            "same node, not a re-created one"
        );
        let mut after =
            StdStream::connect(&path_s).expect("the live core must still accept after the refusal");
        let mut line = String::new();
        BufReader::new(&mut after)
            .read_line(&mut line)
            .expect("read the greeting");
        assert_eq!(
            line.trim(),
            "alive",
            "the original core must still be serving"
        );
        drop(after);

        // A crashed core leaves a node with nobody behind it: that one IS
        // reclaimable, otherwise no restart could ever bind.
        let stale_path = dir.join("stale.sock");
        let stale_s = stale_path.to_str().expect("utf-8 path").to_string();
        let stale = StdListener::bind(&stale_path).expect("bind a node to leave behind");
        drop(stale);
        assert!(
            stale_path.exists(),
            "precondition: the crashed node is on disk"
        );
        claim_socket_path(&stale_s).expect("a stale node must be claimable");
        assert!(
            !stale_path.exists(),
            "a stale node is cleared so bind can succeed"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── #191: the limited hot reload, at the wire ───────────────────────────
    //
    // The issue is about what a client may change LIVE, so both tests below
    // travel the road a panel/CLI does — `handle_line` with the params bytes.
    // Between them they pin the three facts that make the feature safe: the
    // change lands (with its audit record), the NEXT order is judged by the new
    // value, and everything outside the entry limits is refused with nothing
    // applied.

    /// One request/response over the same entry point a session uses.
    async fn rpc(
        core: &Arc<AsyncMutex<Core>>,
        registry: &crate::market::registry::MarketPluginRegistry,
        peer: &PeerAuth,
        line: String,
    ) -> Value {
        let update_state = Arc::new(crate::ipc::version::UpdateState::new(false, false));
        let jobs = Arc::new(crate::backtest_jobs::BacktestJobs::new());
        let mut session = SessionState::default();
        serde_json::from_str(
            &handle_line(
                core,
                registry,
                line,
                peer,
                &update_state,
                &jobs,
                &mut session,
            )
            .await,
        )
        .expect("every reply is one JSON object")
    }

    /// Like [`rpc`], but the caller owns the whole session state — the E29
    /// verbs read AND write the session's kline subscription set, so a test
    /// asserting a session's set LINGERED across requests has to hold the
    /// same cell the arm holds.
    async fn rpc_with_subs(
        core: &Arc<AsyncMutex<Core>>,
        registry: &crate::market::registry::MarketPluginRegistry,
        peer: &PeerAuth,
        session: &mut SessionState,
        line: String,
    ) -> Value {
        let update_state = Arc::new(crate::ipc::version::UpdateState::new(false, false));
        let jobs = Arc::new(crate::backtest_jobs::BacktestJobs::new());
        serde_json::from_str(
            &handle_line(core, registry, line, peer, &update_state, &jobs, session).await,
        )
        .expect("every reply is one JSON object")
    }
    /// A core whose interesting knob is the per-order cap: 3 USD, so the 2.00
    /// USD ticket below (`order_line`) is admitted BEFORE any hot change and
    /// refused once the cap drops to 1. The engine is installed from the startup
    /// config exactly as `serve` does it, so its sizing is the boot-time COPY a
    /// change has to reach to count as "live".
    ///
    /// Every log path is None: this test places real (dry) orders, and a test
    /// must not leave a durable order/position log behind in whatever checkout
    /// it happens to be run from.
    async fn hot_reload_fixture() -> (
        Arc<AsyncMutex<Core>>,
        crate::market::registry::MarketPluginRegistry,
        PeerAuth,
    ) {
        use rust_decimal_macros::dec;
        let cfg = CoreConfig {
            risk: crate::risk::RiskConfig {
                max_order_notional: dec!(3),
                ..Default::default()
            },
            dry_seed_balance: dec!(100),
            trade_log_path: None,
            order_log_path: None,
            position_log_path: None,
            ..Default::default()
        };
        let mut c = Core::new(cfg.clone());
        c.set_balance(cfg.dry_seed_balance);
        cfg.install_engine(&mut c).expect("engine install");
        (
            Arc::new(AsyncMutex::new(c)),
            crate::market::registry::MarketPluginRegistry::new(),
            PeerAuth::SameUid { uid: own_uid() },
        )
    }

    /// A book with 100 ask @ 0.40, which the dry taker in `order_line` crosses.
    fn books_line() -> String {
        r#"{"jsonrpc":"2.0","id":1,"method":"books.snapshot","params":{"tokenId":"tok","bids":[{"price":0.39,"size":100}],"asks":[{"price":0.40,"size":100}]}}"#.to_string()
    }

    /// 5 × 0.40 = 2.00 USD against `tok`. The `roundSlot` is the LIVE one, so the
    /// position this opens expires in the future instead of in 1970 (a stale slot
    /// makes the next tick force-exit it).
    fn order_line(id: u32, key: &str, asset: &str, direction: &str) -> String {
        let slot = now_ms() / 1000 / 900;
        format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"orders.place","params":{{"tokenId":"tok","conditionId":"cond","side":"buy","mode":"taker","price":0.40,"size":5,"internalKey":"{key}","strategy":"s","asset":"{asset}","direction":"{direction}","roundSlot":{slot}}}}}"#
        )
    }

    fn stats_line(id: u32) -> String {
        format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"engine.stats","params":{{}}}}"#)
    }

    // ── #353: the WebUI backtest surface, at the wire ────────────────────────

    /// Like [`rpc`], but the caller holds the job registry — the six backtest
    /// arms all front ONE shared registry, and a test pinning submission-time
    /// validation must hand in the same instance (with its pinned dataset
    /// root), not a fresh one per call.
    #[allow(clippy::too_many_arguments)]
    async fn rpc_jobs(
        core: &Arc<AsyncMutex<Core>>,
        registry: &crate::market::registry::MarketPluginRegistry,
        peer: &PeerAuth,
        jobs: &Arc<crate::backtest_jobs::BacktestJobs>,
        id: u32,
        method: &str,
        params: &str,
    ) -> Value {
        let line =
            format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{params}}}"#);
        let update_state = Arc::new(crate::ipc::version::UpdateState::new(false, false));
        let mut session = SessionState::default();
        serde_json::from_str(
            &handle_line(
                core,
                registry,
                line,
                peer,
                &update_state,
                jobs,
                &mut session,
            )
            .await,
        )
        .expect("every reply is one JSON object")
    }

    /// The six arms are wired to the registry through the REAL entry point:
    /// list answers the envelope, the read trio name unknown jobs, run/pull
    /// validate at the wire (a refusal here is a JSON-RPC error with the
    /// actionable message, and NO job exists behind it).
    #[tokio::test]
    async fn the_backtest_surface_answers_at_the_wire() {
        let (core, registry, peer) = hot_reload_fixture().await;
        let dir =
            std::env::temp_dir().join(format!("bk-ipc-jobs-{}-{}", std::process::id(), now_ms()));
        std::fs::create_dir_all(&dir).expect("scratch root");
        let jobs = Arc::new(crate::backtest_jobs::BacktestJobs::with_root(dir.clone()));

        // list: an empty root is a valid, empty envelope — not an error.
        let list = rpc_jobs(
            &core,
            &registry,
            &peer,
            &jobs,
            1,
            method::BACKTEST_ONCHAIN_LIST,
            "{}",
        )
        .await;
        assert!(list.get("error").is_none(), "list answers: {list}");
        assert!(list["result"]["datasets"].is_array(), "{list}");

        // run: an archive outside the dataset root is refused BY NAME at the
        // wire — and no job was created behind the refusal.
        let run = rpc_jobs(
            &core,
            &registry,
            &peer,
            &jobs,
            2,
            method::BACKTEST_RUN,
            r#"{"archive":"/etc/passwd"}"#,
        )
        .await;
        assert!(
            run["error"]["message"]
                .as_str()
                .is_some_and(|m| m.contains("not a dataset")),
            "the refusal names the boundary: {run}"
        );

        // The read trio on a job id that never existed: named JSON-RPC errors.
        for (id, m) in [
            (3, method::BACKTEST_STATUS),
            (4, method::BACKTEST_RESULT),
            (5, method::BACKTEST_EXPORT),
        ] {
            let v = rpc_jobs(&core, &registry, &peer, &jobs, id, m, r#"{"id":"bt-9999"}"#).await;
            assert!(
                v["error"]["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("unknown job")),
                "{m} names the unknown job: {v}"
            );
        }

        // pull: a malformed wallet is refused with the actionable message.
        let pull = rpc_jobs(
            &core,
            &registry,
            &peer,
            &jobs,
            6,
            method::BACKTEST_ONCHAIN_PULL,
            r#"{"wallet":"0xabc","start":"2026-01-01","end":"2026-01-02"}"#,
        )
        .await;
        assert!(
            pull["error"]["message"]
                .as_str()
                .is_some_and(|m| m.contains("0x")),
            "the refusal names the wallet shape: {pull}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #191 acceptance (a): one `risk.setLimits` call moves the entry limits
    /// without a restart, answers with the audit record (old → new, actor,
    /// instant, `persisted:false`) and the very NEXT order is judged by the NEW
    /// value.
    #[tokio::test]
    async fn risk_set_limits_binds_the_next_order_without_a_restart() {
        let (core, registry, peer) = hot_reload_fixture().await;

        let feed = rpc(&core, &registry, &peer, books_line()).await;
        assert!(feed.get("error").is_none(), "feed the book first: {feed}");

        // 2.00 USD against the 3 USD startup cap: MUST be admitted, or the
        // post-change refusal below would prove nothing about the change.
        let before = rpc(
            &core,
            &registry,
            &peer,
            order_line(2, "probe-before", "BTC", "up"),
        )
        .await;
        assert!(
            before.get("error").is_none(),
            "2.00 must pass the 3 USD startup cap: {before}"
        );
        assert_eq!(
            before["result"]["status"],
            serde_json::json!("FILLED"),
            "the probe must be a real, filled order: {before}"
        );

        let applied = rpc(
            &core,
            &registry,
            &peer,
            r#"{"jsonrpc":"2.0","id":3,"method":"risk.setLimits","params":{"maxOrderNotional":1,"minShares":2,"maxShares":4,"reason":"tighten after drawdown"}}"#.to_string(),
        )
        .await;
        assert!(
            applied.get("error").is_none(),
            "the safe subset must be accepted: {applied}"
        );
        let audit = &applied["result"];
        assert_eq!(
            audit["persisted"],
            serde_json::json!(false),
            "the update must state that it is memory-only: {audit}"
        );
        assert_eq!(
            audit["actor"],
            serde_json::json!(format!("uid:{}", own_uid())),
            "who did it must be on the record: {audit}"
        );
        assert_eq!(audit["reason"], serde_json::json!("tighten after drawdown"));
        assert!(
            audit["atMs"].as_i64().unwrap_or(0) > 0,
            "when it happened must be on the record: {audit}"
        );
        let change = |field: &str| {
            audit["applied"]
                .as_array()
                .expect("an applied list")
                .iter()
                .find(|c| c["field"] == serde_json::json!(field))
                .cloned()
                .unwrap_or_else(|| panic!("{field} must be in the audit record: {audit}"))
        };
        assert_eq!(
            change("maxOrderNotional")["from"],
            serde_json::json!(3.0),
            "the audit keeps the OLD value"
        );
        assert_eq!(change("maxOrderNotional")["to"], serde_json::json!(1.0));
        assert_eq!(change("minShares")["from"], serde_json::json!(10.0));
        assert_eq!(change("minShares")["to"], serde_json::json!(2.0));
        assert_eq!(change("maxShares")["to"], serde_json::json!(4.0));

        // `engine.stats.sizing` is what the panel answers "what may ONE entry
        // commit with?" from, and with an engine installed it is read live off
        // the engine's own config — the same `global_sizing()` the ticket
        // arithmetic calls. So this is the no-restart half: the boot-time copy
        // moved too, not just the `CoreConfig` a new `Core` would be built from.
        let stats = rpc(&core, &registry, &peer, stats_line(4)).await;
        let sizing = &stats["result"]["sizing"];
        assert_eq!(sizing["maxOrderNotionalUsd"], serde_json::json!(1.0));
        assert_eq!(sizing["minShares"], serde_json::json!(2.0));
        assert_eq!(sizing["maxShares"], serde_json::json!(4.0));

        // The SAME 2.00 ticket, on another asset (so the only possible reason is
        // the cap, never the one-position-per-asset gate), judged by the NEW cap.
        let after = rpc(
            &core,
            &registry,
            &peer,
            order_line(5, "probe-after", "ETH", "down"),
        )
        .await;
        assert_eq!(
            after["error"]["data"]["coreCode"],
            serde_json::json!("RISK_REJECTED"),
            "the next order must be judged by the new cap: {after}"
        );
        assert!(
            after["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("exceeds per-order cap 1"),
            "the refusal must name the NEW cap: {after}"
        );
    }

    /// 生效风控可编辑（`risk.setSystemic`）：写路径的 wire 合同 —— patch
    /// 落进 matrix（readout 立即读回 flag 来源的新值）、回执逐项带 effect
    /// （live / next_session）与 old→new、空 patch 拒绝、负值拒绝且原子
    /// （同行 ride-along 的合法字段也不落）。下一笔入场按新值裁决由
    /// `engine_evaluate` → `process_intent` 的 §4.2 测试矩阵覆盖（arbitration/
    /// pipeline.rs 的 judge_entry 用例），这里不重复架一台引擎。
    #[tokio::test]
    async fn risk_set_systemic_writes_the_matrix_and_names_each_effect() {
        let (core, registry, peer) = hot_reload_fixture().await;

        // Arm ONE live bound + the drawdown row, which must carry next_session.
        let applied = rpc(
            &core,
            &registry,
            &peer,
            r#"{"jsonrpc":"2.0","id":1,"method":"risk.setSystemic","params":{"maxTotalPosition":1,"maxDailyDrawdownUsd":50,"reason":"live arm"}}"#.to_string(),
        )
        .await;
        assert!(
            applied.get("error").is_none(),
            "the systemic patch must be accepted: {applied}"
        );
        let audit = &applied["result"];
        assert_eq!(audit["persisted"], serde_json::json!(false));
        assert_eq!(
            audit["actor"],
            serde_json::json!(format!("uid:{}", own_uid())),
            "who did it must be on the record: {audit}"
        );
        let effect_of = |field: &str| {
            audit["applied"]
                .as_array()
                .expect("an applied list")
                .iter()
                .find(|c| c["field"] == serde_json::json!(field))
                .map(|c| c["effect"].as_str().expect("effect").to_string())
        };
        assert_eq!(
            effect_of("maxTotalPosition").as_deref(),
            Some("live"),
            "the position-count cap is a live bound: {audit}"
        );
        assert_eq!(
            effect_of("maxDailyDrawdownUsd").as_deref(),
            Some("next_session"),
            "the drawdown budget must state its next-session arming: {audit}"
        );
        let from_of = |field: &str| {
            audit["applied"]
                .as_array()
                .expect("an applied list")
                .iter()
                .find(|c| c["field"] == serde_json::json!(field))
                .map(|c| c["from"].as_str().expect("from").to_string())
        };
        assert_eq!(
            from_of("maxTotalPosition").as_deref(),
            Some("0"),
            "the old value (0 = off) is on the record: {audit}"
        );

        // The readout answers with the new values, flag-sourced.
        let read = rpc(
            &core,
            &registry,
            &peer,
            r#"{"jsonrpc":"2.0","id":2,"method":"risk.limits","params":{}}"#.to_string(),
        )
        .await;
        assert!(read.get("error").is_none(), "readout must answer: {read}");
        assert_eq!(
            read["result"]["global"]["limits"]["maxTotalPosition"]["value"],
            serde_json::json!("1")
        );
        assert_eq!(
            read["result"]["global"]["limits"]["maxTotalPosition"]["source"],
            serde_json::json!("flag")
        );
        assert_eq!(
            read["result"]["account"]["limits"]["maxDailyDrawdownUsd"]["value"],
            serde_json::json!("50")
        );

        // Empty patch is refused.
        let empty = rpc(
            &core,
            &registry,
            &peer,
            r#"{"jsonrpc":"2.0","id":3,"method":"risk.setSystemic","params":{}}"#.to_string(),
        )
        .await;
        assert!(
            empty["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("at least one"),
            "an empty patch is refused: {empty}"
        );

        // A negative value is refused with the field named — and the atomicity
        // means the legal field riding along did NOT land either.
        let negative = rpc(
            &core,
            &registry,
            &peer,
            r#"{"jsonrpc":"2.0","id":4,"method":"risk.setSystemic","params":{"maxTotalExposureUsd":"-5","maxCorrelationUsd":10}}"#.to_string(),
        )
        .await;
        assert!(
            negative["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("must be >= 0"),
            "a negative bound is refused with the field named: {negative}"
        );
        let read2 = rpc(
            &core,
            &registry,
            &peer,
            r#"{"jsonrpc":"2.0","id":5,"method":"risk.limits","params":{}}"#.to_string(),
        )
        .await;
        assert_eq!(
            read2["result"]["global"]["limits"]["maxCorrelationUsd"]["value"],
            serde_json::json!("0"),
            "a refused patch leaves no trace: {read2}"
        );
    }

    /// 退出纪律可编辑（`risk.setExit`）：写路径的 wire 合同 —— patch 落进
    /// Gate 4 绑定的活配置（readout 立即读回新值）、回执逐项带 old→new 与
    /// effect=live、空 patch 拒绝、非法值拒绝且原子（同行 ride-along 的合法
    /// 字段也不落）、`forceExitSec: 0` 是文档化的「关闭强平」开关照常接受。
    /// 下一笔入场按新值绑定由 arbitration/pipeline.rs 的 Gate 4 用例覆盖。
    #[tokio::test]
    async fn risk_set_exit_writes_the_triple_and_names_each_change() {
        let (core, registry, peer) = hot_reload_fixture().await;

        // Arm all three fields at once; the reply names each old→new.
        let applied = rpc(
            &core,
            &registry,
            &peer,
            r#"{"jsonrpc":"2.0","id":1,"method":"risk.setExit","params":{"stopLossPct":"8","takeProfitPct":"90","forceExitSec":120,"reason":"tighten"}}"#.to_string(),
        )
        .await;
        assert!(
            applied.get("error").is_none(),
            "the exit patch must be accepted: {applied}"
        );
        let audit = &applied["result"];
        assert_eq!(audit["persisted"], serde_json::json!(false));
        assert_eq!(
            audit["actor"],
            serde_json::json!(format!("uid:{}", own_uid())),
            "who did it must be on the record: {audit}"
        );
        let change_of = |field: &str| {
            audit["applied"]
                .as_array()
                .expect("an applied list")
                .iter()
                .find(|c| c["field"] == serde_json::json!(field))
                .map(|c| {
                    (
                        c["from"].as_str().expect("from").to_string(),
                        c["to"].as_str().expect("to").to_string(),
                        c["effect"].as_str().expect("effect").to_string(),
                    )
                })
        };
        // The factory calibration: SL 12, TP 100, guillotine OFF (0).
        assert_eq!(
            change_of("stopLossPct"),
            Some(("12".into(), "8".into(), "live".into())),
            "the stop row carries old→new and live: {audit}"
        );
        assert_eq!(
            change_of("takeProfitPct"),
            Some(("100".into(), "90".into(), "live".into())),
            "the take-profit row carries old→new and live: {audit}"
        );
        assert_eq!(
            change_of("forceExitSec"),
            Some(("0".into(), "120".into(), "live".into())),
            "the guillotine row carries old→new and live: {audit}"
        );

        // The readout answers with the new values.
        let read = rpc(
            &core,
            &registry,
            &peer,
            r#"{"jsonrpc":"2.0","id":2,"method":"risk.limits","params":{}}"#.to_string(),
        )
        .await;
        assert!(read.get("error").is_none(), "readout must answer: {read}");
        // The exit view crosses as NUMBERS (finite decimals serialize as
        // numbers), not the Bound strings the systemic matrix uses.
        assert_eq!(
            read["result"]["exit"]["stopLossPct"],
            serde_json::json!(8.0)
        );
        assert_eq!(
            read["result"]["exit"]["takeProfitPct"],
            serde_json::json!(90.0)
        );
        assert_eq!(
            read["result"]["exit"]["forceExitSec"],
            serde_json::json!(120)
        );

        // forceExitSec: 0 is the documented OFF switch, accepted.
        let off = rpc(
            &core,
            &registry,
            &peer,
            r#"{"jsonrpc":"2.0","id":3,"method":"risk.setExit","params":{"forceExitSec":0}}"#
                .to_string(),
        )
        .await;
        assert!(
            off.get("error").is_none(),
            "0 = guillotine OFF must be accepted: {off}"
        );

        // A zero percentage is refused — and atomicity means the legal field
        // riding along did NOT land either.
        let zero = rpc(
            &core,
            &registry,
            &peer,
            r#"{"jsonrpc":"2.0","id":4,"method":"risk.setExit","params":{"stopLossPct":"0","takeProfitPct":"50"}}"#.to_string(),
        )
        .await;
        assert!(
            zero["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("must be > 0"),
            "a zero percentage is refused with the field named: {zero}"
        );
        let read2 = rpc(
            &core,
            &registry,
            &peer,
            r#"{"jsonrpc":"2.0","id":5,"method":"risk.limits","params":{}}"#.to_string(),
        )
        .await;
        assert_eq!(
            read2["result"]["exit"]["takeProfitPct"],
            serde_json::json!(90.0),
            "a refused patch leaves no trace: {read2}"
        );

        // Empty patch is refused.
        let empty = rpc(
            &core,
            &registry,
            &peer,
            r#"{"jsonrpc":"2.0","id":6,"method":"risk.setExit","params":{}}"#.to_string(),
        )
        .await;
        assert!(
            empty["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("at least one"),
            "an empty patch is refused: {empty}"
        );
    }

    /// #191 acceptance (b): a knob OUTSIDE the safe subset is refused BY NAME,
    /// with the reason and with what to do instead — and the refusal is atomic,
    /// so an allowed field riding along in the same patch is not applied either.
    #[tokio::test]
    async fn risk_set_limits_refuses_a_forbidden_field_and_applies_nothing() {
        let (core, registry, peer) = hot_reload_fixture().await;

        let feed = rpc(&core, &registry, &peer, books_line()).await;
        assert!(feed.get("error").is_none(), "feed the book first: {feed}");
        let before = rpc(&core, &registry, &peer, stats_line(2)).await;

        // A double carrier: `maxOrderNotional` is inside the safe subset,
        // `stopLossPct` is an exit threshold. The whole request must be refused.
        let refused = rpc(
            &core,
            &registry,
            &peer,
            r#"{"jsonrpc":"2.0","id":3,"method":"risk.setLimits","params":{"maxOrderNotional":1,"stopLossPct":"0.50"}}"#.to_string(),
        )
        .await;
        assert_eq!(
            refused["error"]["code"],
            serde_json::json!(-32602),
            "a refused patch is INVALID_PARAMS, not a silent no-op: {refused}"
        );
        let msg = refused["error"]["message"].as_str().unwrap_or_default();
        assert!(
            msg.contains("stopLossPct"),
            "the refusal must name the field: {refused}"
        );
        assert!(
            msg.contains("not hot-reloadable"),
            "and say what is wrong with it: {refused}"
        );
        assert!(
            msg.contains("a restart applies it"),
            "and how to get it: {refused}"
        );
        assert!(
            msg.contains("maxOrderNotional"),
            "and what CAN be changed live: {refused}"
        );

        // Nothing moved — neither the refused knob nor the allowed one that came
        // with it. `sizing` is the live view of every number this call could have
        // touched.
        let after = rpc(&core, &registry, &peer, stats_line(4)).await;
        assert_eq!(
            before["result"]["sizing"], after["result"]["sizing"],
            "a refused patch must leave the live limits exactly as they were"
        );

        // ...and the order path agrees: this 2.00 ticket still passes, so the
        // `maxOrderNotional: 1` that rode along was NOT applied (the same ticket
        // IS refused once that field really is applied — see the test above).
        let probe = rpc(
            &core,
            &registry,
            &peer,
            order_line(5, "probe-after-refusal", "BTC", "up"),
        )
        .await;
        assert!(
            probe.get("error").is_none(),
            "the cap must still be the startup 3 USD after a refused patch: {probe}"
        );
        assert_eq!(
            probe["result"]["status"],
            serde_json::json!("FILLED"),
            "the probe must be a real, filled order: {probe}"
        );
    }

    // ── VERSIONING.md §7.4: the update verbs, at the wire ────────────────────

    /// Like [`rpc`], but the caller owns the update state — the verbs read AND
    /// write it, so a test has to hold the same cell the branch holds. The
    /// session's active account is the same kind of cell: the caller may hold
    /// it to assert a switch LINGERED across requests on one connection.
    async fn rpc_with_updates(
        core: &Arc<AsyncMutex<Core>>,
        registry: &crate::market::registry::MarketPluginRegistry,
        peer: &PeerAuth,
        update_state: &Arc<crate::ipc::version::UpdateState>,
        session: &mut SessionState,
        line: String,
    ) -> Value {
        let jobs = Arc::new(crate::backtest_jobs::BacktestJobs::new());
        serde_json::from_str(
            &handle_line(core, registry, line, peer, update_state, &jobs, session).await,
        )
        .expect("every reply is one JSON object")
    }

    /// The §4.4 factory readout: all nine NEW limits silent (0, "default"),
    /// the exit triple at its factory resolution (12 / 100 / 120), camelCase
    /// keys, and the version tag. This is the wire face of §4.1's
    /// factory-silence — an unconfigured kernel REPORTS that it enforces
    /// nothing new, with provenance saying where every number came from.
    #[tokio::test]
    async fn the_factory_risk_limits_readout_is_silent_and_provenanced() {
        let (core, registry, peer) = hot_reload_fixture().await;
        let reply = rpc(
            &core,
            &registry,
            &peer,
            r#"{"jsonrpc":"2.0","id":7,"method":"risk.limits","params":{}}"#.to_string(),
        )
        .await;
        let r = &reply["result"];
        assert_eq!(r["version"], "1.1");
        assert_eq!(r["account"]["id"], "default");
        for key in [
            "maxSingleLossUsd",
            "maxDailyDrawdownUsd",
            "maxPositionSize",
            "maxConsecutiveLosses",
            "cooldownMinutes",
        ] {
            assert_eq!(r["account"]["limits"][key]["value"], "0", "{key}");
            assert_eq!(r["account"]["limits"][key]["source"], "default", "{key}");
        }
        for key in [
            "maxTotalPosition",
            "maxTotalExposureUsd",
            "maxCorrelationUsd",
            "globalKillSwitchLossUsd",
        ] {
            assert_eq!(r["global"]["limits"][key]["value"], "0", "{key}");
            assert_eq!(r["global"]["limits"][key]["source"], "default", "{key}");
        }
        assert_eq!(r["exit"]["stopLossPct"].as_f64(), Some(12.0));
        assert_eq!(r["exit"]["takeProfitPct"].as_f64(), Some(100.0));
        // Calibrated default: the guillotine ships OFF (`0` disables the rule).
        assert_eq!(r["exit"]["forceExitSec"].as_i64(), Some(0));
    }

    /// A configured limit crosses the wire as a STRING with its provenance —
    /// §4.4's rule that "35" without "toml" is not an answer an operator can
    /// act on. And the untouched siblings stay factory-silent in the same
    /// readout: one configured key must not leak posture onto the others.
    #[tokio::test]
    async fn a_configured_limit_crosses_the_wire_as_a_string_with_provenance() {
        use crate::risk::limits::{AccountRiskLimits, Bound, LimitSource, SystemicRiskLimits};
        use rust_decimal_macros::dec;
        let cfg = CoreConfig {
            risk: crate::risk::RiskConfig {
                max_order_notional: dec!(3),
                systemic: SystemicRiskLimits {
                    account: AccountRiskLimits {
                        max_single_loss_usd: Bound::new(dec!(35), LimitSource::Toml),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            },
            dry_seed_balance: dec!(100),
            trade_log_path: None,
            order_log_path: None,
            position_log_path: None,
            ..Default::default()
        };
        let core = Arc::new(AsyncMutex::new(Core::new(cfg)));
        let registry = crate::market::registry::MarketPluginRegistry::new();
        let peer = PeerAuth::SameUid { uid: own_uid() };
        let reply = rpc(
            &core,
            &registry,
            &peer,
            r#"{"jsonrpc":"2.0","id":7,"method":"risk.limits","params":{}}"#.to_string(),
        )
        .await;
        let single = &reply["result"]["account"]["limits"]["maxSingleLossUsd"];
        assert_eq!(single["value"], "35");
        assert_eq!(single["source"], "toml");
        assert_eq!(
            reply["result"]["global"]["limits"]["maxTotalPosition"]["value"],
            "0"
        );
        assert_eq!(
            reply["result"]["global"]["limits"]["maxTotalPosition"]["source"],
            "default"
        );
    }

    /// A2's wire shape: with the switch off, `system.update.check` answers a
    /// clear APPLICATION error — and the state stays NotChecked, which is only
    /// possible because the fetch future is never even polled (INV-3).
    #[tokio::test]
    async fn a_disabled_update_check_is_refused_by_name() {
        let (core, registry, peer) = hot_reload_fixture().await;
        let updates = Arc::new(crate::ipc::version::UpdateState::new(false, false));
        let reply = rpc_with_updates(
            &core,
            &registry,
            &peer,
            &updates,
            &mut SessionState::default(),
            r#"{"jsonrpc":"2.0","id":1,"method":"system.update.check","params":{}}"#.to_string(),
        )
        .await;
        assert_eq!(
            reply["error"]["code"],
            serde_json::json!(Failure::APPLICATION),
            "a disabled check must be refused, not silently answered: {reply}"
        );
        assert!(
            reply["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("checkEnabled=false"),
            "the refusal must say WHY: {reply}"
        );
        assert_eq!(
            updates.snapshot().outcome,
            crate::ipc::version::CheckOutcome::NotChecked,
            "no request may have been made, not even a failed one"
        );
    }

    /// #379 (§7.5): the staging arm is refused BY NAME when autoUpdate is
    /// off — the switch guards the download whoever asks for it, and the
    /// refusal says which switch to flip (A4's shape for staging).
    #[tokio::test]
    async fn a_disabled_update_stage_is_refused_by_name() {
        let (core, registry, peer) = hot_reload_fixture().await;
        let updates = Arc::new(crate::ipc::version::UpdateState::new(false, false));
        let reply = rpc_with_updates(
            &core,
            &registry,
            &peer,
            &updates,
            &mut SessionState::default(),
            r#"{"jsonrpc":"2.0","id":1,"method":"system.update.stage","params":{}}"#.to_string(),
        )
        .await;
        assert_eq!(
            reply["error"]["code"],
            serde_json::json!(Failure::APPLICATION),
            "a disabled stage must be refused, not silently answered: {reply}"
        );
        let msg = reply["error"]["message"].as_str().unwrap_or_default();
        assert!(
            msg.contains("autoUpdate"),
            "the refusal must name the switch: {reply}"
        );
        assert_eq!(
            updates.stage_snapshot().phase,
            crate::ipc::version::StagePhase::Idle,
            "no download may have been started"
        );
    }

    /// #379: the staging reply IS the staging state machine (§7.5 wire): the
    /// phase travels on the reply, and with nothing `Available` a permitted
    /// call is a no-op that stays `idle` — not an error.
    #[tokio::test]
    async fn a_permitted_stage_with_no_available_verdict_answers_idle() {
        let (core, registry, peer) = hot_reload_fixture().await;
        let updates = Arc::new(crate::ipc::version::UpdateState::new(false, true));
        let reply = rpc_with_updates(
            &core,
            &registry,
            &peer,
            &updates,
            &mut SessionState::default(),
            r#"{"jsonrpc":"2.0","id":1,"method":"system.update.stage","params":{}}"#.to_string(),
        )
        .await;
        let result = reply
            .get("result")
            .expect("a permitted no-op stage answers a result, not an error");
        assert_eq!(
            result["phase"], "idle",
            "no Available verdict → nothing to stage: {result}"
        );
    }

    /// The configure verb: the answer carries the new switch values, the state
    /// cell holds them, and they LAND ON DISK (`data/update/state.json`) —
    /// which is the whole point: a switch that silently reverts on restart is
    /// a switch the operator believed was on. The audit line lands beside it.
    #[tokio::test]
    async fn update_configure_answers_and_persists() {
        let (core, registry, peer) = hot_reload_fixture().await;
        let updates = Arc::new(crate::ipc::version::UpdateState::new(false, false));
        let reply = rpc_with_updates(
            &core,
            &registry,
            &peer,
            &updates,
            &mut SessionState::default(),
            r#"{"jsonrpc":"2.0","id":1,"method":"system.update.configure","params":{"checkEnabled":true,"autoUpdate":false}}"#.to_string(),
        )
        .await;
        assert!(
            reply.get("error").is_none(),
            "a valid configure must succeed: {reply}"
        );
        assert_eq!(reply["result"]["checkEnabled"], serde_json::json!(true));
        assert_eq!(reply["result"]["autoUpdate"], serde_json::json!(false));
        assert!(updates.snapshot().check_enabled, "the cell must hold it");

        // The state.json the next startup will read back.
        let doc: Value = serde_json::from_str(
            &std::fs::read_to_string("data/update/state.json")
                .expect("configure must persist the switch state"),
        )
        .expect("persisted state is JSON");
        assert_eq!(doc["checkEnabled"], serde_json::json!(true));
        assert_eq!(doc["autoUpdate"], serde_json::json!(false));

        // The audit trail names the event (the actor test is uid-shape only —
        // the fixture's peer is the test process's own uid).
        let audit = std::fs::read_to_string("data/update/audit.jsonl")
            .expect("configure must append an audit line");
        assert!(audit.contains("\"event\":\"update.configure\""), "{audit}");

        // Cleanup: these two files belong to this test alone.
        let _ = std::fs::remove_dir_all("data/update");
    }

    // ── v0.3 Wave 0 (#329): the two read-only envelopes, at the wire ────────
    //
    // Both arms are ADDITIVE (two new methods; nothing existing moves), so what
    // these tests pin is the envelope a consumer parses: the echoed routing
    // fields and the empty list — never `null`, never METHOD_NOT_FOUND. E25 and
    // E29 fill the lists later; these shapes must survive that swap.

    /// `kline.history` keeps the frozen envelope (symbol/interval echoed
    /// verbatim, `klines` oldest first). E29 swaps the EMPTY list for real
    /// bars: with no engine data the empty list is still the truth; the
    /// bar-producing path is driven with CONTROLLED timestamps through the
    /// same `engine_on_data` entry the feed arm drives (the arm stamps the
    /// host clock, so a wall-clock wire feed cannot pin bucketing here).
    #[tokio::test]
    async fn kline_history_answers_the_frozen_envelope() {
        let (core, registry, peer) = hot_reload_fixture().await;
        // Fresh core, no data: the empty envelope — never `null`, never 404.
        let reply = rpc(
            &core,
            &registry,
            &peer,
            r#"{"jsonrpc":"2.0","id":1,"method":"kline.history","params":{"symbol":"BTC-UP","interval":"min1"}}"#
                .to_string(),
        )
        .await;
        assert!(
            reply.get("error").is_none(),
            "a Wave-0 envelope must answer, not 404: {reply}"
        );
        assert_eq!(reply["result"]["symbol"], serde_json::json!("BTC-UP"));
        assert_eq!(reply["result"]["interval"], serde_json::json!("min1"));
        assert_eq!(reply["result"]["klines"], serde_json::json!([]));

        // Two mids one minute apart: the first min1 bar closes. Driven
        // through `engine_on_data` — the same entry `books.snapshot` lands
        // in — with venue timestamps under the test's control.
        use rust_decimal_macros::dec;
        for (ts_ms, bid, ask) in [
            (1_758_888_010_000, dec!(0.40), dec!(0.41)),
            (1_758_888_070_000, dec!(0.50), dec!(0.51)),
        ] {
            core.lock().await.engine_on_data(
                crate::engine::DataEvent::Book {
                    token_id: "BTC-UP".to_string(),
                    bids: vec![(bid, dec!(100))],
                    asks: vec![(ask, dec!(100))],
                    now_ms: ts_ms,
                },
                ts_ms,
            );
        }
        let reply = rpc(
            &core,
            &registry,
            &peer,
            r#"{"jsonrpc":"2.0","id":3,"method":"kline.history","params":{"symbol":"BTC-UP","interval":"min1"}}"#
                .to_string(),
        )
        .await;
        let bars = &reply["result"]["klines"];
        assert_eq!(bars.as_array().map(Vec::len), Some(2), "{reply}");
        assert_eq!(bars[0]["isClosed"], serde_json::json!(true));
        assert_eq!(bars[0]["open"], serde_json::json!(0.405));
        assert_eq!(bars[0]["close"], serde_json::json!(0.405));
        assert_eq!(bars[0]["tradeCount"], serde_json::json!(1));
        assert!(!bars[1]["isClosed"].as_bool().unwrap_or(true));
        assert_eq!(bars[1]["open"], serde_json::json!(0.505));
        // `limit` clamps the window.
        let reply = rpc(
            &core,
            &registry,
            &peer,
            r#"{"jsonrpc":"2.0","id":4,"method":"kline.history","params":{"symbol":"BTC-UP","interval":"min1","limit":1}}"#
                .to_string(),
        )
        .await;
        assert_eq!(reply["result"]["klines"].as_array().map(Vec::len), Some(1));
    }

    /// §12.2: the subscription pair writes THE SESSION's set — all three
    /// calls below share one set (as one connection's arms do), the reply
    /// counts the set size AFTER the change, dedup holds (resubscribing an
    /// existing pair does not grow it), and an unsubscribe carves the set
    /// down. The auto-unsubscribe on disconnect is `spawn_session`'s drop of
    /// this very Arc — structural, nothing to assert here beyond the set
    /// arithmetic the reply reports.
    #[tokio::test]
    async fn kline_subscribe_and_unsubscribe_shape_the_session_set() {
        let (core, registry, peer) = hot_reload_fixture().await;
        let mut session = SessionState::default();
        let reply = rpc_with_subs(
            &core,
            &registry,
            &peer,
            &mut session,
            r#"{"jsonrpc":"2.0","id":1,"method":"kline.subscribe","params":{"symbols":["BTC-UP","BTC-DOWN"],"intervals":["min1","min5"]}}"#
                .to_string(),
        )
        .await;
        assert!(reply.get("error").is_none(), "{reply}");
        assert_eq!(reply["result"]["subscribed"], serde_json::json!(4));

        // Dedup: the same four pairs again — the count does not grow.
        let reply = rpc_with_subs(
            &core,
            &registry,
            &peer,
            &mut session,
            r#"{"jsonrpc":"2.0","id":2,"method":"kline.subscribe","params":{"symbols":["BTC-UP"],"intervals":["min1"]}}"#
                .to_string(),
        )
        .await;
        assert_eq!(reply["result"]["subscribed"], serde_json::json!(4));

        // Unsubscribe two pairs.
        let reply = rpc_with_subs(
            &core,
            &registry,
            &peer,
            &mut session,
            r#"{"jsonrpc":"2.0","id":3,"method":"kline.unsubscribe","params":{"symbols":["BTC-UP","BTC-DOWN"],"intervals":["min5"]}}"#
                .to_string(),
        )
        .await;
        assert_eq!(reply["result"]["subscribed"], serde_json::json!(2));

        // The caller-owned set holds exactly what the replies said.
        assert_eq!(session.kline_subs.lock().expect("subs").len(), 2);
        assert!(
            session
                .kline_subs
                .lock()
                .expect("subs")
                .contains(&("BTC-UP".into(), crate::kline::KlineInterval::Min1))
        );
        assert!(
            session
                .kline_subs
                .lock()
                .expect("subs")
                .contains(&("BTC-DOWN".into(), crate::kline::KlineInterval::Min1))
        );
    }

    /// `intent.audit.tail`: a missing log is the documented empty envelope;
    /// with rows present it answers the NEWEST window in file order, and
    /// `accountId` / `decision` filter BEFORE the window is taken (`decision`
    /// case-insensitively against `decision.status`, the §3.4 wire shape).
    #[tokio::test]
    async fn intent_audit_tail_reads_the_tail_and_filters() {
        let dir = std::path::Path::new("data/audit");
        // Remove only THIS test's log: the directory is shared with the
        // execution-policy audit, which the concurrent policy IPC test
        // reads — a directory nuke here raced its history read.
        let _ = std::fs::remove_file(dir.join("intents.jsonl"));
        let (core, registry, peer) = hot_reload_fixture().await;

        // Fresh data dir: the empty envelope, not an error.
        let reply = rpc(
            &core,
            &registry,
            &peer,
            r#"{"jsonrpc":"2.0","id":1,"method":"intent.audit.tail","params":{}}"#.to_string(),
        )
        .await;
        assert!(reply.get("error").is_none(), "{reply}");
        assert_eq!(reply["result"]["records"], serde_json::json!([]));
        assert_eq!(reply["result"]["total"], serde_json::json!(0));

        // Three rows shaped like §3.4's IntentAuditRecord (camelCase; the
        // decision is the status-tagged enum).
        std::fs::create_dir_all(dir).expect("create data/audit");
        let rows = [
            r#"{"tsMs":1,"accountId":"default","strategy":"s","intentId":"i1","decision":{"status":"APPROVED"},"gates":[],"latencyUs":1}"#,
            r#"{"tsMs":2,"accountId":"paper","strategy":"s","intentId":"i2","decision":{"status":"REJECTED"},"gates":[],"latencyUs":1}"#,
            r#"{"tsMs":3,"accountId":"default","strategy":"s","intentId":"i3","decision":{"status":"MODIFIED"},"gates":[],"latencyUs":1}"#,
        ];
        std::fs::write(dir.join("intents.jsonl"), rows.join("\n")).expect("write rows");

        // No filter: all three, oldest → newest (file order preserved).
        let reply = rpc(
            &core,
            &registry,
            &peer,
            r#"{"jsonrpc":"2.0","id":1,"method":"intent.audit.tail","params":{}}"#.to_string(),
        )
        .await;
        assert_eq!(reply["result"]["total"], serde_json::json!(3));
        assert_eq!(
            reply["result"]["records"][0]["intentId"],
            serde_json::json!("i1")
        );
        assert_eq!(
            reply["result"]["records"][2]["intentId"],
            serde_json::json!("i3")
        );

        // `limit` takes the NEWEST rows, still oldest → newest inside the
        // window; `total` keeps reporting the full filtered count.
        let reply = rpc(
            &core,
            &registry,
            &peer,
            r#"{"jsonrpc":"2.0","id":1,"method":"intent.audit.tail","params":{"limit":1}}"#
                .to_string(),
        )
        .await;
        assert_eq!(reply["result"]["records"].as_array().map(Vec::len), Some(1));
        assert_eq!(
            reply["result"]["records"][0]["intentId"],
            serde_json::json!("i3")
        );
        assert_eq!(reply["result"]["total"], serde_json::json!(3));

        // `decision` matches case-insensitively against `decision.status` …
        let reply = rpc(
            &core,
            &registry,
            &peer,
            r#"{"jsonrpc":"2.0","id":1,"method":"intent.audit.tail","params":{"decision":"rejected"}}"#
                .to_string(),
        )
        .await;
        assert_eq!(reply["result"]["total"], serde_json::json!(1));
        assert_eq!(
            reply["result"]["records"][0]["intentId"],
            serde_json::json!("i2")
        );

        // … and `accountId` is an exact match.
        let reply = rpc(
            &core,
            &registry,
            &peer,
            r#"{"jsonrpc":"2.0","id":1,"method":"intent.audit.tail","params":{"accountId":"default"}}"#
                .to_string(),
        )
        .await;
        assert_eq!(reply["result"]["total"], serde_json::json!(2));

        let _ = std::fs::remove_file(dir.join("intents.jsonl"));
    }

    // ── E28 (§12.1): the account verbs, at the wire ─────────────────────────

    /// A two-account core (`default` + `paper`) through the INJECTION
    /// constructor: no test touches the real `user_layer/configs/accounts.toml`
    /// the process cwd decides, and no durable log is left behind.
    async fn two_account_fixture() -> (
        Arc<AsyncMutex<Core>>,
        crate::market::registry::MarketPluginRegistry,
        PeerAuth,
    ) {
        let now = now_ms();
        let book = crate::account::AccountLedgers::new(
            vec![
                crate::account::Account {
                    id: AccountId::from("default"),
                    name: "default".into(),
                    market_type: blitzkrieg_market_api::MarketType::Prediction,
                    status: AccountStatus::Active,
                    credential_keys: crate::account::CredentialKeys::default(),
                    updated_at_ms: now,
                },
                crate::account::Account {
                    id: AccountId::from("paper"),
                    name: "paper".into(),
                    market_type: blitzkrieg_market_api::MarketType::Prediction,
                    status: AccountStatus::Active,
                    credential_keys: crate::account::CredentialKeys::default(),
                    updated_at_ms: now,
                },
            ],
            AccountId::from("default"),
        );
        let cfg = CoreConfig {
            trade_log_path: None,
            order_log_path: None,
            position_log_path: None,
            ..Default::default()
        };
        let c = Core::with_account_book(cfg, book);
        (
            Arc::new(AsyncMutex::new(c)),
            crate::market::registry::MarketPluginRegistry::new(),
            PeerAuth::SameUid { uid: own_uid() },
        )
    }

    /// The §12.1 wire shape: `version` + the SESSION's active + one view per
    /// configured account, each carrying its OWN book (§9.3) — and no
    /// credential value anywhere on the wire (§9.4: `credentialsLoaded` is a
    /// boolean, never a value).
    #[tokio::test]
    async fn account_list_reports_the_session_default_and_every_book() {
        let (core, registry, peer) = two_account_fixture().await;
        let reply = rpc(
            &core,
            &registry,
            &peer,
            r#"{"jsonrpc":"2.0","id":1,"method":"account.list","params":{}}"#.to_string(),
        )
        .await;
        assert!(reply.get("error").is_none(), "list must answer: {reply}");
        assert_eq!(reply["result"]["version"], serde_json::json!("1.1"));
        // No session switch yet: the process-level active is what's reported.
        assert_eq!(reply["result"]["active"], serde_json::json!("default"));
        let accounts = reply["result"]["accounts"].as_array().expect("views");
        assert_eq!(accounts.len(), 2, "one view per configured account");
        assert_eq!(accounts[0]["id"], serde_json::json!("default"));
        assert_eq!(accounts[1]["id"], serde_json::json!("paper"));
        assert_eq!(accounts[0]["status"], serde_json::json!("active"));
        // Money fields are the account's OWN book: the seeded default carries
        // the dry seed; paper starts flat.
        assert!(accounts[0]["balance"].is_number());
        assert!(accounts[1]["balance"].is_number());
        assert_eq!(
            accounts[0]["credentialsLoaded"],
            serde_json::json!(false),
            "§9.4: a presence fact only"
        );
        let raw = reply.to_string();
        assert!(
            !raw.contains("apiKeyValue") && !raw.contains("secretValue"),
            "no credential VALUE may ride the wire: {raw}"
        );
    }

    /// §9.5: the session's default account drives the orders an un-versioned
    /// client places — an omitted accountId rides the session cell (what this
    /// connection switched to), an explicit one always stands.
    #[tokio::test]
    async fn an_omitted_account_id_rides_the_session_default_and_an_explicit_one_stands() {
        let (core, registry, peer) = two_account_fixture().await;
        let updates = Arc::new(crate::ipc::version::UpdateState::new(false, false));
        let mut session = SessionState::default();

        // Switch the session to `paper` first.
        let reply = rpc_with_updates(
            &core,
            &registry,
            &peer,
            &updates,
            &mut session,
            r#"{"jsonrpc":"2.0","id":1,"method":"account.switch","params":{"accountId":"paper"}}"#
                .to_string(),
        )
        .await;
        assert!(reply.get("error").is_none(), "switch must land: {reply}");

        // A book for `tok`, so the taker crosses.
        let feed = |id: u32| {
            format!(
                r#"{{"jsonrpc":"2.0","id":{id},"method":"books.snapshot","params":{{"tokenId":"tok","bids":[{{"price":0.40,"size":100}}],"asks":[{{"price":0.41,"size":100}}]}}}}"#
            )
        };
        let slot = now_ms() / 1000 / 900;
        let buy_omitted = format!(
            r#"{{"jsonrpc":"2.0","id":2,"method":"orders.place","params":{{"tokenId":"tok","conditionId":"cond","side":"buy","mode":"taker","price":0.41,"size":2,"internalKey":"e-omitted","strategy":"s","asset":"BTC","direction":"up","roundSlot":{slot}}}}}"#
        );
        let reply =
            rpc_with_updates(&core, &registry, &peer, &updates, &mut session, feed(1)).await;
        assert!(reply.get("error").is_none(), "book feed must land: {reply}");
        let reply =
            rpc_with_updates(&core, &registry, &peer, &updates, &mut session, buy_omitted).await;
        assert!(
            reply.get("error").is_none(),
            "an orderless place must land: {reply}"
        );

        // The position (and the reservation) belong to `paper` — the session's
        // default, not the process-level `default` account.
        let c = core.lock().await;
        let paper = c.accounts().ledger_of(&AccountId::from("paper")).unwrap();
        assert!(
            paper.reserved() > rust_decimal::Decimal::ZERO || {
                let views = c.position_views(now_ms());
                !views.is_empty() && views.iter().all(|v| v.account_id == "paper")
            },
            "the omitted-account order must spend the SESSION's book"
        );
        let def = c
            .accounts()
            .ledger_of(&AccountId::from("default"))
            .unwrap()
            .balance();
        assert_eq!(
            def,
            rust_decimal::Decimal::from(10_000),
            "the process-level book is untouched"
        );
        drop(c);

        // An EXPLICIT `accountId: "default"` NAMES its account: it spends the
        // process-level book even though the session points at paper. A second
        // token, because the position book is one-position-per-token and the
        // first order already holds `tok`.
        let slot = now_ms() / 1000 / 900;
        let feed2 = r#"{"jsonrpc":"2.0","id":3,"method":"books.snapshot","params":{"tokenId":"tok2","bids":[{"price":0.40,"size":100}],"asks":[{"price":0.41,"size":100}]}}"#.to_string();
        let reply = rpc_with_updates(&core, &registry, &peer, &updates, &mut session, feed2).await;
        assert!(reply.get("error").is_none(), "book 2 must land: {reply}");
        let buy_explicit = format!(
            r#"{{"jsonrpc":"2.0","id":4,"method":"orders.place","params":{{"tokenId":"tok2","conditionId":"cond2","side":"buy","mode":"taker","price":0.41,"size":2,"internalKey":"e-explicit","strategy":"s","asset":"ETH","direction":"up","roundSlot":{slot},"accountId":"default"}}}}"#
        );
        let reply = rpc_with_updates(
            &core,
            &registry,
            &peer,
            &updates,
            &mut session,
            buy_explicit,
        )
        .await;
        assert!(
            reply.get("error").is_none(),
            "an explicit place must land: {reply}"
        );
        let c = core.lock().await;
        let views = c.position_views(now_ms());
        assert!(
            views.iter().any(|v| v.account_id == "default"),
            "the explicit accountId must stand: {views:?}"
        );
    }

    /// `account.switch` is SESSION-level (§9.5): the switched connection sees
    /// the new default on its NEXT request, a second connection still sees the
    /// process-level one, and Core's process active never moves.
    #[tokio::test]
    async fn a_switch_is_session_scoped_and_survives_across_requests() {
        let (core, registry, peer) = two_account_fixture().await;
        let updates = Arc::new(crate::ipc::version::UpdateState::new(false, false));
        let mut session_a = SessionState::default();
        let mut session_b = SessionState::default();

        let reply = rpc_with_updates(
            &core,
            &registry,
            &peer,
            &updates,
            &mut session_a,
            r#"{"jsonrpc":"2.0","id":1,"method":"account.switch","params":{"accountId":"paper"}}"#
                .to_string(),
        )
        .await;
        assert!(reply.get("error").is_none(), "switch must answer: {reply}");
        assert_eq!(reply["result"]["active"], serde_json::json!("paper"));

        // Session A now defaults to paper …
        let reply = rpc_with_updates(
            &core,
            &registry,
            &peer,
            &updates,
            &mut session_a,
            r#"{"jsonrpc":"2.0","id":2,"method":"account.list","params":{}}"#.to_string(),
        )
        .await;
        assert_eq!(
            reply["result"]["active"],
            serde_json::json!("paper"),
            "the session cell lingers across requests"
        );

        // … while session B still sees the process-level one.
        let reply = rpc_with_updates(
            &core,
            &registry,
            &peer,
            &updates,
            &mut session_b,
            r#"{"jsonrpc":"2.0","id":2,"method":"account.list","params":{}}"#.to_string(),
        )
        .await;
        assert_eq!(
            reply["result"]["active"],
            serde_json::json!("default"),
            "another connection is NOT switched"
        );

        // And the process-level active itself never moved.
        let c = core.lock().await;
        assert_eq!(c.accounts().active_id().as_str(), "default");
    }

    /// A switch to an account the deployment never configured is refused and
    /// leaves the session cell exactly where it was (§9.3: no implicit
    /// account, ever).
    #[tokio::test]
    async fn a_switch_to_an_unknown_account_is_refused_without_touching_the_session() {
        let (core, registry, peer) = two_account_fixture().await;
        let updates = Arc::new(crate::ipc::version::UpdateState::new(false, false));
        let mut session = SessionState::default();
        let reply = rpc_with_updates(
            &core,
            &registry,
            &peer,
            &updates,
            &mut session,
            r#"{"jsonrpc":"2.0","id":1,"method":"account.switch","params":{"accountId":"ghost"}}"#
                .to_string(),
        )
        .await;
        assert_eq!(
            reply["error"]["code"],
            serde_json::json!(Failure::INVALID_PARAMS),
            "an unknown account must fail loud: {reply}"
        );
        assert!(
            reply["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("unknown account"),
            "the refusal must say WHY: {reply}"
        );
        // The session still resolves to the process default.
        let reply = rpc_with_updates(
            &core,
            &registry,
            &peer,
            &updates,
            &mut session,
            r#"{"jsonrpc":"2.0","id":2,"method":"account.list","params":{}}"#.to_string(),
        )
        .await;
        assert_eq!(reply["result"]["active"], serde_json::json!("default"));
    }

    /// §12.1 / §14 acceptance: a tighten lands (`active -> frozen`, then
    /// `frozen -> suspended` with the flat reason), and the reverse is
    /// refused with INVALID_PARAMS — an unwind one IPC call could undo is
    /// not an unwind. Unfreezing is config + restart.
    #[tokio::test]
    async fn a_status_tighten_lands_and_a_loosening_is_refused() {
        let (core, registry, peer) = two_account_fixture().await;

        // active -> frozen: allowed, and the view carries no reason.
        let reply = rpc(
            &core,
            &registry,
            &peer,
            r#"{"jsonrpc":"2.0","id":1,"method":"account.status","params":{"accountId":"paper","status":"frozen"}}"#
                .to_string(),
        )
        .await;
        assert!(reply.get("error").is_none(), "a tighten must land: {reply}");
        assert_eq!(reply["result"]["id"], serde_json::json!("paper"));
        assert_eq!(reply["result"]["status"], serde_json::json!("frozen"));
        assert!(
            reply["result"].get("statusReason").is_none(),
            "a freeze without a reason reports none: {reply}"
        );

        // frozen -> suspended WITH a reason: still a tighten, the flat reason
        // is carried onto the view.
        let reply = rpc(
            &core,
            &registry,
            &peer,
            r#"{"jsonrpc":"2.0","id":2,"method":"account.status","params":{"accountId":"paper","status":"suspended","reason":"ops review"}}"#
                .to_string(),
        )
        .await;
        assert!(reply.get("error").is_none(), "a tighten must land: {reply}");
        assert_eq!(reply["result"]["status"], serde_json::json!("suspended"));
        assert_eq!(
            reply["result"]["statusReason"],
            serde_json::json!("ops review")
        );

        // The posture is visible in the list view, as a plain string.
        let reply = rpc(
            &core,
            &registry,
            &peer,
            r#"{"jsonrpc":"2.0","id":3,"method":"account.list","params":{}}"#.to_string(),
        )
        .await;
        let accounts = reply["result"]["accounts"].as_array().expect("views");
        assert_eq!(accounts[1]["status"], serde_json::json!("suspended"));
        assert_eq!(accounts[1]["statusReason"], serde_json::json!("ops review"));

        // And the loosening is refused at the wire with INVALID_PARAMS.
        let reply = rpc(
            &core,
            &registry,
            &peer,
            r#"{"jsonrpc":"2.0","id":4,"method":"account.status","params":{"accountId":"paper","status":"active"}}"#
                .to_string(),
        )
        .await;
        assert_eq!(
            reply["error"]["code"],
            serde_json::json!(Failure::INVALID_PARAMS),
            "suspended -> active must be refused with INVALID_PARAMS: {reply}"
        );
        assert!(
            reply["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("LOOSEN"),
            "the refusal must say WHY: {reply}"
        );
    }

    // ── #363: the execution policy surface, at the wire ─────────────────────

    /// A core pointed at a policy file in a scratch dir. The file ships the
    /// global section only; the arms below add/reset account sections
    /// through the wire and read back what landed. `accounts` names the ids
    /// the book is configured with (through the INJECTION constructor, so
    /// no test touches the real `user_layer/configs/accounts.toml` the
    /// process cwd decides); an empty list means the single-`default` book
    /// a missing accounts.toml produces.
    async fn policy_fixture_with_accounts(
        accounts: &[&str],
    ) -> (
        Arc<AsyncMutex<Core>>,
        crate::market::registry::MarketPluginRegistry,
        PeerAuth,
        std::path::PathBuf,
    ) {
        use rust_decimal_macros::dec;
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "bk-ipc-policy-{}-{}-{}",
            std::process::id(),
            now_ms(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("scratch root");
        let path = dir.join("execution_policy.toml");
        std::fs::write(
            &path,
            "version = 1\n\n[defaults]\nbudget_ratio = \"0.10\"\n",
        )
        .expect("write policy");
        // #386: the audit journal is PRIVATE to this fixture, inside the same
        // scratch root the cleanup at the tests' end removes. The old fixed
        // `data/audit/…` spelling made every test core append to ONE file —
        // two parallel sets interleaved text-then-newline syscalls, fused
        // into a single unparseable line, and two history counts lost their
        // records at once (the flake PR #378 disclosed; run 37262356151).
        let audit_path = dir.join("data/audit/execution_policy.jsonl");
        let cfg = CoreConfig {
            execution_policy_path: Some(path.to_string_lossy().to_string()),
            execution_policy_audit_path: Some(audit_path.to_string_lossy().to_string()),
            dry_seed_balance: dec!(100),
            trade_log_path: None,
            order_log_path: None,
            position_log_path: None,
            ..Default::default()
        };
        let c = if accounts.is_empty() {
            Core::new(cfg)
        } else {
            let now = now_ms();
            let book = crate::account::AccountLedgers::new(
                accounts
                    .iter()
                    .map(|id| crate::account::Account {
                        id: AccountId::from(*id),
                        name: (*id).to_string(),
                        market_type: blitzkrieg_market_api::MarketType::Prediction,
                        status: AccountStatus::Active,
                        credential_keys: crate::account::CredentialKeys::default(),
                        updated_at_ms: now,
                    })
                    .collect(),
                AccountId::from(accounts[0]),
            );
            Core::with_account_book(cfg, book)
        };
        (
            Arc::new(AsyncMutex::new(c)),
            crate::market::registry::MarketPluginRegistry::new(),
            PeerAuth::SameUid { uid: own_uid() },
            path,
        )
    }

    async fn policy_fixture() -> (
        Arc<AsyncMutex<Core>>,
        crate::market::registry::MarketPluginRegistry,
        PeerAuth,
        std::path::PathBuf,
    ) {
        policy_fixture_with_accounts(&[]).await
    }

    /// #386 — the fixture contract that killed the flake: two cores NEVER
    /// share an audit journal. The CI failure (run 37262356151) had two
    /// parallel tests appending to the ONE fixed `data/audit/…` file; their
    /// records fused into a single unparseable line and BOTH history counts
    /// came back empty (`left: 0, right: 1`). Each fixture now carries its
    /// own journal inside its own scratch root — this test pins that a set
    /// through one core lands in that core's journal alone, and the other
    /// core's history stays empty.
    #[tokio::test]
    async fn policy_journals_are_private_per_fixture() {
        let (a, reg_a, peer_a, path_a) = policy_fixture().await;
        let (b, reg_b, peer_b, path_b) = policy_fixture().await;
        assert_ne!(path_a, path_b, "each fixture gets its own scratch root");

        let v = rpc(&a, &reg_a, &peer_a,
            r#"{"jsonrpc":"2.0","id":1,"method":"execution_policy.set","params":{"accountId":"acct-x","budgetRatio":"0.05","maxBudgetUsd":"20"}}"#.into())
            .await;
        assert!(v.get("error").is_none(), "set on core A answers: {v}");
        let v = rpc(&b, &reg_b, &peer_b,
            r#"{"jsonrpc":"2.0","id":1,"method":"execution_policy.set","params":{"accountId":"acct-y","budgetRatio":"0.06","maxBudgetUsd":"25"}}"#.into())
            .await;
        assert!(v.get("error").is_none(), "set on core B answers: {v}");

        // Each journal carries exactly its own core's record — no bleed.
        let ja = a.lock().await.execution_policy_audit();
        assert_eq!(ja.len(), 1, "core A's journal: {ja:?}");
        assert_eq!(ja[0].account_id, "acct-x");
        let jb = b.lock().await.execution_policy_audit();
        assert_eq!(jb.len(), 1, "core B's journal: {jb:?}");
        assert_eq!(jb[0].account_id, "acct-y");

        // And the journals are physically inside each fixture's scratch root
        // (the cleanup at the tests' end removes them with the policy file).
        for path in [&path_a, &path_b] {
            let journal = path
                .parent()
                .expect("scratch root")
                .join("data/audit/execution_policy.jsonl");
            assert!(journal.is_file(), "private journal exists: {journal:?}");
        }
        let _ = std::fs::remove_dir_all(path_a.parent().unwrap());
        let _ = std::fs::remove_dir_all(path_b.parent().unwrap());
    }

    // ── #362: the blueprint editor surface, at the wire ──────────────────────
    //
    // The compile arm is pure (stateless, no Core lock), the save arm is the
    // one place this surface touches the filesystem, so every test here pins
    // the write shape from a temp ROOT via the same entry the sessions use.

    /// The spec's four-node example — the same document the blueprint module's
    /// own DOG_BLUEPRINT test pins, so the wire arm and the compiler agree on
    /// the happy path.
    const WIRE_BLUEPRINT: &str = r#"{"version":1,"name":"wire_dog","nodes":[
        {"id":"n1","type":"data_source","params":{"field":"tick.price"}},
        {"id":"n2","type":"condition","params":{"op":"<=","value":0.25}},
        {"id":"n3","type":"condition","params":{"field":"tick.trend_confirmed","op":"==","value":true}},
        {"id":"n4","type":"action_buy","params":{"price":0.25,"budget_ratio":0.10}}],
        "edges":[{"from":"n1","to":"n2","when":true},{"from":"n2","to":"n3","when":true},
        {"from":"n3","to":"n4","when":true}]}"#;

    #[tokio::test]
    async fn blueprint_compile_answers_the_generated_lua() {
        let (core, registry, peer) = hot_reload_fixture().await;
        let params = serde_json::json!({ "json": WIRE_BLUEPRINT }).to_string();
        let line =
            format!(r#"{{"jsonrpc":"2.0","id":1,"method":"blueprint.compile","params":{params}}}"#);
        let reply = rpc(&core, &registry, &peer, line).await;
        assert!(reply.get("error").is_none(), "compile must answer: {reply}");
        let lua = reply["result"]["lua"].as_str().expect("lua is a string");
        assert!(lua.contains("function on_tick(tick)"), "{lua}");
        assert!(
            lua.contains("tick.price <= __param(\"n2_value\", 0.25)"),
            "{lua}"
        );
        assert!(lua.contains("side = \"buy\""), "{lua}");
    }

    /// Reverse acceptance: every refusal class the editor must render INLINE
    /// reaches the wire with the node id (or the parse position) and nothing
    /// else changed — no partial result, no silent success.
    #[tokio::test]
    async fn blueprint_compile_refusals_carry_the_editor_context() {
        let (core, registry, peer) = hot_reload_fixture().await;

        // Cycle: names the node on it.
        let cyclic = json_line(
            1,
            "blueprint.compile",
            &serde_json::json!({
                "json": r#"{"version":1,"name":"cyc","nodes":[
                {"id":"a1","type":"data_source","params":{"field":"tick.price"}},
                {"id":"a2","type":"condition","params":{"op":"<","value":1}},
                {"id":"a3","type":"condition","params":{"op":">","value":0}},
                {"id":"a4","type":"action_buy","params":{}}],
                "edges":[{"from":"a1","to":"a2"},{"from":"a2","to":"a3"},
                {"from":"a3","to":"a2"},{"from":"a3","to":"a4"}]}"#,
            }),
        );
        let reply = rpc(&core, &registry, &peer, cyclic).await;
        let msg = reply["error"]["message"].as_str().unwrap_or_default();
        assert!(msg.contains("cycle") && msg.contains("a2"), "{reply}");

        // No reachable action: the whole-graph failure with no node id.
        let actionless = json_line(
            2,
            "blueprint.compile",
            &serde_json::json!({
                "json": r#"{"version":1,"name":"noact","nodes":[
                {"id":"a1","type":"data_source","params":{"field":"tick.price"}},
                {"id":"a2","type":"condition","params":{"op":"<","value":1}}],
                "edges":[{"from":"a1","to":"a2"}]}"#,
            }),
        );
        let reply = rpc(&core, &registry, &peer, actionless).await;
        let msg = reply["error"]["message"].as_str().unwrap_or_default();
        assert!(msg.contains("no reachable action output"), "{reply}");

        // Non-whitelisted field: names the node AND the field.
        let bad_field = json_line(
            3,
            "blueprint.compile",
            &serde_json::json!({
                "json": r#"{"version":1,"name":"badf","nodes":[
                {"id":"a1","type":"data_source","params":{"field":"tick.close"}},
                {"id":"a2","type":"condition","params":{"op":"<","value":1}},
                {"id":"a3","type":"action_buy","params":{}}],
                "edges":[{"from":"a1","to":"a2"},{"from":"a2","to":"a3"}]}"#,
            }),
        );
        let reply = rpc(&core, &registry, &peer, bad_field).await;
        let msg = reply["error"]["message"].as_str().unwrap_or_default();
        assert!(msg.contains("a1") && msg.contains("tick.close"), "{reply}");

        // Malformed JSON: the parse error travels verbatim.
        let not_json = json_line(
            4,
            "blueprint.compile",
            &serde_json::json!({ "json": "{not json" }),
        );
        let reply = rpc(&core, &registry, &peer, not_json).await;
        let msg = reply["error"]["message"].as_str().unwrap_or_default();
        assert!(msg.contains("blueprint parse failed"), "{reply}");
    }

    /// Build one JSON-RPC line from a params object (the save tests' helper).
    fn json_line(id: u32, method: &str, params: &serde_json::Value) -> String {
        format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{params}}}"#)
    }

    /// The save tests' core: the standard fixture with its `lua_strategy_dir`
    /// pointed at a private temp root, so a save lands where the assertions
    /// look and never in the checkout. The save arm reads the root from this
    /// config snapshot — the same road `serve` takes, no process-wide knob.
    async fn save_fixture(
        root: &std::path::Path,
    ) -> (
        Arc<AsyncMutex<Core>>,
        crate::market::registry::MarketPluginRegistry,
        PeerAuth,
    ) {
        use rust_decimal_macros::dec;
        let cfg = CoreConfig {
            risk: crate::risk::RiskConfig {
                max_order_notional: dec!(3),
                ..Default::default()
            },
            dry_seed_balance: dec!(100),
            lua_strategy_dir: Some(root.display().to_string()),
            trade_log_path: None,
            order_log_path: None,
            position_log_path: None,
            ..Default::default()
        };
        let mut c = Core::new(cfg.clone());
        c.set_balance(cfg.dry_seed_balance);
        cfg.install_engine(&mut c).expect("engine install");
        (
            Arc::new(AsyncMutex::new(c)),
            crate::market::registry::MarketPluginRegistry::new(),
            PeerAuth::SameUid { uid: own_uid() },
        )
    }

    #[tokio::test]
    async fn execution_policy_surface_list_get_set_reset_history() {
        let (core, registry, peer, path) = policy_fixture().await;

        // The journal is this fixture's PRIVATE file (#386) inside a scratch
        // dir, so parallel test cores cannot interleave appends into it. Its
        // stamps are wall-clock, though — a backward step between two now_ms()
        // calls would make "records stamped after the test started" miss this
        // run's own set. So freshness is proven by COUNT, not by clock: read
        // the account's journal length first, then assert it grew by exactly
        // one — the set that answered, no audit for the refused one.
        let before_set = {
            let c = core.lock().await;
            c.execution_policy_audit()
                .iter()
                .filter(|r| r.account_id == "acct-a")
                .count()
        };

        // list: the shipped global section only, no accounts.
        let v = rpc(
            &core,
            &registry,
            &peer,
            r#"{"jsonrpc":"2.0","id":1,"method":"execution_policy.list","params":{}}"#.into(),
        )
        .await;
        assert!(v.get("error").is_none(), "list answers: {v}");
        assert_eq!(v["result"]["loaded"], serde_json::json!(true));
        assert_eq!(
            v["result"]["defaults"]["budgetRatio"],
            serde_json::json!("0.10")
        );
        assert!(
            v["result"]["accounts"]
                .as_array()
                .is_some_and(|a| a.is_empty())
        );

        // get: an account with no section folds over the globals (effective view).
        let v = rpc(&core, &registry, &peer,
            r#"{"jsonrpc":"2.0","id":2,"method":"execution_policy.get","params":{"accountId":"acct-a"}}"#.into())
            .await;
        assert!(v.get("error").is_none(), "get falls back to globals: {v}");
        assert_eq!(v["result"]["budgetRatio"], serde_json::json!("0.10"));

        // set: write ONE account's section; decimals arrive as strings.
        let v = rpc(&core, &registry, &peer,
            r#"{"jsonrpc":"2.0","id":3,"method":"execution_policy.set","params":{"accountId":"acct-a","budgetRatio":"0.05","maxBudgetUsd":"20"}}"#.into())
            .await;
        assert!(v.get("error").is_none(), "set answers: {v}");

        // The file now carries the section, rendered as valid TOML…
        let text = std::fs::read_to_string(&path).expect("policy file");
        assert!(text.contains("[accounts.acct-a]"), "section landed: {text}");
        // …and the reload sees it (the effective view is the account's own).
        let v = rpc(&core, &registry, &peer,
            r#"{"jsonrpc":"2.0","id":4,"method":"execution_policy.get","params":{"accountId":"acct-a"}}"#.into())
            .await;
        assert_eq!(
            v["result"]["budgetRatio"],
            serde_json::json!("0.05"),
            "set then get: {v}"
        );
        assert_eq!(v["result"]["maxBudgetUsd"], serde_json::json!("20"), "{v}");
        // The untouched global section is byte-identical in intent.
        let v = rpc(&core, &registry, &peer,
            r#"{"jsonrpc":"2.0","id":5,"method":"execution_policy.get","params":{"accountId":"someone-else"}}"#.into())
            .await;
        assert_eq!(
            v["result"]["budgetRatio"],
            serde_json::json!("0.10"),
            "cross-account untouched: {v}"
        );

        // set with an out-of-range ratio is refused on the wire (INVALID_PARAMS),
        // and the in-memory policy is untouched.
        let v = rpc(&core, &registry, &peer,
            r#"{"jsonrpc":"2.0","id":6,"method":"execution_policy.set","params":{"accountId":"acct-a","budgetRatio":"1.5"}}"#.into())
            .await;
        assert_eq!(
            v["error"]["code"],
            serde_json::json!(Failure::INVALID_PARAMS),
            "bad ratio refused: {v}"
        );
        let v = rpc(&core, &registry, &peer,
            r#"{"jsonrpc":"2.0","id":7,"method":"execution_policy.get","params":{"accountId":"acct-a"}}"#.into())
            .await;
        assert_eq!(
            v["result"]["budgetRatio"],
            serde_json::json!("0.05"),
            "refusal kept old section: {v}"
        );

        // history: this run's set landed one audit line; the refused set did
        // not. Counted against the pre-set length (clock-free freshness —
        // see the note at the top of this test).
        let this_run = rpc(&core, &registry, &peer,
            r#"{"jsonrpc":"2.0","id":8,"method":"execution_policy.history","params":{"accountId":"acct-a"}}"#.into())
            .await;
        let lines = this_run["result"].as_array().expect("history array");
        assert_eq!(
            lines.len(),
            before_set + 1,
            "one set line this run, refusals do not audit: {this_run}"
        );
        let fresh = lines.last().expect("the one fresh line");
        assert_eq!(fresh["action"], serde_json::json!("set"));
        assert_eq!(fresh["accountId"], serde_json::json!("acct-a"));
        // The `after` summary is the RELOADED state: the account's own 0.05.
        assert_eq!(
            fresh["after"]["budgetRatio"],
            serde_json::json!("0.05"),
            "{this_run}"
        );

        // reset: the section goes away; the account folds back to the globals.
        let v = rpc(&core, &registry, &peer,
            r#"{"jsonrpc":"2.0","id":9,"method":"execution_policy.reset","params":{"accountId":"acct-a"}}"#.into())
            .await;
        assert!(v.get("error").is_none(), "reset answers: {v}");
        let v = rpc(&core, &registry, &peer,
            r#"{"jsonrpc":"2.0","id":10,"method":"execution_policy.get","params":{"accountId":"acct-a"}}"#.into())
            .await;
        assert_eq!(
            v["result"]["budgetRatio"],
            serde_json::json!("0.10"),
            "reset falls back: {v}"
        );
        let text = std::fs::read_to_string(&path).expect("policy file");
        assert!(
            !text.contains("[accounts.acct-a]"),
            "section removed: {text}"
        );

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// The reset of a section that does not exist is refused by name — an
    /// operator who fat-fingers an id finds out immediately, not "silently ok".
    #[tokio::test]
    async fn execution_policy_reset_of_unknown_section_refuses() {
        let (core, registry, peer, path) = policy_fixture().await;
        let v = rpc(&core, &registry, &peer,
            r#"{"jsonrpc":"2.0","id":1,"method":"execution_policy.reset","params":{"accountId":"never-existed"}}"#.into())
            .await;
        assert_eq!(
            v["error"]["code"],
            serde_json::json!(Failure::INVALID_PARAMS),
            "{v}"
        );
        assert!(
            v["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("never-existed"),
            "the refusal names the account: {v}"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// #364 — the FRESHNESS contract at the wire: after `execution_policy.set`
    /// answers, a `get` on the SAME connection (any connection — the policy
    /// is core state, not session state) must return the NEW view, and
    /// `history` must carry one more audit line for the account. This is the
    /// testable form of "WebUI saves → TUI reads stale → red": the two faces
    /// share the core's state, and this pins that a write is visible to the
    /// next reader without any restart or re-login.
    #[tokio::test]
    async fn execution_policy_set_is_fresh_for_the_next_reader() {
        let (core, registry, peer, path) = policy_fixture().await;

        // The baseline read: the account folds over the globals (0.10).
        let v = rpc(&core, &registry, &peer,
            r#"{"jsonrpc":"2.0","id":1,"method":"execution_policy.get","params":{"accountId":"acct-fresh"}}"#.into())
            .await;
        assert_eq!(
            v["result"]["budgetRatio"],
            serde_json::json!("0.10"),
            "pre-set view: {v}"
        );
        let history_before = rpc(&core, &registry, &peer,
            r#"{"jsonrpc":"2.0","id":2,"method":"execution_policy.history","params":{"accountId":"acct-fresh"}}"#.into())
            .await;
        let before_len = history_before["result"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0);

        // A write lands through the wire…
        let v = rpc(&core, &registry, &peer,
            r#"{"jsonrpc":"2.0","id":3,"method":"execution_policy.set","params":{"accountId":"acct-fresh","budgetRatio":"0.07","maxBudgetUsd":"25"}}"#.into())
            .await;
        assert!(v.get("error").is_none(), "set answers: {v}");

        // …and the VERY NEXT reader sees the new state, no restart, no
        // re-login, no cache to invalidate.
        let v = rpc(&core, &registry, &peer,
            r#"{"jsonrpc":"2.0","id":4,"method":"execution_policy.get","params":{"accountId":"acct-fresh"}}"#.into())
            .await;
        assert_eq!(
            v["result"]["budgetRatio"],
            serde_json::json!("0.07"),
            "post-set get must be FRESH: {v}"
        );
        assert_eq!(v["result"]["maxBudgetUsd"], serde_json::json!("25"), "{v}");

        // The audit trail grew by exactly one line for this account, and it
        // is the set that just answered.
        let history_after = rpc(&core, &registry, &peer,
            r#"{"jsonrpc":"2.0","id":5,"method":"execution_policy.history","params":{"accountId":"acct-fresh"}}"#.into())
            .await;
        let lines = history_after["result"].as_array().expect("history array");
        assert_eq!(
            lines.len(),
            before_len + 1,
            "history must grow by one per set: {history_after}"
        );
        let last = lines.last().expect("one fresh line");
        assert_eq!(last["action"], serde_json::json!("set"));
        assert_eq!(
            last["after"]["budgetRatio"],
            serde_json::json!("0.07"),
            "the after snapshot is the reloaded state"
        );

        // The preview strip rides the same freshness: the new ratio decides
        // the budget the replay reports (0.07 × the seeded balance).
        let v = rpc(&core, &registry, &peer,
            r#"{"jsonrpc":"2.0","id":6,"method":"execution_policy.preview","params":{"accountId":"acct-fresh"}}"#.into())
            .await;
        assert!(v.get("error").is_none(), "preview answers: {v}");
        assert!(
            v["result"]["considered"].is_u64(),
            "preview carries a considered count: {v}"
        );

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// #364 — the preview arm: a skip rule removes exactly the rows it names,
    /// and the budget the replay reports for the survivors is the balance ×
    /// ratio clamp the live order path would have handed out.
    #[tokio::test]
    async fn execution_policy_preview_replays_history_through_evaluate() {
        use rust_decimal_macros::dec;
        let (core, registry, peer, path) = policy_fixture_with_accounts(&["acct-prev"]).await;

        // An account section with a rule that skips BTC outright: the seed
        // trades below hit it by name. The condition rides the table form
        // ({field, op, value}) — the same spelling the file carries.
        let v = rpc(&core, &registry, &peer,
            r#"{"jsonrpc":"2.0","id":1,"method":"execution_policy.set","params":{"accountId":"acct-prev","budgetRatio":"0.10","minBudgetUsd":"1","maxBudgetUsd":"50","rules":[{"name":"no_btc","priority":10,"enabled":true,"when":{"field":"symbol","op":"==","value":"BTC"},"then":{"action":"skip"},"reason":"no bitcoin"}]}}"#.into())
            .await;
        assert!(v.get("error").is_none(), "set with rules: {v}");

        // Seed two closed trades for the account: one BTC (skipped), one ETH
        // (placed). Rows land in the position manager's own closed book —
        // the same book `trades.history` reports from — so the preview
        // replays exactly what the panel shows.
        let balance;
        {
            use crate::model::{ExitReason, OrderRole, SignalDirection};
            use crate::position::ClosedPosition;
            let mut c = core.lock().await;
            balance = dec!(100);
            c.accounts_mut()
                .get_mut(&AccountId::from("acct-prev"))
                .expect("configured ledger")
                .set_balance(balance);
            let mk = |asset: &str, price: Decimal| ClosedPosition {
                id: format!("hft-prev-{asset}"),
                strategy: "spread_arb".into(),
                asset: asset.into(),
                direction: SignalDirection::Up,
                token_id: format!("tok-{asset}"),
                condition_id: "cond".into(),
                account_id: AccountId::from("acct-prev"),
                entry_price: price,
                exit_price: price,
                shares: dec!(10),
                cost_usd: price * dec!(10),
                was_maker_entry: true,
                was_maker_exit: false,
                entry_fee_pct: Decimal::ZERO,
                exit_fee_pct: Decimal::ZERO,
                pnl_usd: Decimal::ZERO,
                pnl_pct: Decimal::ZERO,
                net_pnl_usd: Decimal::ZERO,
                net_pnl_pct: Decimal::ZERO,
                high_pnl_pct: Decimal::ZERO,
                low_pnl_pct: Decimal::ZERO,
                hold_time_sec: 60,
                exit_reason: ExitReason::TakeProfit,
                entered_at_ms: now_ms() - 60_000,
                exited_at_ms: now_ms(),
                entry_role: OrderRole::Maker,
                exit_role: OrderRole::Taker,
                dust_shares: Decimal::ZERO,
            };
            c.positions_mut()
                .push_closed_for_test(mk("BTC", dec!(0.40)));
            c.positions_mut()
                .push_closed_for_test(mk("ETH", dec!(0.60)));
        }
        let v = rpc(&core, &registry, &peer,
            r#"{"jsonrpc":"2.0","id":2,"method":"execution_policy.preview","params":{"accountId":"acct-prev"}}"#.into())
            .await;
        let r = &v["result"];
        assert_eq!(r["considered"], serde_json::json!(2), "{v}");
        assert_eq!(r["skipped"], serde_json::json!(1), "the BTC row skips: {v}");
        // One placed row at 10% of the seeded balance — the budget the rules
        // would have handed that entry.
        let expected = balance * dec!(0.10);
        assert_eq!(
            r["avgBudgetUsd"],
            serde_json::json!(expected.to_string()),
            "average budget = balance × ratio over the placed rows: {v}"
        );
        let rows = r["rows"].as_array().expect("rows");
        assert_eq!(rows.len(), 2, "one row per replayed entry");
        assert!(
            rows.iter()
                .any(|row| row["symbol"] == "BTC" && row["verdict"] == "skip"),
            "the BTC row is a skip: {v}"
        );
        assert!(
            rows.iter()
                .any(|row| row["symbol"] == "ETH" && row["verdict"] == "place"),
            "the ETH row places: {v}"
        );

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn blueprint_save_writes_the_package_files_and_receipt() {
        // A private strategy root through the core's OWN config — no process
        // env knob, so the two save tests cannot race each other for a shared
        // cell and nothing ever writes into the real checkout.
        let root = std::env::temp_dir().join(format!(
            "bk-ipc-bpsave-ok-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let (core, registry, peer) = save_fixture(&root).await;
        let json = serde_json::json!({ "json": WIRE_BLUEPRINT, "name": "wire_dog" }).to_string();
        let line = json_line(
            1,
            "blueprint.save",
            &serde_json::from_str::<Value>(&json).expect("obj"),
        );
        let reply = rpc(&core, &registry, &peer, line).await;
        assert!(reply.get("error").is_none(), "save must land: {reply}");

        let r = &reply["result"];
        assert_eq!(r["name"], serde_json::json!("wire_dog"));
        let pkg = root.join("wire_dog");
        // The receipt's paths are the files on disk, and all three exist.
        assert_eq!(
            r["packageDir"],
            serde_json::json!(pkg.display().to_string())
        );
        let lua = std::fs::read_to_string(pkg.join("strategy.lua")).expect("strategy.lua written");
        assert!(lua.contains("function on_tick(tick)"), "{lua}");
        let bp =
            std::fs::read_to_string(pkg.join("blueprint.json")).expect("blueprint.json written");
        assert_eq!(bp, WIRE_BLUEPRINT, "the blueprint bytes land verbatim");
        let manifest: Value = serde_json::from_str(
            &std::fs::read_to_string(pkg.join("manifest.json")).expect("manifest written"),
        )
        .expect("manifest is JSON");
        assert_eq!(manifest["name"], "wire_dog");
        assert_eq!(manifest["entry"], "strategy.lua");
        assert_eq!(manifest["api"], "1.0");
        // The manifest's digest matches the receipt AND the file — the loader
        // verifies exactly this, so a drifted copy would refuse the package.
        use sha2::Digest;
        let mut hasher = sha2::Sha256::new();
        hasher.update(lua.as_bytes());
        let digest = format!("{:x}", hasher.finalize());
        assert_eq!(r["luaSha256"], serde_json::json!(digest));
        assert_eq!(manifest["sha256"], serde_json::json!(digest));
        // The receipt counts the bytes of all three files.
        assert_eq!(
            r["bytes"],
            serde_json::json!([
                WIRE_BLUEPRINT.len(),
                lua.len(),
                std::fs::read_to_string(pkg.join("manifest.json"))
                    .unwrap()
                    .len()
            ])
        );

        // A second save of the SAME name refuses: the package is now on the
        // loader's scan path and a silent overwrite could swap a strategy an
        // operator believes they know.
        let line2 = json_line(
            2,
            "blueprint.save",
            &serde_json::from_str::<Value>(&json).expect("obj"),
        );
        let reply = rpc(&core, &registry, &peer, line2).await;
        let msg = reply["error"]["message"].as_str().unwrap_or_default();
        assert!(msg.contains("already exists"), "{reply}");

        // With the explicit flag the SAME call lands: overwrite is possible,
        // never silent.
        let line3 = json_line(
            3,
            "blueprint.save",
            &serde_json::json!({
                "name": "wire_dog", "json": WIRE_BLUEPRINT, "overwrite": true,
            }),
        );
        let reply = rpc(&core, &registry, &peer, line3).await;
        assert!(
            reply.get("error").is_none(),
            "an explicit overwrite must land: {reply}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn blueprint_load_returns_the_saved_document() {
        // #393: the read-only mirror of save — a saved package's blueprint
        // document comes back verbatim; a hand-written (save-less) package and
        // a path-flavoured name are refused with the honest reasons.
        let root =
            std::env::temp_dir().join(format!("bk-ipc-bpload-{}-{}", std::process::id(), now_ms()));
        let (core, registry, peer) = save_fixture(&root).await;

        // Save one package first — load answers from what save wrote.
        let save_line = json_line(
            1,
            "blueprint.save",
            &serde_json::json!({ "name": "wire_dog", "json": WIRE_BLUEPRINT }),
        );
        let reply = rpc(&core, &registry, &peer, save_line).await;
        assert!(reply.get("error").is_none(), "save must land: {reply}");

        // The load returns the SAME bytes the save wrote.
        let load_line = json_line(
            2,
            "blueprint.load",
            &serde_json::json!({ "name": "wire_dog" }),
        );
        let reply = rpc(&core, &registry, &peer, load_line).await;
        assert!(reply.get("error").is_none(), "load must answer: {reply}");
        let r = &reply["result"];
        assert_eq!(r["name"], serde_json::json!("wire_dog"));
        assert_eq!(r["json"], serde_json::json!(WIRE_BLUEPRINT));
        assert_eq!(
            r["blueprintPath"],
            serde_json::json!(
                root.join("wire_dog")
                    .join("blueprint.json")
                    .display()
                    .to_string()
            )
        );

        // A package with no blueprint document (never written by save) is an
        // explicit refusal, not an invented graph.
        let no_doc = json_line(
            3,
            "blueprint.load",
            &serde_json::json!({ "name": "no_such_pkg" }),
        );
        let reply = rpc(&core, &registry, &peer, no_doc).await;
        let msg = reply["error"]["message"].as_str().unwrap_or_default();
        assert!(msg.contains("no blueprint document"), "{reply}");

        // The load path screens the name with the save path's own rules, so
        // no caller can steer the read outside the strategy root.
        let escape = json_line(
            4,
            "blueprint.load",
            &serde_json::json!({ "name": "../escape" }),
        );
        let reply = rpc(&core, &registry, &peer, escape).await;
        let msg = reply["error"]["message"].as_str().unwrap_or_default();
        assert!(msg.contains("ASCII letters"), "{reply}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn blueprint_load_source_reads_a_handwritten_package() {
        // 手写包的编辑入口：没有 blueprint.json 的包不再被拒 —— 入口文件与
        // manifest 原样读回；名字闸门与 load/save 同源；不存在的包报「no
        // manifest」而不是编造内容。
        let root =
            std::env::temp_dir().join(format!("bk-ipc-bpsrc-{}-{}", std::process::id(), now_ms()));
        let (core, registry, peer) = save_fixture(&root).await;
        let pkg = root.join("hand_poked");
        std::fs::create_dir_all(&pkg).expect("package dir");
        std::fs::write(pkg.join("strategy.lua"), "function bk_evaluate() end\n")
            .expect("entry file");
        std::fs::write(
            pkg.join("manifest.json"),
            r#"{"name":"hand_poked","version":"2.0.0","api":"1.0","entry":"strategy.lua","sha256":"deadbeef"}"#,
        )
        .expect("manifest");

        let line = json_line(
            1,
            "blueprint.loadSource",
            &serde_json::json!({ "name": "hand_poked" }),
        );
        let reply = rpc(&core, &registry, &peer, line).await;
        assert!(reply.get("error").is_none(), "read must answer: {reply}");
        let r = &reply["result"];
        assert_eq!(r["lua"], serde_json::json!("function bk_evaluate() end\n"));
        assert_eq!(r["sha256"], serde_json::json!("deadbeef"));
        assert_eq!(r["manifest"]["version"], serde_json::json!("2.0.0"));

        // A package with no manifest is refused with the honest reason.
        let line = json_line(
            2,
            "blueprint.loadSource",
            &serde_json::json!({ "name": "no_such_pkg" }),
        );
        let reply = rpc(&core, &registry, &peer, line).await;
        let msg = reply["error"]["message"].as_str().unwrap_or_default();
        assert!(msg.contains("no manifest"), "{reply}");

        // Same package-name screen as load/save: no path steering.
        let line = json_line(
            3,
            "blueprint.loadSource",
            &serde_json::json!({ "name": "../escape" }),
        );
        let reply = rpc(&core, &registry, &peer, line).await;
        let msg = reply["error"]["message"].as_str().unwrap_or_default();
        assert!(msg.contains("ASCII letters"), "{reply}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn blueprint_save_source_reseals_the_manifest_and_refuses_implicit_writes() {
        // 写回镜像：原样落盘 + sha256 现场重算进 manifest（装载器照常验证）；
        // 内容不同且未显式 overwrite 时拒绝写；目标包不存在时拒绝（编辑不
        // 创建）；manifest entry 带路径成分时拒绝（不写出包目录）。
        let root =
            std::env::temp_dir().join(format!("bk-ipc-bpsrcw-{}-{}", std::process::id(), now_ms()));
        let (core, registry, peer) = save_fixture(&root).await;
        let pkg = root.join("hand_poked");
        std::fs::create_dir_all(&pkg).expect("package dir");
        std::fs::write(pkg.join("strategy.lua"), "-- old\n").expect("entry file");
        std::fs::write(
            pkg.join("manifest.json"),
            r#"{"name":"hand_poked","version":"1.0.0","api":"1.0","entry":"strategy.lua","sha256":"old"}"#,
        )
        .expect("manifest");

        // Different body, no overwrite — refused, and nothing changed.
        let line = json_line(
            1,
            "blueprint.saveSource",
            &serde_json::json!({ "name": "hand_poked", "lua": "-- new\n" }),
        );
        let reply = rpc(&core, &registry, &peer, line).await;
        let msg = reply["error"]["message"].as_str().unwrap_or_default();
        assert!(msg.contains("must be explicit"), "{reply}");
        assert_eq!(
            std::fs::read_to_string(pkg.join("strategy.lua")).expect("old body intact"),
            "-- old\n"
        );

        // Explicit overwrite lands, and the manifest's sha256 now matches the
        // NEW bytes (the loader's §6.4 verification would refuse otherwise).
        let line = json_line(
            2,
            "blueprint.saveSource",
            &serde_json::json!({ "name": "hand_poked", "lua": "-- new\n", "overwrite": true }),
        );
        let reply = rpc(&core, &registry, &peer, line).await;
        assert!(reply.get("error").is_none(), "overwrite must land: {reply}");
        let r = &reply["result"];
        let new_sha = r["luaSha256"].as_str().expect("sha in receipt");
        assert_eq!(
            std::fs::read_to_string(pkg.join("strategy.lua")).expect("new body"),
            "-- new\n"
        );
        let manifest: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(pkg.join("manifest.json")).expect("m"))
                .expect("manifest json");
        assert_eq!(manifest["sha256"], serde_json::json!(new_sha));
        assert_eq!(
            manifest["version"],
            serde_json::json!("1.0.0"),
            "untouched fields stay"
        );

        // Identical body without overwrite is a no-op write that still answers.
        let line = json_line(
            3,
            "blueprint.saveSource",
            &serde_json::json!({ "name": "hand_poked", "lua": "-- new\n" }),
        );
        let reply = rpc(&core, &registry, &peer, line).await;
        assert!(reply.get("error").is_none(), "no-op write answers: {reply}");

        // A package with no manifest is refused — saveSource edits, never creates.
        let line = json_line(
            4,
            "blueprint.saveSource",
            &serde_json::json!({ "name": "no_such_pkg", "lua": "x", "overwrite": true }),
        );
        let reply = rpc(&core, &registry, &peer, line).await;
        let msg = reply["error"]["message"].as_str().unwrap_or_default();
        assert!(msg.contains("no manifest"), "{reply}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn blueprint_save_refusals_write_nothing() {
        // Same config-rooted fixture as the ok-path test.
        let root = std::env::temp_dir().join(format!(
            "bk-ipc-bpsave-bad-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let (core, registry, peer) = save_fixture(&root).await;

        // A path-flavoured name is refused BEFORE any directory exists.
        let line = json_line(
            1,
            "blueprint.save",
            &serde_json::json!({
                "name": "../escape", "json": WIRE_BLUEPRINT,
            }),
        );
        let reply = rpc(&core, &registry, &peer, line).await;
        let msg = reply["error"]["message"].as_str().unwrap_or_default();
        assert!(msg.contains("ASCII letters"), "{reply}");
        assert!(!root.join("escape").exists(), "no directory was created");

        // An invalid blueprint is refused by the compiler — nothing on disk.
        let line = json_line(
            2,
            "blueprint.save",
            &serde_json::json!({
                "name": "no_action_pkg",
                "json": r#"{"version":1,"name":"noact","nodes":[
                {"id":"a1","type":"data_source","params":{"field":"tick.price"}},
                {"id":"a2","type":"condition","params":{"op":"<","value":1}}],
                "edges":[{"from":"a1","to":"a2"}]}"#,
            }),
        );
        let reply = rpc(&core, &registry, &peer, line).await;
        let msg = reply["error"]["message"].as_str().unwrap_or_default();
        assert!(msg.contains("no reachable action output"), "{reply}");
        assert!(
            !root.join("no_action_pkg").exists(),
            "a refused compile must write nothing"
        );

        // A name carrying a forbidden substring is refused (it would be
        // embedded in the generated source verbatim).
        let line = json_line(
            3,
            "blueprint.save",
            &serde_json::json!({
                "name": "debug_probe", "json": WIRE_BLUEPRINT,
            }),
        );
        let reply = rpc(&core, &registry, &peer, line).await;
        let msg = reply["error"]["message"].as_str().unwrap_or_default();
        assert!(msg.contains("refused"), "{reply}");
        assert!(
            !root.join("debug_probe").exists(),
            "no directory was created"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}

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
use std::os::fd::{AsRawFd, RawFd};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
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
                    spawn_session(stream, peer, core.clone(), bus_tx.subscribe(), registry.clone())
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

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn spawn_session(
    stream: UnixStream,
    peer: PeerAuth,
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
                    let response = handle_line(&core, &registry, line, &peer).await;
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
    peer: &PeerAuth,
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

        method::ORDER_PLACE => {
            typed(params, |p: PlaceParams| {
                let core = core.clone();
                async move {
                    let mut c = core.lock().await;
                    // `place_outcome`, not `place`: when the kernel refuses the leg
                    // itself (a dry taker the book cannot fill) the result carries
                    // the structured reason instead of a bare REJECTED (#180).
                    let outcome = c.place_outcome(p.order, p.maker_timeout_ms, now_ms())?;
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
        serde_json::from_str(&handle_line(core, registry, line, peer).await)
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
}

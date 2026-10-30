//! Unix-domain-socket JSON-RPC 2.0 server.
//!
//! One JSON object per line. Multiple Node sessions may connect; each receives
//! the full event stream. Requests are dispatched against the shared Core under
//! an async lock; a writer task per connection serialises responses and pushed
//! notifications onto the socket.

use crate::account::AccountId;
#[cfg(test)]
use crate::account::AccountStatus;
use crate::ipc::schema::*;
use crate::model::{CoreError, CoreErrorCode, Mode};
use crate::service::{Core, CoreConfig};
use blitzkrieg_market_api::net::now_ms;
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

/// The data plane's boot-resolved shape, shared with every session: the
/// mount flags (config is the flag's only channel — they never change
/// mid-session) and the ALREADY-MOUNTED discovery instances. `account.venue.set`
/// reads it to decide whether a re-binding needs a runtime mount.
///
/// B1d (轮次多族): the mount book is keyed on (venue, duration) — each
/// round-length family its own discovery instance (the plugin's
/// `DiscoveryConfig` bakes the duration in at start, so one instance per
/// family is the physical shape; kalshi selects its series from
/// (`asset`, `round_duration_sec`), polymarket its slug from
/// (`asset`, duration, now)). The book holds EVERY family the deployment
/// declares — the accounts' `rounds` sets plus the boot's global
/// single value — assembled once at boot (`at_boot`); a mount is
/// idempotent per PAIR, so the same venue can carry 900s and 3600s
/// instances side by side without one restarting the other. Feed mounts
/// stay per-venue (the feed is venue-anchored, tokens arrive later via
/// `subscribe_tokens`); `venue_mounted` (venue alone) keeps its meaning
/// for the FEED idempotence the venue.set/create guards rely on.
#[derive(Clone)]
struct DataPlaneRuntime {
    feed_ws_enabled: bool,
    discovery_enabled: bool,
    /// The boot's discovery parameters, snapshotted so a runtime mount can
    /// hand the plugin the SAME config boot would. Polymarket's discovery
    /// derives everything from the host on its own; kalshi's picks its
    /// series from (`asset`, `round_duration_sec`) — with the zero/empty
    /// values a mount used to pass, `series_ticker_for(_, 0)` is None and
    /// the venue discovers nothing forever (the feed then starves for want
    /// of subscription tokens). #535 production replay proved it: the mount
    /// reported `runtimeEnabled: true` while zero kalshi events ever landed.
    discovery_assets: Vec<String>,
    discovery_round_duration_sec: i64,
    /// Every (venue, duration) discovery family the deployment declares —
    /// boot actives on the global value, plus every account `rounds` row
    /// (B1d). The idempotence guard: an instance whose pair is already here
    /// is never restarted (a duplicate `disc.start` would evict the running
    /// one's host handle).
    discovery_mounted: std::sync::Arc<std::sync::Mutex<std::collections::BTreeSet<(String, i64)>>>,
    /// B2 (轮次多族 软卸): discovery-poll batches whose (venue, duration)
    /// family has been soft-unloaded (removed from every account's active
    /// family set while the instance itself keeps running) are DROPPED at
    /// this gate instead of reaching the engine. Count = batches dropped
    /// since boot — the operator's 「软卸还在漏喂吗」 readout. A plain
    /// AtomicUsize surfaced through `core.status`, not a metrics framework:
    /// this is the only counter the feature needs.
    /// ponytail ceiling: no aggregation/history — if a second runtime gauge
    /// ever appears, promote both into a real register.
    soft_unloaded_polling_instances: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// RFC-661 M2: the `feed_source` reuse plane — the declared consumer→
    /// source edges plus the upstream-mount counter. `None` in tests that
    /// assembled no reuse plane: a consumer binding then runs the HISTORICAL
    /// mounts (its own feed/discovery start) and registers no edges — the
    /// pipeline is strictly additive. The cell lives on the data plane so
    /// the runtime mount path (`enable_venue_at_runtime`) and the boot
    /// assembly share ONE source of truth; the concrete host holds a clone
    /// (the inner Arcs are shared) so its `on_round_markets` fan-out reads
    /// the same edges the mount path writes.
    feed_reuse: Option<crate::market::host::FeedReuse>,
}

impl DataPlaneRuntime {
    /// The boot shape: every boot-selected plugin is mounted on the GLOBAL
    /// duration (the historical single-instance posture), and every account
    /// `rounds` family gains its own discovery instance — the boot half of
    /// 轮次多族's instance pool. Feed mounts stay per-venue and are visible
    /// through [`Self::venue_mounted`]. The reuse cell installs here seeded
    /// with the BOOT-ACTIVE consumers' declared edges only — an inactive
    /// plugin never gains a data-plane face (a later runtime bind appends
    /// its edge through [`Self::feed_reuse`]).
    fn at_boot(
        config: &CoreConfig,
        actives: &[std::sync::Arc<dyn blitzkrieg_market_api::MarketPlugin>],
        accounts: &[crate::account::Account],
        registry: &crate::market::registry::MarketPluginRegistry,
    ) -> Self {
        let global = config.round_duration_sec;
        let mut pairs = std::collections::BTreeSet::new();
        for p in actives {
            pairs.insert((p.name().to_string(), global));
        }
        for a in accounts {
            for &dur in &a.rounds {
                if a.venue.is_empty() {
                    // An unbound account's declaration speaks for EVERY
                    // venue this boot enabled.
                    for p in actives {
                        pairs.insert((p.name().to_string(), dur));
                    }
                } else {
                    pairs.insert((a.venue.clone(), dur));
                }
            }
        }
        // RFC-661 M2: the boot-seeded reuse edges — every boot-ACTIVE plugin
        // whose `feed_source` names a COMPILED-IN plugin. A declaration whose
        // plugin is not enabled this boot registers NOTHING (inactive plugins
        // get no data plane); the runtime mount path appends the edge when an
        // account actually binds the consumer.
        let feed_reuse = crate::market::host::FeedReuse::default();
        for p in actives {
            if let Some(src) = p.feed_source().filter(|src| registry.get(src).is_some()) {
                feed_reuse.add_edge(p.name(), src);
            }
        }
        Self {
            feed_ws_enabled: config.feed_ws_enabled,
            discovery_enabled: config.engine_enabled && config.discovery_enabled,
            discovery_assets: config.assets.clone(),
            discovery_round_duration_sec: global,
            discovery_mounted: std::sync::Arc::new(std::sync::Mutex::new(pairs)),
            soft_unloaded_polling_instances: std::sync::Arc::new(
                std::sync::atomic::AtomicUsize::new(0),
            ),
            feed_reuse: Some(feed_reuse),
        }
    }

    /// The FEED idempotence guard (per-venue: one feed per venue, its
    /// tokens arrive later) — the shape `account.venue.set` /
    /// `account.create` have always checked.
    fn venue_mounted(&self, venue: &str) -> bool {
        self.discovery_mounted
            .lock()
            .expect("data plane mounted set poisoned")
            .iter()
            .any(|(v, _)| v == venue)
    }

    /// The DISCOVERY idempotence guard: this exact (venue, duration) family
    /// already runs its own instance.
    fn discovery_mounted_at(&self, venue: &str, duration: i64) -> bool {
        self.discovery_mounted
            .lock()
            .expect("data plane mounted set poisoned")
            .contains(&(venue.to_string(), duration))
    }

    fn mark_mounted(&self, venue: &str) {
        self.discovery_mounted
            .lock()
            .expect("data plane mounted set poisoned")
            .insert((venue.to_string(), self.discovery_round_duration_sec));
    }

    fn mark_discovery_mounted(&self, venue: &str, duration: i64) {
        self.discovery_mounted
            .lock()
            .expect("data plane mounted set poisoned")
            .insert((venue.to_string(), duration));
    }

    // ── RFC-661 M2 (feed_source 复用管道) ────────────────────────────────────

    /// The reuse plane's cell, when this session assembled one.
    fn feed_reuse(&self) -> Option<&crate::market::host::FeedReuse> {
        self.feed_reuse.as_ref()
    }

    /// Every declared consumer→source edge among `plugins` (the boot actives;
    /// a runtime bind appends its own edge through [`FeedReuse::add_edge`]).
    /// A plugin whose `feed_source` names a COMPILED-IN plugin is an edge; a
    /// declaration naming nothing registered is inert (the consumer then runs
    /// its own mounts — the declaration's absence-of-upstream contract simply
    /// has no upstream to reuse). An INACTIVE plugin appears in no edge list:
    /// it gets no data-plane face until an account binds it.
    fn declared_feed_edges(
        plugins: &[std::sync::Arc<dyn blitzkrieg_market_api::MarketPlugin>],
        registry: &crate::market::registry::MarketPluginRegistry,
    ) -> Vec<crate::market::host::FeedReuseEdge> {
        let mut edges: Vec<crate::market::host::FeedReuseEdge> = plugins
            .iter()
            .filter_map(|p| {
                p.feed_source()
                    .filter(|src| registry.get(src).is_some())
                    .map(|src| crate::market::host::FeedReuseEdge {
                        consumer: p.name().to_string(),
                        source: src.to_string(),
                    })
            })
            .collect();
        edges.sort_by(|a, b| (&a.consumer, &a.source).cmp(&(&b.consumer, &b.source)));
        edges.dedup();
        edges
    }

    /// B2 (轮次多族 软卸): the ACTIVE family set — every family some account
    /// currently declares, UNION the boot's global family. Computed on read
    /// from the live account book (never a second stored copy that could
    /// drift), the same口径 the boot assembly used: a bound account speaks
    /// for ITS venue, an unbound one for EVERY venue it is paired with in
    /// `pairs`'s venue dimension (the boot's actives + runtime mounts).
    /// `pairs` is the caller's already-materialized (venue, duration) mount
    /// book — the venue dimension this session actually runs.
    fn active_families(
        &self,
        accounts: &[crate::account::Account],
        pairs: &std::collections::BTreeSet<(String, i64)>,
    ) -> std::collections::BTreeSet<(String, i64)> {
        let mut active: std::collections::BTreeSet<(String, i64)> = pairs
            .iter()
            .filter(|(_, d)| *d == self.discovery_round_duration_sec)
            .cloned()
            .collect();
        for a in accounts {
            for &dur in &a.rounds {
                if a.venue.is_empty() {
                    // An unbound account's declaration speaks for EVERY
                    // venue this session runs (the boot's same reading).
                    for (v, _) in pairs.iter() {
                        active.insert((v.clone(), dur));
                    }
                } else {
                    active.insert((a.venue.clone(), dur));
                }
            }
        }
        active
    }

    /// B2 (轮次多族 软卸): the discovery poll-output gate. A batch whose
    /// (venue, duration) family is NOT in the active set is dropped here —
    /// the soft-unloaded instance keeps polling (nothing kills it) but its
    /// rounds never reach the engine/scanner again. The batch's family is
    /// resolved the way the plugins stamp their descriptors: the venue tag
    /// plus the batch's own round length derived from the descriptors'
    /// expiry/span (`row_duration_sec`'s reading — 0 = the global family).
    /// Every dropped batch bumps the gauge.
    async fn admit_poll_batch(
        &self,
        core: &Arc<AsyncMutex<Core>>,
        markets: &[blitzkrieg_market_api::MarketDescriptor],
    ) -> bool {
        let Some(first) = markets.first() else {
            return true;
        };
        // The batch's duration: the plugin baked its family into the slot
        // derivation, so the batch's span reproduces it (the same fact
        // `round_slot = end_ms / 1000 / round_sec` was built from). One
        // descriptor is enough — a batch is one family's poll.
        let span_sec = if first.round_slot > 0 && first.expires_at_ms > 0 {
            first.expires_at_ms / 1000 / first.round_slot
        } else {
            0
        };
        let global = self.discovery_round_duration_sec;
        let dur = if span_sec > 0 { span_sec } else { global };
        let venue = first.venue.as_str();
        let active = {
            let pairs = self
                .discovery_mounted
                .lock()
                .expect("data plane mounted set poisoned")
                .clone();
            let accounts: Vec<crate::account::Account> = core
                .lock()
                .await
                .accounts()
                .account_list()
                .into_iter()
                .cloned()
                .collect();
            self.active_families(&accounts, &pairs)
        };
        if active.contains(&(venue.to_string(), dur)) {
            return true;
        }
        self.soft_unloaded_polling_instances
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        eprintln!(
            "blitzkrieg-core: discovery batch for {venue}@{dur}s dropped — the family was \
             soft-unloaded (no account declares it any more); restart the core or re-add the \
             family to resume"
        );
        false
    }

    /// The gauge readout (`core.status`): how many discovery-poll batches
    /// soft-unload has dropped since boot.
    fn soft_unloaded_count(&self) -> usize {
        self.soft_unloaded_polling_instances
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Mount ONE venue's runtime components (feed + discovery) — the same two
/// mounts `run` performs for every boot-selected plugin, reused verbatim so
/// a runtime enablement is byte-for-byte the boot shape (no second mechanism).
///
/// The caller decides WHICH flags gate the mounts and WHAT discovery config
/// the plugin gets (this function does not re-read them): boot passes
/// `config.feed_ws_enabled` / `config.engine_enabled && config.discovery_enabled`;
/// the venue.set/create writes pass the session's same resolved flags plus the
/// boot's snapshotted assets/round length, so a runtime binding follows the
/// boot's data-plane posture exactly. Passing an empty/zero discovery config
/// here is NOT an option: polymarket tolerates it (it derives everything from
/// the host), kalshi selects its series from (`asset`, `round_duration_sec`)
/// and would silently discover nothing forever.
///
/// B1d (轮次多族): `families` lists every (venue, duration) pair to mount —
/// the venue's GLOBAL family plus each family the account's `rounds` row
/// declares (the same set `at_boot` pre-records; pairs already mounted are
/// skipped by the caller's idempotence guard). ONE feed mount runs per
/// call (the feed is venue-anchored, duration-blind); discovery starts
/// once per family.
///
/// Executor: NOT mounted here. `mode.may_trade()` is a boot-frozen promise —
/// Dry/ReadOnly never construct egress, and a Live core's executor set is
/// built around the boot's own plugins (`ExecutorConfig.markets` etc.). A
/// venue bound at runtime gains market DATA (what the 行情 page starves on);
/// live egress for a late-bound venue is the next batch's contract, and the
/// boot-derived set already covers every venue bound BEFORE the process
/// started.
#[allow(clippy::too_many_arguments)]
async fn enable_venue_at_runtime(
    plugin: &dyn blitzkrieg_market_api::MarketPlugin,
    host: std::sync::Arc<dyn blitzkrieg_market_api::MarketHost>,
    feed_ws_enabled: bool,
    discovery_enabled: bool,
    discovery_assets: &[String],
    discovery_round_duration_sec: i64,
    families: &[(String, i64)],
    registry: &crate::market::registry::MarketPluginRegistry,
    // RFC-661 M2: the session's data plane — its `discovery_mounted` set IS
    // the enabled-venues guard (boot actives pre-record every active venue's
    // families; runtime binds append), and its optional reuse cell carries
    // the declared edges + the upstream-mount counter.
    data_plane: &DataPlaneRuntime,
) {
    let name = plugin.name();
    // RFC-661 M2 (feed_source 复用管道): a consumer plugin owns NO upstream —
    // its own feed/discovery must NOT start (that would be a second upstream
    // connection, the exact thing the reuse pipeline exists to prevent).
    // Instead:
    //   * the consumer's venue joins the reuse plane's declared edges (its
    //     own `(venue, token)` rows re-register from the source's batches
    //     through the host's fan-out), and
    //   * the SOURCE plugin's data components mount here when the source is
    //     not already mounted (consumer-only enablement: the data plane is
    //     self-sufficient; the source's EXECUTOR is never constructed — this
    //     function never mounts executors, and the source reaches it as a
    //     data-only registry fetch).
    // The source-mounted guard is the data plane's own `discovery_mounted`
    // set: boot actives pre-record every active venue's families (at_boot),
    // so a source among the actives is already running and a second mount
    // would duplicate the upstream. Each source component mounts EXACTLY
    // once (N consumers of the same source still read one upstream stream —
    // the mount counter is the falsifiable claim's observable).
    if let Some(src) = plugin
        .feed_source()
        .filter(|src| registry.get(src).is_some())
    {
        let reuse = data_plane.feed_reuse();
        // The declared edge (idempotent): a re-bind of the same consumer
        // appends nothing.
        match reuse {
            Some(r) => r.add_edge(name, src),
            None => false,
        };
        let source_plugin = registry.get(src);
        let source_enabled = source_plugin
            .as_ref()
            .map(|sp| data_plane.venue_mounted(sp.name()))
            .unwrap_or(false);
        if let Some(sp) = source_plugin {
            // Whether the SOURCE's feed actually started this bind — the
            // venue-mounted mark at the bottom fires only on a real start,
            // so a failed start stays retryable by the next consumer bind.
            let mut feed_started = false;
            // The source's FEED mounts only when the source is not already
            // venue-mounted (boot actives / an earlier bind): the per-venue
            // guard keeps the upstream at ONE feed for any number of
            // consumers and re-binds.
            if !source_enabled {
                if feed_ws_enabled {
                    match sp.data_feed() {
                        Some(feed) => {
                            let cfg = blitzkrieg_market_api::DataFeedConfig::default();
                            let ok = match feed.start(host.clone(), cfg, Vec::new()).await {
                                Err(e) => {
                                    eprintln!(
                                        "blitzkrieg-core: data feed start failed for {src}: {e}"
                                    );
                                    false
                                }
                                Ok(()) => true,
                            };
                            if ok {
                                feed_started = true;
                                if let Some(r) = reuse {
                                    r.mounts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                }
                                eprintln!(
                                    "blitzkrieg-core: feed started for {src} (consumer {name}'s \
                                     upstream; no executor mounted)"
                                );
                                // NOT mark_mounted here: the mount set is shared by
                                // the feed guard (`venue_mounted`) and the discovery
                                // guard (`discovery_mounted_at`) — marking the source
                                // now would insert (src, global) and make the
                                // discovery loop below read every family as
                                // already-mounted, skipping the source's discovery
                                // entirely.
                            }
                        }
                        None => {
                            eprintln!(
                                "blitzkrieg-core: {src} declares no data feed — nothing to mount"
                            );
                        }
                    }
                }
            } else {
                eprintln!(
                    "blitzkrieg-core: {name} consumes {src}'s stream (already mounted — no second \
                     upstream)"
                );
            }
            if discovery_enabled {
                for (_, duration) in families {
                    // Per-(source, family) idempotence: the same guard the
                    // venue's own mount uses — a consumer binding onto an
                    // already-mounted family must not duplicate the instance.
                    if data_plane.discovery_mounted_at(src, *duration) {
                        continue;
                    }
                    let Some(disc) = sp.discovery() else {
                        eprintln!(
                            "blitzkrieg-core: {src} declares no discovery — nothing to mount"
                        );
                        continue;
                    };
                    let cfg = blitzkrieg_market_api::DiscoveryConfig {
                        assets: discovery_assets.to_vec(),
                        round_duration_sec: *duration,
                        poll_sec: 5,
                    };
                    let ok = match disc.start(host.clone(), cfg).await {
                        Err(e) => {
                            eprintln!(
                                "blitzkrieg-core: discovery start failed for {src}@{duration}s: {e}"
                            );
                            false
                        }
                        Ok(()) => true,
                    };
                    if ok {
                        if let Some(r) = reuse {
                            r.mounts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                        eprintln!(
                            "blitzkrieg-core: discovery started for {src}@{duration}s (consumer \
                             {name}'s upstream; no executor mounted)"
                        );
                        data_plane.mark_discovery_mounted(src, *duration);
                    }
                }
            }
            // The feed's idempotence mark lands AFTER the discovery loop:
            // the first bind mounts the feed and every family, THEN records
            // the source as venue-mounted — a later consumer bind (or a
            // re-bind) reads `venue_mounted(src)` == true and skips both
            // arms entirely. Marking before the loop would poison the
            // discovery guard (same set, same key shape); marking on a
            // FAILED start would wedge the source unmounted-but-marked.
            // No feed declared → nothing to retry: mark so the source still
            // counts as mounted (its discovery is the only component).
            if feed_started || (feed_ws_enabled && sp.data_feed().is_none()) {
                data_plane.mark_mounted(src);
            }
        }
        // The consumer's OWN data components never start: the fan-out is its
        // registrar. Fall through — the historical mounts below are skipped.
        return;
    }
    if feed_ws_enabled {
        match plugin.data_feed() {
            Some(feed) => {
                let cfg = blitzkrieg_market_api::DataFeedConfig::default();
                if let Err(e) = feed.start(host.clone(), cfg, Vec::new()).await {
                    eprintln!("blitzkrieg-core: data feed start failed for {name}: {e}");
                } else {
                    eprintln!("blitzkrieg-core: feed started for {name} (runtime enablement)");
                }
            }
            None => eprintln!("blitzkrieg-core: {name} declares no data feed — nothing to mount"),
        }
    }
    if discovery_enabled {
        for (_, duration) in families {
            match plugin.discovery() {
                Some(disc) => {
                    let cfg = blitzkrieg_market_api::DiscoveryConfig {
                        assets: discovery_assets.to_vec(),
                        round_duration_sec: *duration,
                        poll_sec: 5,
                    };
                    if let Err(e) = disc.start(host.clone(), cfg).await {
                        eprintln!(
                            "blitzkrieg-core: discovery start failed for {name}@{duration}s: {e}"
                        );
                    } else if *duration == discovery_round_duration_sec {
                        eprintln!(
                            "blitzkrieg-core: discovery started for {name} (runtime enablement)"
                        );
                    } else {
                        eprintln!(
                            "blitzkrieg-core: discovery started for {name} @ {duration}s family \
                             (runtime enablement)"
                        );
                    }
                }
                None => {
                    eprintln!("blitzkrieg-core: {name} declares no discovery — nothing to mount")
                }
            }
        }
    }
}

/// P0 #670: Linux/macOS 的 EMFILE 都是 errno 24——fd 配额耗尽时 accept()
/// 立即失败，是自旋刷日志的典型诱因。单测钉 `== libc::EMFILE`。
const ACCEPT_EMFILE_ERRNO: i32 = 24;

/// P0 #670: 同一 [`std::io::ErrorKind`] 的 warn 限速窗。
const ACCEPT_LOG_WINDOW: std::time::Duration = std::time::Duration::from_secs(5);
/// P0 #670: Err 臂退避起点与封顶（毫秒）。50ms 起，指数倍增到 2s 封顶。
const ACCEPT_BACKOFF_START_MS: u64 = 50;
const ACCEPT_BACKOFF_MAX_MS: u64 = 2_000;

/// accept() Err 臂的限速 + 退避状态（P0 #670）。纯逻辑、无 IO：`step` 吃
/// 调用方时钟（测试可控），sleep/打日志/发事件都留在调用点。
#[derive(Debug, Default)]
struct AcceptErrState {
    /// 当前退避毫秒数（Ok 复位为起点）。
    backoff_ms: u64,
    /// 上次放行打印的 (错误种类, 时刻)——同种类 5s 内只打一条。
    last_logged: Option<(std::io::ErrorKind, std::time::Instant)>,
    /// 限速窗内被吞掉的错误条数，下次放行时随 `suppressed` 字段补记。
    suppressed: u64,
}

impl AcceptErrState {
    /// 一次 accept 失败：更新状态，返回 (是否本次放行打印, 截至已抑制条数)。
    /// 同 ErrorKind 在 5s 窗内静默（计数）；跨窗或换种类立即放行；
    /// 放行后倍增退避（50→100→…→2000 封顶）。
    fn step(&mut self, kind: std::io::ErrorKind, now: std::time::Instant) -> (bool, u64) {
        let within_window = match &self.last_logged {
            Some((last_kind, at)) => {
                *last_kind == kind && now.duration_since(*at) < ACCEPT_LOG_WINDOW
            }
            None => false,
        };
        if within_window {
            self.suppressed += 1;
            return (false, self.suppressed);
        }
        let suppressed = std::mem::take(&mut self.suppressed);
        self.last_logged = Some((kind, now));
        self.backoff_ms = (self.backoff_ms.saturating_mul(2))
            .clamp(ACCEPT_BACKOFF_START_MS, ACCEPT_BACKOFF_MAX_MS);
        (true, suppressed)
    }

    /// Ok 臂复位：退避回起点、限速记忆清空、抑制计数清零。
    fn reset_ok(&mut self) {
        self.backoff_ms = ACCEPT_BACKOFF_START_MS;
        self.last_logged = None;
        self.suppressed = 0;
    }
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
            .and_then(|p| {
                crate::ipc::runtime_config::load_market_plugin_from(std::path::Path::new(p))
            })
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
                    crate::ipc::runtime_config::load_market_plugin_from(std::path::Path::new(p))
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
            .map(|p| crate::market::compat::PluginModes::for_plugin(p.as_ref()))
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

/// The DEFAULT-account resolution chain for every wire verb that needs an
/// account when the request does not NAME one (issue 561, operator ruling
/// 2026-10-20): session → display → process default. The display pointer is
/// the panel's single trusted account source, so a fresh connection (no
/// session cell — the dispatcher's 3s reconnect wipes it) follows the panel's
/// display account instead of silently falling back to the process default
/// while the header keeps showing the display account. A session cell that
/// IS set (a CLI / gate explicitly ran `account.switch` on this connection)
/// still wins — that is the backward-compatible layer.
///
/// `ledgers` supplies the fallbacks; the display pointer's invariant
/// (always names a configured account) makes the tail unreachable only in
/// pathological states, hence the expect with a loud message.
fn effective_default_account(
    session: &SessionState,
    ledgers: &crate::account::AccountLedgers,
) -> AccountId {
    if let Some(id) = &session.session_active {
        return id.clone();
    }
    let display = AccountId::from(ledgers.display_id());
    if ledgers.account(&display).is_some() {
        return display;
    }
    ledgers.active_id().clone()
}

#[allow(clippy::too_many_arguments)] // one shared-context param per subsystem arm
async fn handle_line(
    core: &Arc<AsyncMutex<Core>>,
    registry: &crate::market::registry::MarketPluginRegistry,
    line: String,
    peer: &PeerAuth,
    update_state: &Arc<crate::ipc::version::UpdateState>,
    jobs: &Arc<crate::backtest_jobs::BacktestJobs>,
    host: &std::sync::Arc<dyn blitzkrieg_market_api::MarketHost>,
    data_plane: &DataPlaneRuntime,
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

    // #460: kernel-side defense in depth. A `Mode::ReadOnly` core serves
    // observations only — until now the mode only gated venue egress, which
    // left every CONFIGURATION verb open to any local process that can reach
    // the socket. The panel now gates its own proxy routes, but the panel is
    // one client; the kernel is the boundary. One list, checked before any
    // arm: a new write verb must be added HERE, the same discipline as the
    // method table itself.
    const READONLY_REFUSED: &[&str] = &[
        method::RISK_SET_SYSTEMIC,
        method::RISK_SET_EXIT,
        method::EXECUTION_POLICY_SET,
        method::EXECUTION_POLICY_RESET,
        method::BLUEPRINT_SAVE,
        method::BLUEPRINT_SAVE_SOURCE,
        method::SYSTEM_UPDATE_CONFIGURE,
        method::SYSTEM_ENV_RELOAD,
        method::ROUND_DURATION_SET,
        method::MARKET_PLUGIN_SET,
        method::ACCOUNT_VENUE_SET,
        method::ACCOUNT_ROUNDS_SET,
        method::ACCOUNT_CREATE,
        method::ACCOUNT_REMOVE,
        method::DISPLAY_ACCOUNT_SET,
        method::SYSTEM_RESET_TRADING,
        method::STRATEGY_PARAMS_SET,
        method::UNIFIED_CONFIRM,
        method::UNIFIED_REVOKE,
        method::ARB_CONFIG_SET,
    ];
    if READONLY_REFUSED.contains(&method) && core.lock().await.mode() == Mode::ReadOnly {
        return fail(
            id,
            Failure::APPLICATION,
            "this core runs in read-only mode; configuration verbs are refused \
             (start with a writable mode to change risk, policy or strategies)"
                .to_string(),
            None,
        );
    }

    let reply: Result<Value, (i32, String, Option<ErrorData>)> = match method {
        method::PING => {
            handlers::misc::handle_misc(method, core, registry, peer, data_plane, params).await
        }
        method::READY => {
            handlers::misc::handle_misc(method, core, registry, peer, data_plane, params).await
        }
        method::SYSTEM_VERSION => {
            handlers::system::handle_system(method, core, peer, update_state, session, params).await
        }
        method::SYSTEM_ENV_RELOAD => {
            handlers::system::handle_system(method, core, peer, update_state, session, params).await
        }
        method::RISK_LIMITS => handlers::risk::handle_risk(method, core, peer, params).await,
        method::SYSTEM_UPDATE_CONFIGURE => {
            handlers::system::handle_system(method, core, peer, update_state, session, params).await
        }
        method::SYSTEM_UPDATE_CHECK => {
            handlers::system::handle_system(method, core, peer, update_state, session, params).await
        }
        method::SYSTEM_UPDATE_STAGE => {
            handlers::system::handle_system(method, core, peer, update_state, session, params).await
        }
        method::CORE_FEE_QUOTE => {
            handlers::misc::handle_misc(method, core, registry, peer, data_plane, params).await
        }
        method::RISK_KILL => handlers::risk::handle_risk(method, core, peer, params).await,
        method::RISK_RESUME => handlers::risk::handle_risk(method, core, peer, params).await,
        method::RISK_SET_LIMITS => handlers::risk::handle_risk(method, core, peer, params).await,
        method::RISK_SET_SYSTEMIC => handlers::risk::handle_risk(method, core, peer, params).await,
        method::RISK_SET_EXIT => handlers::risk::handle_risk(method, core, peer, params).await,
        method::ROUND_DURATION_GET => {
            handlers::round::handle_round(method, core, peer, params).await
        }
        method::ROUND_DURATION_SET => {
            handlers::round::handle_round(method, core, peer, params).await
        }
        method::MARKET_PLUGIN_GET => {
            handlers::market::handle_market(method, core, registry, peer, params).await
        }
        method::MARKET_PLUGIN_SET => {
            handlers::market::handle_market(method, core, registry, peer, params).await
        }
        method::MARKET_CREDENTIALS_STATUS => {
            handlers::market::handle_market(method, core, registry, peer, params).await
        }
        method::MARKET_CREDENTIALS_SET => {
            handlers::market::handle_market(method, core, registry, peer, params).await
        }
        method::SYSTEM_RESET_TRADING => {
            handlers::system::handle_system(method, core, peer, update_state, session, params).await
        }
        method::ORDER_PLACE => handlers::order::handle_order(method, core, session, params).await,
        method::ORDER_CANCEL => handlers::order::handle_order(method, core, session, params).await,
        method::ORDER_CANCEL_ALL => {
            handlers::order::handle_order(method, core, session, params).await
        }
        method::ORDER_LIST => handlers::order::handle_order(method, core, session, params).await,
        method::ORDER_CANCEL_REMAINING => {
            handlers::order::handle_order(method, core, session, params).await
        }
        method::ORDER_FORCE_CLOSE => {
            handlers::order::handle_order(method, core, session, params).await
        }
        method::ORDER_CLOSE_FILLED => {
            handlers::order::handle_order(method, core, session, params).await
        }
        method::LEDGER_BALANCE => {
            handlers::misc::handle_misc(method, core, registry, peer, data_plane, params).await
        }
        method::ORDER_RECONCILE => {
            handlers::order::handle_order(method, core, session, params).await
        }
        method::POSITIONS_LIST => {
            handlers::position::handle_position(method, core, peer, params).await
        }
        method::TRADES_HISTORY => handlers::trades::handle_trades(method, core, params).await,
        method::TRADES_SUMMARY => handlers::trades::handle_trades(method, core, params).await,
        method::POSITION_EXIT => {
            handlers::position::handle_position(method, core, peer, params).await
        }
        method::POSITION_FORCE_CLOSE => {
            handlers::position::handle_position(method, core, peer, params).await
        }
        method::BOOK_SNAPSHOT => {
            handlers::misc::handle_misc(method, core, registry, peer, data_plane, params).await
        }
        method::ENGINE_BOOK => {
            handlers::engine::handle_engine(method, core, registry, params).await
        }
        method::TOP_OF_BOOK => {
            handlers::misc::handle_misc(method, core, registry, peer, data_plane, params).await
        }
        method::SPOT_PRICE => {
            handlers::misc::handle_misc(method, core, registry, peer, data_plane, params).await
        }
        method::ENGINE_STATS => {
            handlers::engine::handle_engine(method, core, registry, params).await
        }
        method::STRATEGY_LIST => {
            handlers::strategy::handle_strategy(method, core, peer, params).await
        }
        method::STRATEGY_ENABLE => {
            handlers::strategy::handle_strategy(method, core, peer, params).await
        }
        method::STRATEGY_LOAD => {
            handlers::strategy::handle_strategy(method, core, peer, params).await
        }
        method::STRATEGY_UNLOAD => {
            handlers::strategy::handle_strategy(method, core, peer, params).await
        }
        method::STRATEGY_RELOAD => {
            handlers::strategy::handle_strategy(method, core, peer, params).await
        }
        method::STRATEGY_PARAMS_GET => {
            handlers::strategy::handle_strategy(method, core, peer, params).await
        }
        method::STRATEGY_PARAMS_SET => {
            handlers::strategy::handle_strategy(method, core, peer, params).await
        }
        method::UNIFIED_LIST => handlers::unified::handle_unified(method, core, params).await,
        method::UNIFIED_CANDIDATES => handlers::unified::handle_unified(method, core, params).await,
        method::UNIFIED_CONFIRM => handlers::unified::handle_unified(method, core, params).await,
        method::UNIFIED_REVOKE => handlers::unified::handle_unified(method, core, params).await,
        method::ARB_CONFIG_SET => handlers::arb::handle_arb(method, core, params).await,
        method::ARB_CONFIG_GET => handlers::arb::handle_arb(method, core, params).await,
        method::EXTENSION_LIST => handlers::extension::handle_extension(method, core, params).await,
        method::MARKET_LIST => {
            handlers::market::handle_market(method, core, registry, peer, params).await
        }
        method::NET_CHECK => {
            handlers::misc::handle_misc(method, core, registry, peer, data_plane, params).await
        }
        method::EXTENSION_ENABLE => {
            handlers::extension::handle_extension(method, core, params).await
        }
        method::EXTENSION_DISABLE => {
            handlers::extension::handle_extension(method, core, params).await
        }
        method::SE_ENABLE => handlers::se::handle_se(method, core, params).await,
        method::SE_DISABLE => handlers::se::handle_se(method, core, params).await,
        method::SE_STATUS => handlers::se::handle_se(method, core, params).await,
        method::SE_HISTORY => handlers::se::handle_se(method, core, params).await,
        method::SE_ROLLBACK => handlers::se::handle_se(method, core, params).await,
        method::SE_APPLY => handlers::se::handle_se(method, core, params).await,
        method::SE_PROPOSALS => handlers::se::handle_se(method, core, params).await,
        method::SE_DECIDE => handlers::se::handle_se(method, core, params).await,
        method::SE_SET_AUTO => handlers::se::handle_se(method, core, params).await,
        method::ENGINE_ROUND => {
            handlers::engine::handle_engine(method, core, registry, params).await
        }
        method::ENGINE_BOOKS => {
            handlers::engine::handle_engine(method, core, registry, params).await
        }
        method::ENGINE_MARKETS => {
            handlers::engine::handle_engine(method, core, registry, params).await
        }
        method::KLINE_HISTORY => handlers::kline::handle_kline(method, core, session, params).await,
        method::KLINE_SUBSCRIBE => {
            handlers::kline::handle_kline(method, core, session, params).await
        }
        method::KLINE_UNSUBSCRIBE => {
            handlers::kline::handle_kline(method, core, session, params).await
        }
        method::INTENT_AUDIT_TAIL => handlers::intent::handle_intent(method, params).await,
        method::ACCOUNT_LIST => {
            handlers::account::handle_account(
                method, core, registry, peer, host, data_plane, session, params,
            )
            .await
        }
        method::ACCOUNT_SWITCH => {
            handlers::account::handle_account(
                method, core, registry, peer, host, data_plane, session, params,
            )
            .await
        }
        method::ACCOUNT_STATUS => {
            handlers::account::handle_account(
                method, core, registry, peer, host, data_plane, session, params,
            )
            .await
        }
        method::ACCOUNT_VENUE_GET => {
            handlers::account::handle_account(
                method, core, registry, peer, host, data_plane, session, params,
            )
            .await
        }
        method::ACCOUNT_VENUE_SET => {
            handlers::account::handle_account(
                method, core, registry, peer, host, data_plane, session, params,
            )
            .await
        }
        method::ACCOUNT_ROUNDS_SET => {
            handlers::account::handle_account(
                method, core, registry, peer, host, data_plane, session, params,
            )
            .await
        }
        method::ACCOUNT_TRADING_SET => {
            handlers::account::handle_account(
                method, core, registry, peer, host, data_plane, session, params,
            )
            .await
        }
        method::ACCOUNT_FREEZE => {
            handlers::account::handle_account(
                method, core, registry, peer, host, data_plane, session, params,
            )
            .await
        }
        method::ACCOUNT_STRATEGIES_SET => {
            handlers::account::handle_account(
                method, core, registry, peer, host, data_plane, session, params,
            )
            .await
        }
        method::DISPLAY_ACCOUNT_SET => {
            handlers::account::handle_account(
                method, core, registry, peer, host, data_plane, session, params,
            )
            .await
        }
        method::ACCOUNT_CREATE => {
            handlers::account::handle_account(
                method, core, registry, peer, host, data_plane, session, params,
            )
            .await
        }
        method::ACCOUNT_REMOVE => {
            handlers::account::handle_account(
                method, core, registry, peer, host, data_plane, session, params,
            )
            .await
        }
        method::ACCOUNT_JOURNAL => {
            handlers::account::handle_account(
                method, core, registry, peer, host, data_plane, session, params,
            )
            .await
        }
        method::EXECUTION_POLICY_LIST => {
            handlers::execution_policy::handle_execution_policy(method, core, params).await
        }
        method::EXECUTION_POLICY_GET => {
            handlers::execution_policy::handle_execution_policy(method, core, params).await
        }
        method::EXECUTION_POLICY_SET => {
            handlers::execution_policy::handle_execution_policy(method, core, params).await
        }
        method::EXECUTION_POLICY_RESET => {
            handlers::execution_policy::handle_execution_policy(method, core, params).await
        }
        method::EXECUTION_POLICY_HISTORY => {
            handlers::execution_policy::handle_execution_policy(method, core, params).await
        }
        method::EXECUTION_POLICY_PREVIEW => {
            handlers::execution_policy::handle_execution_policy(method, core, params).await
        }
        method::BACKTEST_ONCHAIN_PULL => {
            handlers::backtest::handle_backtest(method, core, jobs, params).await
        }
        method::BACKTEST_ONCHAIN_LIST => {
            handlers::backtest::handle_backtest(method, core, jobs, params).await
        }
        method::BACKTEST_RUN => {
            handlers::backtest::handle_backtest(method, core, jobs, params).await
        }
        method::BACKTEST_STATUS => {
            handlers::backtest::handle_backtest(method, core, jobs, params).await
        }
        method::BACKTEST_RESULT => {
            handlers::backtest::handle_backtest(method, core, jobs, params).await
        }
        method::BACKTEST_EXPORT => {
            handlers::backtest::handle_backtest(method, core, jobs, params).await
        }
        method::BLUEPRINT_COMPILE => {
            handlers::blueprint::handle_blueprint(method, core, params).await
        }
        method::BLUEPRINT_SAVE => handlers::blueprint::handle_blueprint(method, core, params).await,
        method::BLUEPRINT_LOAD => handlers::blueprint::handle_blueprint(method, core, params).await,
        method::BLUEPRINT_LOAD_SOURCE => {
            handlers::blueprint::handle_blueprint(method, core, params).await
        }
        method::BLUEPRINT_SAVE_SOURCE => {
            handlers::blueprint::handle_blueprint(method, core, params).await
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

/// The verb-family handlers (issue 699): one module per method-name family,
/// each exposing one `handle_<family>` the dispatch routes to. The dispatch
/// ORDER above is still the one list — a new verb is added here first, then
/// implemented in its family module. Pure code motion: every arm's bytes
/// (response bodies, error codes, audit lines) are exactly the pre-split
/// ones, and the READONLY / version gates stay in [`handle_line`].
mod handlers;

#[cfg(test)]
mod tests;

#![cfg(test)]

use super::*;

/// A named no-op plugin for registry-driven wire tests: the bindable /
/// switchable set a test exercises is whatever IT registered, never the
/// compiled-in feature set (which varies by build flags).
struct VenueStubPlugin {
    name: String,
}
impl blitzkrieg_market_api::MarketPlugin for VenueStubPlugin {
    fn name(&self) -> &str {
        &self.name
    }
    fn market_type(&self) -> blitzkrieg_market_api::MarketType {
        blitzkrieg_market_api::MarketType::Prediction
    }
}

/// The runtime-enablement stub: a plugin whose feed and discovery really
/// START and really REACH THE CORE — the feed installs a subscription
/// control keyed under the plugin name (what `subscribe_feed_tokens`
/// routes through) and the discovery registers a round market tagged with
/// the plugin's venue (what ENGINE_ROUND reads back). A test that pins
/// 「venue.set 后新 venue 的市场出现在 scanner」 asserts against these two
/// observable effects, not against a log line.
struct VenueLivePlugin {
    name: String,
    discovery: VenueLiveDiscovery,
}
impl blitzkrieg_market_api::MarketPlugin for VenueLivePlugin {
    fn name(&self) -> &str {
        &self.name
    }
    fn market_type(&self) -> blitzkrieg_market_api::MarketType {
        blitzkrieg_market_api::MarketType::Prediction
    }
    fn data_feed(&self) -> Option<&dyn blitzkrieg_market_api::DataFeed> {
        Some(&VenueLiveFeed)
    }
    fn discovery(&self) -> Option<&dyn blitzkrieg_market_api::MarketDiscovery> {
        Some(&self.discovery)
    }
}

/// Installs a subscription control whose `set_tokens` records the LAST set
/// — the wire test reads it back to prove the mount happened.
struct VenueLiveFeed;
impl blitzkrieg_market_api::DataFeed for VenueLiveFeed {
    fn name(&self) -> &str {
        "venue-live-feed"
    }
    fn start<'a>(
        &'a self,
        host: std::sync::Arc<dyn blitzkrieg_market_api::MarketHost>,
        _config: blitzkrieg_market_api::DataFeedConfig,
        initial_tokens: Vec<blitzkrieg_market_api::TokenId>,
    ) -> blitzkrieg_market_api::BoxFuture<'a, Result<(), blitzkrieg_market_api::CoreError>> {
        Box::pin(async move {
            host.install_subscription_control(
                "venuelive",
                std::sync::Arc::new(VenueLiveControl {
                    tokens: std::sync::Mutex::new(initial_tokens),
                }),
            )
            .await;
            Ok(())
        })
    }
}

struct VenueLiveControl {
    tokens: std::sync::Mutex<Vec<String>>,
}
impl blitzkrieg_market_api::SubscriptionControl for VenueLiveControl {
    fn set_tokens(&self, tokens: Vec<blitzkrieg_market_api::TokenId>) {
        *self.tokens.lock().expect("venue live control poisoned") = tokens;
    }
}

/// Every `start` the countable feed has received, keyed by the OWNER name
/// the test registered it under (a process-global log — the same shape as
/// `VENUE_LIVE_DISCOVERY_CFG`; parallel tests filter their own slice).
/// The reuse pipeline's「不重订」observable: a source's feed start count
/// stays 1 across consumer re-binds, unbinds and re-binds.
static COUNTED_FEED_STARTS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// A feed stub that LOGS each start under `owner` and installs the
/// subscription control under the SOURCE venue name the test picks — the
/// consumer-only mount path starts the SOURCE plugin's feed, so the
/// control must land under the source's key for the reuse fan-out's
/// token subscriptions to route.
struct CountedFeed {
    owner: String,
    control_key: String,
}
impl blitzkrieg_market_api::DataFeed for CountedFeed {
    fn name(&self) -> &str {
        "counted-feed"
    }
    fn start<'a>(
        &'a self,
        host: std::sync::Arc<dyn blitzkrieg_market_api::MarketHost>,
        _config: blitzkrieg_market_api::DataFeedConfig,
        initial_tokens: Vec<blitzkrieg_market_api::TokenId>,
    ) -> blitzkrieg_market_api::BoxFuture<'a, Result<(), blitzkrieg_market_api::CoreError>> {
        Box::pin(async move {
            COUNTED_FEED_STARTS
                .lock()
                .expect("counted feed starts poisoned")
                .push(self.owner.clone());
            host.install_subscription_control(
                self.control_key.as_str(),
                std::sync::Arc::new(VenueLiveControl {
                    tokens: std::sync::Mutex::new(initial_tokens),
                }),
            )
            .await;
            Ok(())
        })
    }
}

/// On start, registers one round market tagged with the plugin's venue —
/// exactly what a real discovery does at its first poll. The received
/// config is recorded so tests can pin the boot-shape contract: a runtime
/// mount must hand the plugin the boot's assets/round length, not zeroes
/// (kalshi selects its series from exactly those two fields — #535).
/// The tag carries the OWNING plugin's name: parallel tests register
/// their own plugin instances, and each reads only its own log slice
/// (a process-global capture without the tag raced two mount tests).
struct VenueLiveDiscovery {
    tag: String,
}
/// The venue tag the stub discovery stamps its round market with. The
/// reuse tests register the SAME discovery instance type under a source
/// venue name — the batch must carry the SOURCE's tag (the host's
/// fan-out reads `markets.first().venue` to resolve whose upstream this
/// is), so the tag is the owning plugin's name, parametrizable. Default
/// keeps every existing mount test's `venuelive` spelling.
fn venue_live_discovery_batch_venue(tag: &str) -> String {
    tag.to_string()
}
/// Every (plugin-name, DiscoveryConfig) the start call has received, in
/// order (B1d family mounts start ONE discovery per (venue, duration);
/// readers filter by tag then walk their slice).
static VENUE_LIVE_DISCOVERY_CFG: std::sync::Mutex<
    Vec<(String, blitzkrieg_market_api::DiscoveryConfig)>,
> = std::sync::Mutex::new(Vec::new());
impl blitzkrieg_market_api::MarketDiscovery for VenueLiveDiscovery {
    fn name(&self) -> &str {
        "venue-live-discovery"
    }
    fn start<'a>(
        &'a self,
        host: std::sync::Arc<dyn blitzkrieg_market_api::MarketHost>,
        config: blitzkrieg_market_api::DiscoveryConfig,
    ) -> blitzkrieg_market_api::BoxFuture<'a, Result<(), blitzkrieg_market_api::CoreError>> {
        Box::pin(async move {
            VENUE_LIVE_DISCOVERY_CFG
                .lock()
                .expect("venue live discovery cfg poisoned")
                .push((self.tag.clone(), config));
            use rust_decimal_macros::dec;
            host.on_round_markets(vec![blitzkrieg_market_api::MarketDescriptor {
                asset: "BTC".to_string(),
                condition_id: "live-cond".to_string(),
                question_id: "live-q".to_string(),
                up_token_id: "live-up".to_string(),
                down_token_id: "live-down".to_string(),
                up_price: dec!(0.55),
                down_price: dec!(0.45),
                expires_at_ms: now_ms() + 900_000,
                round_slot: now_ms() / 1000 / 900,
                // The batch carries the OWNING plugin's venue tag (the
                // host resolves whose upstream a batch is from it). Every
                // existing mount test names its plugin `venuelive`, so
                // the wire shape there is unchanged.
                venue: venue_live_discovery_batch_venue(&self.tag),
                neg_risk: false,
                question: "runtime mount market".to_string(),
            }])
            .await;
            Ok(())
        })
    }
}

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

/// One request/response over the same entry point a session uses.
///
/// The test data plane mounts NOTHING at boot (empty actives, flags off)
/// and the test host is a `CoreHost` over the same core — the venue.set
/// arm's runtime mount fires against them exactly as a session's would.
async fn rpc(
    core: &Arc<AsyncMutex<Core>>,
    registry: &crate::market::registry::MarketPluginRegistry,
    peer: &PeerAuth,
    line: String,
) -> Value {
    let update_state = Arc::new(crate::ipc::version::UpdateState::new(false, false));
    let jobs = Arc::new(crate::backtest_jobs::BacktestJobs::new());
    let host: Arc<dyn blitzkrieg_market_api::MarketHost> =
        Arc::new(crate::market::host::CoreHost::new(core.clone()));
    let data_plane = DataPlaneRuntime {
        feed_ws_enabled: false,
        discovery_enabled: false,
        discovery_assets: Vec::new(),
        discovery_round_duration_sec: 0,
        discovery_mounted: std::sync::Arc::new(std::sync::Mutex::new(Default::default())),
        soft_unloaded_polling_instances: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(
            0,
        )),
        feed_reuse: None,
    };
    let mut session = SessionState::default();
    serde_json::from_str(
        &handle_line(
            core,
            registry,
            line,
            peer,
            &update_state,
            &jobs,
            &host,
            &data_plane,
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
    let host: Arc<dyn blitzkrieg_market_api::MarketHost> =
        Arc::new(crate::market::host::CoreHost::new(core.clone()));
    let data_plane = DataPlaneRuntime {
        feed_ws_enabled: false,
        discovery_enabled: false,
        discovery_assets: Vec::new(),
        discovery_round_duration_sec: 0,
        discovery_mounted: std::sync::Arc::new(std::sync::Mutex::new(Default::default())),
        soft_unloaded_polling_instances: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(
            0,
        )),
        feed_reuse: None,
    };
    serde_json::from_str(
        &handle_line(
            core,
            registry,
            line,
            peer,
            &update_state,
            &jobs,
            &host,
            &data_plane,
            session,
        )
        .await,
    )
    .expect("every reply is one JSON object")
}

/// The venue.set arm's runtime mount needs a data plane whose MOUNT FLAGS
/// ARE ON (the default [`rpc`] fixture runs flags-off: it pins the
/// no-op-inert path). This variant hands in the caller's data plane and
/// host so a test can assemble the boot shape it pins — flags on, boot
/// actives pre-marked — and then assert against the SAME cell the arm
/// mutates (the mounted set), the same way `rpc_with_subs` holds the
/// session's kline cell.
#[allow(clippy::too_many_arguments)]
async fn rpc_with_data_plane(
    core: &Arc<AsyncMutex<Core>>,
    registry: &crate::market::registry::MarketPluginRegistry,
    peer: &PeerAuth,
    host: &std::sync::Arc<dyn blitzkrieg_market_api::MarketHost>,
    data_plane: &DataPlaneRuntime,
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
            host,
            data_plane,
            &mut session,
        )
        .await,
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
        // The historical kernel whitelist, spelled as the plugins'
        // declared union (the registry union this build would report):
        // the round-duration arms below judge against it.
        allowed_round_secs: crate::account::config::TEST_ALLOWED_ROUNDS.to_vec(),
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
    let host: Arc<dyn blitzkrieg_market_api::MarketHost> =
        Arc::new(crate::market::host::CoreHost::new(core.clone()));
    let data_plane = DataPlaneRuntime {
        feed_ws_enabled: false,
        discovery_enabled: false,
        discovery_assets: Vec::new(),
        discovery_round_duration_sec: 0,
        discovery_mounted: std::sync::Arc::new(std::sync::Mutex::new(Default::default())),
        soft_unloaded_polling_instances: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(
            0,
        )),
        feed_reuse: None,
    };
    serde_json::from_str(
        &handle_line(
            core,
            registry,
            line,
            peer,
            update_state,
            &jobs,
            &host,
            &data_plane,
            session,
        )
        .await,
    )
    .expect("every reply is one JSON object")
}

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
                    venue: String::new(),
                    trading_enabled: true,
                    strategies: Vec::new(),
                    rounds: Vec::new(),
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
                venue: String::new(),
                trading_enabled: true,
                strategies: Vec::new(),
                rounds: Vec::new(),
                updated_at_ms: now,
            },
            crate::account::Account {
                id: AccountId::from("paper"),
                name: "paper".into(),
                market_type: blitzkrieg_market_api::MarketType::Prediction,
                status: AccountStatus::Active,
                credential_keys: crate::account::CredentialKeys::default(),
                venue: String::new(),
                trading_enabled: true,
                strategies: Vec::new(),
                rounds: Vec::new(),
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

mod tests_accept_err_state;
mod tests_account_books;
mod tests_account_verbs;
mod tests_backtest;
mod tests_blueprint;
mod tests_execution_policy;
mod tests_feed_reuse;
mod tests_hot_reload;
mod tests_read_envelopes;
mod tests_risk_rounds;
mod tests_snapshot_dedupe;
mod tests_update_verbs;
mod tests_venue_binding;

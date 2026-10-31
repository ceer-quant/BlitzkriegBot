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

mod data_plane;
mod session;
use data_plane::{DataPlaneRuntime, enable_venue_at_runtime};
pub use session::run;

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

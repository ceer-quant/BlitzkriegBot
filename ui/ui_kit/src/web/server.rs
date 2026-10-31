use super::*;

/// First candidate whose canonical target is an existing `index.html` file;
/// returns its parent dir.
pub(super) fn resolve_panel_dir(candidates: &[std::path::PathBuf]) -> Option<std::path::PathBuf> {
    for c in candidates {
        if let Ok(dir) = c.canonicalize() {
            if dir.is_file() {
                return dir.parent().map(std::path::Path::to_path_buf);
            }
        }
    }
    None
}

/// Repo-rooted path to the built Vue panel's `index.html`/assets. Relative to
/// the ui_kit crate so it holds for both `cargo run` and a repo checkout. A
/// binary built in one checkout and run from another (the `blitzkrieg`
/// launcher staged into `<repo>/target/release`) has a dead manifest path, so
/// also try two levels up from the executable — that repo shape puts the
/// panel at `<repo>/ui/webapp/webui/dist`.
///
/// issue 487: a release archive unpacked flat (`bin/` next to `webui/`) must
/// serve the bundled panel with NO repo around it, so the exe-sibling `webui/`
/// is the FIRST candidate — that layout is what a "傻白甜启动" user has, and
/// any repo checkout keeps working through the later candidates.
pub(super) fn vue_panel_dir() -> Option<std::path::PathBuf> {
    // CARGO_MANIFEST_DIR is <build>/ui/ui_kit; the built Vue app lives at
    // <build>/ui/webapp/webui/dist.
    let manifest = env!("CARGO_MANIFEST_DIR");
    let mut candidates =
        vec![std::path::Path::new(manifest).join("../webapp/webui/dist/index.html")];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            // Release-archive shape: <stage>/bin/blitzkrieg-core + <stage>/webui/.
            candidates.push(dir.join("../webui/index.html"));
            // Repo checkout shape: <repo>/target/release/<bin>.
            candidates.push(dir.join("../../ui/webapp/webui/dist/index.html"));
        }
    }
    resolve_panel_dir(&candidates)
}

/// Serve a static asset from the Vue panel dir; `/panel/` or `/panel` (no
/// trailing file) resolves to `index.html`. System path traversal is blocked.
pub(super) fn serve_vue_panel_asset(path: &str) -> Option<(Vec<u8>, &'static str)> {
    let rel = path.strip_prefix("/panel/")?;
    if rel.split('/').any(|seg| seg == ".." || seg.is_empty()) {
        return None;
    }
    serve_vue_panel_file(&std::path::PathBuf::from(rel))
}

pub(super) fn serve_vue_panel() -> Option<(Vec<u8>, &'static str)> {
    serve_vue_panel_file(&std::path::PathBuf::from("index.html"))
}

pub(super) fn serve_vue_panel_file(rel: &std::path::Path) -> Option<(Vec<u8>, &'static str)> {
    let dir = vue_panel_dir()?;
    let full = dir.join(rel).canonicalize().ok()?;
    // Canonical path must stay inside the panel dir.
    if !full.starts_with(&dir) {
        return None;
    }
    let body = std::fs::read(&full).ok()?;
    let ctype = match full.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("json") => "application/json",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    };
    // Served as raw bytes: the panel ships PNG icons alongside its text assets,
    // and a lossy UTF-8 round-trip would mangle every byte outside ASCII (and
    // skew the Content-Length with the replacement characters it inserts).
    Some((body, ctype))
}

/// The address this panel actually bound, recorded by [`WebServer::serve`].
#[derive(Debug, Clone)]
pub(super) struct BindInfo {
    pub(super) addr: String,
    /// Bound to a loopback interface only.
    pub(super) loopback_only: bool,
}

/// A trivial, dependency-free HTTP server that always renders a fresh snapshot.
pub struct WebServer {
    pub(super) snapshot_src: Arc<Mutex<IpcClient>>,
    pub(super) trade_limit: usize,
    /// Present only in gateway mode (`--manage`); enables `/api/command`.
    pub(super) dispatcher: Option<Arc<Mutex<Dispatcher>>>,
    /// The last network self-check, and whether one is running now.
    pub(super) net_check: Arc<Mutex<NetCheckCache>>,
    /// The in-flight guard for the update-check button (§6.3).
    pub(super) update_check: Arc<Mutex<UpdateCheckCache>>,
    /// Panel credentials (`BLITZKRIEG_PANEL_USER` / `…_PASSWORD`), read from the
    /// environment. Gateway mode refuses to start without a complete pair (see
    /// [`Self::require_credentials`]) — it never invents one.
    pub(super) panel_user: Option<String>,
    pub(super) panel_password: Option<String>,
    /// Whether `/api/*` demands a valid session. Armed by gateway mode.
    pub(super) auth_required: bool,
    /// Non-loopback origins explicitly trusted (operator opt-in).
    pub(super) allowed_origins: Vec<String>,
    /// Sessions issued by `POST /api/login`, keyed by token.
    pub(super) sessions: Arc<Mutex<std::collections::BTreeMap<String, Session>>>,
    /// Failed-login state, keyed by client address (see [`LoginAttempt`]).
    pub(super) login_failures: Arc<Mutex<std::collections::BTreeMap<String, LoginAttempt>>>,
    /// The address `serve()` bound, recorded so the panel can tell the operator
    /// when it is reachable beyond this machine (#185). `None` before binding.
    pub(super) bind: Arc<Mutex<Option<BindInfo>>>,
}

impl WebServer {
    /// Every field the struct has, in one place. Only two differ between the
    /// read-only panel and a gateway — `dispatcher` (whether the command API
    /// exists) and `auth_required` (whether a session is demanded) — so those are
    /// the two the callers pass. A field added here reaches every surface, which
    /// is the point: three separate literals is how one of them ends up missing
    /// it.
    pub(super) fn surface(
        client: IpcClient,
        trade_limit: usize,
        dispatcher: Option<Arc<Mutex<Dispatcher>>>,
        auth_required: bool,
    ) -> Self {
        Self {
            snapshot_src: Arc::new(Mutex::new(client)),
            trade_limit,
            dispatcher,
            net_check: Arc::new(Mutex::new(NetCheckCache::default())),
            update_check: Arc::new(Mutex::new(UpdateCheckCache::default())),
            panel_user: None,
            panel_password: None,
            auth_required,
            allowed_origins: Vec::new(),
            sessions: Arc::new(Mutex::new(std::collections::BTreeMap::new())),
            login_failures: Arc::new(Mutex::new(std::collections::BTreeMap::new())),
            bind: Arc::new(Mutex::new(None)),
        }
    }

    /// Read-only panel (no command API).
    pub fn new(client: IpcClient, trade_limit: usize) -> Self {
        // No lifecycle verbs on this surface, so auth stays opt-in here.
        // Gateway mode (below) arms it unconditionally.
        Self::surface(client, trade_limit, None, false)
    }

    /// Panel + command API. Lifecycle verbs are gated by the dispatcher's own
    /// `lifecycle_enabled` flag.
    pub fn with_gateway(client: IpcClient, trade_limit: usize, dispatcher: Dispatcher) -> Self {
        Self::with_shared_gateway(client, trade_limit, Arc::new(Mutex::new(dispatcher)))
    }

    /// Like [`Self::with_gateway`], but the caller KEEPS a handle to the same
    /// dispatcher.
    ///
    /// This is the form `blitzkrieg run` must use: the supervisor that spawned
    /// the core lives in that shared dispatcher, and `managed` on the wire is
    /// `supervisor.owns()`. Handing the web server a second, freshly built
    /// dispatcher would make it report `managed: false` for a core its own
    /// process spawned — the panel would then claim the core "was started by
    /// another process" and refuse 停止, even though SIGTERM handling and the
    /// core were both ours. One core, one owning dispatcher, two handles.
    pub fn with_shared_gateway(
        client: IpcClient,
        trade_limit: usize,
        dispatcher: std::sync::Arc<std::sync::Mutex<Dispatcher>>,
    ) -> Self {
        // This surface can start and stop the trading process, so it is never
        // exposed without a session.
        Self::surface(client, trade_limit, Some(dispatcher), true)
    }

    /// Serve until the process is stopped. `addr` e.g. `127.0.0.1:51888`.
    pub fn serve(&self, addr: &str) -> std::io::Result<()> {
        let listener = TcpListener::bind(addr)?;
        // Classify the address that was ACTUALLY bound, not the string we were
        // given: `localhost` can resolve either way, and a hostname says nothing
        // about which interface the kernel chose.
        let local = listener.local_addr().ok();
        let bound = local
            .map(|a| a.to_string())
            .unwrap_or_else(|| addr.to_string());
        let loopback_only = local.map(|a| a.ip().is_loopback()).unwrap_or(true);
        if let Ok(mut b) = self.bind.lock() {
            *b = Some(BindInfo {
                addr: bound.clone(),
                loopback_only,
            });
        }
        let console = self.dispatcher.is_some();
        println!(
            "ui_kit web adapter listening on http://{bound}/  (panel) and /api/snapshot (JSON)"
        );
        // #185: a non-loopback bind is an explicit operator choice, but it is the
        // difference between "this panel is mine" and "anyone on the network can
        // reach the process controls", so it is never allowed to be quiet.
        if !loopback_only {
            println!("{}", self.bind_warning().unwrap_or_default());
        }
        if console {
            println!(
                "  gateway: /api/command (GET ?cmd=… or POST body){}",
                if self.lifecycle_enabled() {
                    " · lifecycle ENABLED"
                } else {
                    " · read-only (pass --manage for start/stop)"
                }
            );
        }
        for stream in listener.incoming() {
            match stream {
                Ok(s) => self.handle(s),
                Err(e) => eprintln!("accept: {e}"),
            }
        }
        Ok(())
    }

    pub(super) fn lifecycle_enabled(&self) -> bool {
        self.dispatcher
            .as_ref()
            .map(|d| d.lock().map(|d| d.lifecycle_enabled()).unwrap_or(false))
            .unwrap_or(false)
    }

    /// The loud warning shown when this panel is listening beyond loopback, or
    /// `None` while it is local-only.
    ///
    /// Plaintext HTTP is the load-bearing part: the panel password crosses the
    /// wire unencrypted and the panel can start and stop the trading kernel, so
    /// "reachable from the network" and "safe to reach from the network" are not
    /// the same claim. The fix for an operator who needs remote access is a
    /// tunnel (`ssh -L`), a VPN, or a TLS-terminating reverse proxy that is
    /// itself authenticated — never exposing this port directly.
    pub fn bind_warning(&self) -> Option<String> {
        let bind = self.bind.lock().ok().and_then(|b| b.clone())?;
        if bind.loopback_only {
            return None;
        }
        Some(format!(
            "\n\
             ======================================================================\n\
             ⚠  安全提示：面板监听在 {}（非本地地址）\n\
                 1) 面板是明文 HTTP，没有 TLS：登录凭据以明文在网络中传输。\n\
                 2) 面板可以启动/停止交易内核，等于把进程控制权暴露给网络。\n\
                 3) 请勿直接暴露该端口；只放在已认证的隧道/VPN 或带 TLS 的\n\
                    反向代理之后，并把可信来源写入 BLITZKRIEG_ALLOWED_ORIGINS。\n\
                 如需回到仅本机可访问：把 .env 里的 BLITZKRIEG_PANEL_ADDR 设为\n\
                 127.0.0.1:51888（默认值），或启动时传 --addr 127.0.0.1:51888。\n\
                 登录失败已限速：同一来源连续 {} 次失败后锁定（指数退避，最长 {} 分钟）。\n\
             ======================================================================\n",
            bind.addr,
            MAX_LOGIN_FAILURES,
            LOGIN_LOCKOUT_MAX_MS / 60_000
        ))
    }

    /// The address this panel actually bound, once `serve()` has it, as
    /// `host:port`. `None` before the listener exists.
    ///
    /// Exists so a caller that needs the port (tests, an embedder that passed
    /// `:0`) can read it from the listener instead of probing for a free port
    /// and racing whoever takes it in between.
    pub fn bound_addr(&self) -> Option<String> {
        self.bind
            .lock()
            .ok()
            .and_then(|b| b.as_ref().map(|i| i.addr.clone()))
    }

    /// Process-control state for the panel. `None` when this server has no
    /// dispatcher at all (pure read-only adapter).
    ///
    /// Calling this also *observes* the core (E12-c): a panel refresh is the
    /// natural heartbeat, and without one a crashed core would keep being
    /// reported as running because nothing ever asked. With the default restart
    /// policy off this only records the crash; a gateway that enabled a policy
    /// gets a replacement as a side effect of being looked at.
    pub(super) fn lifecycle_view(&self) -> Option<LifecycleView> {
        let mut d = self.dispatcher.as_ref()?.lock().ok()?;
        d.pump();
        let health = d.health();
        Some(LifecycleView {
            enabled: d.lifecycle_enabled(),
            managed: health.managed,
            pid: health.pid,
            socket: d.socket_path().to_string(),
            restarts: health.restarts,
            restart_given_up: health.restart_given_up,
            last_exit: health.last_exit.as_ref().map(LifecycleExit::from),
            give_up: health.alert.as_ref().map(LifecycleGiveUp::from),
        })
    }

    pub(super) fn snapshot(&self) -> UiSnapshot {
        match self.snapshot_src.lock() {
            Ok(mut c) => c.snapshot(self.trade_limit),
            Err(_) => UiSnapshot::default(),
        }
    }

    pub(super) fn run_command(&self, cmd: &str) -> String {
        let Some(d) = &self.dispatcher else {
            return serde_json::json!({
                "ok": false, "action": "error",
                "message": "command API disabled; run ui_kit_web with --manage"
            })
            .to_string();
        };
        match d.lock() {
            Ok(mut d) => serde_json::to_string(&d.dispatch_line(cmd)).unwrap_or_else(|e| {
                format!("{{\"ok\":false,\"action\":\"error\",\"message\":\"serialize: {e}\"}}")
            }),
            Err(_) => "{\"ok\":false,\"action\":\"error\",\"message\":\"dispatcher poisoned\"}"
                .to_string(),
        }
    }
}

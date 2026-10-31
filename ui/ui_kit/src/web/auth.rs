use super::*;

/// Minimal std-only base64 decoder for basic-auth passwords (the only place
/// the web layer needs it). Returns raw bytes; callers validate UTF-8.
pub(super) fn data_encoding_free_base64(s: &str) -> Vec<u8> {
    const TBL: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let mut buf = 0u32;
    let mut bits = 0u32;
    for c in s.bytes() {
        if c == b'=' || c == b'\n' || c == b'\r' {
            continue;
        }
        let Some(v) = TBL.iter().position(|t| *t == c) else {
            return out;
        };
        buf = (buf << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    out
}

/// Byte-equal compare in time independent of both inputs' contents (#460).
///
/// `subtle`'s `ConstantTimeEq` is the audited primitive; the hand-rolled XOR
/// fold this replaced had the right shape for equal lengths but its
/// length-mismatch branch was folklore (`subtle`'s own docs call that pattern
/// out as easy to get subtly wrong). Both call sites feed it the env-configured
/// panel credentials, so the expected value's length is not secret anyway —
/// what matters is that a wrong guess never returns measurably earlier.
pub(super) fn same_time_eq(a: &[u8], b: &[u8]) -> bool {
    use subtle::ConstantTimeEq;
    a.ct_eq(b).into()
}

// ── Panel auth ───────────────────────────────────────────────────────────────
//
// The shape follows freqtrade's REST API — the mature peer for exactly this kind
// of program (a single-operator trading bot with a browser panel):
//
//   * **Auth is required, not optional.** freqtrade documents that every endpoint
//     except `/ping` "returns sensitive info and requires authentication". The
//     same reasoning applies with more force here: this server can start and stop
//     the trading core, so gateway mode always demands a session and refuses to
//     start without credentials from the environment, rather than serving an open
//     command surface — and rather than inventing a password of its own, which
//     the operator could neither rotate nor audit.
//   * **Loopback is not a trust boundary.** "Only reachable from localhost" does
//     not stop a page the operator happens to visit from issuing cross-site
//     requests to 127.0.0.1 — a cross-site `<img>`/`<form>` is a *simple* request
//     and gets no CORS preflight, so `<img src="…/api/command?cmd=stop">` used to
//     reach a lifecycle verb. The session requirement is what closes that; the
//     origin gate is the second layer.
//   * **Sessions expire.** freqtrade pairs a 15-minute access token with a
//     refresh token; a panel session here carries an absolute TTL plus an idle
//     timeout, and the live set is capped.
//
// Deliberate divergences: no JWT and no separate refresh endpoint (a single
// opaque, revocable, in-memory token is simpler and supports real logout, which
// a stateless JWT cannot), and credentials stay in the environment rather than a
// config file (`BLITZKRIEG_*` already owns that surface).

/// Absolute session lifetime.
pub(super) const SESSION_TTL_MS: u64 = 12 * 60 * 60 * 1_000;

/// Idle timeout, enforced inside the absolute lifetime.
pub(super) const SESSION_IDLE_MS: u64 = 30 * 60 * 1_000;

/// Live-session cap; the oldest is evicted past it.
pub(super) const MAX_SESSIONS: usize = 64;

/// Failed logins from one client before it is locked out (#185).
///
/// Ten is the number the audit asked for and a defensible one for a password a
/// human types: it leaves room for honest typos and is far below what an online
/// guessing attack needs.
pub(super) const MAX_LOGIN_FAILURES: u32 = 10;

/// First lockout (after the 10th failure). Doubles per further failure, so a
/// client that keeps going is pushed out to [`LOGIN_LOCKOUT_MAX_MS`].
pub(super) const LOGIN_LOCKOUT_BASE_MS: u64 = 30_000;

/// Ceiling on the lockout: an operator who mistypes must not be locked out for
/// a day, and an attacker must not be handed a denial of service against them.
pub(super) const LOGIN_LOCKOUT_MAX_MS: u64 = 15 * 60 * 1_000;

/// Bounded throttle state — at most this many clients are remembered.
pub(super) const MAX_LOGIN_TRACKED: usize = 512;

/// Ceiling on the per-failure delay below.
pub(super) const LOGIN_FAILURE_DELAY_MAX_MS: u64 = 400;

/// Delay applied before answering a FAILED login: 100 ms doubling to 400 ms.
///
/// This is a speed bump, not the defence — the lockout is. It exists so that a
/// fast guessing loop pays for every attempt even before the counter trips. It is
/// deliberately short and capped: this server answers one connection at a time
/// (see [`WebServer::serve`]), so a long sleep would let an attacker stall the
/// operator's own panel, which is the denial of service the throttle is meant to
/// prevent. Successful logins are never delayed.
pub(super) fn login_failure_delay_ms(failures: u32) -> u64 {
    let shift = failures.saturating_sub(1).min(3);
    (100u64 << shift).min(LOGIN_FAILURE_DELAY_MAX_MS)
}

/// Lockout for a client that has failed `failures` times: 0 below the threshold,
/// then doubling from [`LOGIN_LOCKOUT_BASE_MS`], capped.
pub(super) fn login_lockout_ms(failures: u32) -> u64 {
    if failures < MAX_LOGIN_FAILURES {
        return 0;
    }
    let over = u64::from(failures - MAX_LOGIN_FAILURES).min(20);
    LOGIN_LOCKOUT_BASE_MS
        .saturating_mul(1u64 << over)
        .min(LOGIN_LOCKOUT_MAX_MS)
}

pub(super) fn auth_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Failed-login accounting for one client (an IP address).
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct LoginAttempt {
    pub(super) failures: u32,
    /// Unix ms until which this client is refused without the credentials even
    /// being examined.
    pub(super) locked_until_ms: u64,
    /// When this record was last touched, for bounded-map eviction.
    pub(super) last_ms: u64,
}

/// One issued session. `seen_ms` refreshes on use, which is what makes idleness
/// measurable rather than merely "issued long ago".
#[derive(Debug, Clone, Copy)]
pub(super) struct Session {
    pub(super) issued_ms: u64,
    pub(super) seen_ms: u64,
}

/// `n` bytes of OS entropy as lowercase hex.
///
/// Used for session tokens. These are bearer credentials for process control, so
/// they must come from the OS rather than a timestamp/PID mix — anything
/// predictable is forgeable by an attacker who can guess when the process
/// started. (Panel *passwords* are not generated here at all; they come from the
/// environment. See [`WebServer::require_credentials`].)
pub(super) fn random_hex(n_bytes: usize) -> String {
    use std::io::Read;
    let mut bytes = vec![0u8; n_bytes];
    let ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .is_ok();
    if !ok {
        // No /dev/urandom (very unlikely on unix): still produce a value, so the
        // panel stays LOCKED rather than silently open. Weak entropy is the
        // lesser failure against an unlocked command surface.
        let mut seed = auth_now_ms()
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407)
            ^ (std::process::id() as u64) << 32;
        for b in bytes.iter_mut() {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *b = (seed >> 24) as u8;
        }
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Is this `Origin`/`Referer` value the local machine?
///
/// Accepts any loopback spelling (`127.x.y.z`, `localhost`, `[::1]`) with or
/// without a scheme, port, path or `user@` prefix. Anything else is foreign.
pub(super) fn is_loopback_origin(value: &str) -> bool {
    let v = value.trim().to_ascii_lowercase();
    let rest = v
        .strip_prefix("http://")
        .or_else(|| v.strip_prefix("https://"))
        .unwrap_or(&v);
    let authority = rest.split('/').next().unwrap_or("");
    // `user@host` — the credentials are irrelevant, the host is not.
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let host = if let Some(bracketed) = authority.strip_prefix('[') {
        bracketed.split(']').next().unwrap_or("") // [::1]:51888 → ::1
    } else {
        authority.split(':').next().unwrap_or("")
    };
    if host == "localhost" || host == "::1" {
        return true;
    }
    // Must be a literal `127.a.b.c` — *parsed*, not prefix-matched. A bare
    // `starts_with("127.")` also accepts `127.0.0.1.evil.example`, which is a
    // hostname the attacker controls.
    let octets: Vec<&str> = host.split('.').collect();
    octets.len() == 4
        && octets[0] == "127"
        && octets[1..]
            .iter()
            .all(|o| !o.is_empty() && o.len() <= 3 && o.bytes().all(|b| b.is_ascii_digit()))
}

/// `(host, port)` of a URL-ish origin, the port default-filled by scheme
/// (`http` → 80, `https` → 443) so `http://host` equals `Host: host:80`.
/// Returns `None` when there is no authority at all. Hostnames are
/// lowercased — DNS names are case-insensitive.
pub(super) fn origin_authority(candidate: &str) -> Option<(String, String)> {
    let lower = candidate.trim().to_ascii_lowercase();
    let (scheme, rest) = match lower.split_once("://") {
        Some((s, r)) => (s, r),
        None => ("http", lower.as_str()),
    };
    let authority = rest.split('/').next()?;
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    if authority.is_empty() {
        return None;
    }
    let default_port = if scheme == "https" { "443" } else { "80" };
    if let Some(bracketed) = authority.strip_prefix('[') {
        let end = bracketed.find(']')?;
        let host = format!("[{}]", &bracketed[..end]);
        let port = bracketed[end + 1..]
            .strip_prefix(':')
            .filter(|p| !p.is_empty())
            .unwrap_or(default_port);
        return Some((host, port.to_string()));
    }
    match authority.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => {
            Some((h.to_string(), p.to_string()))
        }
        _ => Some((authority.to_string(), default_port.to_string())),
    }
}

/// Same-origin: the `Origin`/`Referer` authority equals the request's own
/// `Host` header. This is the server-deployment branch of
/// [`WebServer::origin_allowed`] — see its docs for why a hostile page cannot
/// produce this pair.
pub(super) fn is_same_origin(req: &HttpRequest, candidate: &str) -> bool {
    let Some(host) = req.header("host") else {
        return false;
    };
    let Some(referer) = origin_authority(candidate) else {
        return false;
    };
    let Some(ours) = origin_authority(&format!("http://{host}")) else {
        return false;
    };
    referer == ours
}

/// Session token from an `Authorization` header: `Bearer <token>`, or the user
/// half of a `Basic` pair (the form the E6-a panel shipped with).
pub(super) fn bearer_or_basic(value: &str) -> Option<String> {
    if let Some(t) = value.strip_prefix("Bearer ") {
        return Some(t.to_string());
    }
    let b = value.strip_prefix("Basic ")?;
    let raw = data_encoding_free_base64(b);
    std::str::from_utf8(&raw)
        .ok()
        .and_then(|d| d.split(':').next().map(String::from))
}

impl WebServer {
    /// Guarantee the panel has credentials before it serves anything.
    ///
    /// Credentials come from the environment and *only* from the environment.
    /// There is deliberately no minted/generated fallback: a password the panel
    /// invents for itself is one the operator did not choose and cannot manage
    /// (no rotation, no secret store, no audit story), and having the process
    /// print it to a terminal makes the terminal — not the credential store —
    /// the source of truth for who can stop the trading core.
    ///
    /// Returns the reason the panel must not start, when it must not.
    pub fn require_credentials(&self) -> Result<(), String> {
        if !self.auth_required {
            return Ok(()); // read-only surface, no command verbs
        }
        if self.panel_user.is_some() && self.panel_password.is_some() {
            return Ok(());
        }
        Err(
            "gateway mode requires panel credentials, but BLITZKRIEG_PANEL_USER and \
             BLITZKRIEG_PANEL_PASSWORD are not both set. A half-configured pair counts as \
             unconfigured, on purpose: guessing which half was meant is how a panel ends up \
             open. Export both and restart."
                .to_string(),
        )
    }

    /// True when a usable user/password pair is configured.
    pub fn credentials_configured(&self) -> bool {
        self.panel_user.is_some() && self.panel_password.is_some()
    }

    /// True when the panel requires a session on `/api/*`.
    pub fn auth_required(&self) -> bool {
        self.auth_required
    }

    /// Arm user/password auth from env (`BLITZKRIEG_PANEL_USER` +
    /// `BLITZKRIEG_PANEL_PASSWORD`).
    ///
    /// A half-configured pair is treated as "nothing configured" rather than as a
    /// password-less panel: guessing which half the operator meant is how a panel
    /// ends up open. Gateway mode then refuses to start, via
    /// [`Self::require_credentials`], rather than substituting a password of its
    /// own choosing.
    pub fn set_panel_credentials(&mut self, user: Option<String>, password: Option<String>) {
        let user = user.filter(|u| !u.trim().is_empty());
        let password = password.filter(|p| !p.trim().is_empty());
        if user.is_none() || password.is_none() {
            self.panel_user = None;
            self.panel_password = None;
            return;
        }
        self.panel_user = user;
        self.panel_password = password;
    }

    /// Extra origins accepted beyond loopback — the `CORS_origins` analogue in
    /// freqtrade's config. Operators who deliberately front the panel with a
    /// hostname list it here; everything else is refused.
    pub fn set_allowed_origins(&mut self, origins: Vec<String>) {
        self.allowed_origins = origins
            .into_iter()
            .map(|o| o.trim().trim_end_matches('/').to_ascii_lowercase())
            .filter(|o| !o.is_empty())
            .collect();
    }

    /// Is this request's origin trustworthy?
    ///
    /// Three things pass, in order of strength:
    ///
    /// 1. **No evidence** — no `Origin`, no `Referer`. The normal shape of
    ///    curl, of the gate scripts, and of non-browser clients. Refusing it
    ///    would buy nothing against a browser (a cross-origin `POST` always
    ///    carries `Origin`, or the opaque `null`, which none of the passing
    ///    branches accept) while breaking every CLI client.
    /// 2. **Loopback origins** — the panel on the operator's own machine.
    /// 3. **Same origin** — the evidence's authority equals the request's own
    ///    `Host` header. This is what makes the SERVER deployment work with
    ///    zero configuration: a browser pointed at `http://<server-ip>:51888`
    ///    attaches exactly that authority, and a same-origin request is not
    ///    cross-site by definition. A hostile page cannot forge the pair —
    ///    the browser fills `Host` with the server it is actually talking to,
    ///    so an attacker's page at `evil.example` produces
    ///    `Origin: evil.example` + `Host: <ours>`, which does not match.
    ///
    /// Anything else must be listed in `allowed_origins` (reverse proxies that
    /// rewrite the hostname). What actually protects the state-changing
    /// surface is the *session* requirement in [`Self::authorize`]; this gate
    /// is the second layer, and the cross-site `GET` vector (`<img src>` with
    /// no `Origin`, no readable cookie) is stopped by the session demand.
    pub(super) fn origin_allowed(&self, req: &HttpRequest) -> bool {
        let Some(evidence) = req.header("origin").or_else(|| req.header("referer")) else {
            return true;
        };
        // A `Referer` is a full URL; only its origin is compared, which
        // `is_loopback_origin` handles by ignoring scheme, port and path.
        let candidate = evidence.trim().trim_end_matches('/').to_ascii_lowercase();
        if is_loopback_origin(&candidate) {
            return true;
        }
        if is_same_origin(req, &candidate) {
            return true;
        }
        self.allowed_origins
            .iter()
            .any(|a| candidate == *a || candidate.starts_with(&format!("{a}/")))
    }

    /// Issue a session token for valid panel credentials.
    pub(super) fn login(&self, user: &str, password: &str) -> Option<String> {
        let (expected_user, expected_pass) =
            (self.panel_user.as_deref()?, self.panel_password.as_deref()?);
        // Constant-time compare to blunt trivial timing probes. Both halves are
        // always evaluated so a wrong user and a wrong password look alike.
        let user_ok = same_time_eq(user.as_bytes(), expected_user.as_bytes());
        let pass_ok = same_time_eq(password.as_bytes(), expected_pass.as_bytes());
        if !(user_ok && pass_ok) {
            return None;
        }
        let now = auth_now_ms();
        let token = random_hex(20); // 40 hex chars, 160 bits of OS entropy
        if let Ok(mut s) = self.sessions.lock() {
            Self::evict_expired(&mut s, now);
            if s.len() >= MAX_SESSIONS {
                // Oldest-first eviction keeps a runaway login loop from growing
                // the map without bound.
                if let Some(oldest) = s
                    .iter()
                    .min_by_key(|(_, v)| v.seen_ms)
                    .map(|(k, _)| k.clone())
                {
                    s.remove(&oldest);
                }
            }
            s.insert(
                token.clone(),
                Session {
                    issued_ms: now,
                    seen_ms: now,
                },
            );
        }
        Some(token)
    }

    pub(super) fn evict_expired(
        sessions: &mut std::collections::BTreeMap<String, Session>,
        now: u64,
    ) {
        sessions.retain(|_, s| {
            now.saturating_sub(s.issued_ms) < SESSION_TTL_MS
                && now.saturating_sub(s.seen_ms) < SESSION_IDLE_MS
        });
    }

    /// Does this token name a live session? Expired ones are dropped on sight.
    pub(super) fn session_valid(&self, token: &str) -> bool {
        let now = auth_now_ms();
        let Ok(mut s) = self.sessions.lock() else {
            return false;
        };
        Self::evict_expired(&mut s, now);
        match s.get_mut(token) {
            Some(entry) => {
                entry.seen_ms = now;
                true
            }
            None => false,
        }
    }

    /// Revoke a session (logout).
    pub(super) fn logout(&self, token: &str) {
        if let Ok(mut s) = self.sessions.lock() {
            s.remove(token);
        }
    }

    /// The client address a login attempt is charged to.
    ///
    /// The peer address, not the submitted username: charging the username would
    /// let an attacker lock the operator's own account out by guessing it, which
    /// turns a throttle into a denial of service. A connection whose peer address
    /// cannot be read shares one bucket, which is the conservative direction.
    pub(super) fn login_client_key(stream: &TcpStream) -> String {
        stream
            .peer_addr()
            .map(|a| a.ip().to_string())
            .unwrap_or_else(|_| "unknown".to_string())
    }

    /// Seconds this client must wait, when it is locked out.
    pub(super) fn login_lockout_remaining(&self, key: &str) -> Option<u64> {
        let now = auth_now_ms();
        let guard = self.login_failures.lock().ok()?;
        let entry = guard.get(key)?;
        (entry.locked_until_ms > now).then(|| (entry.locked_until_ms - now).div_ceil(1_000))
    }

    /// Count a failed login and (past the threshold) lock the client out.
    ///
    /// Returns the new failure count, which the caller uses for the log line and
    /// the pre-response delay. It is deliberately NOT echoed to the client: the
    /// response must not depend on anything the caller supplied.
    pub(super) fn note_login_failure(&self, key: &str) -> u32 {
        let now = auth_now_ms();
        let Ok(mut guard) = self.login_failures.lock() else {
            return 0;
        };
        // Bounded state: a client table that grows with every probe is its own
        // denial of service.
        if guard.len() >= MAX_LOGIN_TRACKED && !guard.contains_key(key) {
            let mut live: Vec<(String, u64)> =
                guard.iter().map(|(k, v)| (k.clone(), v.last_ms)).collect();
            live.sort_by_key(|(_, t)| *t);
            for (k, _) in live.into_iter().take(guard.len() / 4 + 1) {
                guard.remove(&k);
            }
        }
        let entry = guard.entry(key.to_string()).or_default();
        entry.failures = entry.failures.saturating_add(1);
        entry.last_ms = now;
        let lock = login_lockout_ms(entry.failures);
        if lock > 0 {
            entry.locked_until_ms = now + lock;
        }
        entry.failures
    }

    /// A successful login clears the client's record.
    pub(super) fn note_login_success(&self, key: &str) {
        if let Ok(mut guard) = self.login_failures.lock() {
            guard.remove(key);
        }
    }

    /// (tracked clients, currently locked-out clients) — for the panel and tests.
    pub fn login_throttle_summary(&self) -> (usize, usize) {
        let now = auth_now_ms();
        let Ok(guard) = self.login_failures.lock() else {
            return (0, 0);
        };
        let locked = guard.values().filter(|e| e.locked_until_ms > now).count();
        (guard.len(), locked)
    }

    pub(super) fn token_from_cookies(&self, req: &HttpRequest) -> Option<String> {
        let cookies = req.header("cookie")?;
        for pair in cookies.split(';') {
            let pair = pair.trim();
            if let Some(v) = pair.strip_prefix("bk_session=") {
                return Some(v.to_string());
            }
        }
        None
    }

    /// Every form this API accepts a session token in.
    ///
    /// #460: deliberately NOT the URL query. `?token=` leaks into proxy logs,
    /// browser history and `Referer` headers; bearer/cookie/header cover every
    /// legitimate caller (the panel uses the cookie, the gate scripts use the
    /// header). A request that only has the query form is now just
    /// unauthenticated.
    pub(super) fn session_token(&self, req: &HttpRequest) -> Option<String> {
        self.token_from_cookies(req)
            .or_else(|| req.header("x-auth-token").map(String::from))
            .or_else(|| req.header("authorization").and_then(bearer_or_basic))
    }

    /// Gate in front of every route. Returns 200, 401 or 403.
    ///
    /// Two independent layers, in this order:
    ///
    /// 1. **Origin.** A browser's request carries `Origin` (or a `Referer`);
    ///    foreign evidence means another site is driving this request, so it is
    ///    refused — even for a caller that somehow holds a valid session. See
    ///    [`Self::origin_allowed`] for why *absent* evidence is not treated as
    ///    hostile.
    /// 2. **Session.** Required on every `/api/*` route as soon as a session can
    ///    exist (gateway mode, or credentials configured). This is the layer that
    ///    actually stops cross-site request forgery, because an HTML `<img>` or
    ///    `<form>` can issue a request but cannot attach a token — which is why
    ///    gateway mode demands one whether or not a password was configured.
    pub(super) fn authorize(&self, req: &HttpRequest) -> u16 {
        // 1. Origin / CSRF gate — every route, every method, armed or not.
        if !self.origin_allowed(req) {
            return 403;
        }

        // 2. Session gate. `/api/login` and `/api/ping` are the two routes that
        //    must work without one: the first is how a session is obtained, the
        //    second is how a client tells "gateway down" apart from "session
        //    stale" — without it, a dead token is indistinguishable from an
        //    unreachable gateway, which is what left the panel stuck.
        if req.path() == "/api/login" || req.path() == "/api/ping" {
            return 200;
        }
        if !req.path().starts_with("/api/") {
            return 200; // the panel bundle itself is public; the UI gates the view
        }
        // #460: a surface with no credentials armed (`WebServer::new` — the
        // Tauri embed, `ui_kit_panel`'s read-only serve) has no session layer,
        // but it is NOT a write surface. The proxy routes below
        // (`/api/risk/set-systemic`, `/api/risk/set-exit`,
        // `/api/execution-policy/set|reset`, `/api/blueprint/save*`,
        // `/api/backtest/onchain-pull` + `/api/backtest/run`,
        // `/api/version/configure`, `/api/command`) reach the kernel through
        // `snapshot_src` with or without a dispatcher, so "no command route
        // exists" was never true for them. Every state-changing route is a
        // POST; on this surface all of them are refused at the gate.
        if !self.auth_required && self.panel_user.is_none() {
            if req.method == "POST" {
                return 403;
            }
            return 200;
        }
        match self.session_token(req) {
            Some(t) if self.session_valid(&t) => 200,
            _ => 401,
        }
    }

    /// Liveness probe — the freqtrade `/ping` analogue, and the reason the panel
    /// can no longer get stuck.
    ///
    /// Deliberately unauthenticated, and deliberately *narrow*: it reveals only
    /// whether this process is alive and whether a session would be required. It
    /// carries no snapshot, no balance, no position. The panel uses it to tell
    /// "gateway unreachable" (no reply) apart from "my token is stale" (reply
    /// says auth is required, so a rotating token is worth spending).
    pub(super) fn ping_doc(&self) -> String {
        serde_json::json!({
            "ok": true,
            "service": "blitzkrieg-panel",
            "authRequired": self.auth_required || self.panel_user.is_some(),
        })
        .to_string()
    }

    /// The `security` block of the snapshot document: what a panel or an
    /// external monitor needs to see the two exposure facts without reading logs.
    pub(super) fn security_doc(&self) -> serde_json::Value {
        let bind = self.bind.lock().ok().and_then(|b| b.clone());
        let (tracked, locked) = self.login_throttle_summary();
        serde_json::json!({
            // Absent before `serve()` binds: assume the safe answer is unknown,
            // but do not claim exposure we cannot demonstrate either.
            "loopbackOnly": bind.as_ref().map(|b| b.loopback_only).unwrap_or(true),
            "bind": bind.as_ref().map(|b| b.addr.clone()),
            "warning": self.bind_warning(),
            "login": {
                "maxFailures": MAX_LOGIN_FAILURES,
                "lockoutBaseSec": LOGIN_LOCKOUT_BASE_MS / 1_000,
                "lockoutMaxSec": LOGIN_LOCKOUT_MAX_MS / 1_000,
                "trackedClients": tracked,
                "lockedClients": locked,
            },
        })
    }
}

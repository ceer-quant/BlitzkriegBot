use super::*;

/// How long a cached network self-check is served before a new probe is started.
/// Long enough that opening the panel twice does not dial twice, short enough
/// that a 30-second-old reading is not presented as the current state.
pub(super) const NET_CHECK_TTL_MS: u128 = 20_000;

/// The cached network self-check the browser panel reads.
///
/// Why a cache rather than one probe per request: `serve()` accepts connections
/// in a SINGLE thread (`for stream in listener.incoming()`), so a route that
/// dials for ten seconds freezes every other request in the process — the
/// panel's own snapshot poll, its login route, everything. A route that starts
/// ONE background probe when the cache is stale and answers from the last report
/// keeps the panel alive while the probe runs, and the client says "探测中…"
/// from the same `probing` flag the TUI shows.
#[derive(Default)]
pub(super) struct NetCheckCache {
    /// When `report` landed (monotonic — this is an age, not a timestamp).
    pub(super) at: Option<std::time::Instant>,
    /// A probe is running right now.
    pub(super) probing: bool,
    /// The display venue the report was probed FOR (the kernel resolves the
    /// plugin by the display account's venue binding, so the report is only
    /// meaningful for that venue). A display-account switch invalidates the
    /// cached reading even inside the TTL — otherwise the card shows the
    /// previous account's network for up to 20s after a switch.
    pub(super) venue: String,
    /// The last report, exactly as the core serialised it.
    pub(super) report: Option<serde_json::Value>,
    /// Why the last probe failed to ANSWER (core unreachable, socket refused).
    /// Kept apart from a report that answered with failing probes: "the core
    /// cannot be asked" and "the core asked the network and it is broken" are
    /// different problems with different fixes.
    pub(super) error: Option<String>,
}

/// The in-flight guard for `POST /api/version/check` (VERSIONING.md §6.3).
/// Same shape of problem as the net-check cache: the kernel's
/// `system.update.check` dials GitHub with its own timeout, and this server
/// accepts on ONE thread — so the route starts the ask in a background thread
/// and answers immediately; the card polls the snapshot's `systemVersion`
/// until the kernel's `lastCheckMs` moves. Only the transient state lives
/// here: the VERDICT is the kernel's persisted update state, never a gateway
/// copy of it.
#[derive(Default)]
pub(super) struct UpdateCheckCache {
    /// A check is running right now (one at a time — a second click while one
    /// is in flight must not start a second dial).
    pub(super) running: bool,
    /// Why the last background ask could not be ANSWERED (core unreachable,
    /// IPC error). A check that RAN and failed lands in the kernel's state as
    /// unknown, which is where the UI reads it — that is not an error here.
    pub(super) error: Option<String>,
}

impl WebServer {
    /// The `/api/netcheck` answer: the cached report, plus whether a probe is
    /// running right now and why the last one could not answer.
    ///
    /// Drop the cached report's age so the next poll starts a fresh probe.
    ///
    /// Used by the probe button (see the route): the report itself is kept, so a
    /// caller that polls mid-probe still gets the previous result plus
    /// `probing: true` rather than an empty card.
    /// Start one background `system.update.check` against the serving core
    /// (VERSIONING.md §6.3). The kernel holds the switches and the verdict —
    /// this route only triggers the ask and reports whether it started.
    pub(super) fn version_check_start(&self) -> String {
        let socket = self
            .snapshot_src
            .lock()
            .map(|c| c.socket_path().to_string())
            .unwrap_or_default();
        let Ok(mut cache) = self.update_check.lock() else {
            return serde_json::json!({ "started": false, "error": "update-check cache poisoned" })
                .to_string();
        };
        if cache.running {
            return serde_json::json!({ "started": false, "running": true }).to_string();
        }
        if socket.is_empty() {
            return serde_json::json!({ "started": false, "error": "no core socket" }).to_string();
        }
        cache.running = true;
        cache.error = None;
        let shared = self.update_check.clone();
        std::thread::spawn(move || {
            let mut client = IpcClient::new(socket);
            let outcome = client.update_check();
            if let Ok(mut c) = shared.lock() {
                c.running = false;
                if let Err(e) = outcome {
                    // A kernel REFUSAL (checkEnabled=false) arrives here as an
                    // Rpc error — the operator asked with the switch off, which
                    // the card must say out loud, never paper over.
                    c.error = Some(e.to_string());
                }
            }
        });
        serde_json::json!({ "started": true }).to_string()
    }

    /// #379: the in-flight starter for `POST /api/version/stage` — same
    /// shape of problem as [`Self::version_check_start`]: the kernel dials
    /// GitHub and downloads an archive on ITS timeout, so the ask runs in a
    /// background thread; the verdict is the kernel's staging state machine
    /// (the snapshot's `stageState`), never a gateway copy.
    pub(super) fn version_stage_start(&self) -> String {
        let socket = self
            .snapshot_src
            .lock()
            .map(|c| c.socket_path().to_string())
            .unwrap_or_default();
        let Ok(mut cache) = self.update_check.lock() else {
            return serde_json::json!({ "started": false, "error": "update-check cache poisoned" })
                .to_string();
        };
        if cache.running {
            return serde_json::json!({ "started": false, "running": true }).to_string();
        }
        if socket.is_empty() {
            return serde_json::json!({ "started": false, "error": "no core socket" }).to_string();
        }
        cache.running = true;
        cache.error = None;
        let shared = self.update_check.clone();
        std::thread::spawn(move || {
            let mut client = IpcClient::new(socket);
            let outcome = client.update_stage();
            if let Ok(mut c) = shared.lock() {
                c.running = false;
                if let Err(e) = outcome {
                    // A kernel REFUSAL (autoUpdate=false) arrives here as an
                    // Rpc error — say it out loud, never paper over it.
                    c.error = Some(e.to_string());
                }
            }
        });
        serde_json::json!({ "started": true }).to_string()
    }

    /// The update switches write (#393): `{"autoUpdate": bool}` and/or
    /// `{"checkEnabled": bool}` in, the kernel's own echo out. Inline (fast),
    /// and the kernel persists it — a switch that did not land on disk is an
    /// error the card shows verbatim. An absent key leaves that switch exactly
    /// as the kernel holds it (`system.update.configure` takes both keys
    /// optional), and the reply carries BOTH values so the panel re-renders
    /// from kernel state, never from the request.
    pub(super) fn version_configure(&self, body: &str) -> String {
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(body);
        let doc = parsed.ok();
        let auto = doc
            .as_ref()
            .and_then(|v| v.get("autoUpdate"))
            .and_then(|b| b.as_bool());
        let check = doc
            .as_ref()
            .and_then(|v| v.get("checkEnabled"))
            .and_then(|b| b.as_bool());
        if auto.is_none() && check.is_none() {
            return serde_json::json!({
                "ok": false,
                "error": "body must carry {\"autoUpdate\": bool} and/or {\"checkEnabled\": bool}"
            })
            .to_string();
        }
        let socket = self
            .snapshot_src
            .lock()
            .map(|c| c.socket_path().to_string())
            .unwrap_or_default();
        if socket.is_empty() {
            return serde_json::json!({ "ok": false, "error": "no core socket" }).to_string();
        }
        let mut client = IpcClient::new(socket);
        match client.update_configure(check, auto) {
            Ok(v) => serde_json::json!({
                "ok": true,
                "autoUpdate": v.get("autoUpdate"),
                "checkEnabled": v.get("checkEnabled"),
            })
            .to_string(),
            Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }).to_string(),
        }
    }

    pub(super) fn net_check_invalidate(&self) {
        if let Ok(mut cache) = self.net_check.lock() {
            cache.at = None;
        }
    }

    /// Starts a background probe when the cache is stale (see the route for why
    /// nothing is dialled inline). The probe gets its OWN client on its own
    /// connection, so it holds neither the panel's snapshot client nor the
    /// dispatcher while it waits on the network.
    pub(super) fn net_check_poll(&self) -> String {
        let socket = self
            .snapshot_src
            .lock()
            .map(|c| c.socket_path().to_string())
            .unwrap_or_default();
        let Ok(mut cache) = self.net_check.lock() else {
            return serde_json::json!({
                "probing": false,
                "report": serde_json::Value::Null,
                "error": "net-check cache poisoned",
            })
            .to_string();
        };
        // The report follows the display account (the kernel probes the
        // display venue's plugin), so the cache's identity is the venue the
        // SNAPSHOT currently names — a switch re-probes even inside the TTL.
        let display_venue = self.snapshot().display_venue;
        let age = cache.at.map(|t| t.elapsed().as_millis());
        let stale = age.is_none_or(|a| a > NET_CHECK_TTL_MS) || cache.venue != display_venue;
        if stale && !cache.probing && !socket.is_empty() {
            cache.probing = true;
            cache.venue = display_venue;
            // The previous probe's error is not this probe's: clear it, or the
            // panel shows a stale failure beside a fresh "探测中…".
            cache.error = None;
            let shared = self.net_check.clone();
            std::thread::spawn(move || {
                let mut client = IpcClient::new(socket);
                let outcome = client.net_check();
                if let Ok(mut c) = shared.lock() {
                    c.probing = false;
                    match outcome {
                        Ok(report) => match serde_json::to_value(&report) {
                            Ok(v) => {
                                c.report = Some(v);
                                c.at = Some(std::time::Instant::now());
                                c.error = None;
                            }
                            Err(e) => c.error = Some(format!("report is not serializable: {e}")),
                        },
                        Err(e) => c.error = Some(e.to_string()),
                    }
                }
            });
        }
        serde_json::json!({
            "probing": cache.probing,
            "ageMs": age,
            "report": cache.report,
            "error": cache.error,
        })
        .to_string()
    }
}

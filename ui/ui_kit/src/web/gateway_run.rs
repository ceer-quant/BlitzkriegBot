//! Shared plumbing for the two web-gateway entry points — `ui_kit_web` and
//! `blitzkrieg web`. One implementation keeps arg semantics, credential/
//! origin handling and the SIGINT/SIGTERM shutdown (owned core stopped on the
//! blocking pool; exercised by `scripts/gateway-signal-stop-check.mjs`)
//! identical across both binaries instead of two drifting copies.

use crate::gateway::{Dispatcher, SupervisorConfig};
use crate::web::WebServer;
use crate::{resolve_socket_path, IpcClient};
use std::sync::{Arc, Mutex};

/// Run the web gateway with the given argument vector.
///
/// Flags: `--socket <path>`, `--addr <host:port>`, `--manage`,
/// `--allowed-origin <url>` (repeatable), `--help`. Origins beyond loopback
/// and same-origin also come from `BLITZKRIEG_ALLOWED_ORIGINS`
/// (comma-separated) — the reverse-proxy / server-deployment opt-in.
pub async fn run_web_gateway(args: Vec<String>, program: &'static str) -> std::io::Result<()> {
    let mut socket = resolve_socket_path();
    let mut addr = "127.0.0.1:51888".to_string();
    let mut manage = std::env::var("UIKIT_MANAGE")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false);
    let mut allowed_origins = env_allowed_origins();

    let mut iter = args.into_iter();
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--socket" => {
                if let Some(s) = iter.next() {
                    socket = s;
                }
            }
            "--addr" => {
                if let Some(s) = iter.next() {
                    addr = s;
                }
            }
            "--manage" => manage = true,
            "--allowed-origin" => {
                if let Some(s) = iter.next() {
                    let s = s.trim().trim_end_matches('/').to_string();
                    if !s.is_empty() {
                        allowed_origins.push(s);
                    }
                }
            }
            "--help" | "-h" => {
                println!("{program} [--socket <path>] [--addr <127.0.0.1:51888>] [--manage] [--allowed-origin <url>]");
                return Ok(());
            }
            other => eprintln!("ignoring unknown arg: {other}"),
        }
    }

    let client = IpcClient::new(socket.clone());
    println!("{program} → socket {socket}");
    let cfg = SupervisorConfig::from_env(socket);
    let dispatcher = Arc::new(Mutex::new(Dispatcher::new(cfg, manage)));
    // Full trade history: the history tab paginates in the browser, so pass
    // limit=0 (trades.history drains ALL closed rows, no 200-row cap).
    let mut server = WebServer::with_shared_gateway(client, 0, dispatcher.clone());
    // Panel credentials come from the environment, never from this process. The
    // WebUI exchanges them for a session token via POST /api/login. Gateway mode
    // can start and stop the trading core, so a missing pair is a startup error
    // rather than a prompt to invent one.
    server.set_panel_credentials(
        std::env::var("BLITZKRIEG_PANEL_USER").ok(),
        std::env::var("BLITZKRIEG_PANEL_PASSWORD").ok(),
    );
    server.set_allowed_origins(allowed_origins);
    if let Err(why) = server.require_credentials() {
        eprintln!("{program}: {why}");
        std::process::exit(2);
    }
    println!(
        "panel auth: {}",
        if server.auth_required() {
            if server.credentials_configured() {
                "session required on /api/* (credentials from env)"
            } else {
                "session required on /api/*"
            }
        } else {
            "off (read-only surface, no command verbs)"
        }
    );

    // Two owned clones so either signal branch can move its own into the
    // blocking task (E14).
    let signal_dispatcher_int = dispatcher.clone();
    let signal_dispatcher_term = dispatcher.clone();
    let server_task = tokio::task::spawn_blocking(move || server.serve(&addr));
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        result = server_task => {
            match result {
                Ok(result) => result,
                Err(e) => Err(std::io::Error::other(format!("web server task failed: {e}"))),
            }
        }
        _ = sigint.recv() => {
            eprintln!("{program}: stopping managed core on SIGINT...");
            // stop() waits out the TERM grace synchronously — blocking pool (E14).
            let _ = tokio::task::spawn_blocking(move || {
                if let Ok(mut d) = signal_dispatcher_int.lock() { d.stop(); }
            }).await;
            std::process::exit(0);
        }
        _ = sigterm.recv() => {
            eprintln!("{program}: stopping managed core on SIGTERM...");
            let _ = tokio::task::spawn_blocking(move || {
                if let Ok(mut d) = signal_dispatcher_term.lock() { d.stop(); }
            }).await;
            std::process::exit(0);
        }
    }
}

/// Origins from the environment: `BLITZKRIEG_ALLOWED_ORIGINS`, comma-separated,
/// blank entries dropped. The `.env` self-load means a server deployment can put
/// this next to its credentials and forget about it.
///
/// `pub` for the unified launcher, which feeds the same allowlist to its own
/// [`WebServer`].
pub fn env_allowed_origins() -> Vec<String> {
    std::env::var("BLITZKRIEG_ALLOWED_ORIGINS")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

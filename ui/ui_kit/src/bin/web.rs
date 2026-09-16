//! `ui_kit_web` — browser frontend for the UI Kit.
//!
//! Usage: `ui_kit_web [--socket <path>] [--addr 127.0.0.1:51888] [--manage]`
//!   GET  /                → HTML panel (auto-refresh)
//!   GET  /api/snapshot    → JSON snapshot
//!   GET  /api/command?cmd=…  → dispatch one command (JSON)
//!   POST /api/command     → dispatch one command (JSON)
//!
//! `--manage` enables the lifecycle verbs (`start`/`stop`); without it the
//! command API is read-only (`status`/`positions`/`help`). The panel is always
//! read-only. No order-placing API exists on any path.

use blitzkrieg_ui_kit::gateway::{Dispatcher, SupervisorConfig};
use blitzkrieg_ui_kit::web::WebServer;
use blitzkrieg_ui_kit::{resolve_socket_path, IpcClient};

fn main() {
    let mut socket = resolve_socket_path();
    let mut addr = "127.0.0.1:51888".to_string();
    let mut manage = std::env::var("UIKIT_MANAGE")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--socket" => {
                if let Some(s) = args.next() {
                    socket = s;
                }
            }
            "--addr" => {
                if let Some(s) = args.next() {
                    addr = s;
                }
            }
            "--manage" => manage = true,
            other => eprintln!("ignoring unknown arg: {other}"),
        }
    }

    let client = IpcClient::new(socket.clone());
    println!("ui_kit web → socket {socket}");
    let cfg = SupervisorConfig::from_env(socket);
    let dispatcher = Dispatcher::new(cfg, manage);
    // Full trade history: the history tab paginates in the browser, so pass
    // limit=0 (trades.history drains ALL closed rows, no 200-row cap).
    let mut server = WebServer::with_gateway(client, 0, dispatcher);
    // E6-a: user/password auth from env. Set BLITZKRIEG_PANEL_USER and
    // BLITZKRIEG_PANEL_PASSWORD to require a panel login; the WebUI login page
    // exchanges them for a session token via POST /api/login. Unset (or
    // half-set) credentials disable auth — bind to loopback in that case.
    server.set_panel_credentials(
        std::env::var("BLITZKRIEG_PANEL_USER").ok(),
        std::env::var("BLITZKRIEG_PANEL_PASSWORD").ok(),
    );
    println!(
        "panel auth: {}",
        if std::env::var("BLITZKRIEG_PANEL_USER").map(|v| !v.trim().is_empty()).unwrap_or(false)
            && std::env::var("BLITZKRIEG_PANEL_PASSWORD").map(|v| !v.trim().is_empty()).unwrap_or(false)
        {
            "user/password enabled (login via the panel)"
        } else {
            "OFF — loopback-only deployment"
        }
    );
    if let Err(e) = server.serve(&addr) {
        eprintln!("web server error: {e}");
        std::process::exit(1);
    }
}

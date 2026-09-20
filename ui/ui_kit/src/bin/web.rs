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
//!
//! All plumbing (args, credentials, origins, signal shutdown) lives in
//! [`blitzkrieg_ui_kit::web::gateway_run`], shared with `blitzkrieg web`.

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(e) = blitzkrieg_ui_kit::web::run_web_gateway(args, "ui_kit_web").await {
        eprintln!("web server error: {e}");
        std::process::exit(1);
    }
}

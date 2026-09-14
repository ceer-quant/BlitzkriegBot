//! `ui_kit_web` — browser frontend for the UI Kit.
//!
//! Usage: `ui_kit_web [--socket <path>] [--addr 127.0.0.1:18888]`
//!   GET /              → HTML panel (auto-refresh)
//!   GET /api/snapshot  → JSON snapshot

use blitzkrieg_ui_kit::web::WebServer;
use blitzkrieg_ui_kit::{default_socket_path, IpcClient};

fn main() {
    let mut socket = default_socket_path();
    let mut addr = "127.0.0.1:18888".to_string();
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
            other => eprintln!("ignoring unknown arg: {other}"),
        }
    }
    let client = IpcClient::new(socket.clone());
    println!("ui_kit web → socket {socket}");
    let server = WebServer::new(client, 200);
    if let Err(e) = server.serve(&addr) {
        eprintln!("web server error: {e}");
        std::process::exit(1);
    }
}

//! `ui_kit_tui` — terminal frontend for the UI Kit.
//!
//! Usage: `ui_kit_tui [--socket <path>] [--interval-sec N] [--once]`

use blitzkrieg_ui_kit::{resolve_socket_path, tui, IpcClient};
use std::time::Duration;

fn main() {
    let mut socket = resolve_socket_path();
    let mut interval = 2u64;
    let mut once = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--socket" => {
                if let Some(s) = args.next() {
                    socket = s;
                }
            }
            "--interval-sec" => {
                if let Some(v) = args.next().and_then(|v| v.parse().ok()) {
                    interval = v;
                }
            }
            "--once" => once = true,
            other => eprintln!("ignoring unknown arg: {other}"),
        }
    }

    let mut client = IpcClient::new(socket.clone());
    println!("ui_kit TUI → socket {socket}");
    if once {
        let snap = client.snapshot(50);
        print!("{}", tui::render(&snap));
        return;
    }
    if let Err(e) = tui::run(&mut client, Duration::from_secs(interval.max(1)), 50) {
        eprintln!("tui error: {e}");
        std::process::exit(1);
    }
}

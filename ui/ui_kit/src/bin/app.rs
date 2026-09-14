//! `ui_kit_app` — native-app (Tauri/egui) frontend seam for the UI Kit.
//!
//! No GUI toolkit is linked here; this binary exercises the app adapter's view
//! model headlessly so the native shell has a verified data boundary to bind to.
//!
//! Usage: `ui_kit_app [--socket <path>] [--once] [--interval-sec N]`

use blitzkrieg_ui_kit::app::{render_headless, AppViewModel};
use blitzkrieg_ui_kit::{default_socket_path, IpcClient};
use std::time::Duration;

fn main() {
    let mut socket = default_socket_path();
    let mut once = false;
    let mut interval = 2u64;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--socket" => {
                if let Some(s) = args.next() {
                    socket = s;
                }
            }
            "--once" => once = true,
            "--interval-sec" => {
                if let Some(v) = args.next().and_then(|v| v.parse().ok()) {
                    interval = v;
                }
            }
            other => eprintln!("ignoring unknown arg: {other}"),
        }
    }

    println!("ui_kit app (headless view model) → socket {socket}");
    let mut vm = AppViewModel::new(IpcClient::new(socket), 200);
    loop {
        let view = vm.refresh().clone();
        print!("\x1b[2J\x1b[H{}", render_headless(&view));
        if once {
            break;
        }
        std::thread::sleep(Duration::from_secs(interval.max(1)));
    }
}

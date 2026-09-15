//! Blitzkrieg desktop shell (Tauri v2) — E6-a scaffold.
//!
//! Command surface is the SAME dispatcher the web/TUI adapters use (E5
//! contract): trading-logic-free verbs only, lifecycle gated by --manage
//! style flags set at spawn. Data flows through `blitzkrieg-ui-kit`; no GUI
//! dependency ever enters ui_kit.
//!
//! Dev flow: `cargo tauri dev` inside `ui/webapp/` (see ui/webapp/README.md).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use blitzkrieg_ui_kit::app::AppViewModel;
use blitzkrieg_ui_kit::gateway::{Dispatcher, SupervisorConfig};
use blitzkrieg_ui_kit::{resolve_socket_path, IpcClient};

/// Pull a fresh `AppView` JSON through the headless view-model (no GUI deps).
#[tauri::command]
fn desktop_snapshot(socket: Option<String>) -> Result<String, String> {
    let sock = socket.unwrap_or_else(resolve_socket_path);
    let mut vm = AppViewModel::new(IpcClient::new(sock), 200);
    let v = vm.refresh();
    serde_json::to_string(v).map_err(|e| e.to_string())
}

/// Run one E5 command surface verb; returns the standard CommandOutcome JSON.
#[tauri::command]
fn desktop_command(cmd: &str, socket: Option<String>) -> Result<String, String> {
    let sock = socket.unwrap_or_else(resolve_socket_path);
    let cfg = SupervisorConfig::from_env(sock);
    let mut dispatcher = Dispatcher::new(cfg, false); // lifecycle stays off in the webview shell (E6-a: read + plugin only)
    let outcome = dispatcher.dispatch_line(cmd);
    serde_json::to_string(&outcome).map_err(|e| e.to_string())
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![desktop_snapshot, desktop_command])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

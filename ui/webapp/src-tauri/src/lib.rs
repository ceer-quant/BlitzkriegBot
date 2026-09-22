//! Blitzkrieg desktop shell (Tauri v2) — E8-d packaging.
//!
//! The window is the built Vue panel (`ui/webapp/webui/dist`) served over
//! loopback HTTP by an embedded read-only `ui_kit` WebServer. The original
//! shape loaded `index.html` under tauri:// — where every `fetch('/api/…')`
//! dead-ends, because the protocol is not http and no same-origin server
//! exists behind it, which is why the first desktop panel rendered empty. The
//! embedded server restores the browser contract the panel was built against:
//! same origin, real `/api/*`, session auth unused (a read-only server exposes
//! no lifecycle verbs, so auth stays off — `WebServer::new`).
//!
//! Nothing here manages the trading process: the shell attaches no dispatcher
//! to the server and the command surface builds one with lifecycle off, so
//! the desktop surface reads and queries but cannot start or stop a core
//! (E6-a). Quitting the shell therefore leaves whatever core it was watching
//! exactly as it found it — the browser-tab rule.
//!
//! Dev flow: `cargo tauri dev` with `BLITZKRIEG_PANEL_URL` pointing at the
//! Vite dev server (`webui/vite.config.ts` proxies `/api` to the gateway on
//! 51888); the embedded server is skipped when the override is set.
//!
//! The two command bodies stay callable as plain functions so the gate
//! exercises the whole chain headlessly (`tests/chain.rs`) without opening a
//! window. (The `#[tauri::command]` fns themselves must stay private — the
//! macro's generated re-imports collide with `pub` bindings in a lib crate.)

use blitzkrieg_ui_kit::app::AppViewModel;
use blitzkrieg_ui_kit::gateway::{Dispatcher, SupervisorConfig};
use blitzkrieg_ui_kit::{resolve_socket_path, IpcClient};

/// Pull a fresh `AppView` JSON through the headless view-model (no GUI deps).
pub fn snapshot_json(socket: Option<String>) -> Result<String, String> {
    let sock = socket.unwrap_or_else(resolve_socket_path);
    let mut vm = AppViewModel::new(IpcClient::new(sock), 200);
    let v = vm.refresh();
    serde_json::to_string(v).map_err(|e| e.to_string())
}

/// Run one E5 command surface verb; returns the standard CommandOutcome JSON.
pub fn command_json(cmd: &str, socket: Option<String>) -> Result<String, String> {
    let sock = socket.unwrap_or_else(resolve_socket_path);
    let cfg = SupervisorConfig::from_env(sock);
    let mut dispatcher = Dispatcher::new(cfg, false); // lifecycle stays off in the webview shell (E6-a: read + plugin only)
    let outcome = dispatcher.dispatch_line(cmd);
    serde_json::to_string(&outcome).map_err(|e| e.to_string())
}

#[tauri::command]
fn desktop_snapshot(socket: Option<String>) -> Result<String, String> {
    snapshot_json(socket)
}

#[tauri::command]
fn desktop_command(cmd: &str, socket: Option<String>) -> Result<String, String> {
    command_json(cmd, socket)
}

/// Bind-and-drop probe for a free loopback port. The OS hands one out, we
/// release it and `serve` rebinds: the race window is microseconds and
/// loopback-only, and a real collision surfaces as a clean bind error, not a
/// silent hijack. `serve` cannot hand the bound port back, so the caller has
/// to know it first.
fn free_loopback_port() -> std::io::Result<u16> {
    let l = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = l.local_addr()?.port();
    drop(l);
    Ok(port)
}

/// Start the read-only panel server on `127.0.0.1:<free port>` and return the
/// URL the window should load. Runs on a detached thread — its lifetime is
/// the process's, and since it never spawns a core there is nothing to reap
/// on shutdown.
fn spawn_embedded_panel(socket: String) -> std::io::Result<tauri::Url> {
    let port = free_loopback_port()?;
    let url = tauri::Url::parse(&format!("http://127.0.0.1:{port}/panel/"))
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let addr = format!("127.0.0.1:{port}");
    let server = blitzkrieg_ui_kit::web::WebServer::new(IpcClient::new(socket), 0);
    std::thread::Builder::new()
        .name("panel-http".into())
        .spawn(move || {
            if let Err(e) = server.serve(&addr) {
                eprintln!("panel server stopped: {e}");
            }
        })?;
    Ok(url)
}

/// Build the app: embedded panel server + one window on it.
pub fn run() {
    let panel_url = match std::env::var("BLITZKRIEG_PANEL_URL") {
        Ok(u) if !u.trim().is_empty() => {
            Some(tauri::Url::parse(u.trim()).expect("BLITZKRIEG_PANEL_URL is not a valid URL"))
        }
        _ => None,
    };

    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![desktop_snapshot, desktop_command])
        .setup(move |app| {
            let url = match panel_url {
                Some(u) => u,
                None => {
                    let socket = resolve_socket_path();
                    spawn_embedded_panel(socket)
                        .map_err(|e| format!("embedded panel server failed to start: {e}"))
                        .expect("desktop shell cannot serve its panel")
                }
            };
            tauri::WebviewWindowBuilder::new(app, "main", tauri::WebviewUrl::External(url))
                .title("Blitzkrieg Panel")
                .inner_size(1100.0, 760.0)
                .build()?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

//! Blitzkrieg UI Kit — a pure presentation layer over the Rust trading core.
//!
//! Design contract (see `dev-docs/reports/UI_KIT_MIGRATION_REPORT.md`, internal):
//!
//! * The UI Kit **only renders data and subscribes to events**. It holds NO
//!   trading logic, no risk rules, no order decisions. All of that lives in
//!   `blitzkrieg-core`.
//! * It speaks to the core exclusively over the UDS JSON-RPC contract
//!   (`core/blitzkrieg_core/src/ipc/schema.rs`). It never links the core's trading
//!   types, so the whole kit is replaceable without touching the kernel.
//! * One shared `core` (types + IPC client + event bus) fan-outs to three
//!   adapters: `web` (browser), `app` (future Tauri/egui) + the ratatui panel
//!   crate `ui_kit_panel`.
//!
//! Module map mirrors the requested layout:
//!   `core::types`      — DTOs one-to-one with the core's IPC messages
//!   `core::ipc_client` — blocking UDS JSON-RPC client
//!   `core::event_bus`  — subscribe/dispatch of `core.event` notifications
//!   `gateway`          — command dispatch (start/stop/status/positions) +
//!                        core process supervisor (D-4 step ②); no order API
//!   `web`              — HTML/JSON renderers + a dependency-free HTTP server
//!   `app`              — adapter trait + headless stub for the native app

pub mod app;
pub mod core;
pub mod gateway;
pub mod web;

pub use core::event_bus::{EventBus, Subscription};
pub use core::ipc_client::{IpcClient, IpcError};
pub use core::types::*;

/// Canonical socket prefix, matching `blitzkrieg-core`'s `SOCKET_PREFIX`.
pub const SOCKET_PREFIX: &str = "blitzkrieg-core";

fn socket_path_for(prefix: &str) -> String {
    let user = std::env::var("USER").unwrap_or_else(|_| "user".into());
    let dir = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
    format!("{dir}/{prefix}-{user}.sock").replace("//", "/")
}

/// Default core socket path, matching `blitzkrieg-core`'s `default_socket()`.
pub fn default_socket_path() -> String {
    socket_path_for(SOCKET_PREFIX)
}

/// Is something accepting connections on `path` right now?
pub fn socket_served(path: &str) -> bool {
    std::os::unix::net::UnixStream::connect(path).is_ok()
}

/// The socket a client should use: always the canonical path.
///
/// There is no fallback and no probe. A client that probed and then fell back
/// would invent a second path and watch an empty socket — the failure the naming
/// contract exists to prevent; a client that probed and then adopted whatever
/// answered would attach to a core it did not spawn. The answer is the one this
/// returns unconditionally either way, so the probe bought nothing and cost a
/// blocking `connect` before every client could start.
///
/// Callers that need to report liveness ask [`socket_served`] themselves.
pub fn resolve_socket_path() -> String {
    default_socket_path()
}

#[cfg(test)]
mod socket_tests {
    use super::*;

    #[test]
    fn canonical_path_uses_the_brand() {
        let p = default_socket_path();
        assert!(p.ends_with(".sock"), "{p}");
        assert!(
            p.contains("/blitzkrieg-core-"),
            "canonical path must carry the new brand: {p}"
        );
        assert!(!p.contains("legacy-socket-name"), "{p}");
    }

    #[test]
    fn a_bare_name_is_never_a_live_socket() {
        assert!(!socket_served("definitely-not-a-socket-9f2c1a"));
    }
}

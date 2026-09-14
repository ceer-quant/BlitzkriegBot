//! Blitzkrieg UI Kit — a pure presentation layer over the Rust trading core.
//!
//! Design contract (see `docs/reports/UI_KIT_MIGRATION_REPORT.md`):
//!
//! * The UI Kit **only renders data and subscribes to events**. It holds NO
//!   trading logic, no risk rules, no order decisions. All of that lives in
//!   `blitzkrieg-core`.
//! * It speaks to the core exclusively over the UDS JSON-RPC contract
//!   (`Blitzkrieg_core/src/ipc/schema.rs`). It never links the core's trading
//!   types, so the whole kit is replaceable without touching the kernel.
//! * One shared `core` (types + IPC client + event bus) fan-outs to three
//!   adapters: `web` (browser), `tui` (terminal), `app` (future Tauri/egui).
//!
//! Module map mirrors the requested layout:
//!   `core::types`      — DTOs one-to-one with the core's IPC messages
//!   `core::ipc_client` — blocking UDS JSON-RPC client
//!   `core::event_bus`  — subscribe/dispatch of `core.event` notifications
//!   `web`              — HTML/JSON renderers + a dependency-free HTTP server
//!   `tui`              — ANSI renderers + a polling run loop
//!   `app`              — adapter trait + headless stub for the native app

pub mod app;
pub mod core;
pub mod tui;
pub mod web;

pub use core::event_bus::{EventBus, Subscription};
pub use core::ipc_client::{IpcClient, IpcError};
pub use core::types::*;

/// Default core socket path, matching `blitzkrieg-core`'s `default_socket()`.
pub fn default_socket_path() -> String {
    let user = std::env::var("USER").unwrap_or_else(|_| "clodds".into());
    let dir = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
    format!("{dir}/clodds-core-{user}.sock").replace("//", "/")
}

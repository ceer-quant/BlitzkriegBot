//! Blitzkrieg UI Kit — a pure presentation layer over the Rust trading core.
//!
//! Design contract (see `docs/reports/UI_KIT_MIGRATION_REPORT.md`):
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
/// Pre-rename prefix, still discovered for one release.
pub const LEGACY_SOCKET_PREFIX: &str = "clodds-core";

fn socket_path_for(prefix: &str) -> String {
    let user = std::env::var("USER").unwrap_or_else(|_| "user".into());
    let dir = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
    format!("{dir}/{prefix}-{user}.sock").replace("//", "/")
}

/// Default core socket path, matching `blitzkrieg-core`'s `default_socket()`.
pub fn default_socket_path() -> String {
    socket_path_for(SOCKET_PREFIX)
}

/// The pre-rename socket path. A core started before the rename still owns it, so
/// callers adopt that core rather than spawn a second one beside it.
pub fn legacy_socket_path() -> String {
    socket_path_for(LEGACY_SOCKET_PREFIX)
}

/// Is something accepting connections on `path` right now?
pub fn socket_served(path: &str) -> bool {
    std::os::unix::net::UnixStream::connect(path).is_ok()
}

/// The socket a client should use: canonical, unless a pre-rename core still owns
/// the legacy name — adopting that core is the point of the window.
///
/// Note this is deliberately a *probe*, not a constant: a node shell started
/// before the rename passes `--socket <legacy path>` explicitly, so the running
/// core legitimately binds the old name until that shell restarts.
pub fn resolve_socket_path() -> String {
    resolve_socket_path_from(default_socket_path(), legacy_socket_path(), socket_served)
}

/// Precedence rule, split out so it is testable without a live socket on the box.
fn resolve_socket_path_from(
    canonical: String,
    legacy: String,
    served: impl Fn(&str) -> bool,
) -> String {
    if served(&canonical) {
        return canonical;
    }
    if legacy != canonical && served(&legacy) {
        return legacy;
    }
    canonical
}

#[cfg(test)]
mod socket_tests {
    use super::*;

    #[test]
    fn canonical_path_uses_the_new_prefix() {
        let p = default_socket_path();
        assert!(p.ends_with(".sock"), "{p}");
        assert!(
            p.contains("/blitzkrieg-core-"),
            "canonical path must carry the new brand: {p}"
        );
        assert!(!p.contains("clodds"), "{p}");
    }

    #[test]
    fn legacy_path_is_still_derivable_and_distinct() {
        let l = legacy_socket_path();
        assert!(l.contains("/clodds-core-"), "{l}");
        assert_ne!(l, default_socket_path());
    }

    #[test]
    fn both_prefixes_agree_on_user_and_dir() {
        let (a, b) = (default_socket_path(), legacy_socket_path());
        let strip = |s: &str| {
            s.rsplit_once('/')
                .map(|(d, _)| d.to_string())
                .unwrap_or_default()
        };
        assert_eq!(strip(&a), strip(&b));
        let user = std::env::var("USER").unwrap_or_else(|_| "user".into());
        assert!(a.ends_with(&format!("blitzkrieg-core-{user}.sock")), "{a}");
    }

    /// Precedence, tested with an injected probe so the result does not depend on
    /// whatever core happens to be running on the build machine.
    #[test]
    fn resolve_prefers_canonical_when_nothing_is_served() {
        let (c, l) = (default_socket_path(), legacy_socket_path());
        let out = resolve_socket_path_from(c.clone(), l, |_| false);
        assert_eq!(out, c);
    }

    #[test]
    fn resolve_keeps_canonical_when_both_are_served() {
        let (c, l) = (default_socket_path(), legacy_socket_path());
        let out = resolve_socket_path_from(c.clone(), l, |_| true);
        assert_eq!(
            out, c,
            "a live canonical core wins over a leftover legacy one"
        );
    }

    /// The migration case: an old-branded core is up and nothing is on the new
    /// name, so the client must adopt it rather than spawn a second core.
    #[test]
    fn resolve_falls_back_to_legacy_only_when_it_is_the_one_serving() {
        let (c, l) = (default_socket_path(), legacy_socket_path());
        let served = l.clone();
        let out = resolve_socket_path_from(c, l.clone(), move |p| p == served);
        assert_eq!(out, l);
    }

    #[test]
    fn a_bare_name_is_never_a_live_socket() {
        assert!(!socket_served("definitely-not-a-socket-9f2c1a"));
    }
}

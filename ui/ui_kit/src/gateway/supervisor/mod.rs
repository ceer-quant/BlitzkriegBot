//! Core process supervisor — the Rust replacement for the lifecycle half of the
//! deleted Node client (`BlitzkriegCoreClient`, gone with the source layer in
//! 62b16c88).
//!
//! It does exactly three things: spawn `blitzkrieg-core` with parameters,
//! stop it, or **adopt** a core that is already serving the socket. It holds no
//! trading logic — every decision stays inside the Rust core. This is what let
//! the Node shell be removed (D-4, step ②) without losing the `/crypto-hft
//! start|stop` control channel; the shell itself is now gone too.
//!
//! Safety rules carried over from that Node client:
//!   * **Adopt, never double-spawn.** If the socket is already served we connect
//!     to that core instead of spawning a second one (which would abort with
//!     "already listening"). An adopted core is *not ours* — `stop()` will not
//!     kill it.
//!   * `stop()` only signals a process we spawned.
//!   * On drop, a spawned (owned) core is stopped so the gateway never leaks an
//!     unmanaged core. An adopted one is left running.
//!
//! Zero extra dependencies: SIGTERM is sent by invoking the system `kill(1)`
//! (present at `/bin/kill` on macOS and Linux). If that is unavailable we fall
//! back to the std `Child::kill` (SIGKILL). The core's order/position
//! persistence is per-state-change (verified crash-safe under SIGKILL in
//! `scripts/order-recovery-check.mjs` / `position-recovery-check.mjs`), so
//! either signal is safe.

mod alert;
mod exit;
mod policy;
mod process;

pub use alert::*;
pub use exit::*;
pub use policy::*;
pub use process::*;

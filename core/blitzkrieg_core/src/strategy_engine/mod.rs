//! Strategy loading boundary.
//!
//! ARCHITECTURAL BOUNDARY: the *decision logic* lives in the user layer; the
//! kernel owns everything that can move money. Two loading paths exist but they
//! land on the SAME full-featured contract [`crate::strategies::EngineStrategy`]:
//!
//!  1. **In-tree strategies** (e.g. the proven `spread_arb` builtin) compiled
//!     into the kernel and registered at startup.
//!  2. **Dynamic libraries** (`.dylib`/`.so`/`.dll`) implementing C ABI v2,
//!     dlopen'ed through [`loader`] and wrapped as
//!     [`crate::strategies::foreign::ForeignStrategy`].
//!
//! Being external is only a loading difference, not a capability difference
//! (E7 / issue #38). A loaded strategy receives only read-only market/context
//! data and returns intents; it never receives a signer, venue client, order
//! manager, UDS socket or credential, and the kernel validates/runs risk/
//! reserves/signs/submits every order. The pre-v2 reduced one-tick trait and
//! the second-class standalone registry were removed so the gap cannot reopen.

#[cfg(feature = "strategy-loading")]
pub mod loader;

/// Shared-library path policy lives unconditionally (even without the loading
/// feature) so the non-feature build can still report a clean, testable refusal.
#[cfg(not(feature = "strategy-loading"))]
pub mod loader;

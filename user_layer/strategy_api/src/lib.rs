//! Blitzkrieg Strategy API — FROZEN C ABI (v1).
//!
//! This crate defines the ONLY binary contract between the kernel and a
//! user-layer strategy shipped as a shared library (`.so` / `.dylib` / `.dll`).
//! It is deliberately tiny, `#[repr(C)]`, and free of Rust-specific types
//! (no `String`, no `Vec`, no trait objects) so it stays stable across compiler
//! versions and languages.
//!
//! # Contract
//!
//! A strategy shared library MUST export exactly one symbol (C ABI):
//!
//! ```c
//! const BkStrategyVtable* bk_strategy_create(void);
//! ```
//!
//! and SHOULD export one version symbol for negotiation:
//!
//! ```c
//! uint32_t bk_strategy_abi_version(void);   // == BK_ABI_VERSION
//! ```
//!
//! `bk_strategy_create` returns a pointer to a static `BkStrategyVtable`. The
//! kernel copies the vtable, calls `create` to obtain an opaque `handle`, and
//! thereafter drives the strategy through the function pointers.
//!
//! # Memory & safety rules (MUST)
//!
//! - The strategy owns its `handle`; the kernel never frees it. `destroy` is
//!   invoked by the kernel on unload.
//! - Strings in `BkTick`/`BkSignal` are borrowed `const char*` valid ONLY for the
//!   duration of the call. The kernel copies what it needs.
//! - `BkSignal::price`/`size` are decimal strings (no float rounding drift).
//! - A strategy MUST NOT perform I/O, read credentials, or call the venue. It
//!   receives a tick and returns a signal — nothing else crosses this boundary.

use core::ffi::{c_char, c_void};

/// ABI version. Bump ONLY on a breaking change to the structs/vtable below.
pub const BK_ABI_VERSION: u32 = 1;

/// Symbol name for the factory function.
pub const BK_CREATE_SYMBOL: &[u8] = b"bk_strategy_create\0";
/// Symbol name for the ABI-version function.
pub const BK_VERSION_SYMBOL: &[u8] = b"bk_strategy_abi_version\0";

/// Side of a signal.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BkSide {
    None = 0,
    Buy = 1,
    Sell = 2,
}

/// `#[repr(C)]` market tick. All strings are NUL-terminated UTF-8 borrowed
/// pointers valid for the call duration only.
#[repr(C)]
pub struct BkTick {
    /// Token/contract identifier (borrowed).
    pub symbol: *const c_char,
    /// Asset symbol, e.g. "BTC" (borrowed).
    pub asset: *const c_char,
    /// Prices as decimal strings to avoid float drift (borrowed).
    pub best_bid: *const c_char,
    pub best_ask: *const c_char,
    pub mid: *const c_char,
    /// Milliseconds since Unix epoch.
    pub timestamp_ms: i64,
}

/// `#[repr(C)]` signal returned by the strategy. `price`/`size` are decimal
/// strings (NULL/empty when not applicable). The kernel copies them immediately.
#[repr(C)]
pub struct BkSignal {
    pub side: BkSide,
    pub symbol: *const c_char,
    pub price: *const c_char,
    pub size: *const c_char,
}

impl BkSignal {
    /// A "hold"/no-op signal.
    pub const fn hold() -> Self {
        Self { side: BkSide::None, symbol: core::ptr::null(), price: core::ptr::null(), size: core::ptr::null() }
    }
}

/// Opaque per-instance state owned by the strategy.
pub type BkHandle = *mut c_void;

/// The frozen vtable. A strategy returns a pointer to a `'static` instance from
/// `bk_strategy_create`.
///
/// Field order and types MUST NOT change without bumping `BK_ABI_VERSION`.
#[repr(C)]
pub struct BkStrategyVtable {
    /// Human-readable name (NUL-terminated, static lifetime).
    pub name: *const c_char,
    /// Semantic version string (NUL-terminated, static lifetime).
    pub version: *const c_char,
    /// ABI version this vtable was built against (== `BK_ABI_VERSION`).
    pub abi_version: u32,
    /// Allocate/initialise an instance. Returns null on failure.
    pub create: Option<unsafe extern "C" fn() -> BkHandle>,
    /// Free an instance created by `create`.
    pub destroy: Option<unsafe extern "C" fn(handle: BkHandle)>,
    /// Called on each tick. Return `BkSignal::hold()` for no action.
    pub on_tick: Option<unsafe extern "C" fn(handle: BkHandle, tick: *const BkTick) -> BkSignal>,
    /// Called on round change (optional; may be None).
    pub on_round: Option<unsafe extern "C" fn(handle: BkHandle, slot: i64)>,
}

// SAFETY: the vtable pointer is required to point at static data (see the
// contract above), so sharing it across threads is sound.
unsafe impl Send for BkStrategyVtable {}
unsafe impl Sync for BkStrategyVtable {}

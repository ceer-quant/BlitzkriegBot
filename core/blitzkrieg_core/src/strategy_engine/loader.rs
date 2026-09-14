//! Dynamic strategy loader — loads user-layer strategy logic from shared
//! libraries (`.dylib` / `.so` / `.dll`) via `libloading`, driving the FROZEN
//! C ABI defined in the `blitzkrieg_strategy_api` crate.
//!
//! ARCHITECTURAL GUARANTEE: a loaded strategy only ever receives a `MarketTick`
//! and returns a `Signal`. It has no handle to credentials, the CLOB client, the
//! order manager or the UDS socket — none of those are passed across the dynamic
//! boundary. `policy_allows` additionally refuses credential-looking files.
//!
//! Runtime flow:
//!   1. `policy_allows(path)` — static policy check.
//!   2. `dlopen` the library.
//!   3. `bk_strategy_abi_version()` (optional) — negotiate ABI version.
//!   4. `bk_strategy_create()` — obtain the frozen vtable.
//!   5. wrap the vtable into a `DynamicStrategy` implementing `Strategy`.

#[cfg(feature = "strategy-loading")]
use crate::strategy_engine::{MarketTick, Signal, Strategy};
#[cfg(feature = "strategy-loading")]
use blitzkrieg_strategy_api::{
    BkHandle, BkSide, BkStrategyVtable, BkTick, BK_ABI_VERSION, BK_CREATE_SYMBOL, BK_VERSION_SYMBOL,
};
#[cfg(feature = "strategy-loading")]
use rust_decimal::Decimal;
#[cfg(feature = "strategy-loading")]
use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};
#[cfg(feature = "strategy-loading")]
use std::str::FromStr;

/// Outcome of attempting to load a strategy library.
pub enum LoadOutcome {
    /// Policy rejected the path before any dlopen.
    Rejected { path: PathBuf, reason: String },
    /// Library opened and loaded into the engine.
    Loaded { path: PathBuf, name: String, version: String },
    /// A failure at any stage (policy passed but load/negotiate/create failed).
    Failed { path: PathBuf, reason: String },
}

impl std::fmt::Debug for LoadOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadOutcome::Rejected { path, reason } => {
                write!(f, "Rejected {{ path: {}, reason: {} }}", path.display(), reason)
            }
            LoadOutcome::Loaded { path, name, version } => {
                write!(f, "Loaded {{ path: {}, name: {}, version: {} }}", path.display(), name, version)
            }
            LoadOutcome::Failed { path, reason } => {
                write!(f, "Failed {{ path: {}, reason: {} }}", path.display(), reason)
            }
        }
    }
}

/// Policy decision for a candidate strategy library path. Pure and testable.
pub fn policy_allows(path: &Path) -> Result<(), String> {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    for bad in [".env", "private", "secret", "key", "credential"] {
        if name.to_lowercase().contains(bad) {
            return Err(format!("strategy library name looks like it bundles credentials: {name}"));
        }
    }
    let ext_ok = name.ends_with(".so") || name.ends_with(".dylib") || name.ends_with(".dll");
    if !ext_ok {
        return Err(format!("not a shared library: {name}"));
    }
    Ok(())
}

/// Holds the loaded library so it stays resident for the process lifetime, plus
/// the vtable + instance handle for one strategy.
#[cfg(feature = "strategy-loading")]
pub struct DynamicStrategy {
    _lib: Option<libloading::Library>,
    vtable: BkStrategyVtable,
    handle: BkHandle,
    name: String,
    version: String,
}

#[cfg(feature = "strategy-loading")]
impl DynamicStrategy {
    /// Load a strategy shared library and wrap its frozen vtable.
    pub fn load(path: &Path) -> Result<Self, String> {
        policy_allows(path)?;

        // SAFETY: opening a shared library runs its initialisers. We only load
        // libraries the operator placed in the strategy directory and the policy
        // check has passed.
        let lib = unsafe { libloading::Library::new(path) }.map_err(|e| format!("dlopen failed: {e}"))?;

        // Optional ABI-version negotiation.
        let abi = unsafe {
            lib.get::<unsafe extern "C" fn() -> u32>(BK_VERSION_SYMBOL)
                .ok()
                .map(|f| f())
        };
        if let Some(v) = abi {
            if v != BK_ABI_VERSION {
                return Err(format!(
                    "ABI version mismatch: library={v}, kernel={BK_ABI_VERSION}"
                ));
            }
        }

        // Factory → vtable.
        let create = unsafe {
            lib.get::<unsafe extern "C" fn() -> *const BkStrategyVtable>(BK_CREATE_SYMBOL)
        }
        .map_err(|e| format!("missing symbol bk_strategy_create: {e}"))?;

        let vt_ptr = unsafe { create() };
        if vt_ptr.is_null() {
            return Err("bk_strategy_create returned null".into());
        }
        // Copy the vtable by value (the library guarantees it is 'static).
        let vtable = unsafe { std::ptr::read(vt_ptr) };
        if vtable.abi_version != BK_ABI_VERSION {
            return Err(format!(
                "vtable ABI version mismatch: lib={}, kernel={BK_ABI_VERSION}",
                vtable.abi_version
            ));
        }

        let name = unsafe { cstr_to_string(vtable.name) }.unwrap_or_else(|| "unnamed".into());
        let version = unsafe { cstr_to_string(vtable.version) }.unwrap_or_else(|| "0.0.0".into());

        let handle = match vtable.create {
            Some(f) => unsafe { f() },
            None => std::ptr::null_mut(),
        };
        if handle.is_null() {
            return Err("strategy create() returned a null handle".into());
        }

        Ok(Self { _lib: Some(lib), vtable, handle, name, version })
    }
}

// SAFETY: a DynamicStrategy owns a single library instance whose handle is only
// ever driven by the kernel's single-threaded strategy loop; the vtable points
// at static data. Sending the instance between threads (never done concurrently)
// and sharing the &self view are therefore sound under the documented contract.
#[cfg(feature = "strategy-loading")]
unsafe impl Send for DynamicStrategy {}
#[cfg(feature = "strategy-loading")]
unsafe impl Sync for DynamicStrategy {}

#[cfg(feature = "strategy-loading")]
impl Strategy for DynamicStrategy {
    fn name(&self) -> &str {
        &self.name
    }

    fn on_tick(&mut self, tick: &MarketTick) -> Option<Signal> {
        let on_tick = self.vtable.on_tick?;

        // Marshal the internal tick into the frozen C struct. Strings are kept
        // alive for the duration of the call via the CString locals below.
        let symbol = CString::new(tick.symbol.as_str()).ok()?;
        let asset = CString::new(tick.asset.as_str()).ok()?;
        let bid = CString::new(tick.best_bid.to_string()).ok()?;
        let ask = CString::new(tick.best_ask.to_string()).ok()?;
        let mid = CString::new(tick.mid.to_string()).ok()?;
        let c_tick = BkTick {
            symbol: symbol.as_ptr(),
            asset: asset.as_ptr(),
            best_bid: bid.as_ptr(),
            best_ask: ask.as_ptr(),
            mid: mid.as_ptr(),
            timestamp_ms: tick.timestamp_ms,
        };

        // SAFETY: the vtable declares this as an extern "C" fn taking a valid
        // handle created by this library and a pointer to a live tick.
        let out = unsafe { on_tick(self.handle, &c_tick) };

        // Copy the returned signal out immediately (borrowed pointers expire).
        let side = out.side;
        if side == BkSide::None {
            return None;
        }
        let sym = unsafe { cstr_to_string(out.symbol) }.unwrap_or_else(|| tick.symbol.clone());
        let price = unsafe { cstr_to_string(out.price) }.and_then(|s| Decimal::from_str(&s).ok())?;
        let size = unsafe { cstr_to_string(out.size) }.and_then(|s| Decimal::from_str(&s).ok());

        match side {
            BkSide::Buy => Some(Signal::Buy { symbol: sym, price, size: size.unwrap_or_else(|| Decimal::from(10)) }),
            BkSide::Sell => Some(Signal::Sell { symbol: sym, price }),
            BkSide::None => None,
        }
    }

    fn on_round(&mut self, slot: i64) {
        if let Some(f) = self.vtable.on_round {
            // SAFETY: same contract as on_tick; no pointers borrowed.
            unsafe { f(self.handle, slot) };
        }
    }
}

#[cfg(feature = "strategy-loading")]
impl Drop for DynamicStrategy {
    fn drop(&mut self) {
        if let Some(destroy) = self.vtable.destroy {
            // SAFETY: handle came from this library's create().
            unsafe { destroy(self.handle) };
        }
        // `_lib` (if any) drops here, unloading the library after destroy.
    }
}

/// Read a NUL-terminated C string into an owned `String` (None for null).
#[cfg(feature = "strategy-loading")]
unsafe fn cstr_to_string(p: *const std::ffi::c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(p) }.to_str().ok().map(|s| s.to_string())
}

/// High-level entry: load from a path and register into a strategy engine.
/// Kept here (not in the engine) so the engine stays free of `libloading`.
#[cfg(feature = "strategy-loading")]
pub fn load_and_register(
    engine: &mut crate::strategy_engine::StrategyEngine,
    path: &Path,
) -> LoadOutcome {
    match load_boxed(path) {
        Ok(loaded) => {
            engine.register(loaded.strategy, format!("dylib:{}", path.display()));
            LoadOutcome::Loaded { path: path.to_path_buf(), name: loaded.name, version: loaded.version }
        }
        Err(outcome) => outcome,
    }
}

/// A loaded user strategy, before it is registered anywhere.
#[cfg(feature = "strategy-loading")]
pub struct LoadedStrategy {
    pub strategy: Box<dyn crate::strategy_engine::Strategy>,
    pub name: String,
    pub version: String,
}

/// Load a user strategy library and hand back the instance boxed as the
/// user-layer `Strategy` contract. The caller decides which registry it lands
/// in: the self-driving engine's live dispatch, or the standalone engine.
#[cfg(feature = "strategy-loading")]
pub fn load_boxed(path: &Path) -> Result<LoadedStrategy, LoadOutcome> {
    match DynamicStrategy::load(path) {
        Ok(ds) => Ok(LoadedStrategy {
            name: ds.name().to_string(),
            version: ds.version.clone(),
            strategy: Box::new(ds),
        }),
        Err(reason) => Err(if policy_allows(path).is_err() {
            LoadOutcome::Rejected { path: path.to_path_buf(), reason }
        } else {
            LoadOutcome::Failed { path: path.to_path_buf(), reason }
        }),
    }
}

/// Without the feature, report the library as unsupported (builtins still run).
#[cfg(not(feature = "strategy-loading"))]
pub fn load_strategy(path: &Path) -> LoadOutcome {
    match policy_allows(path) {
        Ok(()) => LoadOutcome::Failed {
            path: path.to_path_buf(),
            reason: "dynamic strategy loading not compiled in (enable feature `strategy-loading`)".into(),
        },
        Err(reason) => LoadOutcome::Rejected { path: path.to_path_buf(), reason },
    }
}

#[cfg(feature = "strategy-loading")]
pub fn load_strategy(path: &Path) -> LoadOutcome {
    match DynamicStrategy::load(path) {
        Ok(ds) => LoadOutcome::Loaded {
            path: path.to_path_buf(),
            name: ds.name().to_string(),
            version: ds.version.clone(),
        },
        Err(reason) => {
            if policy_allows(path).is_err() {
                LoadOutcome::Rejected { path: path.to_path_buf(), reason }
            } else {
                LoadOutcome::Failed { path: path.to_path_buf(), reason }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_accepts_shared_libraries() {
        assert!(policy_allows(Path::new("strategies/dog_strategy.dylib")).is_ok());
        assert!(policy_allows(Path::new("strategies/dog_strategy.so")).is_ok());
    }

    #[test]
    fn policy_rejects_credential_like_and_non_libraries() {
        assert!(policy_allows(Path::new("strategies/private_key.dylib")).is_err());
        assert!(policy_allows(Path::new("strategies/secret.so")).is_err());
        assert!(policy_allows(Path::new("strategies/strategy.toml")).is_err());
    }

    #[test]
    fn missing_library_reports_failure_not_panic() {
        let out = load_strategy(Path::new("/nonexistent/dir/foo.dylib"));
        assert!(matches!(out, LoadOutcome::Failed { .. }), "got {out:?}");
    }
}

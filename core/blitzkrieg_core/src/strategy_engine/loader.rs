//! Dynamic strategy loader — dlopen user-layer strategy libraries (`.dylib` /
//! `.so` / `.dll`) implementing C ABI **v2** and wrap them as a full
//! [`crate::strategies::foreign::ForeignStrategy`] (which itself implements the
//! same `EngineStrategy` contract as an in-tree strategy).
//!
//! ARCHITECTURAL GUARANTEE: a loaded strategy only ever receives borrowed
//! read-only market/context views and returns intents as data. It has no handle
//! to credentials, the CLOB client, the order manager or the UDS socket — none
//! of those cross the dynamic boundary. [`policy_allows`] additionally refuses
//! credential-looking filenames before dlopen.
//!
//! Negotiation order is deliberate: policy → dlopen → read the version symbol
//! → read the vtable. Version is settled BEFORE the vtable layout is trusted, so
//! a v1 library is rejected on version and never misread as v2.
//!
//! Runtime flow:
//!   1. [`policy_allows`] — static path policy.
//!   2. `dlopen` the library.
//!   3. `bk_strategy_abi_version()` — MUST equal 2 (clean break, no v1 shim).
//!   4. resolve `bk_strategy_free_string` and `bk_strategy_create` → vtable.
//!   5. verify vtable.abi_version/min_abi and the REQUIRED hooks.
//!   6. `create()` → handle, then wrap as `ForeignStrategy`.

#[cfg(feature = "strategy-loading")]
use blitzkrieg_strategy_api::{
    bk_strategy_free_string, BkHandle, BkStrategyVtable, BK_ABI_VERSION, BK_CREATE_SYMBOL,
    BK_FREE_STRING_SYMBOL, BK_GATE_EXEMPTIONS_SYMBOL, BK_MIN_ABI_VERSION, BK_VERSION_SYMBOL,
};
use std::path::{Path, PathBuf};

/// Outcome of attempting to load a strategy library.
pub enum LoadOutcome {
    /// Policy rejected the path before any dlopen.
    Rejected { path: PathBuf, reason: String },
    /// Library opened, negotiated and registered.
    Loaded { path: PathBuf, name: String, version: String },
    /// Policy passed but load/negotiate/create failed.
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

/// A loaded, negotiated v2 strategy before registration.
#[cfg(feature = "strategy-loading")]
pub struct LoadedForeign {
    pub strategy: crate::strategies::foreign::ForeignStrategy,
    pub name: String,
    pub version: String,
    /// Shared entry gates this library declared it does not need (E2-b / #27).
    /// Default (nothing declared, or no such symbol) = fully gated.
    pub gate_exemptions: crate::strategies::GateExemptions,
}

/// Load and negotiate a v2 strategy library, returning it boxed as the full
/// `EngineStrategy` contract ready for `Engine::register_user_strategy`.
#[cfg(feature = "strategy-loading")]
pub fn load_foreign(path: &Path) -> Result<LoadedForeign, LoadOutcome> {
    let fail = |reason: String| -> Result<LoadedForeign, LoadOutcome> {
        Err(if policy_allows(path).is_err() {
            LoadOutcome::Rejected { path: path.to_path_buf(), reason }
        } else {
            LoadOutcome::Failed { path: path.to_path_buf(), reason }
        })
    };

    if let Err(reason) = policy_allows(path) {
        return Err(LoadOutcome::Rejected { path: path.to_path_buf(), reason });
    }

    // SAFETY: opening a shared library runs its initialisers. We only load
    // libraries the operator placed in the strategy directory after policy.
    let lib = match unsafe { libloading::Library::new(path) } {
        Ok(l) => l,
        Err(e) => return fail(format!("dlopen failed: {e}")),
    };

    // 1) Mandatory version symbol — negotiate before trusting the vtable layout.
    let version_fn = match unsafe {
        lib.get::<unsafe extern "C" fn() -> u32>(BK_VERSION_SYMBOL)
    } {
        Ok(f) => f,
        Err(_) => {
            return fail(format!(
                "missing symbol bk_strategy_abi_version; a v1/pre-v2 library? rebuild against strategy-api ABI v{BK_ABI_VERSION}"
            ))
        }
    };
    let reported = unsafe { version_fn() };
    if reported != BK_ABI_VERSION {
        return fail(format!(
            "ABI version mismatch: library exports {reported}, kernel requires {BK_ABI_VERSION}; \
             rebuild the strategy against strategy-api ABI v{BK_ABI_VERSION} (no v1 shim)"
        ));
    }

    // 2) JSON deallocator, resolved from THIS library.
    let free_string = unsafe {
        lib.get::<unsafe extern "C" fn(*mut std::ffi::c_char)>(BK_FREE_STRING_SYMBOL)
    }
    .ok()
    .map(|s| *s)
    .unwrap_or(bk_strategy_free_string as unsafe extern "C" fn(*mut std::ffi::c_char));

    // 2b) OPTIONAL per-strategy gate exemption declaration (E2-b / #27). Absent
    // symbol = nothing declared = fully gated, which is why adding this
    // capability needs no ABI bump (the vtable layout is untouched).
    let gate_exemptions_fn = unsafe {
        lib.get::<blitzkrieg_strategy_api::BkGateExemptionsFn>(BK_GATE_EXEMPTIONS_SYMBOL)
    }
    .ok()
    .map(|s| *s);

    // 3) Factory → vtable.
    let create_sym = match unsafe {
        lib.get::<unsafe extern "C" fn() -> *const BkStrategyVtable>(BK_CREATE_SYMBOL)
    } {
        Ok(s) => s,
        Err(e) => return fail(format!("missing symbol bk_strategy_create: {e}")),
    };
    let vt_ptr = unsafe { create_sym() };
    if vt_ptr.is_null() {
        return fail("bk_strategy_create returned null".into());
    }
    // SAFETY: library guarantees a pointer to a static vtable; copy by value.
    let vtable = unsafe { std::ptr::read(vt_ptr) };
    if vtable.abi_version != BK_ABI_VERSION {
        return fail(format!(
            "vtable ABI mismatch: vtable={}, kernel={BK_ABI_VERSION}", vtable.abi_version
        ));
    }
    if vtable.min_abi > BK_ABI_VERSION || vtable.abi_version < BK_MIN_ABI_VERSION {
        return fail(format!(
            "ABI range unsupported: library needs >= {}, speaks {}; kernel speaks {BK_ABI_VERSION}",
            vtable.min_abi, vtable.abi_version
        ));
    }

    // 4) Required hooks. Presence only — the copied vtable carries the pointers.
    let missing = |hook: &str| format!("v2 vtable missing required hook: {hook}");
    let required = [
        (vtable.create.is_some(), "create"),
        (vtable.destroy.is_some(), "destroy"),
        (vtable.on_book.is_some(), "on_book"),
        (vtable.on_round.is_some(), "on_round"),
        (vtable.evaluate.is_some(), "evaluate"),
    ];
    let missing_hooks: Vec<String> =
        required.into_iter().filter(|(present, _)| !*present).map(|(_, h)| missing(h)).collect();
    if !missing_hooks.is_empty() {
        return fail(missing_hooks.join("; "));
    }

    let name = unsafe { cstr_to_string(vtable.name) }.unwrap_or_else(|| "unnamed".into());
    let version = unsafe { cstr_to_string(vtable.version) }.unwrap_or_else(|| "0.0.0".into());

    // 5) Instance.
    let handle: BkHandle = unsafe { vtable.create.unwrap_or_else(|| unreachable!())() };
    if handle.is_null() {
        return fail("strategy create() returned a null handle".into());
    }

    // SAFETY: negotiation above established a v2 library with a static vtable,
    // valid handle and matching allocator; ForeignStrategy drives it single-
    // threaded and frees the handle in Drop.
    let strategy = unsafe {
        crate::strategies::foreign::ForeignStrategy::from_loaded(
            lib, vtable, handle, name.clone(), version.clone(), Some(free_string),
            gate_exemptions_fn,
        )
    };
    // Read the OPTIONAL declaration once, here, so the load report can state it
    // (E2-b / #27: an opt-out must be visible at registration, not only later).
    let gate_exemptions = crate::strategies::EngineStrategy::gate_exemptions(&strategy);
    Ok(LoadedForeign { strategy, name, version, gate_exemptions })
}

/// Read a NUL-terminated C string into an owned `String` (None for null).
#[cfg(feature = "strategy-loading")]
unsafe fn cstr_to_string(p: *const std::ffi::c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    unsafe { std::ffi::CStr::from_ptr(p) }.to_str().ok().map(|s| s.to_string())
}

/// Diagnostic summary of loading a library (does not register anywhere).
#[cfg(feature = "strategy-loading")]
pub fn load_strategy(path: &Path) -> LoadOutcome {
    match load_foreign(path) {
        Ok(LoadedForeign { name, version, .. }) => {
            LoadOutcome::Loaded { path: path.to_path_buf(), name, version }
        }
        Err(outcome) => outcome,
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

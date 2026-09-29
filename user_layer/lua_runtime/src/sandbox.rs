//! The Lua 5.4 sandbox (DEV_V0_3 §6.2 / E30 #336 task 1).
//!
//! One Lua state machine per strategy, built with the smallest stdlib set the
//! design allows and then DENY-BY-DEFAULT on top of it:
//!
//! * `Lua::new_with(StdLib::MATH | StdLib::STRING | StdLib::TABLE)` — `os`,
//!   `io`, `debug`, `package` and `coroutine` are never loaded (§6.2);
//! * the base library is ALWAYS opened by mlua itself (`_G`/`luaopen_base`,
//!   mlua `state/raw.rs`), which is exactly why the explicit nil blacklist
//!   below is load-bearing rather than decorative: `require`, `dofile`,
//!   `loadfile`, `load`, `loadstring`, `collectgarbage` and `string.dump`
//!   (the issue's list) all exist on the base surface and are struck here;
//! * `set_memory_limit(16 MB)` — the §6.2 allocation quota, enforced inside
//!   the Lua allocator, so a runaway table build fails as a deterministic
//!   `MemoryError`, never as a host OOM;
//! * an instruction hook at 10 000-instruction granularity: one host-side
//!   callback may execute at most [`INSTRUCTION_BUDGET`] (1e6) VM
//!   instructions before the hook **poisons first, errors second** (§6.3) —
//!   `pcall` can swallow the error, never the poison.
//!
//! Two quotas, one poison rule: every host entry point goes through
//! [`LuaSandbox::invoke`], which refuses before the VM is touched once
//! poisoned and classifies the failure afterwards. A plain Lua error (a
//! strategy bug — `require('io')` is `attempt to call a nil value`) is
//! deterministic and NOT poison: the host keeps calling, the strategy keeps
//! erroring, everything else keeps trading (acceptance A). Resource
//! exhaustion — instruction budget or the memory ceiling — IS poison: the
//! strategy is marked first, one [`PoisonHandle`] alert is recorded for the
//! host to raise exactly once, and every later call is refused host-side.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use mlua::{Error as LuaError, Function, HookTriggers, Lua, LuaOptions, StdLib, VmState};

/// §6.2: 16 MB per Lua state machine (`lua_gc` memory limit).
pub const MEMORY_LIMIT_BYTES: usize = 16 * 1024 * 1024;

/// §6.3: instruction budget per single host-side callback (1e6).
pub const INSTRUCTION_BUDGET: u64 = 1_000_000;

/// Hook granularity: the counter is checked every N VM instructions (§6.3).
const HOOK_INSTRUCTION_STEP: u32 = 10_000;

/// Globals struck from the base surface. `os`/`io`/`debug`/`package`/
/// `coroutine` are never loaded — they are listed here anyway so the
/// guarantee holds by construction, not by mlua's default.
const FORBIDDEN_GLOBALS: &[&str] = &[
    // The issue's explicit blacklist.
    "require",
    "dofile",
    "loadfile",
    "load",
    "loadstring",
    "collectgarbage",
    // Deny-by-default on the rest of base (§6.2's allow-list is pairs/ipairs/
    // type/tostring/tonumber/setmetatable/getmetatable/pcall/error/select/
    // next/rawget/rawset/rawequal/rawlen + the three loaded libraries).
    "print",
    "assert",
    "xpcall",
    "_G",
    // Never loaded, nil'd anyway: the sandbox must not DEPEND on mlua's
    // default staying put.
    "os",
    "io",
    "debug",
    "package",
    "coroutine",
];

/// One-shot poison record: the flag gates every call, the alert carries the
/// single RISK_ALERT message the host raises (§6.3: exactly once, naming the
/// strategy). Cloned into the loader, the adapter and the hook itself.
#[derive(Debug, Clone, Default)]
pub struct PoisonHandle {
    flag: Arc<AtomicBool>,
    alert: Arc<Mutex<Option<String>>>,
}

impl PoisonHandle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark poisoned (idempotent) and record the alert exactly once — the
    /// FIRST cause wins; later causes are the same strategy being broken the
    /// same way.
    pub fn poison(&self, cause: String) {
        self.flag.store(true, Ordering::SeqCst);
        let mut slot = self.alert.lock().expect("poison alert mutex");
        if slot.is_none() {
            *slot = Some(cause);
        }
    }

    pub fn is_poisoned(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// Drain the one-shot alert (None = nothing new). The flag stays set —
    /// draining the MESSAGE never re-enables the strategy.
    pub fn take_alert(&self) -> Option<String> {
        self.alert.lock().expect("poison alert mutex").take()
    }

    /// Peek without draining (diagnostics).
    pub fn alert(&self) -> Option<String> {
        self.alert.lock().expect("poison alert mutex").clone()
    }
}

/// How a guarded call ended. The distinction is the §6.3 rule: a plain Lua
/// error is the strategy's own deterministic bug (host continues), a resource
/// violation poisons the state machine (host refuses everything after).
#[derive(Debug)]
pub enum InvokeError {
    /// Refused BEFORE the VM ran: the state was already poisoned.
    Poisoned(String),
    /// The call poisoned the state (instruction budget / memory ceiling) and
    /// returned this error.
    PoisonedBy(String),
    /// A plain Lua error — deterministic, not poison. The host continues.
    LuaError(String),
}

impl InvokeError {
    pub fn message(&self) -> &str {
        match self {
            Self::Poisoned(m) | Self::PoisonedBy(m) | Self::LuaError(m) => m,
        }
    }

    pub fn is_poison(&self) -> bool {
        matches!(self, Self::Poisoned(_) | Self::PoisonedBy(_))
    }
}

/// The built Lua state machine plus its quotas. Not `Clone`: one sandbox per
/// strategy, exactly one owner ([`super::lua_strategy::LuaStrategy`]).
pub struct LuaSandbox {
    lua: Lua,
    poison: PoisonHandle,
    /// Cumulative VM instructions executed since the last [`LuaSandbox::invoke`]
    /// reset. The hook adds [`HOOK_INSTRUCTION_STEP`] each fire; the budget
    /// compare runs on the same thread as the reset (mlua callbacks are
    /// main-thread), so the atomic is belt-and-braces against future
    /// multi-thread hooks, not a correctness crutch today.
    instructions: Arc<AtomicU64>,
}

// Send/Sync: with mlua's `send` feature the Lua handle is genuinely
// `Send + Sync` (mlua swaps Rc for Arc and locks state access), so
// `LuaSandbox { lua: Lua, ... Arc atoms ... }` derives both honestly — no
// `unsafe impl` over a lie, unlike the C-ABI loader which holds raw pointers.
// The kernel's dispatch (`EngineStrategy: Send + Sync`) requires it.

impl LuaSandbox {
    /// Build the sandbox: whitelisted stdlib, nil blacklist, memory ceiling,
    /// instruction hook. Any failure here is a build-time error (the strategy
    /// never loads half-configured).
    pub fn new() -> Result<Self, LuaError> {
        let lua = Lua::new_with(
            StdLib::MATH | StdLib::STRING | StdLib::TABLE,
            LuaOptions::new(),
        )?;

        // §6.2 blacklist. The base library is always open (mlua opens `_G`/
        // `luaopen_base` unconditionally), so this loop is the actual gate for
        // require/dofile/loadfile/load/loadstring/collectgarbage.
        let globals = lua.globals();
        for name in FORBIDDEN_GLOBALS {
            globals.raw_set(*name, mlua::Value::Nil)?;
        }
        // The issue names `string.dump` separately: string itself stays.
        let string: mlua::Table = globals.raw_get("string")?;
        string.raw_set("dump", mlua::Value::Nil)?;

        // §6.2 quota 1: 16 MB per state machine, enforced in the allocator.
        lua.set_memory_limit(MEMORY_LIMIT_BYTES)?;

        let poison = PoisonHandle::new();
        let instructions = Arc::new(AtomicU64::new(0));
        let hook_poison = poison.clone();
        let hook_counter = Arc::clone(&instructions);
        lua.set_hook(
            HookTriggers::new().every_nth_instruction(HOOK_INSTRUCTION_STEP),
            move |_, _| {
                let executed = hook_counter.fetch_add(HOOK_INSTRUCTION_STEP as u64, Ordering::Relaxed)
                    + HOOK_INSTRUCTION_STEP as u64;
                if executed > INSTRUCTION_BUDGET {
                    // §6.3, in order: mark FIRST, error SECOND. pcall can
                    // catch the error below; it cannot unmark the flag.
                    hook_poison.poison(format!(
                        "instruction budget exceeded ({INSTRUCTION_BUDGET} VM instructions in one callback)"
                    ));
                    return Err(LuaError::runtime(format!(
                        "instruction budget exceeded ({INSTRUCTION_BUDGET} VM instructions in one callback); strategy poisoned"
                    )));
                }
                Ok(VmState::Continue)
            },
        )?;

        Ok(Self {
            lua,
            poison,
            instructions,
        })
    }

    /// Read-only view of the poison state for the host.
    pub fn poison_handle(&self) -> PoisonHandle {
        self.poison.clone()
    }

    pub fn is_poisoned(&self) -> bool {
        self.poison.is_poisoned()
    }

    /// Compile and run a chunk (defines the strategy's globals). The hook and
    /// memory ceiling cover loading too — a `while true do end` at top level
    /// dies here, before any callback exists.
    pub fn load(&self, code: &str, chunk_name: &str) -> Result<(), InvokeError> {
        self.invoke(&format!("load {chunk_name}"), |lua| {
            lua.load(code).set_name(chunk_name.to_string()).exec()
        })
    }

    /// Fetch a global function (`None` when absent or not a function).
    pub fn global_function(&self, name: &str) -> mlua::Result<Option<Function>> {
        self.lua.globals().get(name)
    }

    /// Borrow the VM for read-only host-side use (table building helpers).
    pub fn lua(&self) -> &Lua {
        &self.lua
    }

    /// THE entry point for every host-side call into the sandbox.
    ///
    /// Order of operations is the §6.3 contract:
    /// 1. poisoned already → refused without touching the VM (`Poisoned`);
    /// 2. reset the instruction budget (fresh budget per callback);
    /// 3. run;
    /// 4. on error, classify: the hook already poisoned (budget) or the error
    ///    is the memory ceiling → `PoisonedBy` (state is dead from now on);
    ///    anything else → `LuaError`, deterministic, host continues.
    pub fn invoke<T>(
        &self,
        what: &str,
        f: impl FnOnce(&Lua) -> mlua::Result<T>,
    ) -> Result<T, InvokeError> {
        if self.poison.is_poisoned() {
            return Err(InvokeError::Poisoned(match self.poison.alert() {
                Some(a) => a,
                None => format!("strategy state poisoned; {what} refused"),
            }));
        }
        // Fresh budget per host-side callback (§6.3: 1e6 per callback).
        self.instructions.store(0, Ordering::Relaxed);
        match f(&self.lua) {
            Ok(v) => {
                // The call may have RETURNED normally while still having
                // tripped the hook: `pcall(function() while true do end end)`
                // swallows the hook's error and completes. The poison flag is
                // the truth (§6.3: pcall swallows the error, never the
                // poison), so a success delivered by a poisoned state is
                // converted to the same refusal — acceptance C.
                if self.poison.is_poisoned() {
                    Err(InvokeError::PoisonedBy(format!(
                        "{what} returned after the state was poisoned; result discarded"
                    )))
                } else {
                    Ok(v)
                }
            }
            Err(e) => {
                let text = format!("{e}");
                let memory = matches!(e, LuaError::MemoryError(_));
                let poisoned_by_hook = self.poison.is_poisoned();
                if poisoned_by_hook {
                    Err(InvokeError::PoisonedBy(text))
                } else if memory {
                    // The allocator ceiling is the same class of violation as
                    // the instruction budget: resource exhaustion.
                    self.poison.poison(format!(
                        "memory limit exceeded ({MEMORY_LIMIT_BYTES} bytes) during {what}"
                    ));
                    Err(InvokeError::PoisonedBy(text))
                } else {
                    Err(InvokeError::LuaError(text))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox() -> LuaSandbox {
        LuaSandbox::new().expect("sandbox builds")
    }

    /// §6.2: the loaded surface is exactly math/string/table + the base
    /// allow-list; everything forbidden is a deterministic nil-call error.
    #[test]
    fn blacklist_globals_are_nil() {
        let sb = sandbox();
        for name in FORBIDDEN_GLOBALS {
            let v: mlua::Value = sb.lua().globals().raw_get(*name).expect("globals readable");
            assert!(matches!(v, mlua::Value::Nil), "{name} must be nil");
        }
        let string: mlua::Table = sb.lua().globals().raw_get("string").expect("string loaded");
        let dump: mlua::Value = string.raw_get("dump").expect("string.dump readable");
        assert!(matches!(dump, mlua::Value::Nil), "string.dump must be nil");
    }

    /// §6.2: the allow-list survives (the strategy can actually work).
    #[test]
    fn allowlist_still_works() {
        let sb = sandbox();
        sb.load(
            "local t = {3, 1, 2}; table.sort(t); \
             local sum = 0; for _, v in ipairs(t) do sum = sum + v end; \
             BK_TEST_SUM = sum; \
             BK_TEST_FMT = string.format('%.2f', tonumber('0.5') + math.pi)",
            "test",
        )
        .expect("allow-list code runs");
        let sum: i64 = sb.lua().globals().raw_get("BK_TEST_SUM").expect("sum");
        assert_eq!(sum, 6);
        let fmt: String = sb
            .lua()
            .globals()
            .raw_get("BK_TEST_FMT")
            .expect("formatted");
        assert!(fmt.starts_with("3.64"), "got {fmt}");
    }

    /// Acceptance A: `require('io')` / `os.time()` / `debug.getinfo` are
    /// deterministic Lua errors, not crashes, and NOT poison — the host
    /// continues calling.
    #[test]
    fn sandbox_escape_is_deterministic_non_poison_error() {
        let sb = sandbox();
        sb.load("function bk_evaluate() return require('io') end", "escape")
            .expect("defining functions is legal");
        let f: Function = sb
            .lua()
            .globals()
            .get("bk_evaluate")
            .expect("bk_evaluate present");
        let err = sb
            .invoke("evaluate", |_| f.call::<mlua::Value>(()))
            .expect_err("must fail");
        assert!(matches!(err, InvokeError::LuaError(_)), "got {err:?}");
        assert!(
            err.message().contains("attempt to call a nil value"),
            "deterministic nil-call error, got: {}",
            err.message()
        );
        assert!(!sb.is_poisoned(), "a plain Lua error is not poison");
        // Deterministic: the same call fails the same way again.
        let err2 = sb
            .invoke("evaluate", |_| f.call::<mlua::Value>(()))
            .expect_err("still fails");
        assert!(matches!(err2, InvokeError::LuaError(_)));
    }

    /// Acceptance B: an infinite loop burns the budget → poison → the next
    /// call is refused WITHOUT executing.
    #[test]
    fn infinite_loop_poisons_and_later_calls_are_refused() {
        let sb = sandbox();
        sb.load("function bk_evaluate() while true do end end", "loop")
            .expect("loads");
        let f: Function = sb
            .lua()
            .globals()
            .get("bk_evaluate")
            .expect("bk_evaluate present");
        let err = sb
            .invoke("evaluate", |_| f.call::<mlua::Value>(()))
            .expect_err("budget kills the loop");
        assert!(matches!(err, InvokeError::PoisonedBy(_)), "got {err:?}");
        assert!(sb.is_poisoned());
        let alert = sb.poison_handle().take_alert();
        assert!(
            alert
                .expect("one alert")
                .contains("instruction budget exceeded"),
            "alert names the budget"
        );
        // Refused host-side: the function is never entered again.
        let refused = sb
            .invoke("evaluate", |_| f.call::<mlua::Value>(()))
            .expect_err("refused");
        assert!(
            matches!(refused, InvokeError::Poisoned(_)),
            "got {refused:?}"
        );
    }

    /// Acceptance C: pcall swallows the hook's error, never the poison.
    #[test]
    fn pcall_cannot_swallow_poison() {
        let sb = sandbox();
        sb.load(
            "function bk_evaluate() local ok = pcall(function() while true do end end) return {entries={}, exits={}, breaks={}} end",
            "pcall",
        )
        .expect("loads");
        let f: Function = sb
            .lua()
            .globals()
            .get("bk_evaluate")
            .expect("bk_evaluate present");
        let err = sb
            .invoke("evaluate", |_| f.call::<mlua::Value>(()))
            .expect_err("poison fires through pcall");
        assert!(
            matches!(err, InvokeError::PoisonedBy(_)),
            "hook poison wins over pcall, got {err:?}"
        );
        assert!(sb.is_poisoned());
    }

    /// Acceptance D: a single allocation past the ceiling (20 MB string in a
    /// 16 MB state) dies on the memory limit with a deterministic error
    /// (poison — resource exhaustion, same class as the budget). One call, so
    /// the instruction budget cannot fire first.
    #[test]
    fn oversized_allocation_hits_memory_limit() {
        let sb = sandbox();
        sb.load(
            "function bk_grow() local big = string.rep('x', 20 * 1024 * 1024) return #big end",
            "grow",
        )
        .expect("loads");
        let f: Function = sb.lua().globals().get("bk_grow").expect("bk_grow present");
        let err = sb
            .invoke("grow", |_| f.call::<i64>(()))
            .expect_err("memory ceiling");
        assert!(matches!(err, InvokeError::PoisonedBy(_)), "got {err:?}");
        assert!(sb.is_poisoned());
    }

    /// Loading is covered by the same quotas: top-level `while true` dies at
    /// load, before any function exists.
    #[test]
    fn top_level_runaway_dies_at_load() {
        let sb = sandbox();
        let err = sb
            .load("while true do end", "toplevel")
            .expect_err("budget kills the loader");
        assert!(matches!(err, InvokeError::PoisonedBy(_)), "got {err:?}");
        assert!(sb.is_poisoned());
    }

    /// P3 (§16.5): per-callback latency under the sandbox quotas is MEASURED
    /// via `--nocapture`, and the realistic-callback p99 carries the only NEW
    /// perf threshold 0.3 adds: **≤ 5 ms** (10× headroom against the 50 ms
    /// engine tick). Two shapes are timed in the steady state:
    /// a real-work evaluate (table building + arithmetic — what a legitimate
    /// strategy does every round), and the WORST legal callback — a tight
    /// loop that burns just under the full 1e6-instruction budget before the
    /// hook would poison it (the bound a misbehaving-but-legal strategy
    /// imposes on the host thread). Numbers land in `docs/perf/V0_3.md`.
    #[test]
    fn callback_latency_p50_p99_measured() {
        let sb = sandbox();
        sb.load(
            "function bk_evaluate(book) \
             local entries = {} \
             for i = 1, 20 do \
             entries[i] = { price = book.mid - i * 0.001, size = i, reason = 'grid' } \
             end \
             return { entries = entries, exits = {}, breaks = {} } \
             end",
            "bench",
        )
        .expect("bench callback loads");
        let f: Function = sb.lua().globals().get("bk_evaluate").expect("present");
        let table = sb.lua().create_table().expect("book table");
        table.raw_set("mid", 0.50).expect("book.mid");
        let book = mlua::Value::Table(table);
        let run = || sb.invoke("evaluate", |_| f.call::<mlua::Value>(book.clone()));

        // Warm caches, then time the steady state.
        for _ in 0..200 {
            let _ = run();
        }
        let mut samples: Vec<u128> = Vec::with_capacity(5_000);
        for _ in 0..5_000 {
            let t0 = std::time::Instant::now();
            let _ = run();
            samples.push(t0.elapsed().as_micros());
        }
        samples.sort_unstable();
        let p = |q: f64| -> u128 {
            let idx = ((samples.len() as f64) * q).ceil() as usize;
            samples[(idx - 1).min(samples.len() - 1)]
        };
        let p99_us = p(0.99);
        println!(
            "lua callback latency over {} runs (this build): p50={}us p99={}us max={}us",
            samples.len(),
            p(0.50),
            p99_us,
            samples[samples.len() - 1]
        );
        // THE threshold (§16.5 P3). Exceeded → lower the instruction budget
        // and record the new number — do not widen the line.
        assert!(
            p99_us <= 5_000,
            "P3 violated: realistic-callback p99 {p99_us}us > 5000us — \
             lower the instruction budget and record the measured value"
        );

        // The worst LEGAL callback: ~95k loop iterations ≈ just under the
        // 1e6-instruction budget. It must RETURN (not poison) and cost less
        // than one engine tick even in a debug build — that bound is what
        // keeps a budget-burning strategy from stalling the 50 ms loop.
        let burn = sandbox();
        burn.load(
            "function bk_burn() local i = 0 while i < 95000 do i = i + 1 end return i end",
            "burn",
        )
        .expect("burn callback loads");
        let g: Function = burn.lua().globals().get("bk_burn").expect("present");
        let mut burn_us: Vec<u128> = Vec::with_capacity(200);
        for _ in 0..200 {
            let t0 = std::time::Instant::now();
            let r = burn
                .invoke("burn", |_| g.call::<i64>(()))
                .expect("95000 iterations stay under the budget");
            assert_eq!(r, 95_000);
            burn_us.push(t0.elapsed().as_micros());
        }
        burn_us.sort_unstable();
        println!(
            "lua full-budget callback (≈0.95e6 instructions): p50={}us p99={}us max={}us (this build)",
            burn_us[burn_us.len() / 2],
            burn_us[(burn_us.len() as f64 * 0.99).ceil() as usize - 1],
            burn_us[burn_us.len() - 1]
        );
        assert!(
            burn_us[burn_us.len() - 1] <= 50_000,
            "a budget-bounded callback must cost less than one 50ms engine tick, \
             got max {}us",
            burn_us[burn_us.len() - 1]
        );
    }
}

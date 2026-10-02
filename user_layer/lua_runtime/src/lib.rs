//! Lua 5.4 sandbox host for user-authored strategies (DEV_V0_3 §6 / E30
//! #336).
//!
//! Three modules, one direction:
//!
//! * [`sandbox`] — the Lua 5.4 state machine: whitelisted stdlib
//!   (`math`/`string`/`table` + the base allow-list), the explicit nil
//!   blacklist (`require`/`dofile`/`loadfile`/`load`/`loadstring`/
//!   `collectgarbage`/`string.dump`, plus `os`/`io`/`debug`/`package`/
//!   `coroutine`), the 16 MB memory ceiling and the 1e6-instruction budget
//!   with the §6.3 poison rule (mark first, error second; `pcall` swallows
//!   the error, never the poison).
//! * [`bk_api`] — the read-only `bk.*` surface plus the suggested
//!   entry-point contract (`bk_evaluate` required; `bk_on_book`/
//!   `bk_on_round`/`bk_on_kline` optional). Everything under `bk` projects
//!   host state; nothing under it can move money.
//! * [`lua_strategy`] — `LuaStrategy: SafeStrategy`, the host-facing
//!   contract: marshals host data into the `bk` snapshot, calls the entry
//!   points inside the sandbox guard, parses the intent tables, records
//!   health, and surfaces the one-shot poison alert for the core to raise as
//!   a single RISK_ALERT naming the strategy.
//!
//! Dependency direction (§11.1): `lua_runtime ──► strategy_api` (type
//! alignment only). This crate must never depend on `blitzkrieg-core`; the
//! core's loader/adapter lives on the core side and calls into THIS crate.

pub mod bk_api;
pub mod lua_strategy;
pub mod sandbox;

// The loader-facing surface, at the crate root: the core imports these by
// short name (`blitzkrieg_lua_runtime::{LuaStrategy, PoisonHandle}`), and the
// one-import rule keeps the core/lua_runtime seam auditable in a glance.
pub use bk_api::{AccountView, BkState, FeeScheduleView};
pub use lua_strategy::{LuaHealth, LuaStrategy};
pub use sandbox::{InvokeError, PoisonHandle};

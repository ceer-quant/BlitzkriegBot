//! Lua 5.4 sandbox host for user-authored strategies — **Wave 0 shell**.
//!
//! DEV_V0_3 §14.0 (#329): this crate exists in Wave 0 with its dependency tree
//! (`mlua`, Lua 5.4, vendored — §6.1) and nothing else. The sandbox
//! (`sandbox.rs`), the read-only `bk.*` surface (`bk_api.rs`) and
//! `LuaStrategy: SafeStrategy` (`lua_strategy.rs`) land with **E30** (§14.1).
//!
//! Why the empty crate ships FIRST: `Cargo.lock` is one file shared by every
//! branch and CI builds with `--locked`, so introducing mlua late would make
//! seven parallel branches rebase the same lock hunk. Landing it once, here,
//! costs one extra compile of mlua per workspace build (§14.0) — paid
//! deliberately.
//!
//! Dependency direction (§11.1): `lua_runtime ──► strategy_api` (type
//! alignment only). This crate must never depend on `blitzkrieg-core`.
//!
//! This crate intentionally has NO items in Wave 0: the freeze it delivers is
//! the `Cargo.lock` entry (mlua + lua-src + mlua-sys + luajit-src + which), and
//! an empty surface leaves E30 nothing to reconcile.

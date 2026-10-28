//! Boot-support items for the `blitzkrieg-core` bin target (#699).
//!
//! `main.rs` grew past every sane review budget, so its three independent
//! columns moved into files of their own — a pure code move, byte-for-byte.
//! `boot` carries the CLI/env/config argument surface (`Args`,
//! `parse_args`, …); `subcommands` carries the offline subcommand
//! implementations (`run_backtest`, …). The `#[cfg(test)]` suite now lives
//! in `bin_tests.rs`, declared from `main.rs`'s root, so its `use super::*`
//! keeps resolving to the bin-root imports.
//!
//! Nothing here is reachable from the library: this module is declared only
//! by `main.rs`, so the bin keeps its own private boot layer beside
//! `blitzkrieg_core::cli` (the shared flag table, #228) — the boundary is
//! unchanged by this split.

pub(crate) mod boot;
pub(crate) mod subcommands;

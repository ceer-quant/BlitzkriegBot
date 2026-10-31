//! Command dispatch — the UI Kit's replacement for the Node `/crypto-hft`
//! command surface (`src/skills/bundled/crypto-hft/index.ts`, deleted with the
//! rest of the source layer in `62b16c88`).
//!
//! Supported verbs (identical in meaning to the Node skill's Rust-core path,
//! which this module now owns outright) are listed exactly once — in
//! [`COMMANDS`], the table that [`help_text`], the TUI overlay and the command
//! bar all read.
//!
//! No entry-order verb exists here. `stop`/`start` act on the process, while
//! `flatten` is the explicit human-supervision exit path and delegates to the
//! core's existing `positions.exit` RPC. All trading decisions stay in the core.

mod dispatcher;
mod parse;

pub use dispatcher::*;
pub use parse::*;

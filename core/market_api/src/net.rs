//! `blitzkrieg-market-api::net` — compatibility facade over the canonical
//! `blitzkrieg-bk-contract::net` module (issue #840 Wave 1).
//!
//! Every type and function that lived here moved verbatim into the contract
//! crate (they are std-only — the dependency-free rules the CLI, TUI and WebUI
//! must agree on). This facade re-exports the whole module so every
//! `blitzkrieg_market_api::net::…` import path keeps compiling with a single
//! definition behind it.

pub use blitzkrieg_bk_contract::net::*;

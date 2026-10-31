//! Web adapter — renders a `UiSnapshot` to HTML (and JSON endpoints) and serves
//! it from a dependency-free HTTP/1.1 server so the browser panel can be opened
//! directly. This is the browser-facing sibling of the TUI and app adapters; it
//! contains no trading logic.
//!
//! When constructed with a [`Dispatcher`] (gateway mode) the server additionally
//! exposes the command surface:
//!
//!   GET  /                    HTML panel (auto-refresh)
//!   GET  /api/snapshot        JSON snapshot
//!   GET  /api/command?cmd=..  dispatch one command → JSON outcome
//!   POST /api/command         body = command text (or `{"cmd":"..."}`) → JSON
//!
//! Command dispatch is trading-logic-free: it starts/stops the *process* and
//! reads state. No order is ever placed from here.

use crate::core::ipc_client::IpcClient;
use crate::core::types::UiSnapshot;
use crate::gateway::Dispatcher;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

pub mod gateway_run;
pub use gateway_run::run_web_gateway;

mod auth;
mod checks;
mod http;
mod render;
mod routes;
mod server;

use auth::*;
use checks::*;
use http::*;
pub use render::*;
pub use server::*;

#[cfg(test)]
mod auth_tests;

#[cfg(test)]
mod evolution_tests;

#[cfg(test)]
mod safety_tests;

#[cfg(test)]
mod tests;

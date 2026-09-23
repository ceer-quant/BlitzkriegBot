//! UI Kit **gateway** — the command-dispatch + minimal-gateway layer that let
//! the Node shell be removed (D-4, step ②; the shell is now gone, `62b16c88`).
//!
//! Two responsibilities, both deliberately trading-logic-free:
//!
//! * [`supervisor`] — spawn / stop / adopt the `blitzkrieg-core` child process,
//!   i.e. the lifecycle half of the deleted Node `BlitzkriegCoreClient`.
//! * [`command`] — parse and dispatch `/crypto-hft`-equivalent verbs
//!   (`start|stop|status|positions`) and render their results.
//!
//! This module does NOT place, cancel or size any order. The `web` adapter
//! serves these over HTTP; the TUI/app adapters may call [`command::Dispatcher`]
//! directly.

pub mod command;
pub mod supervisor;

pub use command::{
    command_lines, command_verbs, help_text, parse_command, Command, CommandOutcome, Dispatcher,
    COMMANDS,
};
pub use supervisor::{
    discover_binary, socket_served, StartOutcome, StopOutcome, Supervisor, SupervisorConfig,
    SupervisorError,
};

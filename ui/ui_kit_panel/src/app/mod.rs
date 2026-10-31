//! Panel application state — pure data + key handling. Rendering is in `ui.rs`.

use blitzkrieg_ui_kit::core::types::{EvolutionProposalView, NetCheckReportView};
use blitzkrieg_ui_kit::gateway::command_verbs;
use blitzkrieg_ui_kit::UiSnapshot;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::time::Instant;

mod keys;
mod selection;
mod snapshot;
mod state;

pub use state::*;

#[cfg(test)]
mod tests;

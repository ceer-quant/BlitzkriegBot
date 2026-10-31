//! ratatui rendering for the panel. Pure functions over `App` — no I/O.

use crate::app::{App, CheckStage, Tab, HINTS};
use blitzkrieg_ui_kit::core::types::EvolutionProposalView;
use blitzkrieg_ui_kit::gateway::command_lines;
use blitzkrieg_ui_kit::UiSnapshot;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, Tabs, Wrap};

const GREEN: Color = Color::Green;
const RED: Color = Color::Red;
const DIM: Color = Color::DarkGray;
const ACCENT: Color = Color::Cyan;
/// The default foreground, as a name — rule rows that are ON sit in it so an
/// OFF row's DIM reads as a deliberate tier, not a rendering accident.
const FG: Color = Color::Reset;
/// The attention colour: noticed, but not a failure. It exists as a name because
/// the WebUI renders the same facts at the same volume (see `tui-parity`), and
/// "which tier is this fact in" is a decision the two faces have to share — a
/// bare `Color::Yellow` at the call site is how the two sides drifted apart.
const WARN: Color = Color::Yellow;

mod chrome;
mod evolution;
mod overlays;
mod panes;
mod settings;

pub use chrome::render;

#[cfg(test)]
mod tests;

fn centered_rect(area: Rect, pct_x: u16, height: u16) -> Rect {
    let w = area.width * pct_x / 100;
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect {
        x,
        y,
        width: w.min(area.width),
        height: height.min(area.height),
    }
}

fn short_id(id: &str) -> String {
    let s: String = id.chars().take(16).collect();
    if id.chars().count() > 16 {
        format!("{s}…")
    } else {
        s
    }
}

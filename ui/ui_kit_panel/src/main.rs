//! `ui_kit_panel` — interactive TUI panel for the Blitzkrieg trading core.
//!
//! Stack: **ratatui + crossterm + tokio**, over the `blitzkrieg-ui-kit` data
//! layer (`UiSnapshot` + gateway `Dispatcher`). It renders core state and
//! dispatches the same `start|stop|status|positions` commands as the web
//! gateway — it never places orders.
//!
//! Usage:
//!   ui_kit_panel [--socket <path>] [--interval-ms N] [--manage] [--attach] [--tab 1|2|3]

use blitzkrieg_ui_panel::{parse_args, run_panel};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let args = parse_args();
    run_panel(args).await
}

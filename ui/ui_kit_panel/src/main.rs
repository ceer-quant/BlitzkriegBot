//! `ui_kit_panel` — interactive TUI panel for the Blitzkrieg trading core.
//!
//! Stack: **ratatui + crossterm + tokio**, over the `blitzkrieg-ui-kit` data
//! layer (`UiSnapshot` + gateway `Dispatcher`). It renders core state and
//! dispatches the same `start|stop|status|positions` commands as the web
//! gateway — it never places orders.
//!
//! Usage:
//!   ui_kit_panel [--socket <path>] [--interval-ms N] [--manage] [--tab N]
//!
//! Keys:
//!   q / Ctrl-C   quit          :  focus command bar
//!   1 2 3 / Tab  switch view   r  refresh now
//!   Enter        run command   Esc cancel command

mod app;
mod input;
mod ui;

use app::{Action, App, Tab};
use blitzkrieg_ui_kit::gateway::{Dispatcher, SupervisorConfig};
use blitzkrieg_ui_kit::UiSnapshot;
use crossterm::event::{Event, KeyEventKind};
use ratatui::DefaultTerminal;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc::{self};

/// Messages the main loop reacts to.
enum Msg {
    Input(Event),
    Snapshot { snap: UiSnapshot, managed: bool, pid: Option<u32> },
    CommandDone(String),
    /// Refresh failed to run (join error); surface it in the log.
    RefreshError(String),
}

struct Args {
    socket: String,
    interval_ms: u64,
    manage: bool,
    tab: Tab,
}

fn parse_args() -> Args {
    let mut socket = blitzkrieg_ui_kit::resolve_socket_path();
    let mut interval_ms = 1000u64;
    // Lifecycle verbs are opt-in, exactly like the web gateway's `--manage`.
    let mut manage = std::env::var("UIKIT_MANAGE").map(|v| v == "1" || v == "true").unwrap_or(false);
    let mut tab = Tab::Overview;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--socket" => {
                if let Some(s) = args.next() {
                    socket = s;
                }
            }
            "--interval-ms" => {
                if let Some(v) = args.next().and_then(|v| v.parse().ok()) {
                    interval_ms = v;
                }
            }
            "--manage" => manage = true,
            "--tab" => {
                tab = match args.next().as_deref() {
                    Some("2") | Some("positions") => Tab::Positions,
                    Some("3") | Some("trades") => Tab::Trades,
                    _ => Tab::Overview,
                }
            }
            "--help" | "-h" => {
                println!("{}", HELP);
                std::process::exit(0);
            }
            other => eprintln!("ignoring unknown arg: {other}"),
        }
    }
    Args { socket, interval_ms: interval_ms.max(100), manage, tab }
}

const HELP: &str = "\
ui_kit_panel — Blitzkrieg interactive TUI panel (ratatui + crossterm + tokio)

USAGE:
  ui_kit_panel [--socket <path>] [--interval-ms N] [--manage] [--tab 1|2|3]

FLAGS:
  --socket       Core UDS socket (default: $TMPDIR/blitzkrieg-core-$USER.sock)
  --interval-ms  Snapshot refresh interval (default 1000)
  --manage       Enable lifecycle commands (start/stop). Off by default.
  --tab          Initial view: 1 Overview (default), 2 Positions, 3 Trades

KEYS:
  q / Ctrl-C  quit          :  focus command bar
  1 2 3 / Tab switch view   r  refresh now
  Enter       run command   Esc cancel command

COMMANDS (in the command bar):
  status | positions [N] | help
  start [ASSETS] [--size N] [--dry-run] | stop   (requires --manage)
";

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let args = parse_args();

    let cfg = SupervisorConfig::from_env(args.socket.clone());
    let dispatcher = Arc::new(Mutex::new(Dispatcher::new(cfg, args.manage)));

    // Terminal setup: raw mode + alternate screen; a panic hook restores it.
    let mut terminal: DefaultTerminal = ratatui::init();

    let mut app = App::new(args.socket.clone(), args.manage);
    app.tab = args.tab;
    app.log("ui_kit_panel started. Press : to run a command, q to quit.");
    if !args.manage {
        app.log("lifecycle disabled — restart with --manage to allow start/stop");
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();

    // Input thread (crossterm event reader).
    {
        let tx = tx.clone();
        std::thread::spawn(move || input::read_loop(tx, Msg::Input));
    }

    // Background refresher: read the core snapshot off the async runtime.
    {
        let d = dispatcher.clone();
        let tx = tx.clone();
        let interval = Duration::from_millis(args.interval_ms);
        tokio::spawn(async move {
            loop {
                let d = d.clone();
                match tokio::task::spawn_blocking(move || {
                    let mut g = match d.lock() {
                        Ok(g) => g,
                        Err(_) => return None,
                    };
                    Some((g.snapshot(), g.managed(), g.pid()))
                })
                .await
                {
                    Ok(Some((snap, managed, pid))) => {
                        if tx.send(Msg::Snapshot { snap, managed, pid }).is_err() {
                            return;
                        }
                    }
                    Ok(None) => return,
                    Err(e) => {
                        let _ = tx.send(Msg::RefreshError(e.to_string()));
                    }
                }
                tokio::time::sleep(interval).await;
            }
        });
    }

    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        terminal.draw(|f| ui::render(f, &app))?;

        let msg = tokio::select! {
            maybe = rx.recv() => {
                match maybe {
                    Some(m) => m,
                    None => break,
                }
            }
            _ = tick.tick() => {
                // Redraw only (keeps the "updated Xs ago" header fresh).
                continue;
            }
        };

        match msg {
            Msg::Input(Event::Key(key)) if key.kind != KeyEventKind::Release => {
                match app.on_key(key) {
                    Action::Quit => app.should_quit = true,
                    Action::Refresh => { /* the refresher will fire; nothing to do */ }
                    Action::RunCommand(cmd) => {
                        let d = dispatcher.clone();
                        let tx = tx.clone();
                        tokio::spawn(async move {
                            let out = tokio::task::spawn_blocking(move || {
                                let mut g = match d.lock() {
                                    Ok(g) => g,
                                    Err(_) => return String::from("dispatcher poisoned"),
                                };
                                let o = g.dispatch_line(&cmd);
                                format!("{} {}", if o.ok { "OK " } else { "ERR" }, o.message)
                            })
                            .await
                            .unwrap_or_else(|e| format!("command failed: {e}"));
                            let _ = tx.send(Msg::CommandDone(out));
                        });
                    }
                    Action::None => {}
                }
            }
            Msg::Input(_) => {}
            Msg::Snapshot { snap, managed, pid } => app.on_snapshot(snap, managed, pid),
            Msg::CommandDone(line) => {
                for l in line.lines() {
                    app.log(l.to_string());
                }
            }
            Msg::RefreshError(e) => app.log(format!("refresh error: {e}")),
        }

        if app.should_quit {
            break;
        }
    }

    ratatui::restore();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_quit_and_tabs() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = App::new("/tmp/x.sock".into(), false);
        assert!(matches!(app.on_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)), Action::Quit));
        assert_eq!(app.tab, Tab::Overview);
        app.on_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
        assert_eq!(app.tab, Tab::Positions);
        app.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.tab, Tab::Trades);
    }

    #[test]
    fn command_bar_collects_and_runs() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = App::new("/tmp/x.sock".into(), true);
        app.on_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE));
        assert!(app.input_active);
        for c in "status".chars() {
            app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        match app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)) {
            Action::RunCommand(c) => assert_eq!(c, "status"),
            _ => panic!("expected RunCommand"),
        }
        assert!(!app.input_active);
    }
}

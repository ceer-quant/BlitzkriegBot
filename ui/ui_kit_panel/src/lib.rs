//! `blitzkrieg_ui_panel` — interactive TUI panel library for the Blitzkrieg trading core.
//!
//! Stack: **ratatui + crossterm + tokio**, over the `blitzkrieg-ui-kit` data
//! layer (`UiSnapshot` + gateway `Dispatcher`). It renders core state and
//! dispatches the same `start|stop|status|positions` commands as the web
//! gateway — it never places orders.

pub mod app;
pub mod env_file;
pub mod input;
pub mod stop_stack;
pub mod ui;

pub use app::{Action, App, Tab};
use blitzkrieg_ui_kit::gateway::{Dispatcher, SupervisorConfig};
use blitzkrieg_ui_kit::UiSnapshot;
use crossterm::event::{Event, KeyEventKind};
use ratatui::DefaultTerminal;
use std::io::IsTerminal;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

/// Messages the main loop reacts to.
pub enum Msg {
    Input(Event),
    Snapshot {
        snap: UiSnapshot,
        managed: bool,
        pid: Option<u32>,
    },
    CommandDone(String),
    /// Refresh failed to run (join error); surface it in the log.
    RefreshError(String),
    /// The plugin registry was re-read (after entering the tab or a toggle).
    PluginsLoaded(UiSnapshot),
    /// A decoded core-event batch arrived on the EventBus.
    Events(Vec<blitzkrieg_ui_kit::core::types::CoreEvent>),
    /// The process received an external termination signal.
    Shutdown,
}

#[derive(Debug, Clone)]
pub struct PanelArgs {
    pub socket: String,
    pub interval_ms: u64,
    pub manage: bool,
    pub tab: Tab,
}

pub fn parse_args() -> PanelArgs {
    parse_args_from(std::env::args().skip(1))
}

pub fn parse_args_from(args: impl IntoIterator<Item = String>) -> PanelArgs {
    let mut socket = blitzkrieg_ui_kit::resolve_socket_path();
    let mut interval_ms = 1000u64;
    // Lifecycle verbs are opt-in, exactly like the web gateway's `--manage`.
    let mut manage = std::env::var("UIKIT_MANAGE")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false);
    let mut tab = Tab::Overview;
    let mut iter = args.into_iter();
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--socket" => {
                if let Some(s) = iter.next() {
                    socket = s;
                }
            }
            "--interval-ms" => {
                if let Some(v) = iter.next().and_then(|v| v.parse().ok()) {
                    interval_ms = v;
                }
            }
            "--manage" => manage = true,
            "--attach" => {
                // Attach explicitly to an existing running core; lifecycle is disabled.
                manage = false;
            }
            "--tab" => {
                tab = match iter.next().as_deref() {
                    Some("2") | Some("positions") => Tab::Positions,
                    Some("3") | Some("trades") => Tab::Trades,
                    Some("4") | Some("plugins") => Tab::Plugins,
                    Some("5") | Some("evolution") => Tab::Evolution,
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
    PanelArgs {
        socket,
        interval_ms: interval_ms.max(100),
        manage,
        tab,
    }
}

pub const HELP: &str = "\
ui_kit_panel — Blitzkrieg interactive TUI panel (ratatui + crossterm + tokio)

USAGE:
  ui_kit_panel [--socket <path>] [--interval-ms N] [--manage] [--attach] [--tab 1|2|3|4|5]

FLAGS:
  --socket       Core UDS socket (default: $TMPDIR/blitzkrieg-core-$USER.sock)
  --interval-ms  Snapshot refresh interval (default 1000)
  --manage       Enable lifecycle commands (start/stop). Off by default.
  --attach       Attach to an existing core in monitor mode (disables lifecycle)
  --tab          Initial view: 1 Overview (default), 2 Positions, 3 Trades, 4 Plugins, 5 Evolution

KEYS:
  q / Ctrl-C  quit          :  focus command bar
  1-5 / Tab   switch view   r  refresh now
  ↑/↓         move selection (Plugins / Evolution)
  a x d       Evolution: accept / reject / defer the selected proposal
  e u         Evolution: toggle auto-evolve / rollback selected strategy
  Enter       run command   Esc cancel command

COMMANDS (in the command bar):
  status | positions [N] | proposals [N] | decide <id> accept|reject|defer
  auto-evolve on|off | rollback <strategy> | help
  start [ASSETS] [--size N] [--dry-run] | stop   (requires --manage)
";

pub async fn run_panel(args: PanelArgs) -> std::io::Result<()> {
    let cfg = SupervisorConfig::from_env(args.socket.clone());
    let dispatcher = Arc::new(Mutex::new(Dispatcher::new(cfg, args.manage)));
    run_panel_with_dispatcher(args, dispatcher).await
}

pub async fn run_panel_with_dispatcher(
    args: PanelArgs,
    dispatcher: Arc<Mutex<Dispatcher>>,
) -> std::io::Result<()> {
    // ratatui::init() panics without a real terminal (raw-mode ioctl fails on a
    // plain pipe). In headless output (a script piping stdout) the panel is not
    // usable anyway — fail with guidance instead of a stack trace.
    if !std::io::stdout().is_terminal() {
        eprintln!(
            "ui_kit_panel needs an interactive terminal (stdout is not a TTY). \
             Quitting — this launcher is safe to rerun under a real terminal, \
             e.g. directly or via `bash scripts/tui-demo.sh`."
        );
        return Ok(());
    }

    // Terminal setup: raw mode + alternate screen; a panic hook restores it.
    let mut terminal: DefaultTerminal = ratatui::init();

    let mut app = App::new(args.socket.clone(), args.manage);
    app.tab = args.tab;
    app.log("ui_kit_panel started. Press : to run a command, q to quit.");
    if !args.manage {
        app.log("lifecycle disabled — restart with --manage to allow start/stop");
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();

    // Push channel: a dedicated listener connection feeds the shared EventBus;
    // the main loop drains it and refreshes what actually changed.
    let bus = blitzkrieg_ui_kit::core::event_bus::EventBus::new(1024);
    {
        let reader = blitzkrieg_ui_kit::core::notifier::NotificationReader::new(
            args.socket.clone(),
            bus.clone(),
            1000,
        );
        std::thread::spawn(move || reader.run());
    }
    {
        let bus = bus.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            let sub = bus.subscribe();
            loop {
                let drained = tokio::task::spawn_blocking({
                    let bus = bus.clone();
                    let mut sub = sub.clone();
                    move || bus.drain_new(&mut sub)
                })
                .await
                .unwrap_or_default();
                if !drained.is_empty() && tx.send(Msg::Events(drained)).is_err() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        });
    }

    // Input thread (crossterm event reader).
    {
        let tx = tx.clone();
        std::thread::spawn(move || input::read_loop(tx, Msg::Input));
    }
    {
        let tx = tx.clone();
        tokio::spawn(async move {
            use tokio::signal::unix::{signal, SignalKind};
            let mut sigint = signal(SignalKind::interrupt()).ok();
            let mut sigterm = signal(SignalKind::terminate()).ok();
            match (&mut sigint, &mut sigterm) {
                (Some(int), Some(term)) => {
                    tokio::select! {
                        _ = int.recv() => {}
                        _ = term.recv() => {}
                    }
                }
                (Some(int), None) => {
                    let _ = int.recv().await;
                }
                (None, Some(term)) => {
                    let _ = term.recv().await;
                }
                (None, None) => return,
            }
            let _ = tx.send(Msg::Shutdown);
        });
    }

    // Background refresher: read the core snapshot off the async runtime.
    {
        let d = dispatcher.clone();
        let tx = tx.clone();
        let interval = Duration::from_millis(args.interval_ms);
        tokio::spawn(async move {
            loop {
                match tokio::task::spawn_blocking({
                    let d = d.clone();
                    move || fetch_snapshot_once(d)
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
                    Action::RefreshPlugins => {
                        let d = dispatcher.clone();
                        let tx = tx.clone();
                        tokio::spawn(async move {
                            let out = tokio::task::spawn_blocking(move || {
                                let mut g = match d.lock() {
                                    Ok(g) => g,
                                    Err(_) => {
                                        return Some(Err("dispatcher poisoned".to_string()));
                                    }
                                };
                                Some(Ok(g.plugins_snapshot()))
                            })
                            .await
                            .unwrap_or(None);
                            match out {
                                Some(Ok(snap)) => {
                                    let _ = tx.send(Msg::PluginsLoaded(snap));
                                }
                                Some(Err(e)) => {
                                    let _ = tx.send(Msg::RefreshError(e));
                                }
                                None => {}
                            }
                        });
                    }
                    Action::ConfirmToggle(cmd) => {
                        if app.toggle_needs_confirmation(&cmd) {
                            app.pending_confirmation = Some(cmd);
                        } else {
                            dispatch_command(&dispatcher, &tx, &cmd).await;
                        }
                    }
                    Action::RunCommand(cmd) => {
                        dispatch_command(&dispatcher, &tx, &cmd).await;
                    }
                    Action::None => {}
                }
            }
            Msg::Input(_) => {}
            Msg::Snapshot { snap, managed, pid } => app.on_snapshot(snap, managed, pid),
            Msg::Events(events) => {
                let mut need_snapshot = false;
                for ev in events {
                    use blitzkrieg_ui_kit::core::types::CoreEvent;
                    match ev {
                        // Account-affecting events refresh the whole snapshot
                        // immediately (the poller runs at interval-ms; the
                        // push path makes fills/closes visible at once).
                        CoreEvent::Fill { .. }
                        | CoreEvent::PositionClosed { .. }
                        | CoreEvent::ReconcileReport { .. }
                        | CoreEvent::Ready { .. }
                        | CoreEvent::OrderUpdate { .. } => need_snapshot = true,
                        // Pure notices go to the log pane. The kill switch is
                        // loud: the body area becomes a full-screen red banner
                        // until a resume alert clears it.
                        CoreEvent::RiskAlert { code, message } => {
                            let code = code.as_str().unwrap_or("").to_lowercase();
                            if code.contains("killswitch")
                                || message.to_lowercase().contains("kill switch")
                            {
                                app.kill_banner = Some(message.clone());
                            } else if code.contains("resume")
                                || message.to_lowercase().contains("resumed")
                                || message.to_lowercase().contains("restored")
                            {
                                app.kill_banner = None;
                            }
                            app.log(format!("risk alert: {message}"))
                        }
                        CoreEvent::Error { error } => app.log(format!("core: {error}")),
                        CoreEvent::EvolutionSignal { signal }
                        | CoreEvent::EvolutionApplied { signal }
                        | CoreEvent::EvolutionRejected { signal, .. } => {
                            app.log(format!("evolution: {signal}"))
                        }
                        CoreEvent::EvolutionProposed { proposal } => {
                            let id = proposal.get("id").and_then(|v| v.as_str()).unwrap_or("?");
                            let st = proposal
                                .get("strategy")
                                .and_then(|v| v.as_str())
                                .unwrap_or("?");
                            app.log(format!(
                                "evolution: proposed {id} for {st} — page 5 to review"
                            ));
                        }
                        CoreEvent::EvolutionCycle {
                            cycle_seq,
                            dims,
                            strategies,
                            ..
                        } => app.log(format!(
                            "evolution: deep cycle #{cycle_seq} ({dims}-knob) over {} strategies",
                            strategies.len()
                        )),
                        CoreEvent::Unknown => {}
                    }
                }
                if need_snapshot {
                    // Coalesce a burst into at most one immediate refresh; the
                    // poll loop will pick up anything that happens meanwhile.
                    let d = dispatcher.clone();
                    let tx = tx.clone();
                    tokio::spawn(async move {
                        if let Some((snap, managed, pid)) =
                            tokio::task::spawn_blocking(|| fetch_snapshot_once(d))
                                .await
                                .unwrap_or(None)
                        {
                            let _ = tx.send(Msg::Snapshot { snap, managed, pid });
                        }
                    });
                }
            }
            Msg::PluginsLoaded(snap) => {
                let n = snap.strategies.len() + snap.extensions.len();
                app.snap.strategies = snap.strategies;
                app.snap.extensions = snap.extensions;
                app.snap.market_plugins = snap.market_plugins;
                app.snap.market_active = snap.market_active;
                app.snap.connected = snap.connected;
                if let Some(e) = snap.last_error {
                    app.log(format!("plugin registry error: {e}"));
                }
                if app.plugin_focus > 0 && app.plugin_focus >= n.max(1) {
                    app.plugin_focus = n.saturating_sub(1);
                }
            }
            Msg::CommandDone(line) => {
                for l in line.lines() {
                    app.log(l.to_string());
                }
            }
            Msg::RefreshError(e) => app.log(format!("refresh error: {e}")),
            Msg::Shutdown => app.should_quit = true,
        }

        if app.should_quit {
            break;
        }
    }

    ratatui::restore();
    Ok(())
}

/// One blocking snapshot fetch off the shared dispatcher. `None` = the lock
/// is poisoned and the snapshot stream should stop (the poll loop treats it
/// as fatal; event-driven refreshes just skip the beat).
fn fetch_snapshot_once(
    dispatcher: Arc<Mutex<Dispatcher>>,
) -> Option<(UiSnapshot, bool, Option<u32>)> {
    let mut g = dispatcher.lock().ok()?;
    Some((g.snapshot(), g.managed(), g.pid()))
}

async fn dispatch_command(
    dispatcher: &Arc<Mutex<Dispatcher>>,
    tx: &mpsc::UnboundedSender<Msg>,
    cmd: &str,
) {
    let d = dispatcher.clone();
    let tx = tx.clone();
    let cmd = cmd.to_string();
    tokio::spawn(async move {
        let out = tokio::task::spawn_blocking(move || {
            let mut g = match d.lock() {
                Ok(g) => g,
                Err(_) => return vec![String::from("dispatcher poisoned")],
            };
            let o = g.dispatch_line(&cmd);
            vec![format!(
                "{} {}",
                if o.ok { "OK " } else { "ERR" },
                o.message
            )]
        })
        .await
        .unwrap_or_else(|e| vec![format!("command failed: {e}")]);
        for line in out {
            let _ = tx.send(Msg::CommandDone(line));
        }
    })
    .await
    .ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_quit_and_tabs() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = App::new("/tmp/x.sock".into(), false);
        assert!(matches!(
            app.on_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
            Action::Quit
        ));
        assert_eq!(app.tab, Tab::Overview);
        app.on_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
        assert_eq!(app.tab, Tab::Positions);
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

    #[test]
    fn attach_disables_lifecycle() {
        let args = parse_args_from(["--attach".to_string()]);
        assert!(!args.manage);
    }

    #[test]
    fn evolution_tab_switches_and_decides() {
        use blitzkrieg_ui_kit::core::types::EvolutionProposalView;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let key = |c: char| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        let mut app = App::new("/tmp/x.sock".into(), false);

        // '5' enters the evolution tab; up/down move the cursor harmlessly.
        app.on_key(key('5'));
        assert_eq!(app.tab, Tab::Evolution);
        app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.evo_focus, 1);

        // No pending proposal → a/x/d/u say so and change nothing.
        assert!(matches!(app.on_key(key('a')), Action::None));
        assert!(app.logs.last().is_some_and(|l| l.contains("no pending")));

        // One pending proposal: accept routes through the confirm bar, reject
        // and defer run straight away, and auto-evolve flips off → on.
        app.evo_focus = 0;
        app.snap.evolution_proposals = vec![EvolutionProposalView {
            id: "prop-42".into(),
            strategy: "mean_reversion".into(),
            state: "proposed".into(),
            ..Default::default()
        }];
        assert!(matches!(
            app.on_key(key('a')),
            Action::ConfirmToggle(c) if c == "decide prop-42 accept"
        ));
        assert!(matches!(
            app.on_key(key('x')),
            Action::RunCommand(c) if c == "decide prop-42 reject"
        ));
        assert!(matches!(
            app.on_key(key('d')),
            Action::RunCommand(c) if c == "decide prop-42 defer"
        ));
        assert!(matches!(
            app.on_key(key('e')),
            Action::RunCommand(c) if c == "auto-evolve on"
        ));
        assert!(matches!(
            app.on_key(key('u')),
            Action::ConfirmToggle(c) if c == "rollback mean_reversion"
        ));

        // The confirm predicate agrees: accept and rollback need y/n, reject does not.
        assert!(app.toggle_needs_confirmation("decide prop-42 accept"));
        assert!(app.toggle_needs_confirmation("rollback mean_reversion"));
        assert!(!app.toggle_needs_confirmation("decide prop-42 reject"));

        // Tab cycles through all five pages: from Evolution one press wraps
        // back to Overview, four more come the long way round to Evolution.
        app.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.tab, Tab::Overview);
        for _ in 0..4 {
            app.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        }
        assert_eq!(app.tab, Tab::Evolution);

        // --tab 5 starts directly on the evolution page.
        assert_eq!(
            parse_args_from(["--tab".into(), "5".into()]).tab,
            Tab::Evolution
        );
    }
}

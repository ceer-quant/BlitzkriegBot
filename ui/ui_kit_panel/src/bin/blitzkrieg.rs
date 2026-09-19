//! `blitzkrieg` — Unified quant trading engine & UI launcher.
//!
//! Subcommands:
//!   blitzkrieg [FLAGS]       Unified launcher: core + UI in one command (default)
//!   blitzkrieg run [FLAGS]   Same as default unified launcher
//!   blitzkrieg core [FLAGS]  Run the trading core directly
//!   blitzkrieg tui [--attach] Run interactive terminal panel
//!   blitzkrieg web [FLAGS]   Run the web gateway / browser panel
//!   blitzkrieg stop [FLAGS]  Stop the stack attached to one socket
//!   blitzkrieg help          Show help

use blitzkrieg_ui_kit::gateway::{Dispatcher, StartOutcome, Supervisor, SupervisorConfig};
use blitzkrieg_ui_kit::web::WebServer;
use blitzkrieg_ui_kit::{resolve_socket_path, IpcClient};
use blitzkrieg_ui_panel::{
    parse_args_from, run_panel, run_panel_with_dispatcher, stop_stack, PanelArgs, Tab,
};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const HELP_TEXT: &str = "\
blitzkrieg — Unified quant trading engine & UI launcher

USAGE:
  blitzkrieg [FLAGS]
  blitzkrieg <SUBCOMMAND> [FLAGS]

SUBCOMMANDS:
  run            Start trading core and UI together (default if no subcommand given);
                 choose the UI with --tui / --web (default: web)
  core           Run the trading core standalone
  tui [--attach] Run interactive terminal panel (attach to existing core or manage)
  web            Run web gateway / browser panel
  stop           Stop the stack (UI + core) on one socket; also an orphaned core
  help           Show this help message

FLAGS (for blitzkrieg / blitzkrieg run / core):
  --socket <path>       Core UDS socket path
  --mode <dry|live>     Trading mode (default: dry)
  --readonly            Disable order egress structurally (local settlement only)
  --assets <list>       Comma-separated asset list (default: BTC,ETH,SOL,XRP)
  --tick-ms <N>         Engine tick interval in ms (default: 50)
  --seed-balance <N>    Initial seed balance (default: 1000)
  --max-order-notional <N> Maximum notional per order
  --round-sec <N>       Round duration in seconds (default 900; beats HFT_ROUND_SEC)
  --min-round-age <N>   Minimum round age before entering (default 30)
  --min-time-left <N>   Minimum seconds left in a round to enter (default 180)
  --max-positions <N>   Concurrent position cap (default 2)
  --min-shares <N>      Minimum order size in shares (default 10)
  --max-shares <N>      Maximum order size in shares (default 10)
  --engine --feed-ws    Engine passthrough flags (repeatable, already default)
  --addr <ip:port>      Web panel listen address (default: 127.0.0.1:51888;
                        BLITZKRIEG_PANEL_ADDR overrides, e.g. 0.0.0.0:51888)
  --allowed-origin <url> Extra panel origin to accept beyond loopback and
                        same-origin (repeatable; BLITZKRIEG_ALLOWED_ORIGINS is
                        a comma-separated alternative)
  --tui, -tui           Launch interactive TUI instead of web panel
  --web                 Launch web panel (default)
  --interval-ms <N>     TUI snapshot refresh interval (default 1000)
  --manage              Own the core (default for run/core); conflicts with --attach
  --no-lifecycle        Disable core process supervision (adopt-only)
  --attach              Attach to existing core without starting a new one
  --help, -h            Show help

Part launches:
  blitzkrieg run --tui    core + TUI, no web listener
  blitzkrieg web          gateway only, core not auto-started
  blitzkrieg tui --attach watch an already-running core, never start or stop it
";

#[derive(Default)]
struct ParsedCli {
    socket: Option<String>,
    addr: Option<String>,
    mode: Option<String>,
    readonly: bool,
    assets: Option<Vec<String>>,
    tick_ms: Option<u64>,
    seed_balance: Option<String>,
    max_order_notional: Option<String>,
    /// Engine tuning knobs the supervisor already carries as structured
    /// fields (`HFT_ROUND_SEC` etc. remain the env path; the CLI beats env).
    round_sec: Option<u64>,
    min_round_age: Option<u64>,
    min_time_left: Option<u64>,
    max_positions: Option<u64>,
    min_shares: Option<u64>,
    max_shares: Option<u64>,
    use_tui: bool,
    interval_ms: u64,
    /// Explicit "own the core" marker. `run` and `core` own by default, so
    /// this only matters as a conflict check against `--attach`.
    manage: bool,
    no_lifecycle: bool,
    attach: bool,
    extra_flags: Vec<String>,
    /// Extra origins the web panel accepts beyond loopback and same-origin
    /// (reverse proxies that rewrite the hostname). `--allowed-origin`,
    /// repeatable, plus `BLITZKRIEG_ALLOWED_ORIGINS` (comma-separated).
    allowed_origins: Vec<String>,
}

/// Origins from the environment: `BLITZKRIEG_ALLOWED_ORIGINS`, comma-separated.
/// The `.env` self-load means a server deployment can put this next to its
/// credentials and forget about it.
fn env_allowed_origins() -> Vec<String> {
    std::env::var("BLITZKRIEG_ALLOWED_ORIGINS")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Parse the shared flag surface of `blitzkrieg` / `blitzkrieg run` /
/// `blitzkrieg core`. Errors are strings printed verbatim to stderr with a
/// help hint, then exit code 2 — never a silent swallow: a typo'd flag must
/// not quietly start a trading stack the operator did not ask for.
fn parse_cli_options(args: impl IntoIterator<Item = String>) -> Result<ParsedCli, String> {
    let mut cli = ParsedCli {
        interval_ms: 1000,
        ..ParsedCli::default()
    };
    // Panel mode explicitly requested so far, if any: Some(true) = TUI,
    // Some(false) = web. Two different explicit choices conflict instead of
    // the previous last-one-wins.
    let mut ui_mode: Option<bool> = None;
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--socket" => {
                if let Some(s) = iter.next() {
                    cli.socket = Some(s);
                }
            }
            "--addr" => {
                if let Some(s) = iter.next() {
                    cli.addr = Some(s);
                }
            }
            "--mode" => {
                if let Some(m) = iter.next() {
                    cli.mode = Some(m);
                }
            }
            "--readonly" => cli.readonly = true,
            "--allowed-origin" => {
                if let Some(s) = iter.next() {
                    let s = s.trim().trim_end_matches('/').to_string();
                    if !s.is_empty() {
                        cli.allowed_origins.push(s);
                    }
                }
            }
            "--assets" => {
                if let Some(list) = iter.next() {
                    cli.assets = Some(
                        list.split(',')
                            .map(|s| s.trim().to_uppercase())
                            .filter(|s| !s.is_empty())
                            .collect(),
                    );
                }
            }
            "--tick-ms" => {
                if let Some(v) = iter.next().and_then(|v| v.parse().ok()) {
                    cli.tick_ms = Some(v);
                }
            }
            "--interval-ms" => {
                if let Some(v) = iter.next().and_then(|v| v.parse::<u64>().ok()) {
                    cli.interval_ms = v.max(100);
                }
            }
            "--seed-balance" => {
                if let Some(s) = iter.next() {
                    cli.seed_balance = Some(s);
                }
            }
            "--max-order-notional" => {
                if let Some(s) = iter.next() {
                    cli.max_order_notional = Some(s);
                }
            }
            "--round-sec" => {
                if let Some(v) = iter.next().and_then(|v| v.parse::<u64>().ok()) {
                    cli.round_sec = Some(v);
                }
            }
            "--min-round-age" => {
                if let Some(v) = iter.next().and_then(|v| v.parse::<u64>().ok()) {
                    cli.min_round_age = Some(v);
                }
            }
            "--min-time-left" => {
                if let Some(v) = iter.next().and_then(|v| v.parse::<u64>().ok()) {
                    cli.min_time_left = Some(v);
                }
            }
            "--max-positions" => {
                if let Some(v) = iter.next().and_then(|v| v.parse::<u64>().ok()) {
                    cli.max_positions = Some(v);
                }
            }
            "--min-shares" => {
                if let Some(v) = iter.next().and_then(|v| v.parse::<u64>().ok()) {
                    cli.min_shares = Some(v);
                }
            }
            "--max-shares" => {
                if let Some(v) = iter.next().and_then(|v| v.parse::<u64>().ok()) {
                    cli.max_shares = Some(v);
                }
            }
            "--tui" | "-tui" => {
                if ui_mode == Some(false) {
                    return Err("--tui and --web are mutually exclusive".into());
                }
                ui_mode = Some(true);
                cli.use_tui = true;
            }
            "--web" => {
                if ui_mode == Some(true) {
                    return Err("--tui and --web are mutually exclusive".into());
                }
                ui_mode = Some(false);
                cli.use_tui = false;
            }
            "--manage" => cli.manage = true,
            "--no-lifecycle" => cli.no_lifecycle = true,
            "--attach" => {
                cli.attach = true;
                cli.no_lifecycle = true;
            }
            "--engine" => cli.extra_flags.push("--engine".to_string()),
            "--feed-ws" => cli.extra_flags.push("--feed-ws".to_string()),
            "--no-event-archive" => cli.extra_flags.push("--no-event-archive".to_string()),
            "--no-trade-log" => cli.extra_flags.push("--no-trade-log".to_string()),
            "--no-order-log" => cli.extra_flags.push("--no-order-log".to_string()),
            "--no-position-log" => cli.extra_flags.push("--no-position-log".to_string()),
            other => return Err(format!("unknown argument '{other}'")),
        }
    }
    if cli.manage && (cli.attach || cli.no_lifecycle) {
        return Err(
            "--manage cannot be combined with --attach / --no-lifecycle: \
                    owning the core and adopting one are mutually exclusive"
                .into(),
        );
    }
    Ok(cli)
}

/// Parse or die: print the message verbatim with the help hint and exit 2 —
/// the same contract as the unknown-subcommand path in `tokio_main`.
fn parse_cli_or_exit(args: Vec<String>) -> ParsedCli {
    match parse_cli_options(args) {
        Ok(cli) => cli,
        Err(msg) => {
            eprintln!("blitzkrieg: {msg}");
            eprintln!("See 'blitzkrieg --help' for available flags.");
            std::process::exit(2);
        }
    }
}

fn build_supervisor_config(cli: &ParsedCli, fallback_socket: String) -> SupervisorConfig {
    let socket_path = cli.socket.clone().unwrap_or(fallback_socket);
    let mut cfg = SupervisorConfig::from_env(socket_path);
    if let Some(ref m) = cli.mode {
        cfg.mode = m.clone();
    }
    if cli.readonly && !cfg.extra_args.iter().any(|a| a == "--readonly") {
        cfg.extra_args.push("--readonly".into());
    }
    if let Some(ref a) = cli.assets {
        cfg.assets = a.clone();
    }
    if let Some(t) = cli.tick_ms {
        cfg.tick_ms = t;
    }
    if let Some(ref sb) = cli.seed_balance {
        cfg.seed_balance = sb.clone();
    }
    if let Some(ref mon) = cli.max_order_notional {
        cfg.max_order_notional = mon.clone();
    }
    // CLI knobs beat the HFT_* environment values `from_env` already filled in.
    if let Some(v) = cli.round_sec {
        cfg.round_sec = v;
    }
    if let Some(v) = cli.min_round_age {
        cfg.min_round_age = v;
    }
    if let Some(v) = cli.min_time_left {
        cfg.min_time_left = v;
    }
    if let Some(v) = cli.max_positions {
        cfg.max_positions = v;
    }
    if let Some(v) = cli.min_shares {
        cfg.min_shares = v;
    }
    if let Some(v) = cli.max_shares {
        cfg.max_shares = v;
    }
    for flag in &cli.extra_flags {
        if !cfg.extra_args.contains(flag) {
            cfg.extra_args.push(flag.clone());
        }
    }
    cfg
}

/// `.env` self-load happens here, BEFORE tokio starts: environment variables
/// are process-global and must be settled before any other thread can read
/// them. The launcher therefore needs no `set -a; source .env` ceremony —
/// see `blitzkrieg_ui_panel::env_file` for the precedence rule (the exported
/// environment always wins over the file) and the secrets line.
fn main() -> std::io::Result<()> {
    blitzkrieg_ui_panel::env_file::load_cwd_env();
    tokio_main()
}

#[tokio::main]
async fn tokio_main() -> std::io::Result<()> {
    let mut raw_args: Vec<String> = std::env::args().skip(1).collect();
    let first = raw_args.first().map(|s| s.as_str());

    match first {
        Some("help") | Some("--help") | Some("-h") => {
            println!("{HELP_TEXT}");
            Ok(())
        }
        Some("core") => {
            raw_args.remove(0);
            run_core_subcommand(raw_args).await
        }
        Some("tui") => {
            raw_args.remove(0);
            run_tui_subcommand(raw_args).await
        }
        Some("web") => {
            raw_args.remove(0);
            run_web_subcommand(raw_args).await
        }
        Some("stop") => {
            raw_args.remove(0);
            // stop_stack::run waits out the grace synchronously — blocking
            // pool so the runtime stays free (E14).
            tokio::task::spawn_blocking(move || run_stop_subcommand(raw_args))
                .await
                .map_err(|e| std::io::Error::other(format!("stop task failed: {e}")))?
        }
        Some("run") => {
            raw_args.remove(0);
            run_unified(raw_args).await
        }
        Some(other) if other.starts_with('-') => run_unified(raw_args).await,
        None => run_unified(Vec::new()).await,
        Some(unknown) => {
            eprintln!("blitzkrieg: unknown subcommand '{unknown}'");
            eprintln!("See 'blitzkrieg --help' for available commands.");
            std::process::exit(2);
        }
    }
}

/// Run core standalone under process supervision.
async fn run_core_subcommand(raw_args: Vec<String>) -> std::io::Result<()> {
    let cli = parse_cli_or_exit(raw_args);
    let socket = cli.socket.clone().unwrap_or_else(resolve_socket_path);
    let cfg = build_supervisor_config(&cli, socket.clone());

    // Signal listeners are registered BEFORE the core is spawned: between the
    // spawn and a late registration there is a window where SIGTERM would take
    // the default action — instant death, with the freshly spawned core left
    // orphaned, still serving its socket (the exact shape the orphaned readonly
    // cores in unified-launcher-check exposed). With the streams registered
    // first, a signal that early is caught and routed through supervisor.stop().
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;

    let supervisor = Supervisor::new(cfg);
    // The readiness handshake polls the socket for up to 15s — run it on the
    // blocking pool so the signal streams registered above keep being served
    // while it waits (E14: no blocking calls on the async workers).
    let (mut supervisor, started) = tokio::task::spawn_blocking(move || {
        let mut sup = supervisor;
        let started = sup.start();
        (sup, started)
    })
    .await
    .map_err(|e| std::io::Error::other(format!("core start task failed: {e}")))?;
    match started {
        Ok(StartOutcome::Started { pid }) => {
            println!("blitzkrieg: core started (PID {pid}) on {socket}");
        }
        Ok(StartOutcome::Adopted) => {
            println!("blitzkrieg: core already running on {socket} (adopted)");
        }
        Err(e) => {
            eprintln!("blitzkrieg core error: {e}");
            std::process::exit(1);
        }
    }

    // Keep core running until SIGINT or SIGTERM is caught.
    tokio::select! {
        _ = sigint.recv() => {
            eprintln!("blitzkrieg: stopping core on SIGINT...");
        }
        _ = sigterm.recv() => {
            eprintln!("blitzkrieg: stopping core on SIGTERM...");
        }
    }
    // stop() waits out the TERM grace (≤5s) synchronously; same blocking-pool
    // rule. `stop` clears `owns`, so the later Drop is a no-op.
    tokio::task::spawn_blocking(move || supervisor.stop())
        .await
        .map_err(|e| std::io::Error::other(format!("core stop task failed: {e}")))?;
    Ok(())
}

/// Run the TUI subcommand (`blitzkrieg tui [--attach] [...]`).
async fn run_tui_subcommand(args: Vec<String>) -> std::io::Result<()> {
    let is_attach = args.iter().any(|a| a == "--attach");
    let mut panel_args = parse_args_from(args);
    if is_attach {
        panel_args.manage = false;
    } else if !panel_args.manage {
        panel_args.manage = true;
    }
    run_panel(panel_args).await
}

/// Run the web gateway subcommand (`blitzkrieg web [...]`).
async fn run_web_subcommand(args: Vec<String>) -> std::io::Result<()> {
    let mut socket = resolve_socket_path();
    let mut addr = "127.0.0.1:51888".to_string();
    let mut manage = std::env::var("UIKIT_MANAGE")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false);

    let mut iter = args.into_iter();
    let mut allowed_origins = env_allowed_origins();
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--socket" => {
                if let Some(s) = iter.next() {
                    socket = s;
                }
            }
            "--addr" => {
                if let Some(s) = iter.next() {
                    addr = s;
                }
            }
            "--manage" => manage = true,
            "--allowed-origin" => {
                if let Some(s) = iter.next() {
                    let s = s.trim().trim_end_matches('/').to_string();
                    if !s.is_empty() {
                        allowed_origins.push(s);
                    }
                }
            }
            "--help" | "-h" => {
                println!("blitzkrieg web [--socket <path>] [--addr <127.0.0.1:51888>] [--manage] [--allowed-origin <url>]");
                return Ok(());
            }
            other => eprintln!("ignoring unknown arg: {other}"),
        }
    }

    let client = IpcClient::new(socket.clone());
    let cfg = SupervisorConfig::from_env(socket);
    let dispatcher = Arc::new(Mutex::new(Dispatcher::new(cfg, manage)));
    let mut server = WebServer::with_shared_gateway(client, 0, dispatcher.clone());
    server.set_panel_credentials(
        std::env::var("BLITZKRIEG_PANEL_USER").ok(),
        std::env::var("BLITZKRIEG_PANEL_PASSWORD").ok(),
    );
    server.set_allowed_origins(allowed_origins);
    if let Err(why) = server.require_credentials() {
        eprintln!("blitzkrieg web: {why}");
        std::process::exit(2);
    }
    // Two owned clones so either signal branch can move its own into the
    // blocking task (E14).
    let signal_dispatcher_int = dispatcher.clone();
    let signal_dispatcher_term = dispatcher.clone();
    let server_task = tokio::task::spawn_blocking(move || server.serve(&addr));
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        result = server_task => {
            match result {
                Ok(result) => result,
                Err(e) => Err(std::io::Error::other(format!("web server task failed: {e}"))),
            }
        }
        _ = sigint.recv() => {
            eprintln!("blitzkrieg web: stopping managed core on SIGINT...");
            // stop() waits out the TERM grace synchronously — blocking pool (E14).
            let _ = tokio::task::spawn_blocking(move || {
                if let Ok(mut d) = signal_dispatcher_int.lock() { d.stop(); }
            }).await;
            std::process::exit(0);
        }
        _ = sigterm.recv() => {
            eprintln!("blitzkrieg web: stopping managed core on SIGTERM...");
            let _ = tokio::task::spawn_blocking(move || {
                if let Ok(mut d) = signal_dispatcher_term.lock() { d.stop(); }
            }).await;
            std::process::exit(0);
        }
    }
}

/// `blitzkrieg stop [--socket <path>] [--timeout <sec>]` — stop the stack on
/// one socket. The heavy lifting (scanning, ownership, escalation, stale-socket
/// cleanup) lives in [`stop_stack::run`] so it can be unit-tested; this wrapper
/// only parses the two flags and maps the exit code.
fn run_stop_subcommand(args: Vec<String>) -> std::io::Result<()> {
    let mut socket = resolve_socket_path();
    let mut grace = Duration::from_secs(5);
    let mut iter = args.into_iter();
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--socket" => {
                if let Some(s) = iter.next() {
                    socket = s;
                }
            }
            "--timeout" => {
                if let Some(v) = iter.next().and_then(|v| v.parse::<u64>().ok()) {
                    grace = Duration::from_secs(v.max(1));
                }
            }
            "--help" | "-h" => {
                println!("blitzkrieg stop [--socket <path>] [--timeout <sec>]");
                return Ok(());
            }
            other => eprintln!("ignoring unknown arg: {other}"),
        }
    }
    let code = stop_stack::run(&socket, grace);
    if code == 0 {
        Ok(())
    } else {
        std::process::exit(code)
    }
}

/// Unified launcher: starts the core and the UI together in a single command.
async fn run_unified(args: Vec<String>) -> std::io::Result<()> {
    let cli = parse_cli_or_exit(args);
    let socket = cli.socket.clone().unwrap_or_else(resolve_socket_path);
    // Listen address precedence: --addr > BLITZKRIEG_PANEL_ADDR (put it in .env
    // once on a server and `blitzkrieg run` needs no flag) > loopback default.
    // The default stays LOOPBACK on purpose: the panel can start and stop the
    // trading core, so exposing an interface is an explicit operator choice.
    let addr = cli
        .addr
        .clone()
        .or_else(|| std::env::var("BLITZKRIEG_PANEL_ADDR").ok())
        .filter(|a| !a.trim().is_empty())
        .unwrap_or_else(|| "127.0.0.1:51888".into());
    let allowed_origins = {
        let mut o = env_allowed_origins();
        o.extend(cli.allowed_origins.iter().cloned());
        o
    };

    let lifecycle_enabled = !cli.no_lifecycle && !cli.attach;
    let cfg = build_supervisor_config(&cli, socket.clone());

    let dispatcher = Arc::new(Mutex::new(Dispatcher::new(cfg, lifecycle_enabled)));

    // The signal task is registered BEFORE the core is spawned. Between the
    // spawn and a late registration there is a window where SIGTERM/SIGINT
    // would take the default action — instant launcher death, with the freshly
    // spawned core left orphaned and still serving its socket. (The orphaned
    // readonly cores unified-launcher-check kept finding were exactly this.)
    // Registered first, a signal that early routes through Dispatcher::stop(),
    // which kills a core the supervisor owns and exits cleanly — and if the
    // signal lands before any core exists, stop() is simply NotOwned.
    if !cli.use_tui {
        let d_signal = dispatcher.clone();
        tokio::spawn(async move {
            use tokio::signal::unix::{signal, SignalKind};
            let mut sigint = signal(SignalKind::interrupt()).expect("SIGINT listener");
            let mut sigterm = signal(SignalKind::terminate()).expect("SIGTERM listener");
            tokio::select! {
                _ = sigint.recv() => {
                    eprintln!("blitzkrieg: caught SIGINT, shutting down core cleanly...");
                }
                _ = sigterm.recv() => {
                    eprintln!("blitzkrieg: caught SIGTERM, shutting down core cleanly...");
                }
            }
            // stop() holds the lock and waits out the TERM grace (≤5s) — run
            // it on the blocking pool, then exit (E14).
            let _ = tokio::task::spawn_blocking(move || {
                if let Ok(mut g) = d_signal.lock() {
                    g.stop();
                }
            })
            .await;
            std::process::exit(0);
        });
    }

    if lifecycle_enabled {
        // start() polls the readiness handshake for up to 15s while holding
        // the dispatcher lock — blocking-pool, so the signal task stays
        // serviced (E14).
        let d_spawn = dispatcher.clone();
        let started = tokio::task::spawn_blocking(move || {
            let mut g = d_spawn.lock().unwrap();
            g.supervisor_mut().start()
        })
        .await
        .map_err(|e| std::io::Error::other(format!("core start task failed: {e}")))?;
        match started {
            Ok(StartOutcome::Started { pid }) => {
                println!("blitzkrieg: core spawned (PID {pid}) on {socket}");
            }
            Ok(StartOutcome::Adopted) => {
                println!("blitzkrieg: adopted existing core on {socket}");
            }
            Err(e) => {
                eprintln!("blitzkrieg: failed to start core: {e}");
                std::process::exit(1);
            }
        }
    } else {
        println!("blitzkrieg: lifecycle disabled (attach/monitor mode on {socket})");
    }

    if cli.use_tui {
        let panel_args = PanelArgs {
            socket,
            interval_ms: cli.interval_ms,
            manage: lifecycle_enabled,
            tab: Tab::Overview,
        };
        let res = run_panel_with_dispatcher(panel_args, dispatcher.clone()).await;
        // stop() waits out the TERM grace synchronously — blocking pool (E14).
        let d_stop = dispatcher.clone();
        let _ = tokio::task::spawn_blocking(move || {
            if let Ok(mut g) = d_stop.lock() {
                g.stop();
            }
        })
        .await;
        res
    } else {
        let client = IpcClient::new(socket.clone());
        let user = std::env::var("BLITZKRIEG_PANEL_USER").ok();
        let pass = std::env::var("BLITZKRIEG_PANEL_PASSWORD").ok();

        let server = if user.is_some() && pass.is_some() {
            // Share the dispatcher that OWNS the core — not a fresh one. The
            // panel's 停止 button and the "内核由其他进程启动" notice both read
            // `managed = supervisor.owns()` from THIS dispatcher; a second
            // dispatcher would own nothing and the panel would disown a core
            // this very process spawned.
            let mut s = WebServer::with_shared_gateway(client, 0, dispatcher.clone());
            s.set_panel_credentials(user, pass);
            s.set_allowed_origins(allowed_origins);
            s
        } else {
            if lifecycle_enabled {
                println!(
                    "blitzkrieg: note: BLITZKRIEG_PANEL_USER / BLITZKRIEG_PANEL_PASSWORD are \
                     not set — the web panel runs in read-only mode. Set both to enable the \
                     web command verbs."
                );
            }
            let mut s = WebServer::new(client, 0);
            // Even the read-only surface is behind the origin gate; a server
            // deployment that serves it on a LAN address needs the same-origin
            // and allowlist rules to know about that address.
            s.set_allowed_origins(allowed_origins);
            s
        };

        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let addr_clone = addr.clone();
        std::thread::spawn(move || {
            if let Err(e) = server.serve(&addr_clone) {
                eprintln!("blitzkrieg web server error: {e}");
            }
            let _ = stop_tx.send(());
        });

        let _ = tokio::task::spawn_blocking(move || stop_rx.recv()).await;
        // stop() waits out the TERM grace synchronously — blocking pool (E14).
        let d_stop = dispatcher.clone();
        let _ = tokio::task::spawn_blocking(move || {
            if let Ok(mut g) = d_stop.lock() {
                g.stop();
            }
        })
        .await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<ParsedCli, String> {
        parse_cli_options(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn default_is_web_with_lifecycle() {
        let cli = parse(&[]).expect("default parses");
        assert!(!cli.use_tui);
        assert_eq!(cli.interval_ms, 1000);
        assert!(!cli.no_lifecycle && !cli.attach && !cli.manage);
    }

    #[test]
    fn tui_short_and_long_alias_match() {
        for flag in ["--tui", "-tui"] {
            let cli = parse(&[flag]).unwrap_or_else(|e| panic!("{flag}: {e}"));
            assert!(cli.use_tui, "{flag} must select the TUI");
            // Still owns the core by default.
            assert!(!cli.no_lifecycle && !cli.attach);
        }
    }

    #[test]
    fn explicit_web_matches_default() {
        let cli = parse(&["--web"]).expect("web parses");
        assert!(!cli.use_tui);
    }

    #[test]
    fn tui_and_web_conflict_either_order() {
        assert!(parse(&["--tui", "--web"]).is_err());
        assert!(parse(&["--web", "--tui"]).is_err());
        assert!(parse(&["-tui", "--web"]).is_err());
        // Repeating the same choice is harmless.
        assert!(parse(&["--tui", "-tui"]).is_ok());
        assert!(parse(&["--web", "--web"]).is_ok());
    }

    #[test]
    fn attach_implies_no_lifecycle() {
        let cli = parse(&["--attach"]).expect("attach parses");
        assert!(cli.attach && cli.no_lifecycle);
    }

    #[test]
    fn manage_conflicts_with_adopt_only_flags() {
        assert!(parse(&["--manage"]).is_ok());
        assert!(parse(&["--manage", "--attach"]).is_err());
        assert!(parse(&["--manage", "--no-lifecycle"]).is_err());
        assert!(parse(&["--attach", "--no-lifecycle"]).is_ok());
    }

    #[test]
    fn unknown_argument_is_rejected() {
        assert!(parse(&["--tui-ms"]).is_err());
        assert!(parse(&["--roundsec", "600"]).is_err());
        assert!(parse(&["--managed"]).is_err());
    }

    #[test]
    fn engine_knobs_parse_into_structured_fields() {
        let cli = parse(&[
            "--round-sec",
            "600",
            "--max-positions",
            "3",
            "--min-shares",
            "5",
        ])
        .expect("knobs parse");
        assert_eq!(cli.round_sec, Some(600));
        assert_eq!(cli.max_positions, Some(3));
        assert_eq!(cli.min_shares, Some(5));
        assert_eq!(cli.min_time_left, None);
    }

    #[test]
    fn interval_ms_is_clamped_to_100() {
        let cli = parse(&["--interval-ms", "50"]).expect("interval parses");
        assert_eq!(cli.interval_ms, 100);
    }

    #[test]
    fn assets_are_uppercased_and_trimmed() {
        let cli = parse(&["--assets", " btc , eth ,,"]).expect("assets parse");
        assert_eq!(cli.assets, Some(vec!["BTC".into(), "ETH".into()]));
    }
}

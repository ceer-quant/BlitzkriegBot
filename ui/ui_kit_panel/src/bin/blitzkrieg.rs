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
use blitzkrieg_ui_panel::{parse_args_from, run_panel, stop_stack, PanelArgs, Tab};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const HELP_TEXT: &str = "\
blitzkrieg — Unified quant trading engine & UI launcher

USAGE:
  blitzkrieg [FLAGS]
  blitzkrieg <SUBCOMMAND> [FLAGS]

SUBCOMMANDS:
  run            Start trading core and UI together (default if no subcommand given)
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
  --addr <ip:port>      Web panel listen address (default: 127.0.0.1:51888;
                        BLITZKRIEG_PANEL_ADDR overrides, e.g. 0.0.0.0:51888)
  --allowed-origin <url> Extra panel origin to accept beyond loopback and
                        same-origin (repeatable; BLITZKRIEG_ALLOWED_ORIGINS is
                        a comma-separated alternative)
  --tui                 Launch interactive TUI instead of web panel
  --web                 Launch web panel (default)
  --no-lifecycle        Disable core process supervision (adopt-only)
  --attach              Attach to existing core without starting a new one
  --help, -h            Show help
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
    use_tui: bool,
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

fn parse_cli_options(args: impl IntoIterator<Item = String>) -> ParsedCli {
    let mut cli = ParsedCli::default();
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
            "--tui" => cli.use_tui = true,
            "--web" => cli.use_tui = false,
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
            _ => {}
        }
    }
    cli
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
            run_web_subcommand(raw_args)
        }
        Some("stop") => {
            raw_args.remove(0);
            run_stop_subcommand(raw_args)
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
    let cli = parse_cli_options(raw_args);
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

    let mut supervisor = Supervisor::new(cfg);
    match supervisor.start() {
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
    supervisor.stop();
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
fn run_web_subcommand(args: Vec<String>) -> std::io::Result<()> {
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
    let dispatcher = Dispatcher::new(cfg, manage);
    let mut server = WebServer::with_gateway(client, 0, dispatcher);
    server.set_panel_credentials(
        std::env::var("BLITZKRIEG_PANEL_USER").ok(),
        std::env::var("BLITZKRIEG_PANEL_PASSWORD").ok(),
    );
    server.set_allowed_origins(allowed_origins);
    if let Err(why) = server.require_credentials() {
        eprintln!("blitzkrieg web: {why}");
        std::process::exit(2);
    }
    server.serve(&addr)
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
    let cli = parse_cli_options(args);
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
        if let Ok(mut g) = d_signal.lock() {
            g.stop();
        }
        std::process::exit(0);
    });

    if lifecycle_enabled {
        let mut d = dispatcher.lock().unwrap();
        match d.supervisor_mut().start() {
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
            interval_ms: 1000,
            manage: lifecycle_enabled,
            tab: Tab::Overview,
        };
        let res = run_panel(panel_args).await;
        if let Ok(mut g) = dispatcher.lock() {
            g.stop();
        }
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
        if let Ok(mut g) = dispatcher.lock() {
            g.stop();
        }
        Ok(())
    }
}

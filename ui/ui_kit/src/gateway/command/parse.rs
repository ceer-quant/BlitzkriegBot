//! Command grammar + catalog: [`Command`], [`parse_command`] and the [`COMMANDS`] table.

/// A parsed command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Start {
        assets: Vec<String>,
        size_usd: Option<String>,
        dry_run: bool,
    },
    Stop,
    /// issue 488: stop this gateway's core, wait for the socket to go quiet,
    /// then spawn a replacement with the same config. The point is the
    /// next-session settings (round duration, market plugin): the panel writes
    /// the persisted cell, then restarts so the boot-time resolution re-runs.
    Restart,
    Status,
    Positions {
        limit: usize,
    },
    Strategies,
    StrategySet {
        name: String,
        enabled: bool,
    },
    Extensions,
    ExtensionSet {
        name: String,
        enabled: bool,
    },
    Flatten {
        position_id: String,
    },
    /// issue 456: declare one ghost order dead (`orders.force_close`). The
    /// kernel refuses an order whose round is still live — this is for the
    /// expired leftover a cancel never converged, not for dodging a cancel.
    ForceOrder {
        order_id: String,
    },
    Markets,
    /// Network self-check: probe the paths the active venue trades over.
    NetCheck,
    Help,
    /// E13: every known evolution proposal's latest state.
    EvolutionProposals {
        limit: usize,
    },
    /// E13: the operator's verdict on one held proposal.
    EvolutionDecide {
        id: String,
        decision: String,
    },
    /// E13: the auto-evolve switch (persisted across restarts).
    EvolutionAuto {
        on: bool,
    },
    /// #249: the engine switch — whether the evaluator runs at all (persisted).
    /// Distinct from `EvolutionAuto`, which only decides who applies a change.
    EvolutionEngine {
        on: bool,
    },
    /// E13: roll ONE strategy back to its pre-change parameters.
    EvolutionRollback {
        strategy: String,
    },
    /// E28: the multi-account book — every account plus the account this
    /// connection would trade with (§9.5).
    Accounts,
    /// E28: point THIS connection's session default at another account.
    AccountSwitch {
        account_id: String,
    },
}

/// Parse one command line. Unknown verbs yield `Err(message)` (never a panic).
pub fn parse_command(input: &str) -> Result<Command, String> {
    let parts: Vec<&str> = input.split_whitespace().collect();
    let Some(verb) = parts.first().map(|s| s.to_ascii_lowercase()) else {
        return Err("empty command".into());
    };
    match verb.as_str() {
        "start" => {
            let mut assets: Vec<String> = Vec::new();
            let mut size_usd = None;
            let mut dry_run = std::env::var("DRY_RUN")
                .map(|v| v != "false")
                .unwrap_or(true);
            let mut i = 1;
            while i < parts.len() {
                let p = parts[i];
                if p == "--dry-run" || p == "--dry" {
                    dry_run = true;
                } else if p == "--live" {
                    dry_run = false;
                } else if p == "--size" {
                    i += 1;
                    size_usd = parts.get(i).map(|s| s.to_string());
                } else if let Some(rest) = p.strip_prefix("--size=") {
                    size_usd = Some(rest.to_string());
                } else if p.starts_with('-') {
                    return Err(format!("unknown flag for start: {p}"));
                } else if assets.is_empty() {
                    assets = p
                        .split(',')
                        .map(|a| a.trim().to_uppercase())
                        .filter(|a| !a.is_empty())
                        .collect();
                }
                i += 1;
            }
            Ok(Command::Start {
                assets,
                size_usd,
                dry_run,
            })
        }
        "stop" => {
            if parts.len() != 1 {
                return Err("usage: stop".into());
            }
            Ok(Command::Stop)
        }
        "restart" => {
            if parts.len() != 1 {
                return Err("usage: restart".into());
            }
            Ok(Command::Restart)
        }
        "status" => Ok(Command::Status),
        "positions" => {
            let limit = parts
                .get(1)
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(0);
            Ok(Command::Positions { limit })
        }
        "strategies" => Ok(Command::Strategies),
        "strategy" => {
            let name = parts.get(1).copied().unwrap_or("").to_string();
            let on = parts.get(2).copied();
            if name.is_empty() || parts.len() != 3 || !matches!(on, Some("on") | Some("off")) {
                return Err("usage: strategy <name> on|off".into());
            }
            Ok(Command::StrategySet {
                name,
                enabled: on == Some("on"),
            })
        }
        "extensions" => Ok(Command::Extensions),
        "flatten" | "close" => {
            if parts.len() != 2 || parts[1].is_empty() {
                return Err("usage: flatten <position_id>".into());
            }
            Ok(Command::Flatten {
                position_id: parts[1].to_string(),
            })
        }
        "force-order" | "force_order" => {
            if parts.len() != 2 || parts[1].is_empty() {
                return Err("usage: force-order <order_id>".into());
            }
            Ok(Command::ForceOrder {
                order_id: parts[1].to_string(),
            })
        }
        "extension" => {
            let name = parts.get(1).copied().unwrap_or("").to_string();
            let on = parts.get(2).copied();
            if name.is_empty() || parts.len() != 3 || !matches!(on, Some("on") | Some("off")) {
                return Err("usage: extension <name> on|off".into());
            }
            Ok(Command::ExtensionSet {
                name,
                enabled: on == Some("on"),
            })
        }
        "markets" => Ok(Command::Markets),
        "netcheck" | "net-check" => Ok(Command::NetCheck),
        "proposals" | "evo" => Ok(Command::EvolutionProposals {
            limit: parts
                .get(1)
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(20),
        }),
        "decide" => {
            let id = parts.get(1).copied().unwrap_or("").to_string();
            let decision = parts
                .get(2)
                .map(|s| s.to_ascii_lowercase())
                .unwrap_or_default();
            if id.is_empty() || !matches!(decision.as_str(), "accept" | "reject" | "defer") {
                return Err("usage: decide <proposal_id> accept|reject|defer".into());
            }
            Ok(Command::EvolutionDecide { id, decision })
        }
        "auto-evolve" | "autoevolve" => {
            let on = parts.get(1).copied();
            if !matches!(on, Some("on") | Some("off")) || parts.len() != 2 {
                return Err("usage: auto-evolve on|off".into());
            }
            Ok(Command::EvolutionAuto {
                on: on == Some("on"),
            })
        }
        "evolve" => {
            let on = parts.get(1).copied();
            if !matches!(on, Some("on") | Some("off")) || parts.len() != 2 {
                return Err("usage: evolve on|off".into());
            }
            Ok(Command::EvolutionEngine {
                on: on == Some("on"),
            })
        }
        "rollback" => {
            let strategy = parts.get(1).copied().unwrap_or("").to_string();
            if strategy.is_empty() {
                return Err("usage: rollback <strategy>".into());
            }
            Ok(Command::EvolutionRollback { strategy })
        }
        "accounts" => Ok(Command::Accounts),
        "account" => {
            let id = parts.get(1).copied().unwrap_or("").to_string();
            if id.is_empty() || parts.len() != 2 {
                return Err("usage: account <id>".into());
            }
            Ok(Command::AccountSwitch { account_id: id })
        }
        "help" | "?" => Ok(Command::Help),
        other => Err(format!("unknown command: {other}")),
    }
}

/// The gateway's command surface, `(usage, summary)` per command in the order
/// help prints them. The verb is the usage's first token, so a row cannot
/// disagree with itself.
///
/// This table is the single source: [`help_text`] renders it, the TUI's `?`
/// overlay and `--help` list it, and the command bar completes against it. A
/// verb the parser does not dispatch is therefore impossible to offer to a user,
/// and a verb the parser does dispatch is impossible to leave out of the help.
pub const COMMANDS: &[(&str, &str)] = &[
    (
        "start [ASSETS] [--size N] [--dry-run]",
        "start the core (adopts if already running)",
    ),
    ("stop", "stop the core this gateway spawned"),
    (
        "restart",
        "stop + start the core (applies next-session settings; issue 488)",
    ),
    ("status", "round / stats / positions / balance"),
    ("positions [N]", "closed-trade history (newest first)"),
    ("strategies", "list strategies and enabled state"),
    ("strategy <name> on|off", "enable/disable one strategy"),
    ("extensions", "list extensions and state"),
    ("extension <name> on|off", "enable/disable one extension"),
    ("flatten <position_id>", "force-close one open position"),
    (
        "force-order <order_id>",
        "force-close one ghost order (kernel refuses live rounds; issue 456)",
    ),
    ("markets", "list market plugins"),
    ("netcheck", "probe the network paths the venue uses"),
    ("proposals [N]", "evolution proposals (pending first)"),
    (
        "decide <id> accept|reject|defer",
        "vote on one evolution proposal",
    ),
    ("auto-evolve on|off", "unattended mode switch (persisted)"),
    ("evolve on|off", "evolution engine switch (persisted)"),
    ("rollback <strategy>", "undo one strategy's last evolution"),
    (
        "accounts",
        "list the account book and this connection's default",
    ),
    (
        "account <id>",
        "switch this connection's default account (E28)",
    ),
    ("help", ""),
];

/// The verb a usage dispatches: its first token.
fn verb_of(usage: &'static str) -> &'static str {
    usage.split(' ').next().unwrap_or("")
}

/// Every verb the gateway dispatches, in help order — what the command bar
/// offers as completion candidates.
pub fn command_verbs() -> impl Iterator<Item = &'static str> {
    COMMANDS.iter().map(|&(usage, _)| verb_of(usage))
}

/// One line per command, `usage` and `summary` in columns: the single renderer
/// behind [`help_text`], the TUI overlay and the panel's `--help`.
pub fn command_lines() -> impl Iterator<Item = String> {
    let width = COMMANDS
        .iter()
        .map(|(usage, _)| usage.len())
        .max()
        .unwrap_or(0);
    COMMANDS.iter().map(move |&(usage, summary)| {
        if summary.is_empty() {
            format!("  {usage}")
        } else {
            format!("  {usage:<width$}   {summary}")
        }
    })
}

/// The `help` text.
pub fn help_text() -> String {
    let mut out = String::from("crypto-hft commands (UI Kit gateway):");
    for line in command_lines() {
        out.push('\n');
        out.push_str(&line);
    }
    out
}

#[test]
fn parses_start_defaults_and_flags() {
    match parse_command("start BTC,ETH --size 5 --dry-run").unwrap() {
        Command::Start {
            assets,
            size_usd,
            dry_run,
        } => {
            assert_eq!(assets, vec!["BTC".to_string(), "ETH".to_string()]);
            assert_eq!(size_usd.as_deref(), Some("5"));
            assert!(dry_run);
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn parses_positions_limit_and_status() {
    assert_eq!(
        parse_command("positions 25").unwrap(),
        Command::Positions { limit: 25 }
    );
    assert_eq!(
        parse_command("positions").unwrap(),
        Command::Positions { limit: 0 }
    );
    assert_eq!(parse_command("STATUS").unwrap(), Command::Status);
    assert_eq!(parse_command("stop").unwrap(), Command::Stop);
    assert_eq!(parse_command("RESTART").unwrap(), Command::Restart);
    assert!(parse_command("restart now").is_err());
}

#[test]
fn parses_manual_flatten_with_stable_position_id() {
    assert_eq!(
        parse_command("flatten pos-123").unwrap(),
        Command::Flatten {
            position_id: "pos-123".into()
        }
    );
    assert_eq!(
        parse_command("close pos-123").unwrap(),
        Command::Flatten {
            position_id: "pos-123".into()
        }
    );
    assert!(parse_command("flatten").is_err());
    assert!(parse_command("flatten pos-1 extra").is_err());
}

#[test]
fn rejects_unknown_and_bad_flags() {
    assert!(parse_command("buy BTC").is_err());
    assert!(parse_command("start --nope").is_err());
    assert!(parse_command("").is_err());
}

#[test]
fn parses_plugin_verbs() {
    assert_eq!(parse_command("strategies").unwrap(), Command::Strategies);
    assert_eq!(parse_command("extensions").unwrap(), Command::Extensions);
    assert_eq!(parse_command("markets").unwrap(), Command::Markets);
    assert_eq!(
        parse_command("strategy trend_follow on").unwrap(),
        Command::StrategySet {
            name: "trend_follow".into(),
            enabled: true
        }
    );
    assert_eq!(
        parse_command("strategy mean_reversion off").unwrap(),
        Command::StrategySet {
            name: "mean_reversion".into(),
            enabled: false
        }
    );
    assert_eq!(
        parse_command("extension my_ext off").unwrap(),
        Command::ExtensionSet {
            name: "my_ext".into(),
            enabled: false
        }
    );
}

#[test]
fn rejects_malformed_plugin_verbs() {
    assert!(parse_command("strategy").is_err());
    assert!(parse_command("strategy solo").is_err());
    assert!(parse_command("strategy s maybe").is_err());
    assert!(parse_command("extension").is_err());
    assert!(parse_command("strategie x on").is_err());
}

/// Every row of [`COMMANDS`] must dispatch, and to the command its own usage
/// claims. The match in `variant` is the tripwire: a new `Command` variant
/// stops compiling here until its row is added to the table (the drift that
/// left a phantom `risk` in the panel's completion and hid four real verbs).
#[test]
fn every_command_row_dispatches() {
    fn variant(c: &Command) -> &'static str {
        match c {
            Command::Start { .. } => "start",
            Command::Stop => "stop",
            Command::Restart => "restart",
            Command::Status => "status",
            Command::Positions { .. } => "positions",
            Command::Strategies => "strategies",
            Command::StrategySet { .. } => "strategy",
            Command::Extensions => "extensions",
            Command::ExtensionSet { .. } => "extension",
            Command::Flatten { .. } => "flatten",
            Command::ForceOrder { .. } => "force-order",
            Command::Markets => "markets",
            Command::NetCheck => "netcheck",
            Command::Help => "help",
            Command::EvolutionProposals { .. } => "proposals",
            Command::EvolutionDecide { .. } => "decide",
            Command::EvolutionAuto { .. } => "auto-evolve",
            Command::EvolutionEngine { .. } => "evolve",
            Command::EvolutionRollback { .. } => "rollback",
            Command::Accounts => "accounts",
            Command::AccountSwitch { .. } => "account",
        }
    }
    // Rows whose usage carries placeholders need a filled-in line to parse.
    const FILLED: &[(&str, &str)] = &[
        ("strategy", "strategy alpha on"),
        ("extension", "extension market_polymarket on"),
        ("flatten", "flatten pos-1"),
        ("force-order", "force-order dry_7"),
        ("decide", "decide prop-1 accept"),
        ("rollback", "rollback alpha"),
        ("auto-evolve", "auto-evolve on"),
        ("evolve", "evolve on"),
        ("account", "account main"),
    ];
    for &(usage, _) in COMMANDS {
        let verb = verb_of(usage);
        let line = FILLED
            .iter()
            .find(|(v, _)| *v == verb)
            .map(|(_, line)| *line)
            .unwrap_or(verb);
        let parsed = parse_command(line)
            .unwrap_or_else(|e| panic!("COMMANDS row `{verb}` (`{line}`) does not parse: {e}"));
        assert_eq!(
            variant(&parsed),
            verb,
            "COMMANDS row `{verb}` parses as a different command"
        );
    }
}

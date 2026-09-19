# Changelog

All notable changes to BlitzkriegBot are documented here.
Format: [Keep a Changelog](https://keepachangelog.com); versioning: semver.

## [Unreleased]

### Added

- **`blitzkrieg stop` — the operator's single switch for a stack on one socket.**
  `run` had no counterpart: stopping meant hand-collecting pids, and a stack
  could outlive its owner (an orphaned core still serving its socket; a
  launcher that adopted it can read but, by the adopt-only restraint, never
  stop it). `stop [--socket <path>] [--timeout <sec>]` scans the process table
  once, decides purely on that table, and signals through `libc::kill` — no
  subprocess, no shell, no string ever crosses a process boundary. It touches
  only blitzkrieg-family processes attached to the named socket, signals
  owners first (they cascade to their own children), then any core still
  alive, never signals the ancestor chain that invoked it or any other
  stranger (reported as "left untouched" instead), treats a zombie as dead,
  escalates to SIGKILL only after the grace period, and removes a stale socket
  file once nothing serves it. A second stop is a no-op.

- **The panel reports and recovers from a kernel crash (E12-c / #94).** A core
  started by an `--manage` gateway is now supervised for liveness, not just
  spawned: the gateway notices it died, says **how** (`lastExit.kind` separates
  a crash from a stop the operator asked for, with the signal named), replaces
  it under a finite budget (5 attempts, 250 ms exponential backoff capped at
  8 s), and keeps a count on the wire so a core that silently came back is
  still visible as one that crashed. A stop the operator asked for is reported
  as clean and never triggers a replacement. The panel shows the crash as a
  banner that survives the restart that repaired it, and says "gave up" when
  the budget is spent. Read-only gateways keep the previous behaviour (adopt
  only, report nothing they did not see). Also fixed two defects this work
  uncovered: `Supervisor::reap` discarded a crashed child's exit status (so the
  UI kept reporting a process that was gone), and `connected` was derived from
  a cached socket handle rather than from a core actually answering — so for
  one poll after a crash the panel showed a running engine with no positions.
- **The kernel reads TOML configuration (KI-11 / D-1).**
  `user_layer/configs/default.toml` and its sibling `shadow_evolution.toml`
  are now parsed at startup instead of being inert documentation. Precedence,
  highest first: command-line flag → `BK_*` environment variable → file →
  compiled-in default. Every value above the default is logged at startup as
  `key=value (source)`, so a running process can be asked where a setting came
  from. `--config <path>` / `--config=<path>` / `--no-config` (or
  `BK_CONFIG=<path>` / `BK_CONFIG=none`) choose the file. A missing file is not
  an error, a malformed one only warns, and **an unrecognised key is reported
  individually** rather than silently ignored. `extensions/<name>/config.toml`
  is read for `[meta]` and checked against the linked extension for drift;
  `[market]`/`[risk]`/`[dependencies]` are reported as declared-but-inert
  because no adapter consumes them. There is no hot reload.

### Changed

- **The plugins page no longer lists strategies (E11 / D-32).** The strategy
  page is the single entry point for strategies — source, toggles, per-strategy
  ledger — so the plugins page keeps only what it is actually about: extension
  and market plugins. The identity strip drops the strategy card and its
  enabled-count KPI; `/api/plugins` still carries the strategies array (the
  strategy page's registry fallback uses it), it is just not rendered here.
  The TUI is out of scope for this change.

- **The kernel starts with ZERO strategies enabled, and the operator's toggles
  persist across restarts.** `blitzkrieg-core` hardcoded `spread_arb` as the
  default-enabled strategy — a pre-PR-B leftover that coupled the kernel to one
  strategy and made every fresh boot trade it whether the operator wanted it or
  not. A fresh boot now registers everything the strategy dir holds and
  enables none of it. What starts enabled is the persisted intent:
  `data/strategy-state.json` is rewritten on every `strategy.enable`/`disable`
  (panel, TUI, CLI toggle) and replayed at the next boot; `--enable-strategy`
  adds on top of it and `--disable-strategy` wins over both. `--strategy-state
  <path>` moves the file, `--no-strategy-state` turns persistence off (the
  backtester runs without it). The file is atomic (tmp + rename), sorted and
  deduplicated, and a missing or malformed one degrades to "nothing enabled" —
  a fresh checkout boots clean and the first panel toggle builds the set.

- **`spread_arb` ships a tuned entry discount: win rate 45% → 74% at a better
  payoff ratio.** The resting bid now sits at `0.88 × mid` instead of
  `0.98 × mid` (`trend_entry_factor`, the strategy's own declared knob — the
  kernel pushes no new keys and grows no flags). The mechanism is anti-adverse
  selection, not a tighter filter: at 0.98 the bid is filled by any downtick
  through it, so entries happened on noise and lost 55% of the time; at 0.88
  the fill requires a real flush, so entries land deep inside confirmed-trend
  dips where the trailing stop's +15% arm is a couple of ticks up and the −12%
  stop several ticks down. Evidence: deterministic replay of the frozen
  2026-09-17..19 archive (9.68M events, spread_arb alone, identity fill model)
  gives 235 closed / 73.62% win / payoff 1.38 / PF 3.86 / net +$126.19 vs the
  0.98 baseline's 149 / 44.97% / 1.18 / 0.96 / −$2.36 on the same corpus, with
  a time-split holdout (first three vs last two segments) at 74.67% / 71.76% —
  both halves clear the target with the payoff ratio above, not below, the
  baseline. The discount grid is monotone (0.90 → 70.3%/1.22, 0.85 →
  82.3%/1.80, 0.80 → 93.7%/1.91); 0.88 ships as the value that clears the goal
  with margin while staying off the declared 0.80 domain floor. Also in this
  change: four high-frequency entry filters (min OBI, max spread, max fade from
  the trend high, min short-window bounce) added to the shared strategy-logic
  evaluator and tracker (`spread_arb_tracker_gates`), defaulting OFF and
  settable through the standard `on_params` hot bag — deliberately NOT declared
  evolvable, because the evolution guard requires strictly positive knob values
  and these ship at 0. The kernel's per-strategy config and CLI stay untouched;
  fixture books in three ledger-identity tests now cross the 0.88 resting bid.

- **A `timing` gate exemption can no longer reach into the closing window of a
  round (D-31).** The exemption used to waive the round-timing window outright,
  which included "too close to expiry" — so an exempt strategy could enter with
  seconds left and the exit policy would flatten it on the very next tick by
  design. Those trades were correctly priced but had no round left to reach a
  target, so they were guaranteed zero-hold exits: noise in the dry ledger that
  dragged the win rate down without reflecting a strategy misjudgement. A
  strategy may now declare a `time_left_sec` floor (`timing_min_time_left_sec`,
  defaulting to the scanner's `min_time_left_sec`), and the exemption is honoured
  only at or above it. "Round too young" still waives as before, because its
  `time_left_sec` is large. `dog_strategy` declares 180 s; `mean_reversion`, which
  only waives the momentum gate, is unaffected. The value crosses the C ABI
  through the **existing** optional `bk_strategy_gate_exemptions` JSON — a
  library that omits the key gets the kernel's stricter default — so
  `BK_ABI_VERSION` stays 2 and no shipped library needs recompiling.
  **`engine.stats` numbers are not comparable across this change**: a candidate
  that used to become a trade in the closing window is now counted under
  `blocked.timing` instead. See `docs/rust-core/STRATEGY_GUIDE.md` §3.5.1.
- `user_layer/configs/default.toml` pins `round_sec = 900`. The file previously
  said `300` while the compiled default, the supervisor's `HFT_ROUND_SEC` and
  `scripts/soak-health.sh` all said 900; with the file now live, `300` would
  have been a silent behaviour change to 5-minute rounds. See
  `DECISIONS_PENDING.md` D-26.
- The consecutive-loss breaker is sharded per strategy (KI-10 / D-18 option A).
  One leg's losing streak no longer freezes entries for the whole core; the
  daily loss cap and kill switch stay global by design.
- **The tree is rustfmt- and clippy-clean, and CI enforces both (KI-13).** The
  format and lint gates in `rust-check` were advisory because of a pre-existing
  backlog; it is now cleared (103 formatting hunks, 121 clippy sites) and both
  steps block. Every file was reformatted with stock `cargo fmt --all`, and the
  lint fixes are behaviour-preserving rewrites — nested `if`s folded into
  edition-2024 let-chains, `&[x.clone()]` to `std::slice::from_ref`, and
  `Default::default()` shuffles into struct literals. Seven sites are narrow,
  commented allows where the fix would have changed a public API (`too_many_
  arguments` on existing strategy/venue entry points, `large_enum_variant` on
  `ReconcileAction`). Contributors now get a red build for a new warning.

## [0.2.0] - 2026-09-17

### Removed

- **Legacy Node application stack (759 files, -322k lines).** The production
  stack is 100% Rust: the `blitzkrieg-core` engine and the Polymarket
  extension plugin are spawned and supervised by `ui_kit_web`, which also
  serves the Vue panel and its API. The legacy Node gateway (agents,
  channels, feeds, execution, strategies, skills, MCP, …) had no running
  consumers. `src/` shrinks to the 5-file IPC verification layer used by
  the `core:*` / `account:*` acceptance gates.
- Legacy tests (22 → 2, both covering the IPC socket contract), legacy
  scripts, npm publish config, and the npm dependency tree (~60 → 4 runtime
  deps: pino, pino-pretty, ws, zod; `node_modules` 46 MB).
- Docs describing the deleted Node subsystems (API, architecture, skills,
  ACP, bittensor, EVM wallet, telemetry, security audit of the old stack, …).

### Changed

- License copyright line moves to `BlitzkriegBot contributors (ceer-quant)`.
- `docs/` reorganized: Rust system documentation stays public
  (`docs/`, `docs/rust-core/`); internal development working documents move
  to `dev-docs/` (not published).

### Verified

- `tsc` clean; 11/11 node tests; build clean; secret-scan clean.
- `cargo test --lib`: 232/232.
- Acceptance gates: shutdown-cleanliness, parent-monitor, readonly-egress
  all PASS against the rebuilt core.

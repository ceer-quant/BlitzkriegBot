# Changelog

All notable changes to BlitzkriegBot are documented here.
Format: [Keep a Changelog](https://keepachangelog.com); versioning: semver.

## [Unreleased]

### Added

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

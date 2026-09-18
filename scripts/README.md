# Blitzkrieg Scripts

Gates, observability, and packaging utilities. **All `.mjs` scripts are
zero-dependency bare Node (stdlib only) — no `npm install` needed at the repo
root.** The production runtime is 100% Rust; Node here only drives temporary,
isolated cores for verification.

Most gates spawn a temporary `blitzkrieg-core` (isolated socket via `TMPDIR` or
`BZK_CORE_ISOLATION`) and talk JSON-RPC over UDS through
[`lib/core-client.mjs`](lib/core-client.mjs). Build the core first:

```bash
cargo build --release --workspace --locked
```

## lib/

- `lib/core-client.mjs` — zero-dependency UDS JSON-RPC client + process
  supervisor (`CoreClient`): spawn/boot/retry, request/timeout, events,
  clean stop (cancels resting orders) and `killNow()` (exit guard).
- `lib/core-socket.mjs` — socket path helpers (TMPDIR-prefixed, per-user).
- `lib/child-guard.mjs` — guarded `spawn` for gate drivers. Drop-in replacement
  for `child_process.spawn` that reaps the spawned process *tree* (SIGTERM first
  for a clean shutdown, then SIGKILL) when the gate exits, throws, or is
  signalled. Gates that start a core directly — rather than through `CoreClient`,
  which owns its own exit guard — must import `spawn` from here: an interrupted
  gate otherwise leaves a core running with PPID=1, holding its socket. That is
  not hypothetical: on 2026-09-19 the process table held a 33-hour orphan from an
  interrupted run. See `child-guard-check.mjs` for the pinned behaviour.

## Acceptance gates

| Script | What it pins |
| --- | --- |
| `cycle-check.mjs` | DryRun full order chain: confirm → place → fill → position → revalue → exit |
| `core-parity.mjs` | Order/ledger semantics parity, 22 assertions (taker, maker, risk, kill switch, events) |
| `account-parity.mjs` | dry/live twin-core ledger bit-for-bit comparison |
| `core-adopt-check.mjs` | Duplicate client adopt semantics, no restart storm |
| `shutdown-cleanliness-check.mjs` | Stop means the core is gone and resting orders settled |
| `parent-monitor-check.mjs` | No orphaned core after the driver exits |
| `child-guard-check.mjs` | A child spawned through `lib/child-guard.mjs` cannot outlive its spawner — on normal exit, error, or signal; grandchildren included, and only after a chance to shut down cleanly |
| `readonly-egress-check.mjs` | `--readonly` is structural: live mode + credentials still cannot trade |
| `unified-launcher-check.mjs` | Single binary `blitzkrieg`: `run` starts both parts, subcommands dispatch, `--readonly` holds |
| `data-backup-check.mjs` | `data/` backup refuses dangerous destinations, self-verifies, prunes only its own dirs |
| `crash-recovery-check.mjs` | SIGKILL a live core → in-flight settles, replacement serves the socket |
| `webapp-check.mjs` | Panel: bundle served, auth both ways, CSRF, snapshot non-empty |
| `backtest-check.mjs` | Event-driven backtest: archive → offline replay → bit-identical |
| `order-recovery-check.mjs` / `position-recovery-check.mjs` | Crash recovery for orders / positions |
| `strategy-gate-check.mjs` / `strategy-limit-check.mjs` / `strategy-evolution-check.mjs` | Strategy-scoped gating / funding / shadow evolution |
| `trend-follow-check.mjs` / `mean-reversion-check.mjs` | Built-in strategy legs |
| `scale-plugins-check.mjs` / `feed-scale-check.mjs` | Registry read latency / feed loss rate at scale |
| `ui-eventbus-check.mjs` / `ui-kit-gateway-check.mjs` / `ui-plugin-check.mjs` | UI event push / gateway command surface |
| `trade-log-flag-check.mjs` / `market-plugin-check.mjs` | `--no-trade-log` isolation / plugin selection |

## Observability / ops

- `dry-observe.mjs` — attach to a running dry core and print round/order/position ticks.
- `soak-health.sh` / `soak-health-loop.sh` / `soak-monitor.mjs` — long-run health monitoring.
- `analyze-signals.mjs` / `analyze-strategy.mjs` — offline signal/strategy analysis.
- `feed-live-probe.mjs` / `poly-ws-endurance.mjs` / `poly-wire-measure.mjs` — Polymarket feed probes.
- `price-compare.mjs` / `reconcile-exits.mjs` / `sweep-exits.mjs` / `final-exit-opt.mjs` — pricing and exit sweeps.
- `account-drift-check.mjs` — live panel/account drift diagnosis.
- `blitzkrieg-new-strategy.mjs` — scaffold a new cdylib strategy under `user_layer/strategies/`.

## Data safety

- `data-backup.sh --dest <dir> [--keep N] [--exclude-archive] [--dry-run]` —
  verified, external backup of `data/`. `--dest` is mandatory and validated: a
  destination inside the repository is refused, because a copy that dies with the
  repo is not a backup. Each backup carries a `MANIFEST.sha256` of the source (so
  "the backup is good" is checkable), a `data.tar.gz`, and a `BACKUP.json`
  recording the repo revision. Pruning matches only strict
  `blitzkrieg-data-<UTCSTAMP>` names and skips rather than deletes anything that
  fails a guard (symlink, foreign name, not directly under `--dest`).
  `--verify <dir>` re-hashes the archive and every manifest entry against it.
  `--exclude-archive` drops `data/archive` (95%+ of the bytes) for a fast, light
  backup; the manifest is narrowed to match, so a light backup still verifies.
  It is the mechanism KI-24 found missing after the 2026-09-17 incident.
- `data-backup-check.mjs` — the safety gate for the above. Leads with the refusal
  branches (missing/nonexistent/symlinked `--dest`, repo-internal dest, recursive
  nest) before ever exercising a real backup, since the incident happened because
  the destructive path was tested before the guard was. Runs on a throwaway
  fixture; the real `data/` is never read or written.

## Packaging / CI helpers

- `package-release.mjs` — assemble the distributable bundle (≤ 500 MB) with a
  destination allow-list guard; `binary-size-check.mjs` — per-binary and
  tracked-tree budgets.
- `secret-scan.sh` — zero-dependency secret scan (`--all` includes untracked
  docs; `--history` scans full git history). Reports locations, never values.

## Conventions

- Gate scripts print `RESULT: PASS|FAIL` and exit `0`/`1`.
- Gates never touch the production socket (see the `BZK_CORE_ISOLATION` /
  `TMPDIR` isolation comments inside each script).
- No string interpolation into `execSync`; use `execFileSync` with argument arrays.

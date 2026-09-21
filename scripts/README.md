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
| `unified-launcher-check.mjs` | Single binary `blitzkrieg`: `run` starts both parts, subcommands dispatch, `--readonly` holds, `stop` reaps the stack (unified / orphan / adopted shapes), `.env` self-load supplies credentials, zero leftovers |
| `data-backup-check.mjs` | `data/` backup refuses dangerous destinations, self-verifies, prunes only its own dirs |
| `soak-health-check.mjs` | The ops health check can actually fail: every guard is inject-tested (down/wedged core, panel HTML fallback, stale sampling, panic log, both zero-hold causes) *and* a healthy fixture must exit 0. Bounded by its own watchdog (`BK_GATE_WATCHDOG_MS`) — it must also *exit* |
| `crash-recovery-check.mjs` | SIGKILL a live core → in-flight settles, replacement serves the socket |
| `webapp-check.mjs` | Panel: bundle served, auth both ways, CSRF, snapshot non-empty |
| `backtest-check.mjs` | Event-driven backtest: archive → offline replay → bit-identical |
| `order-recovery-check.mjs` / `position-recovery-check.mjs` | Crash recovery for orders / positions |
| `strategy-gate-check.mjs` / `strategy-limit-check.mjs` / `strategy-evolution-check.mjs` | Strategy-scoped gating / funding / shadow evolution. The gate check runs two cores: one where the timing window is shut by round AGE (the D-31 exemption must still waive it) and one where it is shut by remaining TIME (it must not — the dog declares a 180s floor) |
| `trend-follow-check.mjs` / `mean-reversion-check.mjs` | Strategy legs, driven through the external cdylibs (the kernel ships no builtins) |
| `scale-plugins-check.mjs` / `feed-scale-check.mjs` | Registry read latency / feed loss rate at scale |
| `ui-eventbus-check.mjs` / `ui-kit-gateway-check.mjs` / `ui-plugin-check.mjs` | UI event push / gateway command surface |
| `trade-log-flag-check.mjs` / `market-plugin-check.mjs` | `--no-trade-log` isolation / plugin selection |

## Observability / ops

- `install-blitzkrieg-shim.sh` — one-time install of a `blitzkrieg` command into
  a PATH directory (`~/.local/bin` → `/opt/homebrew/bin` → `/usr/local/bin`,
  first on PATH wins; `BLITZKRIEG_SHIM_DIR` overrides). The shim cd's to the
  checkout it was generated from and execs the freshly built binary there, so
  it can never go stale and core/strategy/data resolution keeps working from
  any directory. It overwrites only its own earlier output.
- `dry-observe.mjs` — attach to a running dry core and print round/order/position ticks.
- `soak-health.sh` / `soak-health-loop.sh` / `soak-monitor.mjs` — long-run health monitoring.
  `soak-health.sh` exits 0 (healthy) / 1 (anomaly) / 2 (not a repo root) and prints
  every sub-status in one line, because that line is all the loop logs:
  `core= round= panel= core-ping= soak= trades= archive= log=`. Anomalies come after
  it. Checks and their seams (each has a real default; they exist so
  `soak-health-check.mjs` can inject failures):
  - panel liveness — `BK_PANEL_URL` (default `http://127.0.0.1:51888`), via the
    panel's unauthenticated `/api/ping`. Note there is **no `/health` route**:
    unknown paths return 200 + the HTML panel, so a substring probe on one can
    never match (that was KI-30).
  - core liveness — `BK_CORE_PGREP` (default `target/release/blitzkrieg-core`) plus a
    `core.ping` over the socket taken from the core's own argv, so "alive but wedged"
    is caught too. `BK_SOCKET` overrides the path.
  - sampling freshness — `BK_SOAK_DIR` (default `data/soak`), judged by the newest
    `soak.jsonl` record's timestamp, not by a process name: `soak-monitor.mjs` can
    have a bounded lifetime (`--hours 12`) and exiting is then its normal end,
    whereas a stalled sampler is invisible to `pgrep`. `BK_SOAK_STALE_SEC` (default
    1800).
  - crash/archive-stop scan — `BK_RUN_LOG`. **Unset means unconfigured, and is
    reported as `log=off`**, because where the log lands is a deployment choice (the
    core's stdout is `/dev/null` and its stderr is inherited). Set it to scan; a
    configured-but-missing path is an anomaly.
  - archive freshness — `BK_ARCH_DIR` (default `data/archive`); trade ledger —
    `BK_TRADES` (default `data/trades/trades.jsonl`). The two ledger-derived
    counts (seconds-flatten bug, inside-the-force-exit-window entries) are bounded
    to `BK_TRADES_LOOKBACK_SEC` (default 86400 = 24h): `trades.jsonl` is
    append-only, so an unbounded count would make the alarm light up forever after
    a single historical occurrence — a check that can never clear is the same
    defect as one that can never fire (KI-30).
  `soak-health-loop.sh` additionally bounds its own `health.log` and `$BK_RUN_LOG`
  (`BK_LOG_MAX_BYTES`, default 20 MiB) by gzip + in-place truncate, leaving a log
  untouched if gzip fails. It no longer calls the deleted `rotate-run-log.sh`.
- `soak-resident.sh` — start/stop/status for the resident soak pair (D-30).
  `soak-monitor.mjs --forever` samples continuously instead of for a 12h window,
  and this wrapper is its explicit stop switch; `start` refuses to stack a second
  sampler (two samplers would double every figure in `soak.jsonl`). Deployment
  config (notably `BK_RUN_LOG`) is read from `$BK_SOAK_DIR/resident.env` as a
  fallback, with an explicit env value or `--run-log` winning over it.
  **It does not survive a reboot**, and that is a TCC limit, not an oversight:
  this repo is on an external volume and macOS denies a launchd-spawned process
  both read and exec access to it (measured: `read-volume: DENIED`,
  `exec-script: DENIED (exit=126)`). Boot persistence would need the user to grant
  Full Disk Access — a security-posture change, so it is left as a decision (D-33).
  `soak-monitor-check.mjs` gates both lifetimes and the stop switch.
- `stack-watchdog.sh` / `com.blitzkrieg.stack-watchdog.plist` — 交易内核的心跳检查与
  「停机」告警（issue #211 的 B 路线：只判定与告警，**不拉起**）。退出码 `0` = 内核存活
  并在服务；`1` = 不在跑 / socket 不可达（已告警）；`2` = 用法或配置错误。存活判定复用
  `soak-resident.sh` 的 `alive()` 语义（pidfile + `kill -0` + 命令行匹配）；内核自己不写
  pidfile，所以未配 `--pidfile` 时用 `pgrep -f` 发现候选、再用同一套语义复核。在此之上多
  一个 UDS connect 探针（与 `ui_kit` 的 `socket_served()` 同语义），于是输出能把「进程不在」
  和「进程在但 socket 不通」分开说——前者是停机，后者是卡死/抢占，处置方式不同。
  告警正文：当前模式（dry / live / readonly，**只读** `.env` 且永不改 `DRY_RUN`）、未平仓与
  未赎回应收（读 `data/`，读不到就写「无法判定」，绝不假装 0）、最后已知存活时间（状态文件
  无记录时退回数据落盘时间并标注是推断）、可直接粘贴的恢复命令。去抖：只在状态翻转时出声，
  持续停机期间最多每 `--repeat-sec`（默认 900s）重复一次。**默认绝不自动拉起**：要动手必须
  同时给出 `--autostart` 与 `BK_AUTOSTART_CMD`（两把钥匙），且 **live 模式一律硬拒绝**。
  计划内停机先 `touch $STATE_DIR/silence`，免得收到一条完全正确的告警。
  自测：`bash scripts/stack-watchdog.sh --self-test`（12 组用例 / 42 项断言，fixture 驱动，
  不需要真内核、不碰生产 `data/`、不建默认状态目录）。
  部署与「重启后谁跑它」见 README.md §3.5；**外置卷的 TCC 限制对它同样成立**（launchd 拉起
  的进程被拒读/执行本卷），三种应对：授予完全磁盘访问权限 / 把脚本复制到内置盘并用
  `BK_REPO_ROOT` 指回本仓库 / 把检出搬到内置盘。仓库里附的
  `com.blitzkrieg.stack-autostart.plist.disabled` 是路线 A 的骨架：**装上它 = 无人值守自动
  拉起 live，属于用户决定，默认不安装**。
- `analyze-signals.mjs` / `analyze-strategy.mjs` — offline signal/strategy analysis.
- `walk-forward-sweep.mjs` / `shadow-export.mjs` / `strategy-ab-compare.mjs` — the
  strategy-evolution toolkit (E15 / #97). The sweep splits a frozen event archive
  into equal-event-count folds and replays every candidate × fold through the
  production backtester — candidates vary the `--spread-arb-*` strategy knobs
  (standard `CoreConfig → engine_config` chain, no side door) — then picks the
  winner on fold i and validates it on fold i+1 (rolling walk-forward; full
  fold×candidate matrix in the report, resume-safe). The exporter writes a
  coverage manifest (first/last ts, event counts, content sha256, per-UTC-day
  histogram) that states the 30-day premise honestly. The A/B tool verdicts two
  backtest reports with the ≥ `--min-better` (default 2) metrics-better rule and
  gateable exit codes. Methodology + first run: `docs/STRATEGY_EVOLUTION.md`.
- `dryrun-report.mjs` — the E16 / #98 7-day DryRun report: per-strategy ledger
  (独立账本), per-UTC-day trend and portfolio totals read from the append-only
  trade ledger, plus breaker/evolution audit events in-window. Read-only over
  `data/trades/trades.jsonl`; `--days N` (default 7) or `--from/--to` pin the
  window.
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

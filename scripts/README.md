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

- `lib/backup-attempt.sh` — the one attempt-record format (`tier`/`source`/`at`/
  `attempt_epoch`/`result`/`detail`/`dest`/`artifact`/`bytes` + the last SUCCESS's
  `success_at`/`success_epoch`/`success_artifact`/`success_bytes`) written by every
  actor that can start a backup: the launchd launcher, the resident loop, and a hand
  run. `artifact`/`bytes` are read out of the `BACKUP.json` the backup wrote itself
  (`sourceBytes + archiveBytes`), so "last success 2 hours ago" can be told apart from
  "last success was an empty directory" — at no extra cost, since a size computed by
  `du` would mean walking an 8 GB full backup again. It lives on the **internal** disk
  (`$BK_BACKUP_STATUS_DIR`, default `$HOME/Library/Logs`) precisely so that "I ran
  and could not reach the repo (Operation not permitted)" survives on a machine
  whose external volume is unreachable. Three inline copies would drift, and a
  drifted record reads as "no attempt" — back to the silence issue #217 is about.
- `templates/` — what a scheduled backup is MADE OF, checked in so it can be read in
  review instead of only after it lands in `~/Library`: the internal-disk launcher and
  both plists, rendered by `data-backup-install.sh`. `templates/README.md` documents the
  manual route and the two facts that cost a month of backups — a plist that exists is
  not a backup that runs, and moving the launcher to the internal disk does not grant
  access to the repository (TCC authorization is still required).
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
- `lib/core-provenance.mjs` — where a core came from (binary path, args, dylibs
  loaded), used by the gates that must prove which build they exercised.

## Acceptance gates

| Script | What it pins |
| --- | --- |
| `cycle-check.mjs` | DryRun full order chain: confirm → place → fill → position → revalue → exit |
| `core-parity.mjs` | Order/ledger semantics parity, 22 assertions (taker, maker, risk, kill switch, events) |
| `account-parity.mjs` | dry/live twin-core ledger bit-for-bit comparison |
| `exit-economics-check.mjs` | The ECONOMIC gate on exit reachability (#272): replays the core over the four sha256-pinned frozen corpus hours with ONE strategy cdylib (`user_layer/strategies/spread_arb` — the measurement fixture) and judges closed / win rate / net USD against a recorded baseline. It goes red when the same corpus pays **less**, which is the question the mechanism tests cannot answer. `--teeth` is the demonstrated-failing arms; `--arm <flags…>` puts any candidate under the same verdict |
| `core-adopt-check.mjs` | Duplicate client adopt semantics, no restart storm |
| `shutdown-cleanliness-check.mjs` | Stop means the core is gone and resting orders settled |
| `parent-monitor-check.mjs` | No orphaned core after the driver exits |
| `child-guard-check.mjs` | A child spawned through `lib/child-guard.mjs` cannot outlive its spawner — on normal exit, error, or signal; grandchildren included, and only after a chance to shut down cleanly |
| `readonly-egress-check.mjs` | `--readonly` is structural: live mode + credentials still cannot trade |
| `unified-launcher-check.mjs` | Single binary `blitzkrieg`: `run` starts both parts, subcommands dispatch, `--readonly` holds, `stop` reaps the stack (unified / orphan / adopted shapes), `.env` self-load supplies credentials, zero leftovers |
| `data-backup-check.mjs` | `data/` backup refuses dangerous destinations, self-verifies, prunes only its own dirs; `--status` separates fresh / stale / never / unreadable / failed-scheduler and clears again; the attempt record is one format for all three actors; a linked worktree (`.git` is a file, #231) is accepted; the resident loop really backs up, records `source=loop` and is loud when a run fails |
| `soak-health-check.mjs` | The ops health check can actually fail: every guard is inject-tested (down/wedged core, panel HTML fallback, stale sampling, panic log, both zero-hold causes, backup freshness incl. "fresh artifact, failing scheduler" and "a missing check script is an anomaly") *and* a healthy fixture must exit 0. Bounded by its own watchdog (`BK_GATE_WATCHDOG_MS`) — it must also *exit* |
| `crash-recovery-check.mjs` | SIGKILL a live core → in-flight settles, replacement serves the socket |
| `webapp-check.mjs` | Panel: bundle served, auth both ways, CSRF, snapshot non-empty |
| `order-recovery-check.mjs` / `position-recovery-check.mjs` | Crash recovery for orders / positions |
| `strategy-devcheck.mjs` | The strategy developer's whole loop, end to end: scaffold a cdylib from the template → build it → `strategy.load` → `strategy.enable` → a signal reaches the engine → its declared knob round-trips → its shadow twin builds. It makes its own probe crate, so it needs no shipped strategy |
| `intent-audit-check.mjs` | DEV_V0_3 §16.4 (E25 / #331): EVERY strategy suggestion leaves exactly ONE self-consistent audit line (`data/audit/intents.jsonl`) — approved rows carry the full four-gate run, rejected rows exactly one REJECT trace — and an ILLEGAL suggestion (price outside the prediction band) is refused at LEGALITY and never becomes an order. Drives a real dry core with a generated probe strategy (template + one patched-in illegal entry). `--self-test` pins the judge; `--teeth` feeds it the two broken pipelines (return-Approved-after-Gate-1 → `gates[1] missing`; neutralized validate() → `reached OME`) and must go red. The rejection-storm acceptance (4000/s: audit line-by-line, `INTENT_DECISION` folded ≤1/s) is pinned by the Rust unit tests `arbitration::tests` |
| `scale-plugins-check.mjs` / `feed-scale-check.mjs` | Registry read latency / feed loss rate at scale |
| `ui-eventbus-check.mjs` / `ui-kit-gateway-check.mjs` / `ui-plugin-check.mjs` | UI event push / gateway command surface |
| `trade-log-flag-check.mjs` / `market-plugin-check.mjs` | `--no-trade-log` isolation / plugin selection |
| `core-args-check.mjs` | #228: an argument the build does not recognise **stops the boot** (exit 2, named, with the closest legal spelling suggested); a safety near-miss (`--read-only`, `--redonly`, `--max-notinal`) is refused with the flag meant *and* what leaving it out would have meant; `--help`/`-h` exit 0 and list exactly the flags the parser accepts; `--version`/`-V` exit 0 with no boot; and a refused argument writes **no** ledger file. `--allow-unknown-args` is the explicit opt-out for a benign unknown argument and never covers a safety near-miss |

## Observability / ops

- `install-blitzkrieg-shim.sh` — one-time install of a `blitzkrieg` command into
  a PATH directory (`~/.local/bin` → `/opt/homebrew/bin` → `/usr/local/bin`,
  first on PATH wins; `BLITZKRIEG_SHIM_DIR` overrides). The shim cd's to the
  checkout it was generated from and execs the freshly built binary there, so
  it can never go stale and core/strategy/data resolution keeps working from
  any directory. It overwrites only its own earlier output.
- `upgrade.sh` — the one-shot production upgrade behind the shim's `upgrade`
  verb (#252, #244): light data backup → fetch the source (`ceer/…`, override
  with `BLITZKRIEG_UPGRADE_SOURCE`) → detached checkout of the build worktree
  `target/bk-main-build` → release build of the workspace, the strategy cdylibs
  and the panel's webui bundle → local gates (core lib tests **and** the panel's
  `check:all`; no pipeline, so a red suite actually stops the deploy) → stage the
  whole release and print what it changes → stop → install → start → verify
  identity through `.core-lock`. A release carries more than the four binaries:
  `user_layer/configs/*.toml` (the factory values), `user_layer/*/target/release/
  *.dylib` (the strategy code the kernel `dlopen`s out of the *running* checkout)
  and `ui/webapp/webui/dist` (the bundle the launcher serves). Every replaced file
  is kept under `target/rollback-<ts>` (three newest kept) and restored if the new
  build does not answer within `BLITZKRIEG_UPGRADE_DEADLINE` (120s). `--check`
  builds, gates and stages without touching the live stack. It never edits `.env`.
- `lib/upgrade-artifacts.sh` — the staging/drift/install/verify/rollback half of
  the above, sourced by `upgrade.sh` and driven directly by
  `upgrade-propagate-test.sh` (no build, no network).
- `upgrade-propagate-test.sh` — `sh scripts/upgrade-propagate-test.sh`, exit 0 =
  pass: stages into two throwaway trees, asserts the drift report, installs,
  catches a half-written install, rolls back (including a hand-edited config the
  upgrade replaced) and prunes the rollback sets. Runs in CI (`ops-gates`, macOS),
  so a break in the propagation path fails with a named assertion instead of
  shipping an upgrade that leaves the previous build's configs and strategies in
  place.
- `dry-observe.mjs` — attach to a running dry core and print round/order/position ticks.
- `soak-health.sh` / `soak-health-loop.sh` / `soak-monitor.mjs` — long-run health monitoring.
  `soak-health.sh` exits 0 (healthy) / 1 (anomaly) / 2 (not a repo root) and prints
  every sub-status in one line, because that line is all the loop logs:
  `core= round= panel= core-ping= soak= trades= archive= backup= log=`. Anomalies come after
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
  - **backup freshness** (issue #217) — `BK_BACKUP_DIR`, `BK_BACKUP_STATUS_DIR`,
    `BK_BACKUP_LOG_DIR`, `BK_BACKUP_STALE_HOURS` (26), `BK_BACKUP_FULL_STALE_HOURS`
    (192). Delegates to `scripts/data-backup.sh --status --quiet` and folds its
    one-line token into the summary, so "zero backups are happening" reaches the
    SAME alert channel as everything else (the README already warned about status
    hiding in four places; a fifth monitor would have been the bug). It is not
    reimplemented here on purpose: two implementations of "is the backup fresh"
    would eventually disagree, and the wrong one would be the one nobody reads.
    A **missing** `data-backup.sh` is an anomaly (`backup=missing-script`), not a
    skip — a check that silently vanishes when a file is renamed is the defect.
    This is what catches the incident's shape: a registered, "installed" schedule
    that had never once produced a backup, whose whole outward trace was one
    94-byte log line (the issue recorded `launchctl list` exit code `0`; a fresh
    read on 2026-09-23 shows `126` — the code was there, and nothing consumed it).
  `soak-health-loop.sh` additionally bounds its own `health.log` and `$BK_RUN_LOG`
  (`BK_LOG_MAX_BYTES`, default 20 MiB) by gzip + in-place truncate, leaving a log
  untouched if gzip fails. It no longer calls the deleted `rotate-run-log.sh`.
  Both health scripts take `BK_REPO_ROOT` (same convention as `stack-watchdog.sh`):
  it is how a copy on the internal disk runs against the external-volume checkout
  under launchd. **The backup check is only as alive as its runner** — with
  `soak-health-loop.sh` not started, nothing runs it; `blitzkrieg backup --status`
  is then the check you run by hand (and it exits 1 in the broken state).
- `soak-resident.sh` — start/stop/status for the resident soak pair (D-30).
  `soak-monitor.mjs --forever` samples continuously instead of for a 12h window,
  and this wrapper is its explicit stop switch; `start` refuses to stack a second
  sampler (two samplers would double every figure in `soak.jsonl`). When a
  pidfile is stale it says WHY before starting a fresh one — "pid is not
  running", or "the pid was recycled by another process" — because replacing a
  pidfile in silence makes a correct refusal to adopt a stranger's pid look like
  `start` being broken (#212). Deployment
  config (notably `BK_RUN_LOG`) is read from `$BK_SOAK_DIR/resident.env` as a
  fallback, with an explicit env value or `--run-log` winning over it.
  **It does not survive a reboot**, and that is a TCC limit, not an oversight:
  this repo is on an external volume and macOS denies a launchd-spawned process
  both read and exec access to it (measured: `read-volume: DENIED`,
  `exec-script: DENIED (exit=126)`). Boot persistence would need the user to grant
  Full Disk Access — a security-posture change, so it is left as a decision (D-33).
  `soak-monitor-check.mjs` gates both lifetimes and the stop switch.
- `stack-watchdog.sh` / `com.blitzkrieg.stack-watchdog.plist` — 交易内核的心跳检查与
  「停机」告警（issue #211 的 B 路线：只判定与告警，**不拉起**）。退出码 `0` = **唯一**内核存活
  并在服务；`1` = 不在跑 / socket 不可达（已告警）；`2` = 用法或配置错误；`3` = 发现 ≥2 个内核
  进程（重复内核，issue #199，**优先于 1**）。存活判定复用
  `soak-resident.sh` 的 `alive()` 语义（pidfile + `kill -0` + 命令行匹配）；内核自己不写
  pidfile，所以未配 `--pidfile` 时用 `pgrep -f` 发现候选、再用同一套语义复核（`pgrep -f | head -1`
  取哪个是**不确定的**——内核之间没有主次，所以计数与告警都以复核后的完整列表为准）。
  命令行用 `ps -ww` 读并拒收僵尸 `state=Z`（`kill -0` 对僵尸是成功的；
  实测 BSD `ps` 只在 stdout 是 tty 时按终端宽度截断，脚本永远接管道，`-ww` 是防御性写法）。在此之上多
  一个 UDS connect 探针（与 `ui_kit` 的 `socket_served()` 同语义），于是输出能把「进程不在」
  和「进程在但 socket 不通」分开说——前者是停机，后者是卡死/抢占，处置方式不同。
  告警正文：当前模式（dry / live / readonly，**只读** `.env` 且永不改 `DRY_RUN`）、未平仓与
  未赎回应收（读 `data/`，读不到就写「无法判定」，绝不假装 0）、最后已知存活时间（状态文件
  无记录时退回数据落盘时间并标注是推断）、可直接粘贴的恢复命令。去抖：只在状态翻转时出声，
  持续停机期间最多每 `--repeat-sec`（默认 900s）重复一次。**默认绝不自动拉起**：要动手必须
  同时给出 `--autostart` 与 `BK_AUTOSTART_CMD`（两把钥匙），且 **live 模式一律硬拒绝**。
  计划内停机先 `touch $STATE_DIR/silence`，免得收到一条完全正确的告警。
  **重复内核（issue #199）是不依附于 up/down 的一等信号**：两个内核写同一份账本与订单库，
  而表现形态恰好是「socket 通、日志干净」。所以 `--status` 与正常检查都给退出码 `3`（调度器
  可编程发现），落 `$STATE_DIR/DUPLICATE_CORES` 标记，告警正文自包含（风险 + 全部 pid +
  每个 pid 各自的 argv 模式与 socket + 止损命令 `blitzkrieg stop`），去抖与停机共用状态机，
  解除时记一行并发「重复内核已解除」（不误报成「栈已恢复」）；**重复期间与「pidfile 判死但
  pgrep 复核发现内核在跑」时不自动拉起**——再拉只会更多。`BK_CORE_VERIFY_PGREP` 是对
  已发现 pid 的命令行复核串（默认同 `BK_CORE_PGREP`），只用于自测注入「发现了但复核不认」
  这一态，生产不要设。
  自测：`bash scripts/stack-watchdog.sh --self-test`（23 组用例 / 129 项断言，fixture 驱动，
  不需要真内核、不碰生产 `data/`、不建默认状态目录；含**无 `--pidfile` 的 pgrep 发现分支**
  命中 / 无命中 / 两个命中 / 复核不认 / pidfile 过期，**子串复核把构建 shell 当内核**的
  回归，僵尸进程与长命令行 pty 两个回归，以及第 23 组的**备份新鲜度**：新鲜时安静、陈旧时
  告警 + `BACKUP_STALE` 标记 + 退出码 `4`、去抖与 `--repeat-sec 0` 重报、内核停机时抑制
  通知但保留标记与退出码、恢复后清标记并记「备份已恢复新鲜」、判定器缺失时报 `unusable`
  而不是当作通过、`--no-backup-check` 关掉整条）。
  **备份新鲜度（issue #217）是看门狗的第二条独立信号**：距上次成功备份超过
  `BK_BACKUP_STALE_HOURS`（light，默认 26h）或 `BK_BACKUP_FULL_STALE_HOURS`（full，默认
  192h）就告警，退出码 `4`（优先级：重复内核 `3` > 内核停机 `1` > 备份陈旧 `4`）。
  判定不在这里重新实现：它调 `scripts/data-backup.sh --status --quiet` 并读那一行，
  两个实现会分歧，一个实现不会。
  复核口径的已知代价：`pgrep -f`/`grep` 都是**整条命令行上的子串匹配**，所以「只在命令行里
  提到内核路径」的进程也会命中（本机实测 4 个 `cargo build`/`ls` 构建 shell）。判定不收紧
  （收紧会漏掉手工起的、argv 里没有 `--socket` 的真内核），而是：告警里逐个 pid 点名，
  argv 里既无 `--mode` 也无 `--socket` 的标「可疑」提示逐条核对；要更严可把
  `BK_CORE_PGREP` 设成 `target/release/blitzkrieg-core.*--socket`。
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
  window. It also folds the order log (`--orders data/orders/orders.jsonl`) into
  the #183 fill metrics — fill rate, partial-fill rate and the filled-size ratio
  — because the trade ledger only knows CLOSED positions, not how their entries
  filled; `--json` prints that manifest to stdout for a script.
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
  fixture; the real `data/` is never read or written. It also pins `--status`
  (empty/absent/stale/unreadable tiers, a failed scheduler, "a manual run is not
  scheduler evidence", and that a success CLEARS the alarm), the shared attempt
  record, the linked-worktree checkout guard (#231), and the resident loop
  (`start --once` must produce a real backup with `source=loop`, be read back as
  scheduler evidence, and leave a `FAIL:` last line plus a `result=fail` record
  when the run fails — route B was itself broken by a `local` scoping slip the
  first time it ran, so it is tested rather than assumed).

### Scheduled backups: two routes, and why (issue #217)

**The fact** (measured 2026-09-21, repo on an external volume): a launchd-spawned
process gets `Operation not permitted` reading **and** exec'ing anything on this
volume. The installed `com.blitzkrieg.databackup.*` agents therefore ran, failed
in under a second, and left one 94-byte log line as their only trace — so a system
with *zero* automatic backups looked "installed and scheduled" on every surface an
operator would check. Nothing consumed the exit status either: the issue recorded
`0` in `launchctl list`, a fresh read on 2026-09-23 shows `126`, and in both cases
no status file, no alert and no line of `--status` said anything. That is the
defect: not that TCC refused (that is a deployment choice) but that the refusal
was invisible.

**Route A — LaunchAgent + Full Disk Access** (survives reboot and logout):

```bash
bash scripts/data-backup-install.sh          # install + probe; non-zero if still blocked
bash scripts/data-backup-install.sh --no-verify   # skip the probe
bash scripts/data-backup-install.sh --uninstall   # remove agents + launcher
```

The installer **renders two templates that live in this repository**
(`scripts/templates/data-backup-launch.sh` and the two
`com.blitzkrieg.databackup.*.plist`) into `~/Library/Application Support/blitzkrieg/`
and `~/Library/LaunchAgents/`, so what a scheduled job will run is readable in review
rather than only after it is installed; it lints the rendered plist (`plutil -lint`)
and parses the rendered launcher (`sh -n`), refusing to finish on a leftover
placeholder. `scripts/templates/README.md` has the manual route and the warnings.
The launcher points `ProgramArguments` at the internal-disk copy, because a `/bin/sh`
that cannot even read its script has no way to report anything. It preflights real
reads of the checkout and of the backup root; when one is denied it does three things
instead of failing silently: writes a `result=fail` **attempt record** (internal disk,
the same file `--status` and the watchdog read), prints a dated
`FAIL: … (Operation not permitted)` line naming the fix, and exits **126**. When the
reads succeed it `exec`s `data-backup-cli.sh <tier> --attempt-source launchd`.

Moving the launcher is necessary but **not sufficient** — the launcher must still
read the repository and the backup volume, and TCC denies exactly that. It changes a
silent failure into a recorded one; it does not make the backup run. The remaining
step is the user's, and it cannot be automated (D-33):

1. System Settings → Privacy & Security → **Full Disk Access**.
2. **+**, then ⌘⇧G and type `/bin/sh` — the interpreter launchd starts, i.e.
   `ProgramArguments[0]`; turn its switch on.
3. Re-run `bash scripts/data-backup-install.sh` (it re-probes) — or wait for the
   next 04:00 and run `bash scripts/data-backup.sh --status`.
4. Only a `PASS` / an `ok` status line counts. "installed" is not evidence.

Issue #217 names two alternatives to this route: move the checkout to the internal
disk (also fixes the same class of problem for the #211 watchdog), or drop launchd and
let the trading core — which you started by hand and which therefore already has TCC
access — trigger the backup itself, with failures going through `emit_error`. Both are
deployment decisions, not code in this repository.

**Route B — the resident loop** (works today, no permission change;
**does not survive reboot/logout**):

```bash
scripts/data-backup-loop.sh start          # daily 04:00 light, Sunday 04:30 full
scripts/data-backup-loop.sh start --once   # one light run now, then exit
scripts/data-backup-loop.sh status|stop
```

A detached `nohup` process from the operator's own session inherits that session's
TCC access, exactly like `scripts/soak-resident.sh`. It writes the same per-tier
logs and the same attempt records as Route A, so `--status` reads either one.

**The check that makes a forgotten restart loud** (both routes):

```bash
bash scripts/data-backup.sh --status          # one line; exit 1 if not happening
blitzkrieg backup --status                    # the same, through the shim
```

`--status` judges two independent things: the newest **artifact** on disk (age per
tier, with `ABSENT` / `NOTDIR` / `DENIED` / `NONE` as distinct causes) and the
newest **automatic** attempt (the launchd/loop log's last line, classified by a
tight failure signature, plus the attempt record written by every actor via
`scripts/lib/backup-attempt.sh` — which also carries the artifact the run produced and
its size, so a 0-byte "success" cannot pass for a backup). A record whose `source` is
`cli` is deliberately NOT scheduler evidence: a successful hand-run backup must never
mark the schedule healthy. An artifact-only check would have reproduced the incident's
false comfort — a fresh hand-made copy with a scheduler that has never once succeeded.

`stack-watchdog.sh` runs the same verdict every 60 s when its agent is installed (exit
`4` + a `BACKUP_STALE` marker when a tier is past its tolerance), so a backup that
stopped happening is alarmable without a human running an inspection first — the
checkup route (`soak-health.sh`) still carries it as a `backup=` sub-status.

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

# Changelog

All notable changes to BlitzkriegBot are documented here.
Format: [Keep a Changelog](https://keepachangelog.com); versioning: semver.

## [0.2.1] - 2026-09-25

### Added

- **版本单事实来源（docs/VERSIONING.md E-V1..V7）**。全部 workspace 成员继承根
  `Cargo.toml` 的 `[workspace.package].version`（0.2.1），由新增的
  `scripts/version-guard.mjs` 守卫（成员继承 / cargo metadata 一致 / 二进制自述
  比对 / src-tauri 双版本同步），并接进 CI（`release/**` 纳入触发）。构建期盖章
  收敛为 `core/build_support`（一处实现）+ `core/build_info`（运行期唯一出口
  `BUILD_INFO`，零依赖）；`core/blitzkrieg_core/build.rs` 的既有语义（12 位短
  sha、tracked-only 脏位、`nogit` 兜底、显式盖章 `BLITZKRIEG_GIT_SHA`）全部保留。
- **`blitzkrieg version [--json] [--core] [--socket]`** 与 `--version`/`-V`/`-v`
  短路（#228 同形风险：这两个旗标此前会落入启动路径拉起整套交易栈）。
  三种格式（人读 4 行 / camelCase JSON / `<semver>+g<sha>` 单行）各有测试钉住。
- **`system.version` IPC**（只读、不取 Core 锁，数据锁未就绪也可回答）：
  11 字段契约含 `updateAvailable` 三态（`null` = 未检查 ≠ `false` = 已是最新），
  见 `docs/rust-core/INTERFACES.md` §2.1/§4。TUI 第 6 个 tab（Settings）与
  WebUI 设置页「版本与更新」卡片同源展示；`tui-parity.check.mjs` 与新增的
  `version-panel.check.mjs` 双向守卫。
- **更新检查（默认关闭）**：开关来源 CLI > env > `user_layer/configs/update.toml`
  > 内置默认（关）；运行时开关持久化 `data/update/state.json`（原子写、落审计）。
  `checkEnabled=false` 时结构性地一个包都不发（INV-3）。`system.update.check` /
  `system.update.configure` 两个动词；semver 比较只有内核一处
  （`0.10.0 > 0.9.0`，rc 不打扰正式版）。
- **`blitzkrieg update` 动词**：`--check` 经内核询问发布源；校验原语
  （SHA256SUMS 缺条目拒绝 / 全 64 位比对 / 同目录临时文件原子替换+读回自证）
  已落地并带反向测试。**安装能力随发布流水线在 0.2.5 落地**（§12.2：安装需要
  已签名的、带 SHA256SUMS 的发布资产；P26 不为此扩依赖树）。
- **`.github/workflows/release.yml`**：tag 守卫（tag == v + 清单版本、可达性
  判定、复用 --manifest-only 守卫）→ 与 CI 同一套门禁 → 三平台构建 + SHA256SUMS
  + 可选 GPG → Release 去重（同 tag 拒绝覆盖）。`VERSION_RE` 接受 `-rc.N`
  （V8-5：首个 rc 之前必须先修好正则）。

## [Unreleased]

### Added

- **The strategy-evolution toolkit (E15 / #97): walk-forward sweep, shadow
  export, A/B verdict.** `scripts/walk-forward-sweep.mjs` splits a frozen event
  archive into equal-event-count folds and replays every candidate × fold
  through the production backtester — candidates are pure config variations
  over the newly CLI-exposed spread_arb entry knobs
  (`--spread-arb-entry-factor`, `--spread-arb-min-obi`,
  `--spread-arb-max-spread-pct`, `--spread-arb-dip-max-pct`,
  `--spread-arb-bounce-min-pct`, `--spread-arb-bounce-window-sec`; defaults
  leave the shipped configuration byte-for-byte, pinned by
  `spread_arb_entry_overrides_flow_through_engine_config`) — then picks on
  fold i and validates on fold i+1 (rolling walk-forward) with the full
  fold×candidate matrix, resume-safe, into `walk-forward.{json,md}`.
  `scripts/shadow-export.mjs` writes a coverage manifest (per-file first/last
  timestamps, event counts, content sha256, per-UTC-day histogram) whose
  verdict line states the 30-day premise honestly — the first run measured the
  frozen corpus at **0.75 days**, so the premise is NOT met and the report says
  so. `scripts/strategy-ab-compare.mjs` verdicts two backtest reports with the
  ≥ `--min-better` (default 2) metrics-better acceptance rule and gateable exit
  codes. Methodology and the honest data-boundary record:
  `docs/STRATEGY_EVOLUTION.md`. The dylib receives the new spreadArb keys
  through `on_config`/`on_params` unchanged.

- **DryRun portfolio layer (E16 / #98, DryRun-only).** 三策略加权资金分配与
  组合级风控落地：`--strategy-limit` 新增第 7 段 `weight`（`策略:权重` 或
  `-` 表示不加权），权重只缩不放——某腿的每笔入场名义 = 自身覆盖或全局
  `size_usd × weight`，钳制在全局上限内；权重 0 = 关闭该腿（份额地板不会
  复活零预算腿，`strategy_size_weight_reweights_but_never_widens`）。
  组合级新增账户开放名义上限 `--max-open-notional-usd`（0 = 关闭）：全部
  持仓成本 + 新入场超过上限即拒绝并计入 `limit.portfolioNotionalCap`
  分桶，读取的是 RiskGate 的**运行时配置**（热更新后仍生效）；
  `portfolio_notional_cap_bounds_total_open_exposure`。逐策略独立账本、
  独立连亏熔断（KI-10/D-18 option A）与日实损上限为既有能力，未动；
  新增 `scripts/dryrun-report.mjs` 从交易总账产出 7 天 DryRun 周报
  （按策略账本 + 逐 UTC 日趋势 + 组合合计 + 窗内熔断/进化事件），并如实
  标注 KI-1 dry 经济性前提。**MarketRegime 状态机**落地为 strategy_logic
  共享参考实现（range/trendUp/trendDown/volatile，tick 口径规则，环形窗 +
  确认滞回，库类型不动 C ABI v2）与 `--regime-eval` 离线评测模式：100 个
  最活跃 token × 300 s 非重叠窗为标注集，离线规则标注 vs 在线状态机窗末
  状态的首轮实测 = **300 窗准确率 97.33%（≥80 验收 PASS**，trend 类
  recall 85.7%、volatile 100%），口径与数据边界记录于
  `docs/MARKET_REGIME.md`。仅 DryRun——实盘属 0.3。

- **Shadow Evolution grows a proposal workflow (E13 / #95).** A winning shadow
  variant no longer silently swaps live parameters: the evaluator now produces
  an **EvolutionProposal** — full baseline-vs-variant 对比 (trades, win rate,
  payoff, profit factor, net PnL), the knob moves, reason, confidence, sample
  count and a 7-day TTL — and the operator decides. Human mode (default) holds
  one pending proposal per strategy in a durable JSONL store and exposes
  accept / reject / defer from all three frontends (a new 进化 page in the
  webui, a fifth Evolution tab in the TUI, and gateway verbs `proposals` /
  `decide <id> accept|reject|defer` / `auto-evolve on|off` /
  `rollback <strategy>`); accepting re-runs the **entire guard chain** (domain
  → gradient → immutable) at decision time before the hot swap. Auto mode
  (`auto_evolve`) applies directly — unattended operation — and every adoption
  is logged to `data/evolution/promotions.jsonl`, which is also the
  cross-restart rollback anchor (one level; a rollback clears the restore
  target and skips only the gradient lock). A 72-hour deep-evolution clock
  (`evolution_cycle_minutes`, compound mutants moving `deep_dims` knobs ±3%
  with a rotating sweep window) re-anchors the variant sets on schedule. The
  switch and clock persist in `data/evolution/state.json`, so 勾选自动进化
  survives a restart, and the persisted runtime state wins over the file
  config.

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

### Removed

- **The kernel's five shipped strategies, and everything that only existed to
  exercise them.** `spread_arb`, `trend_follow`, `mean_reversion`, `pair_arb`
  and `dog` are gone as crates: `user_layer/strategies/{Cargo.toml,Cargo.lock}`
  (the nested workspace) and the five member directories with it. The kernel
  now registers **zero** strategies; `strategy.list` is empty on a fresh boot
  and every strategy arrives through the C ABI v2 `dlopen` path. The reason is
  in the measurements, not in taste: all five measured negative and they had
  become entangled past the point of maintenance — the arm matrix put every one
  of the 61 arms below zero under any honest cost model, `pair_arb`'s win rate
  was reachable at 85% while its profit factor capped near 1 because maker
  fills are adversely selected, and the exit-side work that looked like the
  culprit turned out to be entry-side negative expectancy (the P0 defect in
  that path, #267, was fixed and shipped in #274 and the strategies still lost).
  What did **not** change: the algorithms. `user_layer/strategy_logic/` still
  holds `spread_arb` / `trend_follow` / `mean_reversion`'s pure logic, the
  kernel's `--spread-arb-*` / `--trend-*` knobs and `EngineConfig.spread_arb`
  still exist, and `user_layer/parity_strategy/` remains the reference cdylib
  for writing one. Deleting `strategy_logic` is a separate, larger change.

  **Coverage that went with them (not replaced, and not pretended to be):**

  - Eleven acceptance gates deleted: `strategy-gate-check.mjs`,
    `strategy-limit-check.mjs`, `strategy-evolution-check.mjs`,
    `trend-follow-check.mjs`, `mean-reversion-check.mjs`,
    `mean-reversion-gate-evidence.mjs`, `backtest-check.mjs`,
    `fee-model-sensitivity-check.mjs`, `exit-economics-check.mjs`,
    `settlement-redeem-check.mjs`, `lib/strategy-dylib-freshness.mjs`, plus the
    now-orphaned `lib/strategy-leg-harness.mjs`. Each replayed or drove a
    shipped cdylib.
  - **`backtest-check.mjs` is the sharpest loss**: it pinned "same collection,
    live and replay are bit-identical" (net PnL 5.12208717; 1,025,963 real feed
    events over 13 minutes, 19/19). `--backtest` itself survives and has unit
    tests, but that end-to-end assertion has **no substitute** until the tree
    holds a replayable strategy again.
  - **The fee-model decision guard is gone.** `fee-model-sensitivity-check.mjs`
    was the executable answer to "does switching the default schedule to
    `official` flip net PnL's sign?" (`docs/CAPACITY_AND_EQUITY.md §3`). That
    question now has no gate at all; `FeeSchedule`'s unit tests pin the curve,
    not the economics.
  - `strategy-gate-check.mjs` / `strategy-evolution-check.mjs`'s end-to-end
    coverage (real core + real cdylib) falls back to the kernel's own tests:
    `engine.rs`'s exemption tests and
    `tests/shadow_evolution_per_strategy.rs`. Those are real, but they run on
    `cfg(test)` adapters, not on a loaded library.
  - `strategy-devcheck.mjs`, `ui-plugin-check.mjs` and the PTY panel check kept
    their coverage by pointing at `user_layer/parity_strategy` (the reference
    cdylib) and asserting the registry holds **exactly** that one entry — a
    stronger assertion than the old "the three builtins are listed", at the cost
    of one extra cdylib build in two CI jobs.

  **Upgrade note (operators):** the kernel scans `user_layer/strategies/**` for
  cdylibs by default (`default_strategy_dir()` walks up from the executable),
  so a deployment that ever built the old strategies still has five stale
  `*.dylib` files in `user_layer/strategies/target/release/` — and they will
  still be loaded and listed. `rm -rf user_layer/strategies/target` on the
  deployment is what actually makes it zero strategies. The upgrade path
  deliberately does **not** prune that directory: it is the drop-point where an
  operator's own strategies live, and stale build output is indistinguishable
  from a strategy someone put there on purpose.

  Also removed as fossils: the `spread_arb` provenance label the scripts sent
  with their synthetic orders (now `operator` — it was never validated, it just
  echoed back into `engine.stats.strategies[]` and invented a phantom strategy
  row), and `walk-forward-sweep.mjs`'s `EXPECTED_STRATEGIES = ['spread_arb']`
  constant, which is now a required `--strategy <name>` argument — an
  `--enable-strategy` that resolves to nothing refuses to boot (#265), so the
  sweep would have died at startup rather than silently replaying archives
  through a kernel with no trading logic.

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

# 分支收敛清单（V8-1）— 2026-09-25 生成

基线：发布线 `main` = `release/0.2` = `ba2b04d0`（v0.2.1）。
判定：`git merge-base --is-ancestor`（真合入）/ 未合入（需逐条拍板，**未做任何删除**）。

## 已删（本地，`git branch -d`，git 验证其内容已全部包含在发布线）

- `feat/archive-core-default`
- `feat/e9a-strategy-scaffold`
- `feat/e9f-tui-onboarding`
- `fix/converge-main-fbatch`
- `fix/drift-seeded-identity`
- `fix/panel-reject-title`
- `rust-core-p0`
- `trace/before-d5c7fec3`
- `trace/head-8927`
- `verify-deploy`

`develop` 是分支模型里的长期集成分支（README §6），**未删**：它已全部包含在发布线，
就地 ff 到 `ba2b04d0`。

## 远端（`ceer/*`，57 条）——只分类，未删除

真合入发布线的 5 条（删除与否留给操作者逐条确认；删除远端分支会影响其他克隆）：

`git branch -r --merged main` 输出为准（除 main/HEAD 外）。

未合入的 52 条：同样以 `git branch -r --no-merged main` 输出为准。注意仓库大量使用
squash 合并，squash 过的分支在 git 眼里永远「未合入」——判定要结合下面按日期与
主题的人工核对，不能只看 `--no-merged`。

## V8-3 分支与 tag 保护（GitHub 仓库设置，手工逐项，本会话 gh 未登录无法代配）

| 对象 | 规则 |
|---|---|
| `main` | 禁止直推（仅 release/*、rc/* 经 PR 合入）；要求 PR + 全绿；禁止 force push |
| `release/0.2` | 禁止直推（仅 fix/*、chore/* 经 PR 合入）；禁止 force push |
| tags `v*` | 禁止删除；禁止 force push（一个已发布的版本号不可回收） |

（详细理由见 docs/VERSIONING.md §8.4。）

## 未合入（105 条本地分支）— 操作者逐条拍板

| 分支 | 尾端提交 | 日期 | 最后主题 |
|---|---|---|---|
| `chore/ci-ops-gates` | c048d6d5 | 2026-09-21 | ci(ops): gate the macOS-only ops scripts on a macOS runner (#211) |
| `chore/crazydog-fossil-288` | 6e7c160e | 2026-09-23 | refactor(core): 化石注释不再点名一个已被删除的策略 artifact（#288） |
| `chore/evolution-defaults-and-strategy-docs` | 3198da34 | 2026-09-23 | chore(evolution): 出厂即关自动进化 + 冲突进面板，pair_arb 进入条件落文字，mean_reversion 趋势门补 holdout（#269, #270, #271） |
| `chore/post-purge-entropy-sweep` | 46a89cc7 | 2026-09-18 | docs: every reference to the Node layer now says it was removed |
| `chore/ui-kit-governance` | f79a68b9 | 2026-09-23 | chore(ui): UI kit 治理缺口 —— 告警原语归一、同事实同语气、单一列宽与内容自适应浮层 (#260) |
| `dependabot/github_actions/actions/checkout-7` | e7933179 | 2026-09-16 | chore(deps): bump actions/checkout from 6 to 7 |
| `dependabot/github_actions/actions/setup-node-7` | bb7491fe | 2026-09-17 | chore(deps): bump actions/setup-node from 6 to 7 |
| `dependabot/npm_and_yarn/minor-updates-b638239526` | 842994d0 | 2026-09-16 | chore(deps): bump the minor-updates group across 1 directory with 38 updates |
| `docs-panel-51888` | d3c40611 | 2026-09-16 | docs: panel/51888 sweep — API/deployment/quickstart/i18n rebranded off webchat |
| `docs/d18-global-loss-breaker-coupling` | 28a39b37 | 2026-09-15 | docs: record D-18 core-wide consecutive-loss breaker coupling (E4-a / #30) |
| `docs/e1-ruling-source-comments` | 94e85b11 | 2026-09-15 | docs(decisions): land D-13 rulings — keep salt/ledger memo as historical contracts with in-code annotations, remove origin remote (E1-a #21 / E1-e #25) |
| `docs/e3-f1-calibration` | fec694da | 2026-09-15 | docs(decisions): F1 confirm-window calibration on holdout — 60s baseline stands (E3 / #29) |
| `e11-ux-polish` | 773909da | 2026-09-20 | feat(webui): E11 UX 精细化——三步首次引导、空状态下一步指引、可执行错误反馈、四门禁 |
| `e8-e9-sounds-ui` | d3d53187 | 2026-09-16 | feat(e8/e9): alert sounds port + tech UI polish + E9 scale gates |
| `e9-b-strategy-lifecycle` | 1bc988b5 | 2026-09-15 | feat(e9-b): strategy.unload / strategy.reload RPC + guarded engine unregister + full-chain devcheck legs |
| `e9-c-rejection-causes` | 7178fc71 | 2026-09-15 | feat(e9-c): engine.stats per-strategy rejection causes (rejectionCauses) |
| `e9-webui-vue-panel` | b114cce6 | 2026-09-16 | feat(webui): Vue panel app (E9-g) served at /panel/webapp/webui |
| `feat/e12-shutdown-cleanliness` | a0bbee4d | 2026-09-17 | ci: E12 门禁前补 shell 构建（panel-check 未产出 dist/） |
| `feat/e13-evolution-proposals` | b68c7c74 | 2026-09-20 | feat(shadow): E13 进化提案工作流——人工拍板/自动进化/72h 深度轮/一键回滚（#95） |
| `feat/e15-e16-evolution-toolkit` | d6c270f3 | 2026-09-20 | polymarket: sweep log discipline — print on change, always loud on failure |
| `feat/e2a-strategy-sizing` | 2020b98a | 2026-09-15 | feat(core): per-strategy sizing and quota reporting (E2-a, #26) |
| `feat/e2b-gate-optout` | 9ce52c8a | 2026-09-15 | feat(core): per-strategy entry-gate exemptions (E2-b / #27) |
| `feat/e2c-strategy-evolution` | 38c34b92 | 2026-09-15 | feat(core): per-strategy Shadow Evolution — knobs, twins, audit, apply/rollback (E2-c / #28) |
| `feat/e4a-trend-follow` | e278e84d | 2026-09-15 | feat(core): trend_follow chase leg + startup strategy selection (E4-a / #30) |
| `feat/e4b-mean-reversion` | ec93eb9e | 2026-09-15 | feat(core): E4-b mean-reversion fade leg as third builtin (momentum exemption, default off) (#31) |
| `feat/e5a-plugin-manager` | f0abcba5 | 2026-09-15 | feat(ui): plugin-manager command surface + TUI tab (E5-a / #32) |
| `feat/e5b-eventbus` | 51726ce1 | 2026-09-15 | feat(ui): event bus push channel — dedicated core.event reader, coalesced snapshot refresh, log-pane fix (E5-b / #33) |
| `feat/e5c-remove-tui` | 8f39a860 | 2026-09-15 | refactor(ui): remove handwritten ANSI TUI — single ratatui terminal surface (E5-c / #34) |
| `feat/e6a-tauri-scaffold` | e4bdd7f3 | 2026-09-15 | docs(ui): desktop webapp packaging & launch paths (E6 / #20) |
| `feat/e7-abi-v2` | 3d66a863 | 2026-09-15 | test(core): exercise ABI negotiation on Linux too (copy libc.so.6 to .so) |
| `feat/env-panel-credentials-liquid-glass` | 9cd20fa8 | 2026-09-16 | feat(panel): credentials from env only; liquid-glass header + brand assets |
| `feat/history-full-waterfall-filters` | 62ea4384 | 2026-09-16 | feat(panel): uncapped trade history — cumulative stats, timestamps, waterfall, filters |
| `feat/ledger-fee-accounting` | 5e6e185b | 2026-09-16 | feat(core): charge entry/exit fees in the cash ledger — one accounting basis |
| `feat/mean-reversion-entry-factor` | 2aef6dbf | 2026-09-20 | feat(strategy): mean_reversion 入场折价 0.80 与反落刀转头门（C ABI v2） |
| `feat/net-check` | 893cd6f7 | 2026-09-22 | fix(net): probe the endpoint the client dials, not the base URL |
| `feat/pair-arb-complete-set` | d6cef476 | 2026-09-23 | fix(strategy-dev): the scaffold template must construct Entry.shares |
| `feat/panel-hft-replica` | cd036bf1 | 2026-09-16 | feat(panel): HFT page replicating hft.html |
| `feat/panel-prediction-template` | 313db7fc | 2026-09-16 | feat(panel): redefine HFT page as the binary-prediction template; market plugin identity |
| `feat/poly-rest-feed` | 4b2a5c19 | 2026-09-16 | feat(polymarket): market data over REST polling instead of the WS channel |
| `feat/poly-top-coalescer` | 7b1a268b | 2026-09-16 | fix(feed): stop leaking a Polymarket socket on every round rollover |
| `feat/poly-wire-measurement` | d3f7c521 | 2026-09-16 | feat(scripts): measure the Polymarket market-channel wire rate |
| `feat/risk-limit-hot-reload` | 26184889 | 2026-09-23 | feat(risk): 受限配置热加载 —— 只放开仓限额，其余明确拒绝 + 审计 (#191) |
| `feat/strategy-auto-load` | 7988b7ad | 2026-09-18 | feat(strategy): auto-load user-layer strategy libraries from the strategy dir |
| `feat/tui-launcher` | a3eeb516 | 2026-09-15 | feat(ui): one-command TUI demo — scripts/tui-demo.sh + npm run tui, headless-safe panel exit |
| `fix/225-one-sided-book-stop` | a010170b | 2026-09-22 | fix(core): 单边盘口下的保护性停损不再无声失效 (#225) |
| `fix/243-proposal-store-load-write` | 6fb21672 | 2026-09-22 | fix(core): 提案库 load 不再写盘 —— 重启不再让 proposals.jsonl 翻倍 (#243) |
| `fix/244-upgrade-pipeline` | 5f1d07bc | 2026-09-22 | fix(ops): 升级流水线四处缺陷 —— 假闸门、停机后换文件失败、固定 sleep、不构建面板产物 (#244) |
| `fix/accounting-truth` | fbd6191c | 2026-09-21 | fix(core): exact fill bookings, terminal absorption and a periodic in-kernel accounting audit |
| `fix/audit-f2-f10-f4` | 382efba8 | 2026-09-21 | fix(test): audit fixtures isolate summary.json per-directory |
| `fix/audit-f6-f8` | 4831d90b | 2026-09-21 | style: cargo fmt（CI rust-check 格式门） |
| `fix/audit-f7-f9` | 75299e1a | 2026-09-21 | fix(test): lock drive_cycles books to the twin's resting price (F7 fillability) |
| `fix/backup-launchagent` | 989c2a4c | 2026-09-22 | fix(ops): --status 缺少 mtime 解释器时拒绝作答，而不是把陈旧备份报成新鲜（#217） |
| `fix/balance-gap-sign-and-trade-dedupe` | e59d25dd | 2026-09-16 | fix(panel): follow the sign of the cash gap, and stop deleting real trades |
| `fix/balance-mode-semantics` | b352d31f | 2026-09-16 | fix(panel): balance card labels follow the run mode |
| `fix/capacity-real-book` | 13abcc7e | 2026-09-21 | fix(gates): 容量门禁接入真实盘口快照与卖出侧冲击（#192） |
| `fix/ci-gates-trace` | 5eae5979 | 2026-09-21 | fix(ci): show every ref a provenance line resolves to, not the first stale one (#172) |
| `fix/cli-and-datalock` | 4e1dd305 | 2026-09-22 | fix(core): 数据目录认领改为内核文件锁，原子且随进程死亡自动释放 (#233) |
| `fix/daily-loss-and-fee` | 2ea405a5 | 2026-09-22 | docs(fees): the in-process fee self-check compares parameters, not a sampled price (#234) |
| `fix/daily-loss-baseline` | e7f226e2 | 2026-09-22 | fix(risk): 日亏基准带权益口径，同一 UTC 日内 dry→live 不再放大熔断上限 (#235) |
| `fix/data-lock` | 313fdef7 | 2026-09-21 | Merge remote-tracking branch 'ceer/feat/trading-safety-selfcheck' into fix/data-lock |
| `fix/dry-maker-partial-fill` | 3aab9584 | 2026-09-21 | fix(core): dry maker 按对手盘深度成交，可复现部分成交（#183） |
| `fix/esc-affordable-cross` | 61b60a96 | 2026-09-22 | fix(core): escalation legs must pass the risk gate before the maker dies (#261) |
| `fix/evo-auto-adopt` | 9a640e73 | 2026-09-22 | feat(evolution): auto mode really adopts — engine master switch + persisted runtime switches (#249) |
| `fix/evolution-ui-accept-gate` | 5eb3fd79 | 2026-09-22 | fix(panel): 进化卡片不再自称「当前生效参数」——改为采纳记录 |
| `fix/exit-flags-and-strategy-load` | b94c608c | 2026-09-23 | fix(core): 出场梯子 CLI/TOML 化（#264）＋ 策略加载自检升级为硬错误（#265） |
| `fix/exit-reachability` | d1302180 | 2026-09-23 | fix(core): 无买盘时利润侧仍能判定，断流可辨识且会升级告警 (#267, #268) |
| `fix/fee-abstraction-closeout` | 1c522432 | 2026-09-22 | fix(fees): 费率抽象收口 — 单一曲线拼写、越界钳制、有牙自检 (#234) |
| `fix/fee-model-reestimate` | 758941da | 2026-09-21 | merge(fees): 费率抽象归一 —— FeeSchedule 成为唯一 owner，FeeModel 删除 |
| `fix/gate-integrity` | cd5dccfd | 2026-09-21 | Merge remote-tracking branch 'ceer/feat/trading-safety-selfcheck' into fix/gate-integrity |
| `fix/gates-and-drift` | b0536a8f | 2026-09-21 | fix(tools): account:drift-check no longer false-positives on a seeded core (#200) |
| `fix/kernel-cleanup` | 14e2c038 | 2026-09-21 | Merge remote-tracking branch 'ceer/feat/trading-safety-selfcheck' into fix/kernel-cleanup |
| `fix/ki-24-data-backup` | b0ed6348 | 2026-09-19 | fix(ops): add verified external backup for data/, close KI-24's engineering gap |
| `fix/ki-30-dead-health-guards` | 27c9b7d8 | 2026-09-19 | fix(ops): KI-30 的门禁必须会退出（KI-31 / #122） |
| `fix/meanrev-trend-gate` | fffcee5a | 2026-09-21 | fix(strategy): gate the mean-reversion fade on a one-sided slide (#176) |
| `fix/notifier-partial-line-loss` | b844e823 | 2026-09-16 | fix(ui): stop the notifier dropping half a record at a read timeout |
| `fix/observability` | def05dcb | 2026-09-21 | merge: 把 #223 后的部署线合入 fix/observability |
| `fix/panel-safety-banners` | 334143e9 | 2026-09-22 | fix(panel): 交易安全字段在快照 seam 处不再静默丢弃，横幅显示真实错误与冻结状态 (#236, #232) |
| `fix/position-sizing` | 8533bf60 | 2026-09-21 | fix(engine): size entries from equity and bound any order's notional (#202) |
| `fix/pty-gate-ci-260` | 106584ff | 2026-09-23 | fix(gates): PTY 门禁接进 CI —— 脚本假设「出厂即启用」是它一直红的原因（#260 第 6 条） |
| `fix/rejection-reason-ledger` | 3a8c87c4 | 2026-09-22 | fix(evolution): 拒因落盘 + 计数器与台账一致 + 无人工采纳入账 (#251) |
| `fix/risk-gates` | 9bc717df | 2026-09-21 | test(core): the kill-switch close exemption survives maker-to-taker escalation |
| `fix/sec-ipc-panel` | de7046c5 | 2026-09-21 | test(ipc): pin that a live socket is not stolen by a second core (#187) |
| `fix/settlement-redeem` | c393989c | 2026-09-21 | Merge remote-tracking branch 'ceer/feat/trading-safety-selfcheck' into fix/settlement-redeem |
| `fix/soak-gate-robustness` | 5b3fb97d | 2026-09-21 | fix(scripts): make the resident soak gate deterministic and diagnosable (#212) |
| `fix/stack-watchdog` | 1bdea8b4 | 2026-09-21 | fix(watchdog): 重复内核升为与状态无关的一等信号 + 自测补上无 pidfile 的发现分支 |
| `fix/stderr-capture-boundary` | fdca7d27 | 2026-09-21 | fix(gates): stderr 捕获不再在 chunk 间注入空格；轮询断言完整事实而非前缀 |
| `fix/ui-kit-notifier-flake` | e0f16a4c | 2026-09-21 | test(ui): notifier 夹具不再猜测读连接，消除第二类假红（#201） |
| `fix/unknown-args` | 1ba16142 | 2026-09-22 | fix(cli): 未知参数拒绝启动，--help 列出全部合法参数 (#228) |
| `fix/upgrade-full-propagation` | 02e8f9f1 | 2026-09-22 | ci(ops): gate the upgrade propagation path (#252) |
| `fix/variant-birth-clock` | afdf88b3 | 2026-09-22 | fix(evolution): variant birth stamp must be the caller's clock (#250) |
| `integration/2026-09-21-wave2` | 3623537d | 2026-09-21 | merge: gate-integrity (#207, #187 test, #175 gate) |
| `local-line-20260923` | ba1a58e4 | 2026-09-23 | chore(perf): dhat 堆采样刷新到 trading-safety 代码之后 |
| `login-input-width` | 6ff7fe48 | 2026-09-16 | fix(webui): login inputs fill the card width |
| `ops/backup-visibility-and-credential-hygiene` | 718e7a62 | 2026-09-23 | fix(ops): 备份失败不再静默 + TCC 拒绝留下可见证据 (#217)；凭据卫生复核 (#193) |
| `panel-auth-session-hardening` | 795b31ae | 2026-09-16 | fix(scripts): stop scale:feed --full failing on dry-fund starvation |
| `panel-e8e9-design-overhaul` | c9f220d4 | 2026-09-16 | style(ui): rustfmt the notifier split-line test |
| `panel-userpass-auth` | 41bb8782 | 2026-09-16 | feat(panel): user/password auth + login page; serve Vue app at /panel; richer overview |
| `simplify/delete-shipped-strategies` | 36c5dee2 | 2026-09-23 | fix(gates): 参考 cdylib 的扩展名按平台取，网关参数解析支持带空格的路径 |
| `simplify/extension-bus-290` | cd4e9357 | 2026-09-23 | refactor(extension): 删掉从未接线的扩展事件接收侧，文档改成实话（#290） |
| `test/exit-economics-gate` | 091778d9 | 2026-09-23 | docs(gate): BEFORE 不是干净基线 —— d5c7fec3 早于 #171 的诚实记账（#272/#267） |
| `tmp-union` | 316c5f10 | 2026-09-21 | merge-tmp |
| `tmp-union2` | 9c2dd6c2 | 2026-09-21 | tmp-u2 |
| `tmp/walk-fix` | 03236e81 | 2026-09-21 | fix(gates): feed resting depth to the FOK scenarios; assert the honest economy |
| `ui-gateway-auth-plugins` | b55418f2 | 2026-09-15 | chore: de-CloddsBot purge — /panel cutover, port 51888, canonical-only paths |
| `heads/wip/local-line-20260923` | ba1a58e4 | 2026-09-23 | chore(perf): dhat 堆采样刷新到 trading-safety 代码之后 |

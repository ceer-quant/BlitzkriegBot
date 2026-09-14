# 待决策清单（DECISIONS_PENDING）

> 本文件汇集**需要用户拍板**的分歧。全托管执行期间，AI 遇到「无法从代码/需求/合理默认值判定」的事项：
> **不改动、不猜测**，只在此记录现象与选项。
> 格式：背景 / 选项 / AI 倾向 / 理由 / 需要用户确认的点。

---

## [待决策] D-1 配置文件是「死配置」：内核不读取任何 TOML

- **背景**：`user_layer/configs/shadow_evolution.toml`、`user_layer/configs/default.toml`、
  `extensions/*/config.toml` 均**无任何代码读取**（全仓 grep 对文件名与 `toml::from_str` 无命中）。
  内核配置来源只有 CLI 参数与 `ShadowEvolutionConfig::default()` 等代码默认值。
  任务一 Step 1 要求修改 `shadow_evolution.toml` 来启用影子进化——**改了不会生效**。
- **选项 A**：保留现状，把 TOML 当作「文档/人类可读的默认值说明」，并在文件头标注「仅供阅读，不生效」。
- **选项 B**：让内核真正解析这些 TOML（新增配置加载层），使文件成为权威配置源。
- **选项 C**：删除这些 TOML，避免误导。
- **AI 倾向**：**B**（中期）——机构化需要「声明式配置 + 可审计的参数来源」；
  短期先做 **A** 的标注以免后人误改。
- **理由**：把配置散落在 CLI/env 与代码常量中，不利于运维、审计与回滚；但立刻解析 TOML 需要引入
  依赖（本机离线仅有 `toml_edit` 0.25.15 可用）并定义加载优先级（CLI > TOML > 默认）。
- **需要用户确认的点**：是否希望内核引入配置文件作为权威配置源？若是，优先级顺序（CLI/环境变量/TOML）如何定？

---

## [已结案] D-2 影子变体的出场止损与 live 不一致（保真度缺陷）→ 采纳选项 A

- **背景**：影子变体模拟出场时用 `ImmutableConfig.hard_stop_loss_pct`（默认 **50%**，
  `variants.rs`），而 live 用 `ExitConfig.stop_loss_pct`（**12%**，`exit_policy.rs`）。
- **结论（2026-09-14）**：**已修复，采纳选项 A**。`ShadowEvolutionConfig` 新增 `exit_cfg: ExitConfig`，
  由 `Core::new` 从 `config.positions.exit` 注入；`Variant` 一律复用该 live `ExitConfig`，
  不再使用 50% 硬止损。新增单测 `variant_uses_the_live_exit_config_not_a_fabricated_stop` 锁定该行为。
- **效果**：变体的反事实与 live 唯一的差异只剩可变入场旋钮，评估无系统性偏置。
  详见 `SHADOW_EVOLUTION_REPORT.md` §0/§4。

---

## [已结案] D-3 影子进化的触发条件在生产回合下几乎不可达 → 采纳选项 A + B 组合

- **背景**：`ShadowEvolution::on_round()` **每回合重建变体**，样本清零；
  而触发需 `min_sample_count=30` 且 `min_observation_secs=300`。
  生产回合 900s，一回合内很难凑满 30 笔可判定虚拟成交 → 影子进化**长期不会触发**。
  此外变体对四个参数**等比例缩放**，使入场决策相互抵消（本实验变体与基准胜率相同）。
- **结论（2026-09-14）**：**已修复，采纳 A + B 组合**：
  - **A（样本跨回合累积）**：`on_round()` 不再重建变体，只更新到期时间并保留已平仓历史
    （`Variant::retain_tokens` 丢弃已过期 token 的未平仓虚拟仓，保留 closed 历史）。
  - **B（定向变异）**：`build_variants` 改为**每个变体只动 1 个旋钮**（四选一，奇偶交替保守/激进方向），
    差异可归因到单一参数。
  - 附带消除第三条建模缺陷：影子改为在引擎**消费该 tick 之后**喂入**同刻盘口与同刻确认**（消除一档滞后）。
- **效果**：整机 A/B 中进化**真实触发 1 次并带来增益**（B 组净盈亏 +12.66、胜率 +6.6pt、回撤不变），
  参数单调收敛一步（cap 0.45→0.4365）。详见 `SHADOW_EVOLUTION_REPORT.md` §3。

---

## [待决策] D-4 「删除 CloddsBot」的实际范围与时机（关键）

- **背景**：`CloddsBot/` 目录**已不存在**（更早提交 `fc6e93c` 已扁平化）。
  今天存在的 Node 侧是 **`src/`（584 个 .ts 文件）+ `dist/`**，且**正在运行**（`node dist/index.js`，:18789）。
  任务书要求「删除 CloddsBot 目录」，实际等价于删除整个 Node 外壳——远超「删一个遗留目录」。
- **已完成的替代**：`ui_kit/`（core/web/tui/app）已实现并三前端跑通，可替代**展示层**
  （`ui/hft.html`、`src/tui`）。
- **命令下发已补齐（2026-09-14，提交 `1b1c98a`）**：`ui_kit/src/gateway/`（新）实现了命令通道——
  `Supervisor`（spawn/stop/**adopt** 内核进程，含"绝不重复起核"守卫）+ `Dispatcher`
  （`start|stop|status|positions`，语义对齐 Node 的 Rust-core 路径），并经 `ui_kit_web --manage`
  暴露 `/api/command`（GET/POST）。端到端验证脚本 `scripts/ui-kit-gateway-check.mjs` **PASS**
  （status→start→status→positions→start(adopt)→stop→unknown 拒绝→help）。**无任何下单 API**。
- **仍未完成的替代**：webchat 的聊天/网关（Express+WS，服务 `/chat`、命令派发到聊天流）**仍依赖 Node**；
  且 `src/gateway` 在模块顶层静态 import 了整张 Node 交易图，删除需同步改造。UI Kit gateway 目前提供的是
  **HTTP 命令面**，尚未接入 `ui/hft.html` 现有的 `ws://…/chat` 通道。
- **选项 A（AI 已采用）**：**暂不删除**。保留 `src/`，先冻结 §2.1 的 Node 交易域，分阶段迁移后再说。
- **选项 B**：立即删除 `src/` 中的 Node 交易域目录（strategies/execution/feeds/risk/trading/…）——
  **会中断当前生产进程与 webchat 命令通道**。
- **AI 倾向**：**A**。分阶段：① 冻结 Node 交易域只读；② **UI Kit 增补命令下发 + 最小网关（✅ 本次已完成）**；
  ③ HFT 面板切到 UI Kit web（待做）；④ 逐目录删 Node 交易域（每步跑 `cycle-check`）；⑤ 最后评估网关/聊天域。
- **理由**：任务全局约束明令「禁止删除未经备份的文件、禁止破坏运行」；且删除范围与风险远大于任务书预期。
  在命令通道未被 UI Kit 接管前删除，等于让机器人失去人工控制入口。
- **需要用户确认的点**：
  1. 「删除 CloddsBot」是否确指删除整个 `src/` Node 外壳？还是仅指 §2.1 的 8 个交易域目录？
  2. 是否接受「先让 UI Kit 接管命令下发与网关，再删除」的分阶段路径？
  3. 聊天/Agent 域（agents/channels/mcp/…）是否也在删除范围内？（这些与交易无关，删除会移除机器人交互能力）

---

## [待决策] D-5 实盘小额验证的时机与额度

- **背景**：§29/§30/§32 的实盘安全层（订单/持仓崩溃恢复、启动孤儿清算、live executor、余额播种）
  **全部只在 DRY 验证过**；任务全局约束禁止启用 Live，故本次未验证。
- **选项 A**：待用户返回后，用极小额度（如 4.8u / 每笔 4 股，`HFT_MAX_SHARES=4`）跑一次真实下单，观察
  `startup sweep cancelled N orphan order(s)` 与订单/持仓落盘。
- **选项 B**：继续 DRY 累积更多 soak 数据，暂不碰实盘。
- **AI 倾向**：**A（用户在场时尽快做）**。live 链路是唯一无法离线证伪的环节，越早小额打通越好。
- **理由**：所有抽象与回测的地基都建立在「live 真的能下单且能恢复」之上；不验证就扩张等于沙地建楼。
- **需要用户确认的点**：验证额度、时间窗、以及是否允许把 `DRY_RUN=false`（**本 AI 不会自行开启**）。

---

## [待决策] D-6 既有 Rust 代码未通过 rustfmt / clippy（新 CI 中为建议性检查）

- **背景**：仓库规范化新增 `rust-check` 作业。实测：`cargo build --release` ✅、
  `cargo test` ✅（120 用例）；但 `cargo fmt --all --check` 有 **537 处 diff（约 57 个文件）**，
  `cargo clippy --workspace --all-targets -- -D warnings` 有 **3 个既存错误**
  （`core/market_api/src/types.rs:245` `collapsible_if`；`ui/ui_kit/src/core/event_bus.rs:144` `unused_mut`）。
- **选项 A（AI 已采用）**：CI 中 `rustfmt` / `clippy` 设为 **建议性（continue-on-error）**，
  由独立 PR 专项清理后再转为阻塞。理由：治理 PR 不应顺带重排 57 个无关文件
  （违反「禁止顺手优化业务逻辑」）。
- **选项 B**：在本次治理 PR 内一并 `cargo fmt` + 修 3 个 lint，CI 直接阻塞。
- **AI 倾向**：**A**。清理应可独立审阅、可回滚；A 让治理变更保持最小且可审计。
- **需要用户确认的点**：何时启动格式/静态检查清理专项？（清理完成后本项转阻塞）

---

## [待决策] D-7 仓库身份元数据（npm / README / CONTRIBUTING）未随私有仓库改名

- **背景**：`package.json` 的 `name=clodds`、`author=alsk1992`、`repository.url=github.com/alsk1992/CloddsBot`，
  以及 `README.md`/`CONTRIBUTING.md` 的品牌文案仍指向旧上游；而新私有仓库为 `ceer-quant/BlitzkriegBot`。
- **本次已做的最小改动**：README 顶部与 CONTRIBUTING 顶部加入私有仓库 + AI 协作规范横幅，
  指向 `docs/AI_WORKFLOW.md`；**未**改动 `package.json` 与全文品牌文案。
- **选项 A（AI 已采用）**：暂不改分发元数据。改 `name`/`repository`/`author` 影响 npm 发布与 CI，
  属于外向变更，需用户明确决定。
- **需要用户确认的点**：
  1. 是否重命名 npm 包（`clodds` → 其他）？还是本仓库不再发布 npm、仅内部使用？
  2. 是否把 `repository.url` 指向 `ceer-quant/BlitzkriegBot` 并清理 README 旧上游品牌？

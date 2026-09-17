# 开发规范与门禁（DEVELOPMENT）

> **本文是「改代码怎么改、怎么验」的唯一权威说明**：工具链、目录职责、门禁矩阵、
> 页面/内核的验证方法、测试约定、代码风格、工具链陷阱。
>
> 相关文档：
> - [`GITHUB_GOVERNANCE.md`](GITHUB_GOVERNANCE.md) —— GitHub 侧规范（身份 / 分支 / 提交 / PR / API）。
> - [`AI_WORKFLOW.md`](AI_WORKFLOW.md) —— 人与 AI 的协作契约与**硬约束**（红线在那边）。
> - [`blitzkrieg/ARCHITECTURE.md`](blitzkrieg/ARCHITECTURE.md) / [`RUST_CORE.md`](RUST_CORE.md) —— 架构细节。
>
> 最后核对：2026-09-17（`main` = `4c4b351`）。

---

## 1. 工具链

| 项 | 版本 | 备注 |
| --- | --- | --- |
| Rust | edition 2024（CI 用 stable） | 本机核对：`rustc 1.97.1` |
| Node.js | **CI 固定 22** | 本机核对：`v26.8.2`。**CI 用 22**，本地高版本通过不代表 CI 通过 |
| 包管理 | npm（`package-lock.json` 已提交） | CI 用 `npm ci`，不要手改 lockfile |
| Vue 面板 | Vue 3 + `<script setup>` + TS + Vite | 独立 `package.json`，见 §4 |
| 面板 UI 栈 | shadcn-vue 风格的本地组件 + Reka UI + Tailwind v4 + ECharts + Pinia + VueUse | 组件以源码形式拷进仓库，可直接改 |

**Rust 工作区结构有个必须知道的点**：`user_layer/strategies` 与 `user_layer/parity_strategy`
是**独立的嵌套 workspace**（各自带 `Cargo.lock`），**不是**根 workspace 的成员。
这样设计是为了模拟真实第三方策略作者的项目布局。因此：

```bash
# 必须先单独构建这两个 cdylib，否则 BK_REQUIRE_DYLIB=1 的测试会失败
(cd user_layer/strategies && cargo build --release --locked)
(cd user_layer/parity_strategy && cargo build --release --locked)

# 再构建根 workspace
cargo build --release --workspace --locked
```

`BK_REQUIRE_DYLIB=1` 会把「dylib 驱动的测试」从**跳过**变成**硬失败**。CI 已设该变量。

---

## 2. 目录职责（改哪里之前先看这张表）

| 路径 | 职责 | 谁能改 |
| --- | --- | --- |
| `core/blitzkrieg_core/` | **交易内核**：引擎 / 撮合 / 订单 / 持仓 / 风控 / 账本 / 出场 / 影子进化 / IPC | 谨慎；**账本语义不可擅动** |
| `core/market_api/` | 市场插件契约（DTO + `DataFeed`/`MarketDiscovery`/`OrderExecutor`/`MarketHost`） | 契约变更需评估全部扩展 |
| `extensions/polymarket/` | Polymarket 扩展（venue/live/feed/discovery/gamma） | 市场相关改动在此，**不进内核** |
| `extensions/binance_spot/` | 现货行情扩展（供趋势腿使用） | 同上 |
| `user_layer/strategy_api/` | 用户策略 trait / FFI 稳定表面（C ABI v2） | **vtable 已冻结**，见 §9 |
| `user_layer/strategies/` | 示例外挂策略（独立 workspace → cdylib） | 示例与第三方样板 |
| `ui/ui_kit/` | Rust UI 数据层 + 网关（bins: `ui_kit_web` / `ui_kit_app`） | **零 GUI 依赖**契约（不许引 tauri） |
| `ui/ui_kit_panel/` | TUI（ratatui + crossterm） | 重依赖只进这里 |
| `ui/webapp/webui/` | **Vue 3 面板**（`/panel`，见 §4） | 前端改动主战场 |
| `ui/webapp/src-tauri/` | Tauri 桌面壳 | 打包相关 |
| `src/` | Node 外壳（网关、命令、技能、日志渲染） | **不再参与交易决策** |
| `scripts/` | 门禁与运维脚本 | 新门禁加这里，并接进 CI |
| `docs/` | 文档 | 见 [`docs/README` 索引](#11-文档地图) |

**架构硬约束（改代码前必须内化）**：

1. **内核零市场代码**。`grep -ri polymarket core/blitzkrieg_core/src` 只应剩 feature 注册。
2. **扩展不得依赖内核**（否则 Cargo 成环）。
3. **Node 外壳不做交易决策**：不持私钥、不直连 CLOB、不维护订单状态机、不做资金计算、
   不吞原始错误。交易逻辑一律在 Rust 侧。
4. **凭证只走环境变量**，不进代码、不进提交。

---

## 3. 门禁矩阵（Definition of Done）

一次改动「完成」= 下表**全部通过并附证据**。

### 3.1 必跑（阻塞）

| # | 命令 | 覆盖 | 何时必须 |
| --- | --- | --- | --- |
| 1 | `cargo build --release --workspace --locked` | 根 workspace 编译 | 每次 |
| 2 | `cargo test --workspace --locked` | 全部 Rust 测试（含集成测试） | 每次 |
| 3 | 两个嵌套 workspace 的 `cargo build --release --locked` | 外挂 cdylib 可编译 | 每次（或依赖 CI） |
| 4 | `BK_REQUIRE_DYLIB=1 cargo test --workspace --locked` | dylib 驱动的测试**不许跳过** | 动了策略接口时 |
| 5 | `npm run typecheck` | Node/TS 类型 | 每次 |
| 6 | `npm test` | Node 单元测试（37 个文件，`node --test`） | 每次 |
| 7 | `npm run build` | Node 构建 | 每次 |
| 8 | `bash scripts/secret-scan.sh` | 密钥泄漏（**零依赖**，只报位置不报值） | 每次 |
| 9 | `node scripts/cycle-check.mjs` | DryRun 全链路：确认→挂单→成交→持仓→重估→出场 | **涉及下单链** |
| 10 | `npm run ui:webapp` | 面板端到端（构建产物 + 快照非空 + 鉴权双向） | **涉及面板** |

### 3.2 面板专属门禁（`ui/webapp/webui/`，8 套）

```bash
cd ui/webapp/webui && npm run check:all
```

`check:all` = `check` + 8 个回归套件，共 9 条。每个套件都**钉住一个曾经真实发生过的缺陷**：

| 套件 | 钉住什么 |
| --- | --- |
| `check` | `vue-tsc --noEmit` 类型检查 |
| `check:theme` | 主题跟随系统（用可控 `matchMedia` 模拟 OS 外观切换，含 Safari <14 旧 API 分支） |
| `check:balance` | 余额/账本口径（本金＋净利润、费用计支出、无本金则无净值、按启动计数器去重） |
| `check:lifecycle` | 引擎启停控制与门禁 |
| `check:rejections` | 拒单归因 |
| `check:feed` | 行情存活（feed-dead 判定） |
| `check:session` | 会话失效恢复（401 是**状态**不是**错误**；不再拿死 token 硬撞） |
| `check:history` | 历史订单 tab 顺序/分页/id 键 |
| `check:round` | 轮次头部：等宽数字固定槽位、滚动数字基线、不走样、漏改扫描器 |

> `check:round` 里的**模板扫描器**会断言「没有任何页面再把数字插值进 `stat-num` / `num`
> 槽位」，并自测「正例必抓、反例不误报」。**它是防止新页面漏加滚动数字的唯一自动化手段**，
> 新增数字展示时请依赖它而不是靠人眼。

### 3.3 其他专用门禁（按需）

> ⚠️ **本节的 22 个门禁没有任何一个接进 CI** —— 它们**全靠人工在本地记得跑**。
> CI 只覆盖 §3.1 的 1/2/5/6/7/8/10 与 §3.2 的面板套件（外加建议性的 3.4）。
> 「CI 全绿」**不代表** §3.3 被验证过。这是当前回归保护最大的单点缺口，
> 记录为 [`KNOWN_ISSUES.md`](./KNOWN_ISSUES.md) **KI-9**。

| 命令 | 内容 |
| --- | --- |
| `npm run core:parity` | Node↔Rust 订单/账本语义对拍（22 项） |
| `npm run core:parity-engines` | Node↔Rust 决策等价 |
| `npm run core:strategy-limit` | 按策略资金分配 / 配额 |
| `npm run core:strategy-gate` | 按策略门禁豁免 |
| `npm run core:strategy-evolve` | 影子进化按策略化 |
| `npm run core:trend-follow` | 趋势腿 |
| `npm run core:mean-reversion` | 逆向腿 |
| `npm run strategy:devcheck` | **策略开发者全链路**：模板生成 → 构建 → load → enable → 信号 → 旋钮 → 影子孪生 |
| `npm run scale:plugins` | 规模化：50 策略 × 100 插件注册表读取 < 100ms |
| `npm run scale:feed` | 40 连接 10 分钟行情推送丢包率 |
| `npm run ui:plugin-gateway` / `ui:plugin-panel-pty` | UI Kit 网关命令面 / TUI PTY 冒烟 |
| `npm run tui:check` | TUI 启动器 |
| `node scripts/backtest-check.mjs` | 事件驱动回测：归档 → 离线重放 → 逐位一致 |
| `node scripts/order-recovery-check.mjs` / `position-recovery-check.mjs` | 崩溃后订单/持仓恢复（孤儿 & 失管防护） |
| `node scripts/core-adopt-check.mjs` | 重复客户端接管内核，无重启风暴 |
| `node scripts/market-plugin-check.mjs` | 市场插件选择 |
| `node scripts/trade-log-flag-check.mjs` | `--no-trade-log` 隔离是否真的隔离 |
| `node scripts/ui-eventbus-check.mjs` / `ui-kit-gateway-check.mjs` | 事件推送 / 网关命令面 |

**纪律**：涉及对应模块的改动，**必须在 PR 正文里贴出这些门禁的原始输出**。
CI 不会替你跑。

### 3.4 建议性（不阻塞，但**必须知情**）

| 命令 | 现状 | 债务编号 |
| --- | --- | --- |
| `cargo fmt --all -- --check` | **99 个文件**有 diff | KI-13（原 D-6） |
| `cargo clippy --workspace --all-targets -- -D warnings` | **135 处 warning** | KI-13（原 D-6） |
| `npm audit --audit-level=high --omit=dev` | **86 个生产漏洞**（2 critical / 49 high / 33 moderate / 2 low），全在旧 Node 外壳的交易所/消息 SDK 传递依赖 | KI-14（原 D-9） |

**纪律**：建议性**不是**「可以忽略」。新代码不得**新增**债务；PR 应能说明新增为 0。
不要为了让它们变绿而做破坏性升级（交易 SDK major 升级属「顺手优化」，被硬约束禁止）。

---

## 4. 面板开发（`ui/webapp/webui/`）

### 4.1 运行

```bash
# 生产内核 + 网关（注意：网关是 ui_kit_web，不是 Node 外壳）
./target/release/ui_kit_web --socket <sock> --addr 127.0.0.1:51888 --manage

# 前端开发服务器
cd ui/webapp/webui && npm run dev

# 构建（产物 dist/ 由网关在 /panel/webapp/webui 静态服务）
cd ui/webapp/webui && npm run build
```

`/panel` 的路由在 [`ui/ui_kit/src/web/mod.rs`](../ui/ui_kit/src/web/mod.rs)：

| 路由 | 鉴权 |
| --- | --- |
| `GET /api/ping` | 免鉴权（用于区分「网关不可达」与「会话已死」） |
| `POST /api/login` | 免鉴权（换取会话 token） |
| `GET|POST /api/logout` | 会话 |
| `GET /api/snapshot` | 会话（**未鉴权返回 401**） |
| `GET /api/plugins` | 会话 |
| `GET|POST /api/command` | 会话 |
| `GET /` `GET /panel` | 静态（重定向到 `/panel/`） |
| `GET /panel/*` | 静态资源（`dist/`） |

### 4.2 页面

| 页面 | 文件 | 内容 |
| --- | --- | --- |
| 总览 | `src/pages/Overview.vue` | 余额卡、权益曲线、引擎统计、策略与持仓摘要 |
| 行情面板 | `src/pages/HftPage.vue` | 轮次头部、盘口/筹码价格、持仓表、历史订单（分页/筛选） |
| 回放复盘 | `src/pages/BacktestPage.vue` | `--backtest` 报告：KPI、策略表、拒单、极值、风险/错误 |
| 策略 | `src/pages/Strategies.vue` | 按策略分账、配额、旋钮 |
| 插件 | `src/pages/Plugins.vue` | 策略/市场插件/扩展三类启停与状态 |

### 4.3 前端约定

- **共享原语优先**：`StatTile` / `StatRow` / `Badge` / `Card` / `Button` / `EmptyState` /
  `SegmentedControl` / `Tooltip` / `AlertBanner`。改展示先想能不能在原语上改
  （`StatTile`/`StatRow` 的 `roll` props 就是这么做到「一处改、全站生效」的）。
- **数字一律用 `RollingNumber`**：`StatTile` / `StatRow` **默认滚动**；
  唯一刻意排除的是 `DRY` / `LIVE` 这类**词**（`:roll="false"`）。
  `check:round` 的扫描器会抓漏。
- **等宽数字是承重的**：`.roll` / `.num` 依赖 `font-variant-numeric: tabular-nums`，
  且 `letter-spacing` 必须为 0（字距在每个字符后追加，会破坏单字形栈的宽度假设）。
- **CSS 陷阱（已踩过，勿复现）**：`inline-block` 上任何**非 `visible` 的 `overflow`**
  会强制该盒子基线取**下边缘**。滚动数字的裁切因此放在内层绝对定位层
  （`.roll-clip`），格子本身保持 `overflow: visible`，由文档流中的 `.roll-sizer`
  提供基线与定宽。**动 `RollingNumber` 的 CSS 前请先跑 `check:round`。**
- **可访问性**：每个滚动数字在 DOM 中存在两份（`sr-only` 可读文本 + 绘制字形），
  绘制副本必须 `user-select: none`，否则复制一行会得到 `288288`。
- 主题跟随系统（`lib/theme.ts`），有 `check:theme` 钉住。

---

## 5. 内核开发（`core/blitzkrieg_core/`）

### 5.1 不可擅动的边界

| 边界 | 说明 |
| --- | --- |
| **账本语义** | `Ledger` 的记账口径（费用计支出、本金＋净利润、启动种子）是多方校验的基准，改动会连锁影响面板/对账/回测。**未经明确批准不得改。** |
| **风控硬边界** | `RiskGate`、kill switch、单日亏损帽、全局容量、配额、定寸 —— 这些**物理上不可被策略豁免**。 |
| `ImmutableConfig` | 策略不可变地拿不到，也不可绕过。 |
| ABI vtable | `BK_ABI_VERSION = 2`，**vtable 已冻结**。新能力走**可选符号**（见 §9）。 |

### 5.2 改动后至少要跑的

```bash
cargo test -p blitzkrieg-core --lib          # 单元
cargo test --workspace --locked              # 全部（含 3 个集成测试）
node scripts/cycle-check.mjs                 # 下单链
node scripts/core-parity.mjs                 # 语义对拍
node scripts/backtest-check.mjs              # 回测保真度（改了引擎/撮合/出场时必跑）
```

### 5.3 引擎参数与默认值

- 生产 dry 内核的启动参数由启动方（`ui_kit_web --manage` 或 Node 侧）拼装；
  **不要手改**，也不要临时加开关绕过。
- 新策略一律**默认 DISABLED**（E4-a/E4-b 的两个内建腿也是）。
  因此升级内核**不会**改变在运行会话的交易行为——这是刻意的安全属性，勿破坏。
- 影子进化默认 **`enabled: false`**（`shadow_evolution/config.rs`，"Disabled by default (opt-in)"）。
- 行情归档**默认开启**（内核侧默认，`--no-event-archive` 可关）；
  分段轮转 256 MB、无会话上限、可用空间 <5 GB 自动停录、单写者锁。

---

## 6. 测试约定

### 6.1 Rust

- 单元测试就近写在模块内（`#[cfg(test)]`）；集成测试放 `core/blitzkrieg_core/tests/`
  （`dynamic_strategy.rs`、`foreign_parity.rs`、`shadow_evolution_per_strategy.rs`）。
- **测试要钉行为，不是钉实现**。命名写清「钉住什么」，例：
  `variant_uses_the_live_exit_config_not_a_fabricated_stop`。
- 外挂策略的树内/外挂等价性靠 `foreign_parity.rs` + 共享算法 crate `parity_logic`
  **逐信号对拍**。
- 全仓约 244 处 `#[test]`。

### 6.2 Node / TS

- `tests/**/*.test.ts`，用 `node --test` + tsx 加载，37 个文件。
- `npm test` 即全部。

### 6.3 面板回归检查（纯 Node ESM，无测试框架）

风格固定，**照抄现有文件**：

```js
#!/usr/bin/env node
import assert from 'node:assert/strict'

let failures = 0
const check = (label, fn) => { /* try/catch, 计数, 打印 "  ok   <label>" */ }

check('钉住的行为', () => { ... })

console.log(`RESULT: ${failures ? 'FAIL' : 'PASS'}`)
process.exit(failures ? 1 : 0)
```

运行方式：`node --experimental-strip-types scripts/<name>.check.mjs`（不编译 TS）。

**每个检查都必须能说明它钉住的是哪个真实缺陷。** 写不出这句话的检查通常是
「测实现」而非「钉行为」，应当删掉或重写。

**不要为了让检查通过而放宽断言。** 如果检查抓到了真实漏改（历史上真的抓到过 3 处），
改代码，不是改检查。反过来，如果断言依赖了**无保证的偶然细节**（例如属性顺序），
应当放宽到语义层面——但要在注释里说明为什么。

---

## 7. 代码风格

### 7.1 通用

- 只做被要求的改动（**禁止顺手优化**）。发现无关问题 → 记 Issue 或 `DECISIONS_PENDING.md`。
- 注释解释**为什么**（约束、反直觉的取舍、踩过的坑），不复述代码在做什么。
  本仓库的注释密度偏高且都是「为什么」，请沿用。
- 不写「本次改动新增」这类会随 PR 合并立即过期的注释。
- 错误不得吞掉：外壳/前端不得把原始错误替换成笼统文案。

### 7.2 Rust

- edition 2024；`Decimal` 而非 `f64` 做金额；时间的毫秒语义要明确（事件自带时间戳优先）。
- 当前 `fmt`/`clippy` 尚未清零（D-6），但**新代码应尽量干净**，
  改动文件不应显著增加 diff。

### 7.3 TypeScript

- strict 模式；避免 `any`；共享类型或 zod 镜像校验（契约源在 Rust serde 结构体）。
- 前端只做展示与指令下发，**绝不**直接接触凭证/签名/下单原语。

### 7.4 Shell / 脚本

- **禁止字符串插值进 `execSync`**（命令注入）。一律 `execFileSync` + 数组参数：

  ```js
  // BAD
  execSync(`which ${cmd}`)
  // GOOD
  execFileSync('which', [cmd])
  ```

- 校验并净化用户提供的路径与输入。
- 脚本打印**位置**而不是**值**（`secret-scan.sh` 的纪律）。

---

## 8. 工具链陷阱（本机特有，务必知情）

### 8.1 Mimosa 钩子

开发环境装有一个写保护钩子（Mimosa），行为如下：

- **会拒绝用 shell 命令写 Rust 源文件** → 改 Rust 代码请用 **Write / Edit 工具**，不要
  用 `cat >`、`sed -i`、`tee`。
- **会对「只是提到某些源路径」的只读命令误报** → 例如 `grep` 到受保护文件路径时可能被拦。
- **会对提交信息中提到受保护文件名误报** → 因此提交信息**写成文件再 `git commit -F`**，
  不要在 `-m` 里内联。

### 8.2 关于安全扫描结论的口径（重要）

Mimosa 最近一次扫描**未得出完整结论**（`scanner_enobufs`）。
因此：

> **不得声称「项目已通过安全审计」或「项目是安全的」。**
> 需要在 PR / 文档中提及时，如实写明「本次未对项目安全性作任何声称，
> 完整审计待重跑」。

这条约束的完整背景见 [`AI_WORKFLOW.md`](AI_WORKFLOW.md) §2 与 [`SECURITY.md`](../SECURITY.md)。

### 8.3 密钥扫描

```bash
bash scripts/secret-scan.sh            # 已跟踪文件
bash scripts/secret-scan.sh --all      # 含 docs/tests
bash scripts/secret-scan.sh --history  # 全历史（慢）
```

零依赖；**只报位置不报值**。CI 里 `--history` 每周一跑一次。

---

## 9. 策略开发（面向第三方）

```bash
# 一键生成脚手架（零 unsafe）
npm run strategy:new -- my_strategy
cd user_layer/strategies/my_strategy && cargo build --release

# 全链路门禁：模板 → 构建 → strategy.load → enable → 信号 → 旋钮 → 影子孪生
npm run strategy:devcheck
```

要点：

- 写 `SafeStrategy` trait，**不用手写 unsafe FFI**（库内部生成 vtable + 胶水）。
- **新策略默认 DISABLED**，必须显式 `strategy.enable` 才会交易。
- **热加载/卸载语义**：`strategy.reload` 是「原子交换」，同 name 旧实例在确认无持仓后
  drop，带审计；`strategy.unload` 同理。
- **ABI 冻结规则**：`BK_ABI_VERSION = 2`，**vtable 不得改**（改了已发布的 v2 库就得重编译）。
  新能力一律加**可选符号**，已有两例：
  `bk_strategy_gate_exemptions`（门禁豁免，E2-b）、
  `bk_strategy_evolvable_knobs`（可进化旋钮，E2-c）。**不导出 = 明确「不支持」**。
- 策略可**自声明**不要 `timing`/`momentum` 两个入场质量闸门，但**安全边界物理上不可豁免**。
  这是「策略自声明」，**不是**运维配置开关；运维侧若不接受，不 `strategy.enable` 即可。
- 详细指南：[`blitzkrieg/STRATEGY_GUIDE.md`](blitzkrieg/STRATEGY_GUIDE.md)；
  ABI 设计：[`blitzkrieg/ABI_V2_DESIGN.md`](blitzkrieg/ABI_V2_DESIGN.md)。

---

## 10. 市场扩展开发

- 新交易所 = 写一个扩展 crate 并用 **Cargo feature** 注册，**内核不改一行**。
- 契约在 `core/market_api`：`DataFeed` / `MarketDiscovery` / `OrderExecutor` /
  `MarketPlugin` / `MarketHost`。
- 扩展**不得依赖内核**（会成环）。
- 指南：[`blitzkrieg/EXTENSION_GUIDE.md`](blitzkrieg/EXTENSION_GUIDE.md)。

---

## 11. 文档地图

写文档前先看这张表，**不要新建重复文档**。

### 11.1 入口与规范层

| 文档 | 内容 |
| --- | --- |
| [`../README.md`](../README.md) | 项目是什么、快速上手、门禁速查 |
| [`../HANDOFF.md`](../HANDOFF.md) | **交接入口**：新人阅读顺序、当前状态、运行配置、未完成项 |
| [`AI_WORKFLOW.md`](AI_WORKFLOW.md) | 角色、**硬约束**、分支模型、DoD、署名、证据要求 |
| [`GITHUB_GOVERNANCE.md`](GITHUB_GOVERNANCE.md) | GitHub 身份/远端/凭证/提交/PR/标签/API |
| **`DEVELOPMENT.md`**（本文） | 开发规范、门禁矩阵、测试与风格、工具链陷阱 |
| [`../CONTRIBUTING.md`](../CONTRIBUTING.md) | 对外贡献者入口（简化版） |

### 11.2 状态与盘点层

| 文档 | 内容 |
| --- | --- |
| [`FEATURES.md`](FEATURES.md) | **功能清单与完成度**（做完了什么、做到什么程度、证据在哪） |
| [`KNOWN_ISSUES.md`](KNOWN_ISSUES.md) | **已知未修缺陷与技术债**（含未验证路径） |
| [`DECISIONS_PENDING.md`](DECISIONS_PENDING.md) | **待裁决 / 已裁决**的取舍记录（D-1 … D-19） |
| [`ROADMAP_V0_1.md`](ROADMAP_V0_1.md) | 0.1 路线图（E1–E7，**唯一计划源**） |
| [`ROADMAP_INSTITUTIONAL.md`](ROADMAP_INSTITUTIONAL.md) | 机构级长期路线（P-1…P-6） |

### 11.3 架构与指南层

| 文档 | 内容 |
| --- | --- |
| [`blitzkrieg/ARCHITECTURE.md`](blitzkrieg/ARCHITECTURE.md) | 分层架构与扩展体系 |
| [`blitzkrieg/INTERFACES.md`](blitzkrieg/INTERFACES.md) | 接口契约 |
| [`blitzkrieg/ABI_V2_DESIGN.md`](blitzkrieg/ABI_V2_DESIGN.md) | C ABI v2 设计 |
| [`blitzkrieg/STRATEGY_GUIDE.md`](blitzkrieg/STRATEGY_GUIDE.md) | 写与加载策略 |
| [`blitzkrieg/EXTENSION_GUIDE.md`](blitzkrieg/EXTENSION_GUIDE.md) | 新增市场扩展 |
| [`blitzkrieg/SHADOW_EVOLUTION.md`](blitzkrieg/SHADOW_EVOLUTION.md) | 影子进化 |
| [`RUST_CORE.md`](RUST_CORE.md) | Rust 内核架构、进程/IPC 契约 |
| [`TRADING.md`](TRADING.md) / [`RISK_MANAGEMENT.md`](RISK_MANAGEMENT.md) | 交易执行与风控 |
| [`DEPLOYMENT.md`](DEPLOYMENT.md) / [`VPS_SECURITY.md`](VPS_SECURITY.md) | 部署与主机安全 |
| [`API.md`](API.md) / [`API_REFERENCE.md`](API_REFERENCE.md) | 网关 API |

### 11.4 历史层（**只读，不改**）

| 文档 | 说明 |
| --- | --- |
| [`blitzkrieg/MIGRATION_LOG.md`](blitzkrieg/MIGRATION_LOG.md) | **权威变更史**（§1–§48），迁移与每个里程碑的落地记录 |
| [`reports/`](reports/) | 交付报告（治理 / UI Kit / 出参优化 / 留出段回放 / 影子进化 A/B） |
| [`../CHANGELOG.md`](../CHANGELOG.md) | 品牌迁移前的旧变更记录 |

> 历史层文件**保留原文**（含旧品牌名），顶部已标注「历史记录，命名已废弃」。
> 删除等于篡改审计轨迹——**不要清理它们**。

---

## 12. 快速上手（新人 30 分钟）

```bash
# 1. 构建
(cd user_layer/strategies     && cargo build --release --locked)
(cd user_layer/parity_strategy && cargo build --release --locked)
cargo build --release --workspace --locked

# 2. 类型 + 测试
npm ci
npm run typecheck && npm test

# 3. 离线验下单链（自起临时内核，隔离 socket，不碰生产）
node scripts/cycle-check.mjs

# 4. 面板
cd ui/webapp/webui && npm ci && npm run check:all && cd ../../..
npm run ui:webapp

# 5. 通读（顺序很重要）
#    README.md → HANDOFF.md → docs/AI_WORKFLOW.md → docs/GITHUB_GOVERNANCE.md
#    → docs/FEATURES.md → docs/KNOWN_ISSUES.md → docs/blitzkrieg/ARCHITECTURE.md
#    → git log --oneline -30
```

> ⚠️ **不要乱动正在运行的实例。** 本机有一个**真实运行的 dry 面板与内核**
> （面板 `ui_kit_web` 在 `127.0.0.1:51888`，内核为其子进程）。
> 除既有的「门禁全绿后可自行重启 dry 内核」授权（[`AI_WORKFLOW.md`](AI_WORKFLOW.md) §2.1 第 8 条）外，
> **不要 kill 面板进程**，不要用测试脚本指向生产 socket。

---

_维护者：ceer_quant · 相关：[`GITHUB_GOVERNANCE.md`](GITHUB_GOVERNANCE.md) · [`AI_WORKFLOW.md`](AI_WORKFLOW.md) · [`KNOWN_ISSUES.md`](KNOWN_ISSUES.md)_

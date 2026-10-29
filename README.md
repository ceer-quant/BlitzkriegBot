# ⚡ BlitzkriegBot

<div align="center">

<img src="assets/logo.png" alt="BlitzkriegBot Logo" width="180"/>

### 预测市场高频量化与跨所套利的「重型武器」

**专为 Polymarket、Kalshi 打造的工业级纯 Rust 自动化量化交易系统**  
*极致吞吐 · 微秒响应 · 100% 模拟无损实验 · 现代可视化控制台*

[![CI](https://github.com/ceer-quant/BlitzkriegBot/actions/workflows/ci.yml/badge.svg)](https://github.com/ceer-quant/BlitzkriegBot/actions/workflows/ci.yml)
[![Security](https://github.com/ceer-quant/BlitzkriegBot/actions/workflows/security.yml/badge.svg)](https://github.com/ceer-quant/BlitzkriegBot/actions/workflows/security.yml)
[![Secret Scan](https://github.com/ceer-quant/BlitzkriegBot/actions/workflows/secret-scan.yml/badge.svg)](https://github.com/ceer-quant/BlitzkriegBot/actions/workflows/secret-scan.yml)
[![Release](https://github.com/ceer-quant/BlitzkriegBot/actions/workflows/release.yml/badge.svg)](https://github.com/ceer-quant/BlitzkriegBot/releases)
[![Rust 2024](https://img.shields.io/badge/Rust-2024%20edition-orange?logo=rust)](https://doc.rust-lang.org/edition-guide/)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](./LICENSE)

[快速开始](#-30-秒极速上手) · [核心卖点](#-为什么选择-blitzkriegbot) · [功能全景](#-功能全景) · [策略蓝图](#-无代码策略蓝图) · [常见问答](#-常见问题-faq)

---

**⚠️ 资金安全免责声明**：预测市场交易伴随真实资金风险。BlitzkriegBot 为 MIT 开源学习与研究软件，不构成任何投资建议。系统**出厂默认 100% 模拟资金（Dry-Run）**，在未经充分验证与明确授权前，请勿随意连接真实资金。

</div>

---

## 💡 为什么选择 BlitzkriegBot？

面对瞬息万变的预测市场，业余的 Python/Node 胶水脚本常常遇到 **GC 垃圾回收延迟、订单卡死、实盘逻辑不可逆崩盘** 等痛点。BlitzkriegBot 用顶尖的高频量化系统工程标准，为你提供坚如磐石的技术底座：

| 关键考量 | 传统脚本 / 简易开源机器人 | ⚡ BlitzkriegBot | 带来的商业与研发价值 |
| :--- | :--- | :--- | :--- |
| **底层性能** | 解释型语言，存在不可预测的 GC 抖动 | **纯 Rust 编写**，裸金属速度，内存零拷贝 | 抢单快人一步，毫秒级响应订单簿剧烈波动 |
| **资金风控** | 简易 try/catch，容易超额下单或爆仓 | **9 重硬件级风控闸门**（单日亏损熔断、仓位封顶、敞口硬限） | 物理级防击穿，策略逻辑无论如何 bug 也无法绕过风控锁 |
| **仿真保真度** | 理想化撮合，实盘立刻滑点亏损 | **对侧订单簿深度穿越撮合 + 确定性回放** | 模拟盘与真实市场 1:1 像素级拟真，拒绝虚假回测盈利 |
| **策略迭代** | 每次修改必须停止进程、丢失网络连接 | **可视化节点蓝图 + Lua 动态热加载** | 零停机热切换策略，无代码拖拽 3 分钟落地新模型 |
| **多所聚合** | 针对单个交易所硬编码，拓展即重构 | **核心与交易所完全解耦（Market Plugin）** | Polymarket / Kalshi / Predict.fun 统一对齐，天然支持跨所套利 |

---

## 🔥 核心杀手级特性

### 1. 🚀 纯 Rust 微秒级自驱动内核
交易核心 `blitzkrieg-core` 拥有自研的订单撮合引擎（OME）、自适应出场状态机与事件总线。不依赖任何重型外部运行时，单二进制即可并发承载全天候的高密度行情扫描与对账。

### 2. 🛡️ 零风险出厂沙盒（Dry-Run First）
- **安全第一准则**：默认以 `--mode dry` 启动，所有订单在本地高仿真账本中虚拟流转，绝不主动触碰私钥与真实资金。
- **结构性只读防护**：提供 `--readonly` 硬件级安全标志，从进程内存层面直接拆除出网下单通道。

### 3. 🎨 双模态全景量化驾驶舱
告别黑屏字符日志，BlitzkriegBot 交付现代化工业级管理界面：
- **实时 Web 面板（Vue 3 + Tailwind v4 + ECharts）**：毫秒级权益曲线、实时订单簿深度（Orderbook Depth）、持仓盈亏看板、拒单归因热力图。
- **极客 TUI 终端界面（Ratatui + Tokio）**：服务器 SSH 直连一键呼出，无网络环境下的全功能监控操作台。

### 4. 🧩 无代码可视化策略蓝图（Blueprint）
无需编写复杂的 Rust 编译宏，在浏览器面板中通过**拖拽数据源、逻辑门、比较算子、买卖动作节点**，一键编译为沙盒化 Lua 策略包，现场编译、现场校验、秒级生效！同时支持官方内置的 11 款专业套利与做市算法（价差套利、跨所对冲、动量跟踪、做市等）。

### 5. 🧬 影子进化与近失样本复盘（Shadow Evolution）
让 AI 变体策略在后台「影子空间」并行无声竞赛！系统自动捕捉「差一点成交（Near-miss）」的极端行情快照，按留出段冷数据真实客观优胜劣汰，用数据事实驱动参数晋升，彻底告别盲目参数调优。

---

## ⚡ 30 秒极速上手

BlitzkriegBot 追求极致的部署工效。你无需搭建复杂的微服务集群，单个二进制开箱即跑！

### 方式 A：预编译免构建路线（最省心 · 推荐新手）
直接到 [Releases](https://github.com/ceer-quant/BlitzkriegBot/releases) 下载对应操作系统（Linux x86_64 / macOS aarch64）的 `tar.gz` 压缩包：

```bash
# 1. 解压并进入目录
tar -zxvf BlitzkriegBot-*.tar.gz && cd BlitzkriegBot-*

# 2. 一键启动（默认安全模拟盘）
bin/blitzkrieg run
```

浏览器打开 **`http://127.0.0.1:51888/panel`**，立刻查看属于你的量化交易大屏！

---

### 方式 B：源码编译路线（面向量化开发者）
需要本地具备 [Rust](https://rustup.rs) 工具链（1.85+）：

```bash
# 1. 获取最新源码
git clone https://github.com/ceer-quant/BlitzkriegBot.git
cd BlitzkriegBot

# 2. 编译生产二进制文件
cargo build --release --workspace --locked

# 3. 安装 blitzkrieg 全局命令（可选）
bash scripts/install-blitzkrieg-shim.sh

# 4. 启动一体化栈
blitzkrieg run
```

> **Windows 用户**：本项目深度利用 Unix 域套接字（UDS），Windows 用户推荐在 **WSL2（Ubuntu）** 环境下按上述 Linux 步骤运行，浏览器直接访问 `http://127.0.0.1:51888/panel` 即可完美使用。

---

## 🎮 常用操作一览

BlitzkriegBot 提供了极其顺手的终端交互与生命周期管理：

```bash
blitzkrieg run              # 一体化启动：交易核心 + Web 仪表盘 + 崩溃自愈看门狗
blitzkrieg run --tui        # 纯终端极客模式：拉起 TUI 字符交互界面
blitzkrieg stop             # 优雅停机：安全关闭同一套接字上的所有核心与网关进程
blitzkrieg net-check        # 网络与交易所连通性快速诊断探针
blitzkrieg --help           # 查看全部进阶调参开关
```

想要在 Web 网页上解锁“启动/停止策略、执行强平”等操作权限？
只需复制配置文件模板并配置管理凭证：
```bash
cp .env.example .env
# 编辑 .env 设置 BLITZKRIEG_PANEL_USER 与 BLITZKRIEG_PANEL_PASSWORD
blitzkrieg run
```

---

## 🏛️ 模块化分层架构

BlitzkriegBot 的分层契约保证了高内聚与低耦合，任何人都可以轻松接入自己的策略与市场扩展：

```
┌────────────────────────────────────────────────────────────┐
│ 🎯 用户策略层 (User Strategies)                           │
│   · 可视化拖拽蓝图 (Blueprint)                              │
│   · 动态热加载 Lua 策略包 (11 款官方精选算法)              │
│   · C ABI v2 裸金属高频动态链接库 (.dylib / .so)           │
├────────────────────────────────────────────────────────────┤
│ ⚡ 交易核心 (blitzkrieg-core)                              │
│   · 撮合引擎 · 挂单生命周期 · 仓位重估 · 多账本对账        │
│   · 9 重风控断路器 · 跨所原子两腿对冲状态机 · 影子进化     │
├────────────────────────────────────────────────────────────┤
│ 🔌 市场扩展插件 (Market Plugins)                          │
│   · Polymarket (CLOB + Gamma) · Kalshi · Predict.fun       │
│   · PolySimulator (离线仿真沙盒)                           │
├────────────────────────────────────────────────────────────┤
│ 🖥️ 双模交互前端 (Presentation Layer)                      │
│   · 现代 Vue 3 仪表盘 (127.0.0.1:51888) · Ratatui TUI 终端 │
└────────────────────────────────────────────────────────────┘
```

---

## ❓ 常见问题 FAQ

<details>
<summary><b>Q: 我完全没有加密货币或预测市场的量化经验，可以使用吗？</b></summary>
当然可以！BlitzkriegBot 出厂设定即为“只读与模拟”。你启动它后不需要质押任何真金白银，也不需要绑定私钥，它可以作为一个纯粹的“实时预测市场动态雷达”，用来观察全网 15 分钟加密二元期权市场的波动与套利空间。
</details>

<details>
<summary><b>Q: 为什么策略安装后默认是关闭的？</b></summary>
这是专业的机构级量化风控准则（Fail-Safe）。在 BlitzkriegBot 中，把策略放入目录仅代表“已安装”。只有当你在控制面板中根据具体账户明确点击“启用”，或者在配置文件中指定启用的策略时，它才会进入实际扫描周期。
</details>

<details>
<summary><b>Q: 支持多账户和跨交易所同时对冲交易吗？</b></summary>
原生支持！系统支持多套独立账本。你可以将 A 账户绑定至 Polymarket，B 账户绑定至 Kalshi，两所盘口异动时，内核的原子对冲状态机会按价差阈值自动发起双腿对冲与敞口平账。
</details>

<details>
<summary><b>Q: 数据和日志保存在哪里？</b></summary>
全部保留在本地仓库的 <code>data/</code> 目录下（包括订单日志 <code>orders.jsonl</code>、成交日志 <code>trades.jsonl</code>、审计账目与行情归档）。绝不上传任何第三方服务器，数据主权 100% 归你所有。
</details>

---

## 📚 进阶开发指南与文档地图

想要深入内核细节或定制专属的交易模型？查看我们的权威技术文档：

- 📖 [功能清单与完成度矩阵 (FEATURES.md)](./docs/FEATURES.md)
- 🏗️ [核心架构设计详解 (ARCHITECTURE.md)](./docs/rust-core/ARCHITECTURE.md)
- 📝 [策略编写与加载实操指南 (STRATEGY_GUIDE.md)](./docs/rust-core/STRATEGY_GUIDE.md)
- 🔌 [新增交易所市场插件指南 (EXTENSION_GUIDE.md)](./docs/rust-core/EXTENSION_GUIDE.md)
- 🧬 [AI 影子进化机制揭秘 (SHADOW_EVOLUTION.md)](./docs/rust-core/SHADOW_EVOLUTION.md)
- 🔒 [安全披露政策 (SECURITY.md)](./SECURITY.md)

---

## 📄 许可证与开源共建

BlitzkriegBot 遵循 **[MIT 许可证](./LICENSE)** 开源。

我们热忱欢迎社区开发者的贡献！提交代码前请查阅 [贡献指南 (CONTRIBUTING.md)](./CONTRIBUTING.md)。

<div align="center">

**[⚡ 立即体验 BlitzkriegBot，开启你的现代化高频量化之旅 ⚡](https://github.com/ceer-quant/BlitzkriegBot)**

</div>

# 已知问题与未完成项（KNOWN ISSUES）

> **本文件回答一个问题**：这个仓库现在**有哪些已知的、但还没有修的问题**？
>
> 与相邻文档的分工（**不要重复造**）：
>
> | 文档 | 管什么 | 不管什么 |
> | --- | --- | --- |
> | **本文件** | **缺陷 / 未验证路径 / 未完成项 / 技术债**，带严重度、证据、影响、状态 | 决策的来龙去脉与备选方案 |
> | [`DECISIONS_PENDING.md`](./DECISIONS_PENDING.md) | 需要**人类拍板**的分歧：背景 / 选项 / AI 倾向 / 用户裁决 | 缺陷清单（本文件按编号引用它） |
> | [`FEATURES.md`](./FEATURES.md) | 功能**有什么**、完成到什么证据等级 | 缺陷 |
> | [`blitzkrieg/MIGRATION_LOG.md`](./blitzkrieg/MIGRATION_LOG.md) | **已修复**缺陷的变更史（§1–§48） | 未修复项 |
>
> 编号规则：`KI-<n>`。`D-<n>` 指 `DECISIONS_PENDING.md` 的决策项。
> 本文件**只描述现状**，不含「应该怎么修」的完整方案——方案随 Issue 走。

---

## 0. 严重度与状态定义

| 严重度 | 含义 | 处理要求 |
| --- | --- | --- |
| 🔴 **阻断** | 影响**资金正确性或状态一致性**，或会让验证结论失真 | 优先修；修完前不得据此结论调参 |
| 🟠 **高** | 功能声明与实现不符，或关键路径**从未被验证** | 进当前里程碑 |
| 🟡 **中** | 影响可维护性 / 开发体验 / 回归保护 | 排期修，不阻断主线 |
| 🔵 **低 / 已知限制** | 有意的取舍，或外部平台限制 | 记录在案即可 |

| 状态 | 含义 |
| --- | --- |
| **开放** | 未修，未排期 |
| **已排期** | 已定里程碑（0.2 / 0.3） |
| **阻塞** | 等待用户裁决或外部条件 |
| **已裁决待实现** | 用户已给出口径，代码未动 |
| **有意保留** | 已裁决为「不修」，永久记录以免后人误当 bug 修 |

**最后核对：2026-09-17，针对 `main` @ `4e93d6e`。**

> 📌 **上一次核对时工作树里那条 `OrderRole` 工作线（成交角色 / 费用记账）已经落地**：
> `fe9c871` 实现，`4e93d6e`（PR #89）补好它的两道验收门禁，变更史见
> [`blitzkrieg/MIGRATION_LOG.md`](./blitzkrieg/MIGRATION_LOG.md) §48。
> 对本文件的影响**只有 KI-1**，而且**没有修好它**——详见 KI-1 里的边界说明。
> 其余条目本次核对未发现由该批改动引起的变化。

---

## 1. 🔴 阻断级

### KI-1 · dry 行情路径不跑穿越撮合：挂单永不成交，入场全部升级为 taker

| 字段 | 内容 |
| --- | --- |
| **编号** | KI-1（对应 D-11） |
| **严重度** | 🔴 阻断 |
| **状态** | **已裁决待实现**（用户裁决「B 是」：把穿越撮合接进行情路径） |
| **影响面** | 所有 DryRun 会话与**基于 dry 数据的回放结论** |

**现象**：dry 模式下一笔限价单挂出后，即使行情已经穿过它的价格，它也**永远不会成交**；
入场因此全部走 taker 分支，按 **1.7% 往返费**计。

**证据（`HEAD`）**：`core/blitzkrieg_core/src/service.rs` 中 `try_maker_fill` 的调用点只有两处：

- `place_after_submit`（约 `service.rs:2004`）——下单那一刻自检一次；
- `book_snapshot`（约 `service.rs:2193`）——显式请求盘口快照时。

而行情主路径 `engine_on_data`（`service.rs:1046`）只把盘口镜像进 `self.books`，
**从不调用 `try_maker_fill`**。行情推进不会触发挂单成交。

**为什么是阻断级**：这不是「少一个功能」，而是**测量基准错了**。dry 的成交率、费用、
净盈亏分布都系统性偏向 taker，任何基于 dry 样本做的参数取舍（止损宽度、移动止盈下限、
入场时点）都建立在一个比实况更贵的经济模型上。E3 的多次校准结论（§ `DECISIONS_PENDING`
E3-c / F1 附注）都建立在 dry 回放上，需在 KI-1 修复后复核。

**已知边界**：修复不改变任何风控/账本语义，只是让 dry 的成交判定与 live 同源。

**与 E17（`MIGRATION_LOG` §48）的边界——不要把两者当成同一件事（2026-09-17 复核）**：
E17 已在 `fe9c871` / `4e93d6e` 落地，但**本条未修复**。复核 `HEAD`（`4e93d6e`）：
`try_maker_fill` 的调用点依然只有 `place_after_submit`（`service.rs:2100`）与
`book_snapshot`（`service.rs:2297`）两处，行情主路径 `engine_on_data`（`service.rs:1046`）
仍只把盘口镜像进 `self.books`，**从不调用 `try_maker_fill`**。

两者的分工是互补的，不是重叠的：

| | 回答的问题 | 状态 |
| --- | --- | --- |
| **KI-1（本条）** | dry 的挂单**到底会不会成交** | ❌ 未修 |
| **E17** | 成交之后**费用该按哪个角色算** | ✅ 已修（§48） |

E17 修好的是本条的**前置条件**：角色一旦按实际成交判定，费用自动跟随，于是
「账本按 taker 收、持仓记录却写 maker 0」这类**自相矛盾**消失了。
但 dry 的成交率与费用分布**仍然系统性偏向 taker**（入场走 taker / 升级分支），
所以本条的结论一字不改：

- 「dry 的经济模型比实况更贵」**仍然成立**；
- 基于 dry 样本做的参数取舍（止损宽度、移动止盈下限、入场时点）**仍需在本条修复后复核**；
- E3 系列校准结论**仍然未复核**。

反之亦然：E17 的 69 条 parity 断言证明的是**记账正确性**（dry 与 live 逐位一致），
**不是成交率基准正确性**。不要用 E17 的绿灯关闭本条。

---

## 2. 🟠 高风险：从未被验证的路径

> 这些不是「已知会错」，而是**从未被证伪**。对交易系统而言，未验证 = 未知风险。

### KI-2 · Live 全链路从未验证

| 字段 | 内容 |
| --- | --- |
| **严重度** | 🟠 高 |
| **状态** | **已裁决待实现**（D-5：用户裁 B —— 等策略重构与多策略之后再小额验证） |

**从未跑过的部分**：Poly1271 签名、交易所授权（allowance）、live 启动时的**孤儿订单清扫**。
孤儿清扫只在 DRY 下验证过；首次 live 启动必须确认日志出现
`startup sweep cancelled N orphan order(s)`。**首次真实下单是唯一能做这项验证的时机**，
此前 live 链路的状态应一律描述为「未验证」，不得描述为「可用」。

### KI-3 · 影子进化从未在生产启用，也无长期实况数据

| 字段 | 内容 |
| --- | --- |
| **严重度** | 🟠 高 |
| **状态** | 开放 |

`ShadowEvolutionConfig` 默认 `enabled: false`，需显式 `--shadow-evolution` 才开；
生产 dry 会话从未长期开启过，因此**「进化出的参数在长期实况下是否稳定」没有任何数据**。
现有证据是回放/留出段级的（见 [`reports/`](./reports/)），不是实况级的。
按 E2-c 的设计，默认关闭 = 行为与改动前逐位一致，这一点由测试钉住；
但**开启后的长期行为未知**。

### KI-4 · E6 Tauri 桌面端到端未验证

| 字段 | 内容 |
| --- | --- |
| **严重度** | 🟠 高 |
| **状态** | 开放 |

`ui/webapp/src-tauri/tauri.conf.json` 存在、脚手架已交付，但
`cargo tauri build` 后的 `desktop_snapshot` / `desktop_command` **全链路从未走通并留下证据**。
E8（#57）的验收里包含这条，属未达成项。

### KI-5 · 没有性能/资源基线

| 字段 | 内容 |
| --- | --- |
| **严重度** | 🟠 高 |
| **状态** | 开放（部分由 E9 规模化门禁覆盖，但见 KI-9） |

无长期 soak 下的延迟分布、内存增长、归档写入吞吐基线。
这意味着「某次改动让引擎变慢了」在现有门禁下**不会被发现**——
单测与功能门禁都不测时间与内存。

### KI-6 · 无系统性故障注入 / 混沌测试

| 字段 | 内容 |
| --- | --- |
| **严重度** | 🟠 高 |
| **状态** | 开放 |

崩溃恢复有专项脚本（`scripts/order-recovery-check.mjs`、`position-recovery-check.mjs`），
但都是**定向场景**，不是系统性故障注入（磁盘满、时钟跳变、UDS 半开连接、
归档锁竞争、SQLite 写失败等）。已知的历史事故（孤儿订单、持仓失管、归档同秒撞名）
**都是被真实运行逮到的，不是被测试逮到的**——这个模式说明故障注入面是缺的。

---

## 3. 🟠 功能缺口：实现与声明不符

### KI-7 · E9 验收未达成：`dog_strategy` 仍是 327 行手写 unsafe FFI

| 字段 | 内容 |
| --- | --- |
| **严重度** | 🟠 高 |
| **状态** | 开放（E9-d 的一半已交付，示例改写未做） |

**事实**：

- `user_layer/strategy_api/src/safe.rs` 已存在（约 28 KB）——`SafeStrategy` 包装层**已实现**；
- `user_layer/strategies/dog_strategy.rs` 仍是 **327 行**，含 **45 处 `unsafe`**。

**E9（#59）的验收原文要求**：「`dog_strategy` 改为 `SafeStrategy` 示例后 **LOC ≤ 40**」。
当前是 327 行。**这是 E9 未关闭的硬项**，也是「策略 SDK 好不好用」的唯一实证——
包装层有了但没人用它写过一遍，就还不能说它可用。

### KI-8 · E9-h（策略开发者签名页）延期到 0.3

| 字段 | 内容 |
| --- | --- |
| **严重度** | 🟡 中 |
| **状态** | 已排期（0.3） |

浏览器无法直连 UDS，该页需先有网关侧的 dylib 加载通道。当前只提供 CLI 路径（`strategy.load`）与文档。

### KI-9 · 🟠 专项门禁**大面积没有接进 CI**

| 字段 | 内容 |
| --- | --- |
| **严重度** | 🟠 高 |
| **状态** | 开放 |

仓库里存在 **22 个可运行的验证门禁**，其中**只有 `secret-scan.sh` 一个在 CI 里**
（加上 `npm test` / `typecheck` / `build` 与 `ui:webapp`，它们覆盖的是单元测试与面板验收）：

**A. 有 npm 脚本别名、但 CI 未调用（13 个）**：

| 门禁 | 覆盖什么 |
| --- | --- |
| `core:parity` | Node ↔ Rust 核心行为一致性（订单/账本语义） |
| `core:parity-engines` | Node ↔ Rust **决策**等价 |
| `core:strategy-limit` | E2-a 按策略定寸与配额 |
| `core:strategy-gate` | E2-b 按策略门禁豁免 |
| `core:strategy-evolve` | E2-c 按策略影子进化 |
| `core:trend-follow` | E4-a 趋势腿 |
| `core:mean-reversion` | E4-b 逆向腿 |
| `strategy:devcheck` | E9-a 模板 → 构建 → load → signal 全链 |
| `scale:plugins` | E9 规模化：50 策略 × 注册表读取延迟 |
| `scale:feed` | E9 规模化：行情推送丢包率 |
| `tui:check` | TUI 冒烟 |
| `ui:plugin-gateway` | 网关插件命令面 |
| `ui:plugin-panel-pty` | TUI 插件操作（PTY 实测） |

**B. 无 npm 别名、只能直接 `node scripts/…` 跑（9 个）**：

`cycle-check.mjs`（DryRun 订单链端到端）、`order-recovery-check.mjs`（崩溃后订单恢复）、
`position-recovery-check.mjs`（崩溃后持仓恢复）、`core-adopt-check.mjs`（重复客户端接管）、
`market-plugin-check.mjs`（市场插件选择）、`backtest-check.mjs`（回放保真）、
`trade-log-flag-check.mjs`、`ui-eventbus-check.mjs`、`ui-kit-gateway-check.mjs`。

**CI 实际只跑**（`.github/workflows/ci.yml`）：`cargo build --release --locked`、
`BK_REQUIRE_DYLIB=1 cargo test --workspace --locked`、`cargo fmt`（建议性）、
`cargo clippy`（建议性）、`npm ci`、`npm run typecheck`、`npm test`、`npm run build`、
`npm audit`（建议性）、`npm run ui:webapp`（含面板 8 套 `check:*`）、`bash scripts/secret-scan.sh`。

**后果**：上面这 22 个门禁**全部依赖人工在本地记得跑**。任何一次「改完直接开 PR」
都可能把 E2 / E4 / E9 的回归放进 `main` 而 **CI 依然全绿**。
**这是当前回归保护上最大的单点缺口**，也是它被排在处理顺序第一位的原因。

### KI-10 · 全局连亏熔断未按策略分片

| 字段 | 内容 |
| --- | --- |
| **严重度** | 🟠 高 |
| **状态** | 已排期（0.3，D-18 用户已同意选项 A） |

`Core` 只有一个 `LossBreaker` 字段（`service.rs:274`），由
`self.breaker.record(closed.net_pnl_usd, now_ms)`（`service.rs:1744`）喂入，**没有策略维度**。
多策略并跑时，**任一策略连亏会冻结全核入场**，污染样本外结果
（证据见 [`reports/TREND_FOLLOW_HOLDOUT_REPORT.md`](./reports/TREND_FOLLOW_HOLDOUT_REPORT.md) §3.2）。

**已定口径**：熔断按策略分片；**日亏上限与 kill switch 保持全局**（这两者的全局性是有意的）。

### KI-11 · 内核不读取任何 TOML，「配置文件」是死配置

| 字段 | 内容 |
| --- | --- |
| **严重度** | 🟡 中 |
| **状态** | **已裁决待实现**（D-1：用户裁「是，顺序你决定」） |

`user_layer/configs/*.toml`、`extensions/*/config.toml` **没有任何代码读取**——
全仓对文件名与 `toml::from_str` 均无命中。内核配置来源只有 CLI 参数与代码默认值。
**改这些 TOML 不会生效。**

用户已裁定要引入配置文件作为权威配置源，优先级由实现方定（建议 CLI > env > TOML）。
在落地前，这些文件应视作文档而非配置。

### KI-12 · 实盘小额验证未做

见 KI-2。D-5 用户裁 B：等策略表现与多策略实现之后再审。
**在它做完之前，整个 live 面都属未验证。**

---

## 4. 🟡 技术债

### KI-13 · Rust 格式与 lint 债：99 个文件 fmt 未过，135 条 clippy 警告

| 字段 | 内容 |
| --- | --- |
| **严重度** | 🟡 中 |
| **状态** | **已裁决待实现**（D-6：用户裁「由你决定，等待核心开发完成，你便可实施」） |

**实测（2026-09-17，`main` @ `4c4b351`；数字本身与 HEAD 无关，E17 未触碰格式化）**：

- `cargo fmt --all -- --check` → **99 个文件**有 diff（99 条 `Diff in`）；
- `cargo clippy --workspace --all-targets` → **135 条 warning**，主要集中在：

| 条数 | lint | 说明 |
| --- | --- | --- |
| 44 | `collapsible_if` | 嵌套 `if` 可合并 |
| 42 | `unnecessary clone → slice` | `&x.clone()[..]` 类写法 |
| 11 | `field_reassign_with_default` | `Default::default()` 后逐字段赋值 |
| 3 | `assert_eq!` 用字面量 bool | 测试写法 |
| 3 | `unnecessary closure with bool::then` | |
| 3 | `sort_by_key` | |
| 6 | 复杂类型 / 参数过多（7–10 个） | 建议拆类型 |

CI 中这两项都是 **advisory**（`continue-on-error: true`，见 `ci.yml:53,56`），不阻塞 PR。
清理完成后应转为阻塞——**但这需要在一次独立、可回归的专项 PR 里做**，
不要夹在功能变更里（会淹没真实 diff）。

### KI-14 · 86 个 Node 生产层依赖漏洞（含 2 critical / 49 high）

| 字段 | 内容 |
| --- | --- |
| **严重度** | 🟡 中 |
| **状态** | **已裁决待实现**（D-9：用户裁「C 随 D-4 下线」） |

`npm audit --omit=dev` → **86 vulnerabilities（2 low / 33 moderate / 49 high / 2 critical）**。
`npm audit --audit-level=high --omit=dev` 在 CI 中为 **advisory**。
漏洞主体在旧 Node 交易域依赖链（含 `viem` 等）。

**已定处置**：不单独做 `--force` 全量升级（风险高、范围大），
**随 D-4 / KI-15 的旧 Node 交易域下线一并解决**。

### KI-15 · 旧 Node 交易域仍在 `src/` 中

| 字段 | 内容 |
| --- | --- |
| **严重度** | 🟡 中 |
| **状态** | **已裁决待实现**（D-4：用户裁：不删 `src/`，先冻结其交易域，等自有面板完成后替换） |

`src/` 中仍保留 Node 侧的交易域目录。当前**约定**是 Node 只做 UI/展示层、不参与交易决策；
但代码仍在，且 KI-14 的漏洞面主要来自它。**计划是自有面板成熟后替换，不做提前删除。**

### KI-16 · 源码中唯一的 TODO

| 字段 | 内容 |
| --- | --- |
| **严重度** | 🔵 低 |
| **状态** | 有意保留 |

全仓源码只有一处 `TODO`：`scripts/blitzkrieg-new-strategy.mjs:141`
（策略模板里的「持仓管理写在这里」占位）。**这是模板的预期占位**，
配套还有 `:304` 对三个 TODO 的说明文字，不是遗留债。
（历史上曾有多处，均已清掉；这项记录在此，是为了让接手人知道「只有一处、且是故意的」。）

### KI-23 · 持续 panic 的影子变体不会被隔离：`crashed` 置位在外层，短路在内层

| 字段 | 内容 |
| --- | --- |
| **严重度** | 🟡 中（**性能 / 可观测性**，非正确性） |
| **状态** | 开放（E10-d 过程中发现，2026-09-17） |

**现象**：一个**每个 tick 都 panic** 的用户策略变体，会**永远不被标记为 `crashed`**，
于是每个 tick 都 panic 一次、被吞一次，无限重复。引擎不会倒，但也没有任何告警。

**证据（`HEAD` @ `4e93d6e`）**：代码里有两层 `catch_unwind`，但置位标志的那层在**外面**：

- **内层**（吞掉用户策略 panic 的那层）`strategies/shadow_twin.rs:277`：
  `catch_unwind(…).unwrap_or_default()`，调用点是 `TwinReplay::on_tick` 的
  `shadow_twin.rs:227`（出场判定）与 `:239`（入场判定）。
  用户策略（如 `find_candidates`）的 panic 在这里就被转换成默认返回值，**错误被丢弃**。
- **外层**（置位 `crashed` 的那层）`shadow_evolution/mod.rs:327`：
  `catch_unwind(|| v.on_tick(&ctx))` → 失败则 `v.crashed = true` + `tracing::warn!`。
  但它包的 `v.on_tick` 内部**已经**有内层 `catch`，所以这层实际只兜出场/回放机制
  （`update_exit_state` / `decide_exit` / 持仓簿记）的 panic。

**因此**：外层那条 `tracing::warn!("shadow variant panicked — quarantined")` 对
**用户策略自身的 panic 永不触发**；`crashed` 对这类变体永不置位。

**与文档/注释不符**：`shadow_twin.rs:273-275` 的注释写
「the caller quarantines it on the panic」，但唯一的调用方 `TwinReplay::on_tick`
只是 `unwrap_or_default()` 丢弃错误，**没有任何隔离**。照该注释理解这个系统会判断错。

**影响面**：

- **不是正确性**：引擎不会崩，live 路径不受影响——这一点已由新增单测
  `a_panicking_variant_cannot_take_down_the_engine` 与
  `a_twins_own_panic_is_absorbed_and_never_sets_the_crashed_flag`
  （`shadow_evolution/mod.rs`）双向钉住。
- **是性能与可观测性**：每 tick 一次 panic/unwind（unwind 比正常返回贵得多），
  且**静默**——运维看不到任何日志，只会觉得影子进化「有点慢」。变体是用户外挂的，
  上线坏策略是现实场景。

**为什么登记而不在这里修**：修法有多种（把内层 `catch` 的错误上抛给 `TwinReplay`、
或在 `Variant` 记录连续 panic 次数并自行熔断），都会改动影子进化的内部契约，
属于独立变更，不该夹在 E10-d 的构建配置 PR 里（会淹没真实 diff）。
AI 倾向：**让内层把 panic 事实回报给 `Variant`**（例如 `ShadowTickResult` 增
`panicked: bool`），由 `Variant` 累计并置 `crashed`——这样「隔离」的实现在**一层**里，注释也不再骗人。

**与 D-20 的关系**：这条是 D-20 的**前提证据**——`panic = "abort"` 会同时废掉这两层
（内层的吞掉与外层的隔离都不再成立，一次 panic 直接 abort）。
因此 D-20 选「维持 `panic = "unwind"`」时，**不能**以「反正有隔离」为理由——
隔离对用户策略其实没生效，真正保住引擎的是**内层那条 `unwrap_or_default()`**。

---

## 5. 🔵 平台与外部限制（查过、确认无解，非缺陷）

### KI-17 · 私有仓库在 GitHub Free 下无服务端分支保护

| 字段 | 内容 |
| --- | --- |
| **状态** | **有意保留**（D-8：用户裁「A 后续会开源」） |

实测：对分支保护 / 规则集调用 API 返回 `403 Upgrade to GitHub Pro`；
原生 Secret Scanning / Push Protection 返回 `422` / `404`。
**因此「main 受保护」目前只是流程约定，不是平台强制。**

**后果（必须知道）**：任何人都能直推 main。防线的唯一保障是
「本地门禁 → PR → CI 绿灯 → squash 合并」这条**自觉执行**的流程。
后续开源（或升级套餐）后应立刻补上服务端保护。

### KI-18 · `gh` CLI 未安装

| 字段 | 内容 |
| --- | --- |
| **状态** | 🔵 低 |

Issue / PR / 标签 / 合并等全部操作**走 curl + GitHub REST API**。
本文档与 [`GITHUB_GOVERNANCE.md`](./GITHUB_GOVERNANCE.md) 中的 API 配方即为此而写。

### KI-19 · CI 的 `cancel-in-progress` 会让「没有失败」看起来像「通过」

| 字段 | 内容 |
| --- | --- |
| **状态** | 🔵 低（但是个容易踩的坑） |

`ci.yml` 的并发组是 `ci-<ref>` 且 `cancel-in-progress: true`。
**连续快速推送会取消前一次运行**——因此「最新 run 是绿的」不等于「这次改动被验证过」。
判断一次推送是否真的通过，必须核对 check run 的 **HEAD SHA** 与你要合并的 commit 一致。

---

## 6. 🟡 治理/仓库卫生债

### KI-20 · 分支与标签堆积

| 字段 | 内容 |
| --- | --- |
| **严重度** | 🟡 中 |
| **状态** | 开放 |

**实测（2026-09-17）**：

- 本地分支 **31** 个，其中只有 **3** 个已并入 `main`（`develop`、`feat/e9a-strategy-scaffold`、
  `feat/e9f-tui-onboarding`，其余为已合并但未清理的历史分支）；
- 远端分支 **44** 个（仓库设置为**合并后不自动删分支**，`delete_branch_on_merge: false`）；
- 本地标签 **14** 个，**从未推送到远端**（`git ls-remote --tags ceer` 为空）。

**风险**：合并后用 `git branch -d` 会报「not fully merged」——这是 squash 合并的正常现象，
**不是**内容丢失。正确判定见 [`GITHUB_GOVERNANCE.md`](./GITHUB_GOVERNANCE.md) §4：
先 `git diff --name-only main <branch>`，输出为空才可 `-D`。

### KI-21 · 专项门禁未接 CI

见 KI-9（同一问题的治理侧）。此处不重复。

### KI-22 · Dependabot 的三个 PR 长期开放

| 字段 | 内容 |
| --- | --- |
| **严重度** | 🔵 低 |
| **状态** | 开放 |

开放中的自动化 PR：#1 `actions/checkout` 6→7、#2 `actions/setup-node` 6→7、
#3 minor-updates 组（38 项）。与 KI-14 同源：依赖升级的取舍应随 D-4 一并决定，
不要单独合并大版本跳跃。

---

## 7. 有意保留项（**不要当 bug 修**）

这些看起来像遗留，但**已经用户明确裁决保留**。改它们等于破坏历史契约。

| 项 | 位置 | 为什么保留 |
| --- | --- | --- |
| 加密盐 `clodds-secrets-v1` | `src/security/index.ts:442` | 改盐 = 既有密文**永不可解**（D-13.1，用户裁：由新版创建的数据不留痕迹，历史测试数据不追溯） |
| 链上账本备注 `clodds:ledger:<hash>` | `src/ledger/anchor.ts:79,152,153,289` | 改备注 = 历史锚点**无法校验**（D-13.2，同上裁决） |
| `origin` remote 已移除 | `.git/config` | 上游 `alsk1992/BlitzkriegBot` 无推送权（403），用户裁移除；**代价是失去与上游比对能力** |
| `docs/AI_WORKFLOW.md` 的「仓库已启用 Secret Scanning + Push Protection」表述 | `AI_WORKFLOW.md:141` | 与 KI-17 矛盾：Free 私有仓上**实际未启用**。属**文档未同步**而非有意保留，见下方注 |
| `ui/*` 被 gitignore | `.gitignore:107` | `ui/hft.html`、`ui/hft-dashboard.html` 是**本地专用**的历史单文件面板，**从未入库**（`ui/ui_kit/`、`ui/ui_kit_panel/`、`ui/webapp/` 已用 `!` 反忽略）。E8 要替换的正是这两个文件 |

> **注（文档与现实的偏差，建议顺手修）**：`docs/AI_WORKFLOW.md:141` 写
> 「仓库启用 Secret Scanning + Push Protection（见 `scripts/github/bootstrap-repo.sh`）」，
> 但实测在 GitHub Free 私有仓上**不可用**（KI-17）。读者若按此文档假设有服务端密钥防线，
> 会得出错误的安全判断。**修文档即可，不涉及代码。**

---

## 8. 快速索引：一句话状态

| 编号 | 一句话 | 严重度 | 状态 |
| --- | --- | --- | --- |
| KI-1 | dry 挂单永不成交，入场全 taker（测量基准错）——**E17 未修本条**，只修好了它的前置条件 | 🔴 | 已裁决待实现 |
| KI-2 | live 全链路从未验证 | 🟠 | 已裁决待实现 |
| KI-3 | 影子进化从未在生产启用 | 🟠 | 开放 |
| KI-4 | Tauri 端到端未打通 | 🟠 | 开放 |
| KI-5 | 无性能/资源基线 | 🟠 | 开放 |
| KI-6 | 无系统性故障注入 | 🟠 | 开放 |
| KI-7 | `dog_strategy` 327 行 / 45 unsafe，验收要求 ≤40 | 🟠 | 开放 |
| KI-8 | E9-h 延期 0.3 | 🟡 | 已排期 |
| KI-9 | **22 个专项门禁只有 1 个接进 CI** | 🟠 | 开放 |
| KI-10 | 连亏熔断未按策略分片 | 🟠 | 已排期（0.3） |
| KI-11 | 内核不读 TOML（死配置） | 🟡 | 已裁决待实现 |
| KI-12 | 实盘小额验证未做（同 KI-2） | 🟠 | 已裁决待实现 |
| KI-13 | 99 文件 fmt + 135 clippy | 🟡 | 已裁决待实现 |
| KI-14 | 86 个 Node 生产依赖漏洞 | 🟡 | 已裁决待实现 |
| KI-15 | 旧 Node 交易域仍在 `src/` | 🟡 | 已裁决待实现 |
| KI-16 | 源码仅 1 处 TODO（模板占位） | 🔵 | 有意保留 |
| KI-17 | Free 私有仓无服务端保护 | 🔵 | 有意保留 |
| KI-18 | `gh` CLI 未装，走 REST | 🔵 | 已知限制 |
| KI-19 | CI cancel-in-progress 会造成假绿灯 | 🔵 | 已知限制 |
| KI-20 | 31 本地 / 44 远端分支、14 未推标签 | 🟡 | 开放 |
| KI-22 | 3 个 Dependabot PR 长期开放 | 🔵 | 开放 |
| KI-23 | 持续 panic 的影子变体不被隔离（`crashed` 在内外层错位） | 🟡 | 开放 |

---

## 9. 接手人建议的处理顺序

按「先修测量基准，再谈调参」的原则：

1. **KI-9（门禁接 CI）** —— 成本最低、收益最大。在它修好之前，其它结论的回归保护都不完整。
2. **KI-1（dry 穿越撮合）** —— 所有 dry 经济结论的前提。修完需复核 E3 系列校准结论。
   E17（`MIGRATION_LOG` §48）已经**移除了它语义上的顾虑**：账本与持仓记录现在都按
   **实际成交角色**记账，所以把 `try_maker_fill` 接进行情路径后，费用会**自动**跟随真实角色，
   不需要再改任何记账代码。KI-1 因此现在是一次**纯粹的成交路径改动**。
3. **KI-7（`dog_strategy` 改写）** —— E9 关闭的唯一硬项，也是策略 SDK 可用性的唯一实证。
4. **KI-10 / KI-11（连亏熔断分片 / 配置源）** —— 已定 0.3 口径，按里程碑推进。
5. **KI-13 / KI-14 / KI-15** —— 技术债，各自独立 PR，**不要夹在功能变更里**。
6. **KI-2（live 小额验证）** —— 需用户在场并授权，不得自行开启（硬约束 §2.1）。

---

_维护约定：新增已知问题请追加 `KI-<n>` 编号并写明**证据（文件:行）**与**影响面**；
问题修复后**不要删除本条目**，改为把状态置为「已修复」并附 PR 号与
`MIGRATION_LOG` 章节号——本文件同时是「曾经有什么问题」的记录。_

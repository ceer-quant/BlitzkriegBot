# RFC-661: venue 独立身份与 API 层行情复用——寄生知识的 OME 出清路线

> Refs #661（决策层 RFC，P0 建模层）。修订链：#627（shadow 显示簿）→ #629（books 合成）→ #634（shadow 进裁决）→ #638（engine mirror 表）→ #640/#641 → #643 → #652（收尾）→ #647/#653（结算归属）。
> **Supersedes**：#627 的「token 全局唯一一份行」不变式与 #638 的 engine-mirror 合同（两者由本 RFC 整体替换，非局部修补）。#652 是按旧合同正确完成的收尾，issue 已结案，无需重开——本 RFC 退役的是它实现时所在的合同。
> 状态：**已裁决**（操作者 2026-10-27，见 §三）。M0 = 本文档；M1–M4 按 §六迁移序逐步落地，每步独立 Issue/PR、可回滚，RFC 未合入前实现 PR 仅为提案。

## 一、背景

### 1.1 寄生的由来

#637/#638（2026-10-26 操作者裁决）：polysimulator 的数据面（~3.4GB/天轮询 + 双份装配线）整体退役，行情/发现/轮次寄生 polymarket 插件——PolySimulator 与 Polymarket 同窗口的 conditionId / upTokenId / downTokenId **逐字节相同**，token 逐字节同是构造性事实。

当时选择的建模是「**引擎知道寄生**」：

- `market_api::MarketPlugin::mirror_of()`（core/market_api/src/plugin.rs:283）——插件向 trait 申明「我镜像谁」；
- 引擎 `mirror_of: HashMap<String, String>`（core/blitzkrieg_core/src/engine.rs:456），`set_venue_mirror`（engine.rs:692）由 boot（service.rs:4664 `apply_plugin_mirrors`）与运行时启用（ipc/server.rs:478 `enable_venue_at_runtime`）双路推入；
- 受理与路由读它：`scope_admits_token`（engine.rs:787）的镜像臂、`execution_venue_for`（engine.rs:2151）的镜像覆盖臂；
- 显示面三处合成：engine merge 臂的 `shadow_markets` 显示簿（engine.rs:506、插入点 engine.rs:1148）、`ENGINE_ROUND` 行情合成行（ipc/server.rs:16928 一带的装配）、`books_view` 镜像合成臂（service.rs:8904 `for (mirror, source) in engine.mirror_pairs()`）。

### 1.2 症状：OME 可感知寄生的五处违建

「寄生」本应是**插件自己与 API 层之间的数据拓扑**——谁吃谁的流、上游连接怎么省。但上述建模把它变成了**撮合引擎（OME）可感知的属性**，五处违建：

1. **引擎 mirror 表**：`mirror_of` 被 boot 推进 engine（`set_venue_mirror`），`scope_admits_token` / `execution_venue_for` 读它做受理（#638/#640/#641/#643/#652 链）。撮合裁决里住进了「这个所是那个所的镜像」这条来源关系知识。
2. **成交行 venue 标签泄漏**：token 表按 token 唯一键行走（scanner.rs:161 `market_for_token` 首中即返回），镜像 token 的表行永远标着先到的 polymarket。#638 之前，成交行 venue 从表行抄（=polymarket），而执行插件是 polysimulator——面板滤网 `venue:polysimulator` 把行全滤掉（用户截图 bug：汇总徽标计 1 单、筛后 0 行）。#638 改为按执行身份打戳（service.rs:10316 `execution_venue_of`）修了标签，但那是对症退烧：**根因是两个 venue 在表里根本没有各自的行**。
3. **shadow_markets 同 slot 合并**：#627 不变式——「镜像盘与被镜像所在 scanner 表里就是同一市场的一份行（token 唯一键不变式保持）」——镜像批进不了表，整批塞进 `shadow_markets` 显示簿（engine.rs:1148）。两个交易场所被建模成一份行的两个视图。
4. **books_view 兄弟装配线为被遮蔽 venue 合成显示深度**（#629，service.rs:8888-8930 一带）：`shadow_markets` 批次 + `mirror_pairs` 遍历两条合成臂，把镜像源的深度行**改标签**给镜像 venue 用——因为该 venue 在表里没有自己的行可装配。
5. **#653 的寄生结算归属补偿**：`execution_venue_of` 打戳进 SettlementBooking → JournalLine → RedemptionClaim/MergeClaim → `take_redemptions_for_venue` 分桶排水。其中「寄生 token 的归属归属到执行所」这一半是为 mirror 形态打的补丁（位置/成交 venue 戳曾从表行抄）；而「任意两个 venue 互取结算请求的隔离 / take-once / 无归属 journal replay」是通用多 venue 保证，与本 RFC 无关。

### 1.3 为什么这是建模错误而不是 bug

上面每一处单独看都有正当的修 bug 动机，合起来却构成一个反微内核的事实：**OME 的受理、路由、成交归属、账本都开始依赖「数据从哪来」**。第二条上游插件将来以非镜像形态复用别人的 token 空间（做市模拟、延迟回放、对敲盘）时，每一处 mirror 臂都要再改一遍——因为「寄生」被建成了引擎里的一等概念，而不是 API 层的数据复用。

操作者裁决（原文，2026-10-27）：

> 「镜像（寄生）是向 API 申明的属性和需求，数据不应该从撮合（OME）这些部分实现，而是从 API 层就实现寄生，OME 要无法分辨寄生与原生的区别」
> 「只有插件自己知道自己是寄生，其他部分必须无法感知与原生插件区别」

## 二、目标 / 非目标

**目标**：

- **G1（OME 盲）**：OME 对寄生无知——受理 / 路由 / 成交归属 / 账本全按本 venue 的原生数据行走，内核无任何 mirror/shadow/feed-source 表与特判。寄生插件在引擎眼里就是一个自带 (venue, token) 行、自有盘口、自有成交的普通 venue。
- **G2（API 层复用）**：「寄生」降级为 API 数据面的声明——`feed_source`。API 层多路复用一条上游 feed/discovery 流给多个消费 venue，各注册各的 (venue, token) 行。
- **G3（双键标的）**：token 表按 `(venue, token)` 双键存行。同字节 token 在两个 venue 下各有市场行 / 盘口 / 持仓 / 成交，互不污染，插入次序无关。

**非目标**：

- 不退役通用多 venue 结算隔离——#653 的双 venue 请求分桶、take-once、无归属 journal replay 回归全部保留（§五.3 只清寄生专属半边）。
- 不动 RFC-645 全部成果（策略身份声明化，M1/M2 已落）。
- 不动 M3-ARB_STRATEGY 分流点的去名化（RFC-645 §四 M3，独立另行）。
- 不扩 `BkStrategyVtable` 槽位、不改 C 结构布局——冻结的是 `user_layer/strategy_api` 的 ABI（`BK_ABI_VERSION=2`），**不是** Rust `market_api` trait；本 RFC 的 `feed_source` 落在 Rust trait 上，与 ABI 冻结无关。
- 不改面板账户+venue 过滤语义（violation 2 的修法是让行自然存在，不是改滤网）。

## 三、裁决（2026-10-27，操作者）

1. **寄生是 API 层的事**：镜像（寄生）是向 API 申明的属性和需求；数据不在撮合（OME）层实现，寄生在 API 层实现。
2. **OME 盲是硬约束**：OME 必须无法分辨寄生与原生的区别；只有插件自己知道自己是寄生，其他部分（引擎、账本、风控、分发）必须无法感知寄生插件与原生插件的区别。
3. **迁移方向**：M1 `(venue, token)` 双键独立标的 → M2 API 层 feed 复用（`feed_source` 声明 + 一条上游流喂多 venue）→ M3 拆引擎 mirror/shadow/显示合成 → M4 结算寄生补偿清理。
4. **#653 只退役寄生专属部分**：通用多 venue 结算隔离保留。

## 四、目标形态

### 4.1 API 数据面：`feed_source` 声明与多路复用

- **声明**：`MarketPlugin` 新增 trait 方法 `fn feed_source(&self) -> Option<&str> { None }`（尾随默认实现，现有插件零改动编译）——「我吃谁的流」是插件向 API 申明的**数据拓扑需求**：值为上游插件名（如 polysimulator 声明 `Some("polymarket")`）。
- **复用管道**：API 数据面（host 层）读到 `feed_source` 后，**不启动**该插件自己的上游连接，而是把被源插件（feed 源）拉到的一条上游 feed/discovery 流**多路复用**给所有声明消费它的 venue。每个消费 venue 在引擎里注册**自己的** `(venue, token)` 市场行与盘口行——同一字节 token 在表里出现两份，各标各的 venue。
- **上游组件生命周期**：只启用消费 venue（源 venue 未启用）时，API 层仍启动必要的数据组件（feed / discovery），但**不启用**上游交易执行——数据消费与执行启用解耦。
- **接入面变化清单**（M2 落地时逐项核对）：Rust trait `feed_source`；venue-qualified ingress（`on_book` / `on_books` / `on_top_of_book` / `on_round_markets` 的批次按申报 venue 归属，见 §4.2）；venue-qualified query（token→市场查询全链带 venue 限定）；Lua / C ABI / IPC 消费者的迁移对照表。公共语义不得以内部重构名义暗改。

### 4.2 OME：只见原生插件

- **受理**：`scope_admits_token` 删除镜像臂，只读本 venue 自己的 (venue, token) 行——绑定所的 token 候选在它自己的行里，天然受理。
- **路由**：`execution_venue_for` 删除镜像覆盖臂，token 表行 venue ≡ 执行 venue（数据进来时就归属对了），双账户各达各的 executor 由「行本身按 venue 分立」保证，无需覆盖逻辑。
- **成交归属**：入场/持仓/成交的 venue 戳读本行 venue，表行 venue ≡ 执行身份，service.rs:10316 的 `execution_venue_of` 修正性覆盖自然退化为普通读数。
- **账本/结算**：无寄生重 bucket——position.venue 戳开仓时即为本 venue，#653 的通用分桶按本就正确的 venue 归属工作。
- 内核白名单（本 RFC 同步建立该主题的微内核边界）：**留内核**——撮合、账本、硬风控、生命周期、分发、跨市场强一致不变量；**出内核**——具体来源关系（谁镜像/复用谁）、token→venue 解析、流复用。前者是交易正确性，后者是数据拓扑。

### 4.3 token 表双键化语义

- **键**：`(venue, token)` 独立标的。`scanner` 表 / `market_for_token` / `venue_of` 的 token-only 查找全部 venue 化；同字节 token 在两个 venue 下**各有**市场行、盘口（book 快照各归属各行）、持仓、成交行，互不污染。
- **插入次序无关**：PM 先到还是 sim 先到，两所各得各的行——#638 前的「先到者占行、后者被遮蔽」从根上消失，#627 的 fresh-empty merge 吞行路径不复存在。
- **token-only 旧调用兼容语义**：仅在**唯一候选**时解析（全表只有一行含该 token）；歧义（两个 venue 各有一行）明确拒绝返回错误/None，**绝不按插入顺序首中**。单 venue 部署与旧语料行为保持。
- **隔离清单**：同 token 不同盘口读数、不同到期时间、不同结算结果互不污染；重连、退订、轮钟换轮、撤单均按 venue 各自进行，不跨 venue 串扰。
- **跨所策略**：跨所套利继续走显式 listing/leg 身份（#427 mapping、entry_pair 声明），**不将相同 token 自动等价**——字节相同只是巧合事实，不是市场等价声明。

### 4.4 显示面：自然行替换合成行

- `shadow_markets` 显示簿删除（它存在的唯一原因是 token 唯一键遮蔽）。
- `books_view` 两条合成臂（mirror_pairs 遍历 + shadow 批次）删除——每个 venue 的行自己就在表里，自然装配。
- `ENGINE_ROUND` 行情合成装配删除——同理。
- 原 #629 想修的症状（原生市场行的 round/books 显示）由自然行生成：round_labels（#648 A2）、is_live_round_row 族钟过滤照旧工作。
- 面板滤网（账户 + venue）不改——行自带正确标签，滤网天然命中。

## 五、可证伪验收

Issue #661 的 8 条验收全数照录，另补真线探针验收点。M1–M4 各自 PR 必须携带对应断言；**不能以绿 CI 替代行为断言**。

1. [ ] 字节相同的 token 在 PM 与模拟 venue 下各有市场行、盘口、持仓和成交归属，插入次序无关。（→ M1；隔离模拟：先 PM 批后 sim 批、先 sim 批后 PM 批双向喂，两所行数/盘口/归属对称）
2. [ ] API 数据面只启动一条上游 feed/discovery；给每个消费 venue 注册自己的市场和盘口。只启用消费 venue 时仍启动必要上游数据组件，不启用上游交易执行。（→ M2；连接计数断言）
3. [ ] Engine/OME 无 `mirror_of`/feed_source 表和特判，不从 feed 来源推断交易归属。（→ M3；代码级验收：engine.rs 无 mirror_of/shadow_markets 字段与方法，`scope_admits_token`/`execution_venue_for` 无镜像臂）
4. [ ] 账户绑定、订单、持仓、成交、结算始终按本 venue；跨所套利通过显式 listing/leg 身份，不将相同 token 自动等价。（→ M1–M3 全程；既有跨所测试保持绿）
5. [ ] token-only 旧调用仅在唯一候选时解析；歧义明确拒绝，绝不按插入顺序首中。单 venue/旧语料行为保持。（→ M1；单 venue 回归全绿 + 双 venue 歧义拒绝新钉）
6. [ ] 两个 venue 同 token、不同盘口/到期时间/结算结果互不污染；重连、退订、轮换和撤单也不跨 venue。（→ M1/M2；隔离模拟喂不同 expiry/盘口的双流断言不串）
7. [ ] 原 #629 显示合成删除后，原生市场行自然生成 round/books；面板账户+venue 过滤不改。（→ M3；wire 级断言 books_view/round 行由自然行装配）
8. [ ] #653 双 venue 请求隔离、take-once、旧无归属 journal replay 回归保持通过。（→ M4 全程回归，`settlement_queues_drain_per_venue_bucket` 等既有钉不动）

**真线只读探针验收点**（M3 落地后的真线验收，不启动 Live、不改运行账户配置）：

- **engine.books 行数按 venue 计数**：`(polymarket, N_polymarket)` 与 `(polysimulator, N_polysimulator)` 各自非零、各自 ≥ 对方为 0 时的全量——同窗口同 token 两所**各有自己的行**，总行数 = 两所之和（而非旧形态的 N + 0）。
- **trade.venue = 执行身份**：polysimulator 账户成交行 venue ≡ polysimulator（execution 身份），面板 `venue:polysimulator` 滤网筛出行数 ≡ 徽标计数——violation 2 的截图 bug 在自然行形态下不可复现。
- **上游连接数一份**：探针读 API 数据面连接台账——polymarket + polysimulator 双 venue 启用时，上游 feed/discovery 连接数 ≡ 单 venue 启用时的份数（复用生效），且无第二条上游轮询流。

**隔离模拟双 venue 语义验收**（记录版本/数据身份）：模拟环境下双 venue 各喂独立批次序列（不同盘口深度、不同 expiry、不同结算裁定），断言全链无串扰（§五.6）。

## 六、迁移序

| 步 | 内容 | 触碰面（预期） | 验收锚点 |
|---|---|---|---|
| **M0** | 本 RFC（决策文档）。合入后实现 PR 从「提案」转「执行」 | docs only | — |
| **M1** | `(venue, token)` 双键独立标的：scanner 表键化、`market_for_token`/`venue_of` venue 化、ingress 按申报 venue 注册行、token-only 查询兼容（唯一候选解析 / 歧义拒绝）。**先铺兼容入口，最终歧义拒绝** | scanner.rs、engine.rs（表与查询面）、model.rs、ingress 路径 | §五.1/5/6；单 venue 回归全绿 |
| **M2** | API 层 feed 复用：`feed_source` trait 方法 + 复用管道（一条上游流喂多消费 venue、各注册各的行）；数据组件生命周期解耦执行启用；Lua/C ABI/IPC 消费者迁移对照 | market_api（trait）、market/host.rs、ipc/server.rs（启用路径）、polysimulator（声明改造：mirror_of → feed_source） | §五.2/6；连接数一份断言 |
| **M3** | 拆引擎特例：mirror 表 / `set_venue_mirror` / `apply_plugin_mirrors` / `scope_admits_token`·`execution_venue_for` 镜像臂 / `shadow_markets` / books_view 与 ENGINE_ROUND 合成臂全删 | engine.rs、service.rs、ipc/server.rs | §五.3/7；真线探针三点 |
| **M4** | 结算寄生补偿清理：#653 中寄生专属的归属修正退场（通用双 venue 隔离 / take-once / journal replay 保留并回归） | settlement.rs、position.rs、service.rs（结算链寄生半边） | §五.8 |

M1 与 M2 无循环依赖可先后独立开 PR；M3 依赖 M1+M2 稳定（引擎特例只有在「行已分立 + 复用已生效」后才可拆，否则显示/受理断流）；M4 收尾。每步单独 Issue/PR，可回滚。

## 七、影响面清单

**退役（deletion over addition）**：

- engine `mirror_of` 表、`set_venue_mirror`、`mirror_source_of`、`mirror_pairs`（engine.rs:456/692/703/710）与 boot 推送 `apply_plugin_mirrors`（service.rs:4664）+ 运行时推送 `enable_venue_at_runtime` 的 mirror 半边（ipc/server.rs:478）。
- `scope_admits_token` / `execution_venue_for` 的镜像臂（engine.rs:787/2151）。
- `shadow_markets` 显示簿及 `shadow_markets_for`（engine.rs:506/861）、merge 臂入簿路径（engine.rs:1148）。
- `books_view` 两条合成臂（service.rs:8904 mirror_pairs 遍历 + shadow 批次臂）、`ENGINE_ROUND` 行情合成装配（ipc/server.rs:16928 一带）。
- `MarketPlugin::mirror_of` 声明（plugin.rs:283）与 polysimulator 的寄生自认。
- #653 中寄生专属的重 bucket 半边（修正性 `execution_venue_of` 归属补偿；通用分桶保留）。
- **#627 的「token 全局唯一一份行」不变式**（被 §4.3 双键取代）；**#638 的 engine-mirror 合同**（被 §4.1/4.2 取代）。#652 按旧合同已正确结案（issue 已关），不重开、不回滚。

**保留（逐条回归保护）**：

- #653 通用部分：双 venue 结算请求隔离（per-venue 分桶排水）、take-once dispatch 语义、空归属 legacy claim 的 journal replay 兼容。
- #647 结算赎回/合并队列的 venue 维度本体。
- RFC-645 全部成果（策略身份声明化、pair-book 统计声明、on_config JSON 投影）。
- M3-ARB_STRATEGY 去名化——RFC-645 §四 M3，独立另行，不在本 RFC 波次内。
- #648 的 round_labels 声明化、credential env 声明化等插件自申报成果。
- 面板账户 + venue 过滤语义、`is_live_round_row` 族钟谓词、2 轮 observe guard 等显示面通用机制。

**新增**：

- `MarketPlugin::feed_source(&self) -> Option<&str>`（尾随默认 None，现有插件零改动）。
- API 层复用管道：feed 源判定、多路复用、per-venue 行注册、数据组件生命周期（feed/discovery 与 executor 解耦启用）。
- token 表双键 + venue-qualified ingress/query 兼容入口与歧义拒绝路径。

## 八、微内核白名单（本 RFC 同步建立）

撮合/账本/硬风控/生命周期/分发与跨市场强一致不变量**留内核**；具体来源关系、token→venue 解析与流复用**在 API 适配层**。判定一句话：前者是「按什么规则成交与记账」（交易正确性），后者是「数据从哪来、怎么省」（数据拓扑）——#638 把拓扑知识搬进了裁决路径，本 RFC 把它搬回去。

冻结边界重申：冻结的是 `user_layer/strategy_api` 的 `BkStrategyVtable` / `BK_ABI_VERSION=2` 与 C 结构布局——**不是** Rust `market_api` trait。`feed_source` 是 Rust trait 的尾随默认方法（与 #627 `supported_round_secs`、#648 `round_labels` 同模式），不扩 C 槽位。

## 九、风险与反例

1. **歧义拒绝的过渡期裂缝**：M1 铺兼容入口时，token-only 调用在双 venue 同 token 下若静默首中，会复活 #638 前的「先到者占行」——钉死：唯一候选才解析，歧义拒绝有测试钉（§五.5），兼容期不豁免。
2. **复用管道的单点**：一条上游流喂多 venue，上游断则多 venue 同时无数据——这与今日单 venue 断流同构（寄生形态下本来就共享同一份流），不引入新可用性等级；但 API 层必须把「源断」按消费 venue 各自的口径上报（各 venue 的连接状态读数独立），不能让消费 venue 显示源 venue 的健康度。
3. **结算归属的 legacy 行**：#653 的空归属 journal replay 兼容必须全量保留——M4 只删寄生专属半边，删后旧无归属行仍走 fallback 排水，回归钉（§五.8）不许动。
4. **撤回条件**：若 M1/M2 落地后发现双键表使上游多路复用失去意义（例如所有消费 venue 都要求独立上游），本 RFC 退化为「仅双键 + OME 盲」，M2 管道不建——但 §五.3（OME 无特判）不可撤回，那是操作者裁决的硬约束。

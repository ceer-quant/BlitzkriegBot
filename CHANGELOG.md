# Changelog

All notable changes to BlitzkriegBot are documented here.
Format: [Keep a Changelog](https://keepachangelog.com); versioning: semver.

## [Unreleased]

## [0.4.1] - 2026-10-25

0.4.0 → 0.4.1 的 ~110 条提交，按主题归并。六大主线：**多账户体系**（账户簿
运行时增删 + 绑定即启用 + 真实记账钱包 + 策略级虚拟账户 + kill switch 按账
户 + 显示账户指针）、**跨所套利**（统一映射账本 + 原子双腿 + 配置化弹窗）、
**轮次多族**（(venue,duration) 族钟全链 + 账户轮次集热管理）、**插件版图**
（Kalshi 真线重建 + PolySimulator 模拟盘 + 多插件数据面 + API 凭据）、
**风控与退出**（trailing 止盈归策略自声明）、**面板与运维**（doctor 探针 +
蓝图导入 + env 唯一可信化 + 网络诊断跟随账户）。

### Added

- **多账户体系**：账户簿运行时增删（`account.create`/`account.remove`，运行
  时铸造账户带注资与 ledger journal 路径，#516/#517/#521/#522）；每账户独立
  启停与持仓账户维度 + 行情跟随绑定（#532）；多账户同时跑策略扇出 + 账户交
  易开关 + 策略分配编辑器（空=不跑，废除 INHERIT，#518/#519/#523）；绑定即
  启用闭环——插件页转只读、boot 启用集从账户绑定派生（#508/#510/#512）；
  真实记账钱包——每账户现金流水 journal + 持久化回放 + 赔付归属 + 面板现金
  流水卡（#509/#511/#513）；显示账户指针配置化（`display.account.set` +
  快照字段 + 账户维重置/余额跟随单源化，#561/#562/#571）；账户级急冻簿 +
  `account.freeze`（#530）；策略级虚拟账户——(account, strategy) 配额簿 +
  配队双腿原子预占 + 终态精确释放（#528）；升级不再覆盖 accounts.toml/
  shadow_evolution.toml（操作员状态运行副本永远赢，#565/#581）。
- **跨所套利**：统一映射账本——自动配对只提议、操作员 confirm 才生效 +
  candidates/confirm/revoke IPC + 回灌（#501）；策略级虚拟账户配队双腿原子
  预占（E-VA）+ `atomic_batch` 接线 + `--virtual-limit` 旗标 + 单腿裸奔看护
  （#528/#531）；跨所套利创建弹窗 v2——配置意图模型、双腿双账户、币对多选、
  凭证门与内核 may_trade 同形（#569/#570/#576）；策略页套利 tab + 套利收益
  合计 + gate 四计数切账户（#577/#579）；凭证轮换运维语义与 RFC 决议落文档
  （#544/#554/#566/#574）。
- **轮次多族**：账户可订阅 (venue, duration) 轮次族集合——accounts.toml
  rounds 声明 + 白名单 fail-closed + `account.rounds.set` 热管理动词（增族即
  挂/移除敞口拒绝/软卸 poll 丢弃，#586/#589/#590/#591/#599）；scanner/入场
  门/rollover/slot 判定全链按族（#598/#600）；面板轮次周期集多选卡 + 周期切
  换入口挪行情页倒计时卡 + 瓦片身份标注（#593/#596/#597/#601/#602）；插件自
  声明轮次族 `supported_round_secs`——面板按声明筛档 + kalshi 14400 静默盲点
  补告警（#604/#605）。
- **插件**：PolySimulator 模拟盘插件全链路（SIMULATED 能力位 + 查询式身份解
  析 + 双腿成交归属，#454/#455/#480）；Kalshi OrderExecutor（cancel-first/
  timeout→reconcile/孤儿清扫，#479）与 predictfun OrderExecutor（EIP-712 签
  名下单，#484）——三组件合同全部收口；插件页开启/停用（extension 启停即时
  生效 + 行情插件 next_session 切换，#486）；多插件并发行情数据面（多 venue
  同时喂数 + feeds per-plugin，#502）；行情插件 API 凭据（`market.credentials.
  status/set` + 凭据卡，#500）；executor per-venue 路由 + PluginModes 多值 +
  账户↔venue 绑定（#505/#506）；`blitzkrieg doctor`——环境探针读数面子命令
  （NTP 时钟偏差/tick 断流/spot 失效，#529）；盘口批次原子化（整批同瞬无撕
  裂）+ 书视图前 10 档挂单与 `bk.vwap_for_size`（#524）。
- **面板**：网络诊断跟随显示账户——探针走操作员正在看的 venue，绑定 venue
  无插件时诚实报 `unsupported`，网关缓存带 venue 维度切户即失效（#612）；蓝
  图编辑器导入 JSON 粘贴闭环 + `in` 操作符成员逐行编辑（#610）；策略启用复
  合键读数修复 + 启停即时反映（#552/#553/#558/#563）；首页全内容跟随账户
  （权益曲线账户×venue 过滤 + 轮次钟身份链 + 引擎计数「进程级」诚实标注 +
  真实余额读账户切片，#585/#592/#608）；挂单强平三端（#477）；交易环境重置
  （#475）；轮次时间周期可设置（#474/#499/#503）；网关 restart 动词 + 10s
  自动重启（#488）；策略 tunables 编辑 + 跨所配对监控（#482）；K 线卡按市场
  类型门控（#495）。
- **发布工程与配置**：发布包与编译包**完全等效**（#491）+ webui 预编译解压
  即启动（#489）；env 唯一可信化 P1——配置面盘点判决书 `docs/ENV_MIGRATION.
  md` + `.env.example` 零漂移对齐 + 4 个死键退役（POLYMARKET_API_*/HFT_CORE，
  #588/#607）；Windows WSL2 安装指南（#467）；全策略评审报告 0.4.1（#473）。

### Fixed

- **风控**：PolySimulator 场地 429 按 Retry-After 重试——启动突发自愈，不再
  把瞬态限速当成能力失败触发交易冻结/丢余额种子/行情卡顿；自检探针失败原因
  进冻结行（#611）；trailing_enabled 升格 `risk.setExit` 热写 + `trailing_
  exit` 策略自声明三态面（manifest 键，spread_arb 声明 true——出场教义归策
  略不归全局开关，#603/#606）；TP maker 退出单挂买一上方一档——post-only 结
  构性拒单根治（#496/#497）；资金穿透四路封堵（#461/#468）；订单状态机三缺
  陷（#462/#469）；回撤净值口径 + DRY post-only 对齐 + risk.ladder 真接线
  （#464/#471）；过期幽灵订单根因——ForceClosed 终态 + `orders.force_close`
  （#457/#472）。
- **插件与行情**：Kalshi 插件对真线重建——series 发现 + `orderbook_fp` 解析
  + 词边界 + 轮终防线，真线只读验收 30/30（#494）；策略兼容性握手收敛——判
  据收敛为 market_type + 结构两条，能力位退出判定，boot 棘轮不再抹持久格
  （#493）；迟上架轮内补查 + 同 slot 增量合并——币对中途缩减根治（#498）；
  轮次长度持久格抬到 TOML 之上——面板切换形同虚设根治（#499）；行情快照按
  到期 slot 过滤 + (venue,asset) 唯一化——行情页重复币对行根治（#560/#567）；
  PM 盘口批路径归档/探针双盲区（#548/#555）；运行时挂载把 boot discovery
  config 传给插件（#536）；kalshi daily 族禁用（行权阶梯非二元）+ polymarket
  1h/daily ET 命名族 slug 解锁（#583/#584）；行情页账户切换重绑定 + 倒计时
  per-venue 重臂（#551/#572）。
- **面板与运维**：长时间空闲后假死——IPC 客户端 64 行跳过上限移除（#476/
  #478）；supervisor 放弃重启告警的 stderr 尾巴竞态（#470）；套利弹窗 dry 豁
  免与执行账户（#539/#546/#576）；审计泄漏三连修 + 空行情轮次判因（#537）；
  建户即挂行情数据面（#535）；strategy.enable 默认分支 scoped 写接管（#563）；
  策略开关读代理失败信封——HTTP 200 {error} 不再弹绿色假成功（#564）；进化文
  件唯一可信源——开关写回 toml、废除面板独立态（#578）；账户删除文案汉化
  （#534）；reset 语义收口 + 倒计时卡随所（#556）。
- **安全**：面板认证加固——无凭据写面封死 + token 查询通道废除 + 内核只读
  防御（#460/#465）；持久化加固——原子重写统一 + 损坏 fail-loud +
  close_credentials 跨重启（#463/#466）。

## [0.4.0] - 2026-10-12

0.3.0 → 0.4.0 的 66 条提交，按主题归并。三大主线：**0.3.1 回测对标线**（链上
数据管道 + 三模式回测 + @almach 对标与逐策略重校准）、**0.3.3 跨平台线**
（Kalshi / predict.fun 扩展 + 统一事件映射 + 两腿对冲执行）、**0.4.0 审计线**
（#439–#443 五项审计落地 + 热修复）。

### Added

- **回测对标线（0.3.1）**：链上数据管道（Polymarket Data API 拉取 → 确定性
  JSONL 事件流，activity-ledger 全量成交面 + offset 硬顶分段 + CLOB 元数据
  回退 + 有界并发元数据/价格阶段，#360 栈）；订单生命周期三时间戳
  （decision/execute/report）+ InflightTaker 两段式撮合 + 回测三模式 CLI
  （mine / verify / sweep，#356）；回测校准四钉（轮次节奏闸门 fail-closed /
  UMA invalid 0.5 结算双源 / 费率严格等式 / 语料完整性对账，#354）；校准 A/B
  基准工具（`almach-calib` / `almach-ground-truth` / `almach-overlay`，
  #398）；@almach 全窗对标报告——偏差未过 20% 线，逐项如实归因（#360）。
- **跨平台（0.3.3）**：Kalshi + predict.fun 传输层客户端（认证签名 / 令牌桶
  限频 / 幂等对账键，#429）；两个市场插件以 MarketPlugin 形态接入（价格归一化
  在插件边界完成，opt-in feature，默认构建仍是 Polymarket-only，#430）；市场
  无关的统一事件映射层（#431）；两腿原子对冲执行 `hedge/`——可行性门计入
  双腿费率、500ms 每腿提交超时、裸腿按规模差判定 + 保护阶梯（补单→强平）、
  结算双场地三重对齐（容差 1¢，Mismatched 永不静默记平）、联合风控门（#432）；
  `cross_venue_arb` 跨平台套利策略 + venue 全链路呈现（#433）。
- **策略蓝图**：蓝图数据模型 + Rust 编译链（`blueprint.rs`，#365）；面板画布
  编辑器（@vue-flow/core 8 节点调色板 + Lua 预览 + 内联报错，#370）；手写包
  可被编辑入口打开（`blueprint.loadSource`/`saveSource` + 源码模式，#414）；
  codegen 产出 `bk_evaluate` 桥接 §6.5 运行时（#413）。
- **账户执行策略（0.3.1）**：按账户隔离的下单规则引擎（`execution_policy.rs`，
  #366）+ 执行策略 UI（WebUI + TUI 合入「生效风控」，#369）；策略路径解析改为
  相对安装根而非 cwd（#386）；编译默认不再内嵌规则——缺文件即 NEUTRAL 内核
  （#372）。
- **风控可编辑**：九项系统限额热编辑 `risk.setSystemic` + 面板编辑卡（#415）；
  退出纪律（止损/止盈/强平）可编辑 `risk.setExit` + 面板三行输入（#417）；权益
  百分比封顶覆盖 declared-shares 策略路径（#421）。
- **策略与进化**：`single_leg_pair` 单腿轮次参与策略（#394）；`pair_discount_arb`
  补腿闭环——部分成交的腿不再裸奔到结算（内核策略持仓视图 + 终态释放 +
  GAP-FILL 豁免，#420）；全部 Lua 策略包经 manifest 显式参数域加入影子进化
  （69 knobs / 10 包，#401）；设置/策略页产品化六件套 + Lua 进化审计修复
  （#393/#395）。
- **面板**：信息架构重构——两级导航 + SettingsPage 拆分 + TUI 同步（#383），
  评审轮收平（#384）；策略页重设计——状态分组/重叠根除/三卡 KPI/搜索与详情行
  （#422）。
- **自更新**：端到端更新检查调度器 + 下载暂存状态机（内核永不改写正在执行的
  二进制，WebUI + TUI，#379）。
- **结算与事件流**：链上裁决进入回放事件流（REDEEM 行折叠为 per-condition
  verdict，dry ladder 让位，#378）；MAKER_REBATE/REWARD 台账行成为钱包级
  cashflow 事件且回放入账（#388）。
- **0.4.0 审计线新增**：`bk.*` 投影内容键缓存 + 冻结只读代理（#449）；Args
  快照门禁钉死 parse_args 契约（122 行逐字段 + 竞争测试，#451）；oracle_ruler
  9 臂测量层校验（修后 RULER SOUND，#450）；`common/decimal.lua` 单一十进制
  源 + 逐字节门禁（#446）。

### Changed

- **官方 `spread_arb` 是 Lua 策略，仓库不再附带任何 Rust 策略。**
  `user_layer/strategies_lua/spread_arb/` 是 Rust 参考实现的逐位精确移植
  （移植前在冻结语料经济门禁上逐字复现全部 4 窗基线 `75fd8140`，经济性原样
  承接）。旋钮来自 manifest tunables；`--spread-arb-*` CLI 旗标不达 Lua 策略。
  随包修复两处百分数单位 bug（滑坡门返回分数而非百分数 / tracker `move_pct`
  同病）+ 零阈值拒全部入场的边界；滑坡门默认关（截断签名 = 负期望入场集，
  不足以动出厂默认）。
- **策略重普查（#434，#419 修复后全 11 臂回放）**：`lua_momentum` 退役为
  生产危险品（−$17,575 / PF 0.43——单 tick 阈值确认几乎每 tick 开火；manifest
  改 EXAMPLE ONLY，警告头落 README 与 strategy.lua）；`spread_arb` 重点燃为
  负期望（−$475 / WR 10%）；生产组合 `pair_discount_arb`+`single_leg_pair`
  保持采用默认（$240.31 / PF 1.238）。零信号可行性静态分析 + WR/PF 口径
  定义见 #403。
- **出厂默认校准**：`single_leg_pair` 两轮校准（带 0.55–0.88 + 每轮封顶 10，
  #435；min_gap 0.10→0.30 对齐 @almach leader-gap 域，预注册 H1，#436）；
  `pair_discount_arb`/`single_leg_pair` 第二轮校准采纳（#405）。
- **CLI 严格化（#441）**：内核无法兑现的值是启动错误，绝不静默回默认——
  `--mode LIVE` 拼写大写曾静默跑 dry；未知 flag 早已拒绝，未知 value 现在同罪。
- **执行策略（shrink judge #382）**：缩容裁决按记录自身的 bound stop 度量
  （时间感知止损），不再按配置止损重算。
- **备份与升级**：light 档备份提速到 19.7s（排除可再生 onchain 缓存 +
  档案哈希完整性，#416）；upgrade 的 step-0 `data/` 轻备份删除——data/ 长到
  7.1GB 后该步骤主导升级耗时，而升级从不写 data/（#408）；webui lockfile 比
  node_modules 新时强制重建依赖（#391）；shim 接管 `upgrade` 动词（此前
  文档承诺但从未接线）。
- **回放性能**：deadline 驱动的 nofast 调度 + 免生命周期 `BookView` 退出路径
  （#399）；`--backtest-fast` 门控冻结盘口下冗余的维护工作（#375）；kline
  preview pass 回放门控——只喂实时面板的静默 decoration 不再烧回放时钟
  （#418）；OME 三级索引优化（submitted_at_ms 有序索引 / per-token live 索引 /
  增量维护 live 视图，#371/#373/#374）。
- **面板**：扁平导航评审轮（#384）。
- **依赖**：rustls 0.23.45（RUSTSEC-2026-0285）+ source-map-js 1.2.2
  （GHSA-68fv-2mgg-jv7q）+ rsa 审计豁免记录（#444）。

### Fixed

- **#419 锁顶盘口双侧删除**：`update_top` 用严格边界应用 top 更新，best_bid ==
  best_ask 的合法锁顶状态把共享档位从两侧同时删掉——盘口坍缩成单边、mid 被
  F6 规则扣发、所有 mid 依赖的策略状态机永久停摆（v2 语料 100% top 事件流经
  此路径）。修复后保持两侧；非正入场价 fail-closed 守卫。
- **#438 dry 结算读对侧盘口**：dry 结算梯子只看持仓腿的盘口——对手腿已被
  0.99 坚定报价（市场已定价赢家）时输腿永远悬置、过期后占着 max_positions
  名额。对侧盘口证据关闭输腿。
- **#437 过期轮持仓不再占名额**：生产事故（两个过期 12 天的持仓占满全部
  max_positions 名额 6 天）——过期轮位置在轮终态即释放。
- **重启恢复订单 id 计数器跳过恢复 id**——「order already registered」撞车
  （与持仓恢复同形，#a94dd61c）。
- 面板策略规则编辑按钮静默失效（structuredClone vs reactive proxy，#411）。

### Performance

- Lua `bk.*` 投影内容键缓存 + 冻结只读代理（#449，#440 采样定位的
  luaH_newkey/luaS_newlstr 热点）。

### Removed

- **Rust `spread_arb` cdylib 夹具**（`user_layer/strategies/spread_arb` 与
  随附的嵌套 workspace）；`user_layer/strategies/` 保留为操作员 cdylib 投放点；
  `strategy-dylib-freshness.mjs` 随最后一个 cdylib 夹具退役。
- **升级 step-0 data/ 轻备份**（见 Changed）。

## [0.3.0] - 2026-09-29

### Added
- **BlitzkriegStrategy API 1.0**（原 C ABI v2 更名，`BK_ABI_VERSION` 仍为 2）：
  新增可选符号 `bk_strategy_declare_modes`；`SafeStrategy` 增 `declare_modes()` /
  `on_kline()`（均有默认实现，0.2 构建的 dylib 零改动可加载）（E24 / #330）。
- **策略建议 / 内核裁决分离**：`process_intent` 四层裁决流水线 + `Decision`
  （Approved/Modified/Rejected）+ 审计 `data/audit/intents.jsonl` +
  `intent.audit.tail` / `INTENT_DECISION`（E25 / #331）。
- **系统级风控**：账户级 5 项 + 全局级 4 项限额（**默认全部 0 = 关闭**），
  `apply_physics` 把既有出场纪律随单绑定并落审计；`risk.limits` 返回生效值与来源
  （E26 / #332）。
- **市场三层声明**：`MarketType`（+Margin/Earn/Bot）、`MarketStructure`、
  `MarketCapabilities`；插件与策略双向声明 + 加载期兼容性握手（E27 / #333）。
- **多账户一等公民**：`AccountId` 贯穿订单/持仓/账本/成交/事件；`AccountLedgers`
  隔离；`account.list` / `account.switch` / `account.status`（只能收紧）
  （E28 / #334）。
- **K 线兼容**：`Kline` / `KlineInterval` / `KlineAggregator` / `on_kline` /
  `kline.history|subscribe|unsubscribe` / `KLINE_UPDATE`；面板 `KlineChart.vue`
  （E29 / #335）。
- **Lua 5.4 沙箱**（mlua + vendored）：16MB / 1e6 指令配额、投毒语义、`bk.*`
  只读 API、`strategy.lua + manifest.json + README.md` 分发格式、
  `--lua-strategy-dir`（E30 / #336）。
- 9 个新门禁，全部带 `--teeth` 反向验收并接入 CI（`core-gates`）。

### Changed
- 对外命名统一为 BlitzkriegStrategy API 1.0（旧称进入废弃期，符号全部保留）。
- `apply_physics` 使既有出场纪律（止损价 / 时间退出 / 阶梯）**显式可见**：
  写入裁决审计与面板；触发路径未变。

### Deprecated
- "C ABI v2" 名称（仅名称；无代码删除）。

### Not changed（明示）
- `exit_policy` / `position` / `sim` / `ome` 既有行为；所有既有风控默认阈值；
  `frozen-corpus.mjs` 的 sha256；`exit-economics-check.mjs` 的 `BASELINE`。

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

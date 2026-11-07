# #842 预写验收单（gemini 施工参照 + 我 merge audit 时的清单）

> 交付方：gemini（UI lane）。我方：transport/依赖图验收 + merge audit。
> 基准：issue #842 原文 + 契约冻结 e7f8bbd2。

## A. 依赖图（硬门）

| # | 检查 | 命令/方法 | 通过判据 |
|---|---|---|---|
| A1 | UI 不依赖 core | `cargo tree -p <每个 ui crate> -e normal,dev` | 全部输出无 `blitzkrieg_core`（normal 和 dev 依赖都查） |
| A2 | core 不依赖 UI | `cargo tree -p blitzkrieg-core -e normal,dev` | 无 ui_kit / ui_kit_panel / webui 任何 UI crate |
| A3 | 无手写 DTO 重复 | `grep -rn "JsonRpcRequest\|JsonRpcResponse" ui/ --include=*.rs -l` | 全部来自 bk_contract or generated 客户端工件；旧本地 schema 模块应已删除或只剩 re-export |
| A3b | TS 类型来源 | ui/webui 的 TS types 文件头 | 从冻结 schema 生成（注明生成器命令）或 schema 校验器；不是手抄 |

## B. Mock IPC Server（硬门）

| # | 场景 | 通过判据 |
|---|---|---|
| B1 | mock 零内核启动 | mock server 不链 blitzkrieg_core（`cargo tree` 同 A1判据）；golden 族 01-23（含新增 `positions.unverified_exposure` 族 23）在 mock 上全走一遍（fixture 直读 wire golden fixtures） |
| B2 | 5 场景 fixture | success / stale revision / reconnect / malformed payload / RPC error，每场景 ≥1 集成测试 |
| B3 | 方法覆盖 | 至少 account list、market snapshot、order lifecycle、risk/freeze state、reconnect |
| B4 | TUI/web 共用 fixture | TUI 与 webUI 契约测试跑同一套 fixture（同一目录/同一生成物），双跑都绿 |

## C. 行为保持（边界）

| # | 检查 | 通过判据 |
|---|---|---|
| C1 | 视觉/行为零变化 | 截图对比或手工走查声明；无视觉重设计 commit |
| C1b | 无意外契约变更 | PR body `Contract change: none` + diff 实核：bk_contract/schema/golden 零修改（只允许 UI 侧消费端适配） |
| C2 | a11y/现有 UI 门禁绿 | panel-check 等车道全绿 |
| C3 | INTERFACES.md | §2 IPC 方法清单与 mock 覆盖列表一致 |

## D. PR body 完整性

- 依赖图 before/after、schema 适配表、最大模块/文件计数、精确测试总数、`Closes #842`、`Contract change: none`。

## 已知好样例（gemini 可直接抄）

- IPC 方法覆盖样例：`positions.unverified_exposure`（#1000 刚合，golden 族 23，{version, report} reply shape）——mock fixture 直读 golden fixture 文件
- additive wire field 样例：entry `source` 字段 `#[serde(default)]`（#1000）
- 依赖图门测试写法：#840 PR1 的 lane-planned CI 依赖方向守卫（#844）已有先例

## 我方 transport 验收步骤（merge audit 时执行）

1. cargo tree A1/A2 复验
2. mock server 零 core 链接复验
3. golden 23 族 × mock 全通过复验（本地复跑 mock 契约测试）
4. schema 适配表 vs diff 实核（无手抄 DTO 残留）
5. panel-check + a11y 绿 → squash-merge → #842 关 → #839 epic 关 → CHANGELOG 0.4.2 日期定稿 + tag
6. 全绿后 v0.4.2 tag + gemini worktree 剩余波（#936 mobile i18n）收尾走独立 PR

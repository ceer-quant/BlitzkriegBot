# AI 协作工作流（AI Workflow）

> 本文件规定 **人类** 与 **AI 代理** 在本仓库中的协作方式、署名约定与不可逾越的硬约束。
> 适用对象：ceer_quant 团队、外部贡献者、以及任何自动化编码代理。

---

## 1. 角色与责任

| 角色 | 职责 | 出口权限 |
| --- | --- | --- |
| 人类指挥官 (ceer_quant) | 设定目标、批准高风险变更、做最终决策 | 合并 / 启用 Live |
| AI 代理 | 分析、实现、验证、写证据、提交到功能分支 | **仅功能分支**，不得直推受保护分支 |
| 评审者 | 复核 diff、门禁与证据 | 批准 PR |

**核心原则**：AI 可以 100% 完成"分析 → 实现 → 验证 → 提交证据"，但**涉及资金、凭证、分支合并的决策永远由人类完成**。

---

## 2. 🔴 硬约束（违反即失败，不可协商）

以下约束对每一次 AI 会话都生效，必须逐条遵守：

1. **禁止启用 Live 交易**。可在 DRY / 离线确定性回放中验证，不得让任何改动把系统切到真实下单。
2. **禁止修改真实凭证、私钥、API Key**。不得读取、回显、改写 `.env` 或任何密钥文件。
3. **禁止删除任何未经备份的文件**。删除前必须确认有备份或有版本控制可恢复。
4. **禁止在未验证的情况下提交到主分支**。所有改动先落功能分支，通过门禁与评审后再合并。
5. **禁止"顺手优化"业务逻辑**。只做被要求的改动；发现无关问题 → 记录为 Issue / 写入 `docs/DECISIONS_PENDING.md`。
6. **不确定的分歧写入 `docs/DECISIONS_PENDING.md`，不要询问用户**（托管执行时），也不得自行拍板高风险项。
7. **提交前必须通过门禁**（见 §4）。门禁不过 = 不允许提交。

### 2.1 已授权的长期规则（用户明确授予，逐字记录）

8. **【2026-09-14 用户授权】今后只要全部门禁通过，允许 AI 自行重启 dry 内核。**
   - 适用范围：**仅 DryRun 模式**的 `blitzkrieg-core` 内核（node 外壳拉起的那个）。
     **绝不适用于**：Live 模式（任何情况下都不启用）、node 外壳本身、或其他任何进程。
   - 前置条件（缺一不可）：§4 门禁全绿（`cargo build --release`、`cargo test`、
     `npm run typecheck`、`npm run build`、`scripts/secret-scan.sh`、
     `node scripts/cycle-check.mjs`），且合入 `main` 的代码 CI 绿灯。
   - 执行方式：`SIGTERM` 旧内核进程即可——node 外壳（`BlitzkriegCoreClient`）
     `autoRestart` 默认开启，约 1 秒内以仓库根 `target/release/blitzkrieg-core`
     （新二进制）原参数自动拉起。重启前先快照 `data/` 运行数据（只拷贝不删除），
     重启后核对 `run.log` 启动行 `exit tuning: stop_loss=…` 与 `health` 端点。
   - 首次执行记录：2026-09-14 18:01，旧二进制（SL50 时代，inode 12,884,112）→
     新二进制（SL12/trail8，12,951,728），新 PID 12189，见 D-10 结案与
     `docs/reports/HFT_OPTIMIZATION_REPORT.md`。

9. **【2026-09-14 用户授权】7×24 持续开发：AI 可连续推进，每日汇报一次即可，无需逐项等待确认。**
   - **含义**：按 `ROADMAP_INSTITUTIONAL.md §5` 的优先级自行选取下一项，完成一个增量后**直接继续**
     下一个，不必在中途停下征求批准；每个自然日输出**一份**汇报（当日完成项 / 证据 / 门禁结果 /
     下一项计划 / 待决项）。
   - **不放松任何既有约束**：§2 的 7 条硬约束、本节的 dry-重启规则、分支模型与 §4 门禁**全部继续生效**。
     即：仍然只能推功能分支、必须门禁全绿 + CI 绿后才合并、仍然禁止 Live / 改凭证 / 未备份删除 /
     顺手优化业务逻辑。
   - **必须继续停下来问的只有两类**：§2.6 的**高风险拍板项**（改真实资金/凭证/风控阈值等），
     以及**超出授权范围的破坏性操作**。其余不确定项一律写 `docs/DECISIONS_PENDING.md` 并继续推进。
   - **长时间无人值守的前置条件**：`scripts/soak-health.sh` 零 token 巡检须保持运行
     （launchd/定时任务），异常由它报警；AI 不依赖"被唤醒"来发现问题。
   - 首次执行记录：2026-09-14，存量事项见 `docs/DECISIONS_PENDING.md`；当日汇报模板见 §2.2。

---

## 3. 分支模型

| 分支 | 用途 | 保护 |
| --- | --- | --- |
| `main` | 发布分支，始终可部署 | ✅ 需 PR + 1 评审 + 门禁全绿；禁止直推/强推 |
| `develop` | 集成分支 | ✅ 需 PR + 门禁 |
| `feature/*` | 新功能 | 无 |
| `fix/*` | 缺陷修复 | 无 |
| `chore/*` | 构建/文档/治理等杂项 | 无 |
| `release/*` | 发布准备 | ✅ 需 PR |

命名：`feat/<简短描述>`、`fix/<简短描述>`、`chore/<简短描述>`
（本仓库历史亦使用 `feature/…`、`fix/…`，两者等价。）

**AI 只能推送到功能分支**；`main` / `develop` / `release/*` 受分支保护规则拦截。

---

## 4. 提交门禁（Definition of Done）

一次改动"完成"的充要条件是以下全部通过，并附证据：

- [ ] `cargo build --release`（工作区）
- [ ] `cargo test`（工作区）
- [ ] `npm run typecheck`
- [ ] 涉及下单链：`node scripts/cycle-check.mjs`（DryRun 订单链校验）
- [ ] `bash scripts/secret-scan.sh`（无凭证泄漏）
- [ ] CI 三个作业全绿：`rust-check` / `node-check` / `secret-scan`

> 说明：`rustfmt` 与 `clippy` 当前为**建议性（advisory）**检查——既有代码尚未格式化干净，
> 详见 `docs/DECISIONS_PENDING.md` D-6。清理完成后将转为阻塞。

---

## 5. 作者署名

- **唯一作者署名**：所有提交的 Git 作者一律为 `ceer_quant <ceer_quant@users.noreply.github.com>`。
- **禁止任何 AI 署名**：提交信息中**不得**出现 `Co-authored-by:`、`Generated with`、`🤖` 等
  将代理/模型列为共同作者或生成者的标记；不写模型名、不加 AI trailer。
- 人机协作的事实通过正常的评审记录与 PR 讨论体现，**不在提交署名中体现**。

示例提交信息：

```
feat(core): add shadow-evolution re-anchor after apply

...
```

- AI 生成但**尚未人工复核**的改动打标签 `ai/authored`；人工复核后改 `ai/reviewed`
  （这是评审状态标记，不是署名）。
- PR 模板中的「AI 参与说明」区块只用于写清复核范围，**不**用于署名。

---

## 6. 证据与可复现性

- 任何"结论性"改动（策略、影子进化、风控参数）必须附**确定性、离线**证据：
  固定随机种子/时钟、无网络、无实时行情。
- 证据文件提交到 `docs/reports/data/`（已被 `.gitignore` 白名单纳管）。
- 报告写入 `docs/reports/<TOPIC>_REPORT.md`，并在 PR 中引用。

---

## 7. Issue / PR 流

1. 任何非平凡改动**先开 Issue**（`task` / `bug` / `feature` 模板），写清验收标准与门禁。
2. 从最新 `develop`（或 `main`，视流程）切出功能分支。
3. 实现 → 自测 → 写证据 → 本地跑全部门禁。
4. 开 PR，填全模板（尤其安全约束逐条勾选）。
5. 评审通过 + CI 全绿 → 人类合并。

---

## 8. 安全红线速查

- 私钥 / API Key / 助记词 / `.env` **永不**进入提交；只允许 `.env.example`。
- 发现疑似泄漏 → 立即轮换凭证 + 走 GitHub Security Advisory（不要开公开 Issue）。
- 仓库启用 **Secret Scanning + Push Protection**（见 `scripts/github/bootstrap-repo.sh`）。

---

_最后更新：2026-09-14 · 维护者：ceer_quant_

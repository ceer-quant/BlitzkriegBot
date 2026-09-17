# GitHub 使用规范与身份信息（GITHUB_GOVERNANCE）

> **本文是仓库在 GitHub 上「怎么用」的唯一权威说明**：身份、远端、凭证、分支、提交、合并、
> Issue/PR、标签、平台限制与常用 API 配方。
>
> 与本文的关系：
> - [`docs/AI_WORKFLOW.md`](AI_WORKFLOW.md) —— **人与 AI 的协作契约与硬约束**（红线在这里，不在本文）。
> - [`docs/DEVELOPMENT.md`](DEVELOPMENT.md) —— **本地开发规范与门禁矩阵**（改代码怎么验，在那边）。
> - 本文只讲 **GitHub 这一侧的规范与事实**。
>
> 适用：接手本仓库的任何人类贡献者与自动化代理。
> 最后核对：2026-09-17（`main` = `4c4b351`，CI 6/6 绿）。

---

## 1. 身份信息（Identity）——先读这一节

### 1.1 仓库与组织

| 项 | 值 |
| --- | --- |
| 仓库全名 | `ceer-quant/BlitzkriegBot` |
| 组织 | `ceer-quant` |
| 可见性 | **private**（`visibility: private`，`private: true`） |
| 默认分支 | `main` |
| 许可 | MIT |
| Issues | 开启 |
| Wiki / Discussions | **关闭**（不用；文档一律进 `docs/`） |
| Projects | 开启（当前未作为流程依赖） |
| 当前账号权限 | `admin`（owner/admin） |

### 1.2 Git 作者署名（硬规范）

**唯一允许的提交作者身份：**

```
ceer_quant <ceer_quant@users.noreply.github.com>
```

- **不修改全局 `git config`**。仓库目录下不要 `git config user.name/email` 落地。
  每个提交用 `-c` 逐次指定，避免污染本机其他项目：

  ```bash
  git -c user.name=ceer_quant \
      -c user.email=ceer_quant@users.noreply.github.com \
      commit -F /tmp/bk_msg.txt
  ```

  推送同理（若需要）：

  ```bash
  git -c user.name=ceer_quant -c user.email=ceer_quant@users.noreply.github.com push ceer <branch>
  ```

- **禁止任何 AI 署名**：提交信息与 PR 正文中不得出现 `Co-authored-by:`、`Generated with`、
  `🤖`、模型名，或任何把代理/模型列为作者、共同作者、生成者的标记。
  人机协作的事实通过 PR 评审记录体现，**不体现在署名里**。
- **历史遗留说明**：2026-09-14 之前的提交里存在其他作者身份（上游 `alsk1992`、本机身份
  `fancer`、`AL` 等）与个别 `Co-authored-by: ZCode` trailer。这些是**既有已推送历史**，
  不改写、不追溯；规范自 2026-09-14 起对新提交生效。

### 1.3 远端（重要：远端叫 `ceer`，不是 `origin`）

```bash
git remote -v
# ceer  https://x-access-token:<TOKEN>@github.com/ceer-quant/BlitzkriegBot.git (fetch)
# ceer  https://x-access-token:<TOKEN>@github.com/ceer-quant/BlitzkriegBot.git (push)
```

- **`origin` 不存在，且被有意移除**（旧的 `origin` 指向上游 `alsk1992/*`，无推送权限 403）。
  见 [`DECISIONS_PENDING.md`](DECISIONS_PENDING.md) D-13.3。
- 因此**所有 git 命令都要显式写 `ceer`**：`git push ceer <branch>`、`git fetch ceer`、
  `git rev-list --left-right --count ceer/main...main`。
  只写 `git push` 会报「没有配置的上游」，**不要**临时加回 `origin`。
- 远端 URL 内嵌了 token。**任何打印 remote 或推送输出的命令都必须脱敏**：

  ```bash
  git remote -v | sed 's|https://[^@]*@|redacted:|'
  git push ceer <branch> 2>&1 | sed 's|https://x-access-token:[^@]*@|redacted:|'
  ```

  这是硬要求：token 一旦进终端记录、日志文件或 Issue 正文就等于泄漏。

### 1.4 凭证（Credentials）

| 项 | 值 |
| --- | --- |
| 路径 | `~/.config/blitzkrieg-bot/git-credentials` |
| 目录权限 | `700` |
| 文件权限 | `600`（已加固，保持） |
| 内容形态 | `https://x-access-token:<TOKEN>@github.com`（单行） |

**提取 token 给 REST API 用**（不打印 token 本身）：

```bash
TOKEN=$(sed 's|^https://||; s|@github.com$||; s|.*:||' ~/.config/blitzkrieg-bot/git-credentials)
echo "token len: ${#TOKEN}"   # 只打印长度，用于确认读取成功
```

使用纪律：

1. **绝不** `cat` 该文件、**绝不**把 token 写进任何被跟踪的文件、**绝不**贴进 Issue/PR/聊天。
2. 任何命令输出若可能包含 token，先过 `sed 's|https://x-access-token:[^@]*@|redacted:|'`。
3. 不得修改、轮换、重建该文件（属 [`AI_WORKFLOW.md`](AI_WORKFLOW.md) §2 硬约束第 2 条）。
   发现疑似泄漏的处理流程见 [`SECURITY.md`](../SECURITY.md)，**不要**开公开 Issue。
4. `.env` 与任何密钥文件同理：不得读取、回显、改写。

### 1.5 `gh` CLI 不可用 —— 一律走 REST API

**本机未安装 `gh`。** 所有 GitHub 操作（查 Issue、建 PR、合并、删分支、改标题正文）
一律用 `curl` + GitHub REST API。常用配方见本文 §11。

> 不要尝试 `brew install gh` 或写依赖 `gh` 的脚本；仓库内脚本一律不得假设 `gh` 存在。

---

## 2. 分支模型与保护现状

| 分支 | 用途 | 规范 |
| --- | --- | --- |
| `main` | 发布分支，始终可部署 | **只经 PR 合入，禁止直推、禁止 force-push** |
| `develop` | 集成分支 | 经 PR；当前实际流程以 `main` 为主线 |
| `feat/*` `fix/*` `chore/*` `docs/*` `refactor/*` | 短生命分支 | 自由推送 |
| `release/*` | 发布准备 | 经 PR |

命名：`<type>/<简短描述>`，小写连字符，例如
`fix/panel-round-header-jitter-roll-mobile-spacing`、`docs/github-governance-handoff`。

**分支保护的真实状态（务必知情）**：组织在 **GitHub Free** 计划上，**私有仓库无法开启**
分支保护、规则集（rulesets）与原生 Secret Scanning / Push Protection——API 实测返回
`403`（要求升级 Pro）/ `422` / `404`。见 D-8。

**这意味着服务端不会拦住你直推 `main`。** 保护完全靠：

1. 本规范与 [`AI_WORKFLOW.md`](AI_WORKFLOW.md) §2 的硬约束（**禁止未验证直推主分支**）；
2. CI 阻塞门（`secret-scan` 等）；
3. 每周全历史密钥深扫（`.github/workflows/secret-scan.yml`，周一 03:00 UTC）。

**因此「CI 会拦住我」是错误假设——它只在 PR/推送之后才跑。**

---

## 3. 提交信息规范（Conventional Commits）

格式：

```
<type>(<scope>): <祈使句摘要> (#<PR号>)

<正文：为什么这么改，而不是改了什么>

<门禁与验证证据>
```

### 3.1 type（与 `.github/pull_request_template.md` 的变更类型一致）

| type | 含义 |
| --- | --- |
| `feat` | 新功能 |
| `fix` | 缺陷修复 |
| `refactor` | 重构，**不改变行为** |
| `perf` | 性能 |
| `docs` | 文档 |
| `chore` | 杂项 / 构建 / 依赖 / CI |

### 3.2 scope（与标签 `module/*` 对齐）

`core`（Rust 内核）· `strategy` · `risk` · `shadow`（影子进化）· `polymarket`（市场扩展）·
`panel`（Web 面板 / UI Kit）· `tui` · `gateway`（Node 外壳）· `ci` · `docs` · `scripts`

### 3.3 摘要与正文写法

- 摘要**用祈使句、写「结果」**，不写「修改了某某函数」。
  好例子：`fix(panel): follow the sign of the cash gap, and stop deleting real trades`。
- **正文解释「为什么」**，尤其是根因。本仓库的既有提交正文普遍是「症状 → 根因 → 修法 →
  为什么不改别的 → 门禁证据」的结构，**请沿用**——它是交接价值最高的部分。
- **一条提交只做一件事**。发现无关问题 → 记 Issue 或写 `DECISIONS_PENDING.md`，
  不要顺手改（硬约束第 5 条）。
- squash 合并时 GitHub 会把 PR 标题作为提交标题、PR 正文各提交信息作为正文
  （仓库设置：`squash_merge_commit_title = COMMIT_OR_PR_TITLE`、
  `squash_merge_commit_message = COMMIT_MESSAGES`）。

### 3.4 提交信息的落盘写法（规避工具链陷阱）

仓库所在的开发环境有一个会拦截「用 shell 写受保护源文件」的钩子（Mimosa），
它对**提交信息里出现受保护文件名**也会误判。稳妥做法：

```bash
# 用 Write 工具把信息写成文件，再 -F 引用，不要在 -m 里内联长文本
git -c user.name=ceer_quant -c user.email=ceer_quant@users.noreply.github.com \
    commit -F /tmp/bk_msg.txt
```

---

## 4. 合并流程（唯一允许的路径）

### 4.1 标准流程

```
① 从最新 main 切分支
② 本地改 + 本地全门禁（见 docs/DEVELOPMENT.md §3）
③ 推 ceer <branch>（输出脱敏）
④ 开 PR（REST API），填全模板，正文写 `Closes #N` 与证据
⑤ 等 CI 全绿（6 项）
⑥ squash 合并
⑦ 删远端分支 → 同步本地 main → 删本地分支（用 diff 复核，见下）
```

### 4.2 合并方式：**squash**

```bash
# PUT /repos/ceer-quant/BlitzkriegBot/pulls/<N>/merge
# body:
{
  "commit_title": "<PR 标题> (#<N>)",
  "commit_message": "<正文>",
  "merge_method": "squash"
}
```

仓库同时允许 merge commit 与 rebase merge，但**规范只用 squash**：主线历史保持
「一个 PR = 一个提交」，便于回滚与二分。

### 4.3 删分支：远端要手动删，本地删前必须复核

仓库设置 `delete_branch_on_merge = **false**`，squash 合并后远端分支**不会自动删除**。

```bash
# 远端：DELETE /repos/ceer-quant/BlitzkriegBot/git/refs/heads/<branch>   → 期望 204
```

本地删分支有个**必须知道的陷阱**：squash 合并会新建一个提交，因此
`git branch -d <branch>` 会以「未完全合并」拒绝——**这是正常的，不代表没合进去**。

**正确的安全复核**是比对内容差异：

```bash
git diff --name-only main <branch>
# 输出为空 = 该分支的内容已全部在 main 中 → 可以安全强删
git branch -D <branch>
```

**不要**为了绕过 `-d` 的拒绝而未经复核就 `-D`；也不要用 `--force` 推任何分支。

### 4.4 合并后的收尾（本仓库的既有纪律）

合并后在 `main` 上**重跑一遍关键门禁**，并用构建哈希证明「测的就是 main 的内容」：

```bash
git fetch ceer && git checkout main && git pull ceer main
git rev-list --left-right --count ceer/main...main   # 期望 "0	0"

# 重建产物，与验证时的哈希比对（哈希相同 = 树一致）
cd ui/webapp/webui && npm run build && ls dist/assets/
```

上一次交付即以此法确认：合并后重建得到**完全相同的哈希**
（`index-B7ZMR_RT.js` / `index-U00YopTr.css`），证明验证结论可迁移到 `main`。

---

## 5. Issue 规范

### 5.1 模板（`.github/ISSUE_TEMPLATE/`）

**空白 Issue 已禁用**（`blank_issues_enabled: false`），必须选模板：

| 模板 | 标题前缀 | 自动标签 | 用途 |
| --- | --- | --- | --- |
| `bug_report.yml` | `[bug] ` | `type/bug`, `status/triage` | 可复现缺陷 |
| `feature_request.yml` | `[feat] ` | `type/feature`, `status/triage` | 新功能 / 改进 |
| `task.yml` | `[task] ` | `type/chore`, `status/ready` | 可验证的工程任务（含 AI 托管） |

三个模板都强制：**影响模块**（下拉，与 `module/*` 对齐）、**安全确认**（不得含真实凭证）。
`bug_report` 还强制「可在 DRY / 离线模式复现」的意识；
`task` 强制写「完成 = 什么可被检验的证据」并内嵌门禁勾选清单。

### 5.2 写 Issue 的要求

- 标题写**结果或症状**，不写猜测的修法。
- 正文必须有**验收标准**：怎样才算完成、用什么命令/数字证明。无法验证的 Issue 不开。
- 贴 `priority/*`、`module/*`、`type/*` 三类标签；进行中改 `status/*`。
- **拿不准的分歧不进 Issue 正文悬置**，写 [`DECISIONS_PENDING.md`](DECISIONS_PENDING.md) 并互链。

### 5.3 现状

- 里程碑 **v0.1 #1 已关闭**（22 个 Issue 全关）。
- 仍**开启**的 Epic：**#57（E8 Web 前端重构）**、**#59（E9 产品化补完）**。
  > ⚠️ 这两个 Issue 是 0.1 之后的既定规划，**不要为了「清理」而关闭它们**
  > （除非其中条目已逐项交付并有证据，关闭时按 §5.4 附证据）。
- 其余 87 个 Issue/PR 全部已关闭。

### 5.4 关闭 Issue 的要求

**关闭必须附证据**：门禁输出、`docs/blitzkrieg/MIGRATION_LOG.md` 章节号、报告文件路径，
或可被第三方复跑的命令。`ROADMAP_V0_1.md` §0 把这条写成了执行约定。

---

## 6. PR 规范

模板：[`.github/pull_request_template.md`](../.github/pull_request_template.md)。必填四块：

1. **描述** —— 做了什么、为什么、`Closes #N`。
2. **变更类型 + 涉及模块** —— 勾选，与标签一致。
3. **🚦 门禁（必须全绿）** —— `cargo build --release` / `cargo test` /
   `npm run typecheck` / 涉及下单链的 DryRun 校验。
4. **🔐 安全与约束（逐条确认）** —— 未开启 Live、未动凭证、未删未备份文件、
   未顺手优化无关逻辑、`secret-scan` 通过、分歧已入 `DECISIONS_PENDING.md`。

外加两块：**🧪 证据**（关键输出、A/B 数字、日志片段）、
**🤖 AI 参与说明**（只写人工复核范围，**不署名**）。

### 6.1 证据要求

- 「完成」必须是**可被第三方复跑验证**的：贴命令与原始输出，不要只贴结论。
- 涉及策略/影子进化/风控参数的结论性改动，必须附**确定性、离线**证据
  （固定随机种子/时钟、无网络、无实时行情），证据文件放 `docs/reports/data/`
  （已被 `.gitignore` 白名单纳管），报告写 `docs/reports/<TOPIC>_REPORT.md` 并在 PR 中引用。
- **不得就安全性作无依据声称**。若安全扫描未得出完整结论，PR 正文应如实写明
  （本仓库既有 PR 的写法：「本条不对项目安全性作任何声称」）。

---

## 7. CI 与门禁（GitHub 侧）

### 7.1 工作流

| 文件 | 触发 | 作业 |
| --- | --- | --- |
| `.github/workflows/ci.yml` | `push` 到 `main`/`develop`、所有 PR | `rust-check`、`node-check`、`panel-check`、`secret-scan`、`notify (wechat)` |
| `.github/workflows/secret-scan.yml` | 周一 03:00 UTC + 手动 | `history-scan`（**全历史**密钥扫描） |
| `.github/workflows/security.yml` | `main` push、PR、周一 09:00 UTC | `Security Audit`（npm audit，**建议性**） |

### 7.2 六个必绿检查

合并前**全部**要绿：

| 检查 | 内容 | 阻塞 |
| --- | --- | --- |
| `rust-check` | 两个嵌套 workspace 的 cdylib 构建 → `cargo build --release --workspace --locked` → `BK_REQUIRE_DYLIB=1 cargo test` | ✅ |
| `node-check` | `npm ci` → `typecheck` → `test` → `build` | ✅ |
| `panel-check` | 构建网关+内核 → 构建 Vue 包 → `npm run check` + `check:all` → `npm run ui:webapp` | ✅ |
| `secret-scan` | `bash scripts/secret-scan.sh`（零依赖，扫工作树） | ✅ |
| `notify (wechat)` | 汇总结果推企业微信（未配 `WECHAT_WEBHOOK` 时安全跳过） | — |
| `Security Audit` | npm audit / audit-ci | ⚠️ **建议性**（D-9） |

`rust-check` 中另有两条**建议性**：`cargo fmt --check` 与 `clippy -D warnings`（D-6）。

> **别把建议性当绿灯。** 建议性步骤仍会每次运行并打印报告；PR 应说明新增债务是否为零。

### 7.3 超时与并发

`ci.yml` 设了 `concurrency: ci-<ref>` + `cancel-in-progress: true`：
**同一分支连推会取消上一次运行**，不要以为「没看到失败」就是通过——确认最新那次跑完了。

---

## 8. 标签体系（44 个，六个维度）

源文件 [`.github/labels.yml`](../.github/labels.yml)，应用脚本 `scripts/github/bootstrap-repo.sh --labels`。

| 维度 | 取值 |
| --- | --- |
| `priority/*` | `P0`（资金/安全，立即）· `P1`（本迭代必做）· `P2`（计划内）· `P3`（锦上添花） |
| `status/*` | `triage` · `ready` · `in-progress` · `blocked` · `review` · `done` |
| `type/*` | `bug` · `feature` · `refactor` · `perf` · `docs` · `test` · `chore` |
| `module/*` | `rust-core` · `strategy` · `risk` · `shadow-evolution` · `market-extension` · `ui-kit` · `gateway` · `ci` |
| `ai/*` | `authored`（AI 生成待复核）· `reviewed`（已人工复核）· `managed`（AI 托管执行） |
| `special/*` | `needs-decision`（分歧已记 `DECISIONS_PENDING`）· `no-live`（禁 Live 相关）· `security` |

另有 GitHub 默认标签（`bug`/`enhancement`/`documentation`/`good first issue`/`help wanted`/
`duplicate`/`invalid`/`question`/`wontfix`/`dependencies`/`github_actions`/`javascript`）与
Dependabot 自动标签。

**约定**：`ai/*` 是**评审状态标记，不是署名**——AI 生成未复核打 `ai/authored`，
人工复核后改 `ai/reviewed`。

---

## 9. 平台限制与其补偿（D-8）

| 能力 | 私有仓库 + Free 计划 | 补偿措施 |
| --- | --- | --- |
| 分支保护（强制 PR / 必需检查） | ❌ `403 Upgrade to GitHub Pro` | 流程自律 + CI 阻塞门 |
| 仓库规则集（rulesets） | ❌ `403` | 同上 |
| 原生 Secret Scanning / Push Protection | ❌ `422 / 404` | 零依赖 `scripts/secret-scan.sh`（阻塞）+ 每周全历史深扫 + gitleaks（建议性） |
| CodeQL | 需 Advanced Security | `.github/workflows/security.yml` 中已写好但**注释掉**，待开源或升级后启用 |

用户裁决：**维持 A（不升级套餐），后续会开源**（D-8）。因此**在开源之前，
服务端零强制**——所有防线都是流程与 CI。

---

## 10. Dependabot 与依赖升级

配置：`.github/dependabot.yml`。

- 生态：`npm`（根目录）+ `github-actions`；每周。
- `open-pull-requests-limit: 10`。
- 分组：`security`（安全更新全收）、`minor-updates`（minor+patch 归一个 PR）。
- **忽略 major 版本**（`version-update:semver-major`），避免交易 SDK 破坏性升级。

**当前待处理（3 个开放 PR）**：

| PR | 内容 | 状态 |
| --- | --- | --- |
| #1 | `actions/checkout` 6 → 7 | `mergeable: clean` |
| #2 | `actions/setup-node` 6 → 7 | `mergeable: clean` |
| #3 | npm `minor-updates` 组，38 个包 | `mergeable: unknown`（需重跑/解冲突） |

处理纪律：**Dependabot PR 也是 PR**，同样走门禁与 CI 全绿；`#3` 涉及 38 个包，
应按 [`DECISIONS_PENDING.md`](DECISIONS_PENDING.md) D-9 的口径（旧 Node 外壳依赖**随 D-4 下线**，
不做破坏性升级）判断取舍，不要直接合并。

---

## 11. 常用 REST API 配方

所有命令先取 `TOKEN`（见 §1.4）。约定：`R=ceer-quant/BlitzkriegBot`。

```bash
TOKEN=$(sed 's|^https://||; s|@github.com$||; s|.*:||' ~/.config/blitzkrieg-bot/git-credentials)
R=ceer-quant/BlitzkriegBot
AUTH=(-H "Authorization: Bearer $TOKEN" -H "Accept: application/vnd.github+json")
```

### 列表与查询

```bash
# 所有 Issue + PR（PR 会带 pull_request 字段）
curl -s "${AUTH[@]}" "https://api.github.com/repos/$R/issues?state=all&per_page=100"

# 只要开放项
curl -s "${AUTH[@]}" "https://api.github.com/repos/$R/issues?state=open&per_page=100"

# 单个 Issue / PR
curl -s "${AUTH[@]}" "https://api.github.com/repos/$R/issues/59"
curl -s "${AUTH[@]}" "https://api.github.com/repos/$R/pulls/87"

# 里程碑 / 标签 / 仓库设置
curl -s "${AUTH[@]}" "https://api.github.com/repos/$R/milestones?state=all"
curl -s "${AUTH[@]}" "https://api.github.com/repos/$R/labels?per_page=100"
curl -s "${AUTH[@]}" "https://api.github.com/repos/$R"

# 某提交的 CI 检查结果（判断 6 项是否全绿）
curl -s "${AUTH[@]}" "https://api.github.com/repos/$R/commits/main/check-runs"

# 最近的 Actions 运行
curl -s "${AUTH[@]}" "https://api.github.com/repos/$R/actions/runs?per_page=10"
```

### 开 PR（正文用文件，避免转义地狱）

```bash
python3 - <<'PY' > /tmp/pr.json
import json
body = open('/tmp/bk_pr_body.md').read()
print(json.dumps({"title": "fix(panel): ...", "head": "fix/xxx", "base": "main", "body": body}))
PY
curl -s -X POST "${AUTH[@]}" "https://api.github.com/repos/$R/pulls" -d @/tmp/pr.json
```

### 改 PR 标题 / 正文

```bash
curl -s -X PATCH "${AUTH[@]}" "https://api.github.com/repos/$R/pulls/<N>" -d @/tmp/pr_patch.json
```

### 合并（squash）

```bash
curl -s -X PUT "${AUTH[@]}" "https://api.github.com/repos/$R/pulls/<N>/merge" \
     -d '{"commit_title":"...(#N)","commit_message":"...","merge_method":"squash"}'
```

### 删远端分支

```bash
curl -s -o /dev/null -w "%{http_code}\n" -X DELETE \
  "${AUTH[@]}" "https://api.github.com/repos/$R/git/refs/heads/<branch>"   # 期望 204
```

### 改 Issue/PR 标签

```bash
curl -s -X POST "${AUTH[@]}" "https://api.github.com/repos/$R/issues/<N>/labels" \
     -d '{"labels":["type/docs","module/ci","priority/P2"]}'
```

> 写操作后**务必回读确认**（HTTP 状态码 + 一次 GET），不要假设成功。

---

## 12. 版本与发布

**当前状态：未采用 release 流程。**

- GitHub **Releases：0 个**。
- 本地克隆里有 14 个 tag（`v0.1.0` … `v1.9.1`），它们是**上游 CloddsBot 时代的遗留 tag**，
  **从未推送到 `ceer` 远端**（`git ls-remote --tags ceer` 为空）。
  不要把它们当作本项目版本，也不要在其上做发布。
- 项目版本号在 `package.json`（`0.1.0`）与 `Cargo.toml`；里程碑对齐 GitHub Milestone `v0.1`。
- 真正的交付形态是 `main` 上的提交 + CI 绿灯 + 合并后的构建哈希，**不是 tag**。

建立发布流程时（0.2 之后）再补本节：需要决定 tag 命名、是否用 GitHub Releases、
产物（二进制 / 面板 dist / 镜像）如何附加。

---

## 13. 反面清单（不要做）

1. **不要直推 `main`**，即使服务端拦不住。所有改动走 PR。
2. **不要用 `--force` 推任何分支**；不要改写已推送历史。
3. **不要把 token / `.env` / 私钥打进任何输出、日志、Issue 或提交**。
4. **不要在提交信息或 PR 里署名 AI**。
5. **不要临时加回 `origin`** remote。
6. **不要依赖 `gh`**，它没装。
7. **不要顺手改与本次目标无关的业务逻辑**（硬约束第 5 条）。
8. **不要为了让检查变绿而削弱断言**——如果门禁抓到了真实问题，改代码，不是改门禁。
9. **不要删除未经备份的文件**；不要关闭 #57 / #59 来「清空列表」。
10. **enable Live / 改动风控阈值 / 改真实凭证**——这三类必须人工显式批准。

---

## 14. 一页速查

```bash
# 身份（逐次指定，不落全局 config）
GIT="git -c user.name=ceer_quant -c user.email=ceer_quant@users.noreply.github.com"

# 远端：叫 ceer
git fetch ceer && git checkout main && git pull ceer main

# 分支
git checkout -b fix/简短描述

# 提交（信息写文件，规避工具链陷阱）
$GIT commit -F /tmp/msg.txt

# 推送（脱敏）
git push ceer fix/简短描述 2>&1 | sed 's|https://x-access-token:[^@]*@|redacted:|'

# 之后：REST API 开 PR → 等 6 项 CI 全绿 → squash 合并 → 删远端分支
#       → 同步 main → git diff --name-only main <branch> 为空后 git branch -D <branch>
```

---

_维护者：ceer_quant · 相关文档：[`AI_WORKFLOW.md`](AI_WORKFLOW.md)（协作红线）· [`DEVELOPMENT.md`](DEVELOPMENT.md)（开发与门禁）· [`../../CONTRIBUTING.md`](../CONTRIBUTING.md)_

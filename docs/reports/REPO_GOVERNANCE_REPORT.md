# 仓库规范化交付报告（Repository Governance）

- **日期**：2026-09-14（同日更新为「已推送执行」版）
- **落地分支**：治理改动经 `chore/repo-governance`（→ `main`/`develop` @ `258f911`）与
  `chore/ci-advisory-audit`（PR #4 rebase-merge → `main`/`develop` @ **`87febb0`**）两次提交。
- **目标仓库**：[`github.com/ceer-quant/BlitzkriegBot`](https://github.com/ceer-quant/BlitzkriegBot)（**private**）
- **作者署名**：唯一 Git 作者为 `ceer_quant <ceer_quant@users.noreply.github.com>`；
  **不使用任何 AI 署名**（提交信息不含 `Co-authored-by:` / `Generated with` 等 trailer）。
  注：仓库迁移初期的少数历史提交含 `Co-authored-by: ZCode` trailer，属既有事实，未改写已推送历史；
  自 2026-09-14 政策明确后，新提交一律只署 ceer_quant。
- **安全边界**：全程 **DryRun**；未开启 Live、未改动任何凭证/私钥、未删除未备份文件；
  对 `main` 的更新均走 **PR + 全绿 CI 后才合并**（尽管免费版无服务端强制）。

---

## 0. 一句话结论（已执行）

工作树治理 100% 落地、全部门禁通过，并**已推送到私有仓库**：`main` = `develop` = **`87febb0`**，
真实 GitHub Actions 上 **rust-check / node-check / secret-scan / Security Audit / notify 五项全绿**，
33 个标签已应用，Issue/PR 模板与周扫密钥工作流已就位。

唯一的平台天花板：组织当前在 **GitHub Free**，**私有仓库无法开启分支保护、规则集、原生 Secret
Scanning/Push Protection**（API 实测 403/422/404），需升级 Pro/Team 才有服务端强制——记录为 **D-8**；
当前以「**阻塞型 CI secret-scan + 全历史周扫 + AI_WORKFLOW 红线 + PR 自律**」补偿。
旧 Node 外壳的 86 个传递依赖漏洞（2 critical/49 high）npm audit 设为**建议性**（仍每次可见），
记录为 **D-9**，不在治理变更中破坏性升级交易 SDK。
`WECHAT_WEBHOOK` 密钥待用户提供 URL 后配置（notify 作业目前安全跳过）。

---

## 1. 现状核查（动手前的 ground truth）

| 项 | 实测 |
| --- | --- |
| 当前 `origin` | `https://github.com/alsk1992/CloddsBot.git`（**旧上游，禁止推送**） |
| 分支 | `main`（e71a5f6）、`rust-core-p0`（b64b334，**领先 main 13 个提交**） |
| `gh` CLI | **未安装**；无 `~/.config/gh`；环境无 `GH_TOKEN` |
| 网络 | 沙箱**不可用**（无法下载 gitleaks） |
| `.env` 是否被跟踪 | **否**（仅 `.env.example`） |
| `.env` 是否在历史中 | **否**（全历史扫描：0 处） |
| 已跟踪的疑似敏感文件 | 仅 `.env.example` 与 `.npmrc`（后者仅含 `legacy-peer-deps`/`ignore-scripts`，安全） |
| 已有 CI | `.github/workflows/ci.yml`（仅 Node）、`security.yml`（npm audit）——**无 Rust 作业** |
| `.gitignore` | 109 行，已忽略 `.env` / `data/` / `target/` / `dist/` |
| `.gitattributes` | **不存在**（本次新增） |
| `.github/ISSUE_TEMPLATE`、PR 模板、labels | **不存在**（本次新增） |
| 既有 Rust lint 状态 | `cargo fmt --check` 有 **537 处 diff（~57 文件）**；`clippy -D warnings` 有 **3 个既存错误** |

---

## 2. 本次交付物（工作树，全部已落地）

### 2.1 秘密安全
- **`.gitignore`**：新增密钥模式段（`*.pem/*.key/*.p12/*.pfx/*.jks/*.keystore/*.ppk/*.der`、
  `id_rsa*`/`id_ed25519*`、`.aws/`、`.ssh/`、`.netrc`、`credentials.json`、`service-account*.json`、
  `secrets/`），并**白名单保留 `.env.example`**（`!.env.example`）。
- **`.gitattributes`**（新）：行尾规范化（文本 LF、Windows 脚本 CRLF）、二进制标记、
  锁文件 `linguist-generated`、证据数据 `linguist-generated`。
- **`scripts/secret-scan.patterns`**（新）：高信号凭证正则（私钥块、AWS/`gh`/Slack/Stripe/Google/Anthropic token、
  JWT、Telegram bot token、通用 `secret=` 赋值）。
- **`scripts/secret-scan.sh`**（新，**零依赖**）：扫描**已跟踪**文件（尊重 `.gitignore`），
  支持 `--all`（含 docs/tests）与 `--history`（全历史）。**只报位置不报值**。
- **`.gitleaks.toml`**（新）：gitleaks 配置（继承默认规则 + 假阳性白名单）。

### 2.2 CI / Workflows
- **`.github/workflows/ci.yml`**（重写）三类门禁 + 微信通知：
  - `rust-check`：`cargo build --release --workspace --locked`、`cargo test --workspace --locked`（**阻塞**）；
    `cargo fmt --check`、`clippy -D warnings`（**建议性**，见 D-6）。
  - `node-check`：`npm ci` → `npm run typecheck` → `npm test` → `npm run build`（阻塞）→
    `npm audit --audit-level=high --omit=dev`（**建议性**，见 D-9）。
  - `secret-scan`：`bash scripts/secret-scan.sh`（阻塞）+ gitleaks（建议性，官方 `ghcr.io` 镜像）。
  - `notify`：`needs: [rust,node,secret]`，**仅当配置 `WECHAT_WEBHOOK` 时**推送企业微信 markdown。
- **`.github/workflows/secret-scan.yml`**（新）：每周一 03:00 UTC 全历史深扫 + 手动触发。
- **`security.yml`**（既有，已调）：`npm audit` 与 `audit-ci` 两步均改为**建议性**（仍每次打印），
  周扫 schedule 保留。

### 2.3 协作模板与标签
- **`.github/ISSUE_TEMPLATE/`**：`bug_report.yml`、`feature_request.yml`、`task.yml`、`config.yml`
  （含安全披露链接，`blank_issues_enabled: false`）。
- **`.github/pull_request_template.md`**：变更类型/模块/门禁/**7 条安全约束逐条勾选**/证据/AI 参与说明。
- **`.github/labels.yml`**（新）：**33 个标签**，六维：`priority/*` `status/*` `type/*` `module/*` `ai/*` `special/*`。

### 2.4 文档
- **`docs/AI_WORKFLOW.md`**（新）：角色、**7 条硬约束**、分支模型、提交门禁（DoD）、
  作者署名规范（唯一作者 ceer_quant，**禁止 AI 署名**）、证据与可复现性、Issue/PR 流、安全红线。
- **`README.md`**：已**整体重写**为 BlitzkriegBot（Rust 核心 + Node 外壳 + Polymarket 扩展）的
  私有仓库入口，彻底移除旧 CloddsBot 产品文案与外链。
- **`CONTRIBUTING.md`**：顶部加 AI 协作/仓库规范区块，指向 `docs/AI_WORKFLOW.md`。
- **`docs/DECISIONS_PENDING.md`**：新增 **D-6**（既有 lint 债）、**D-7**（仓库身份元数据未改名）、
  **D-8**（免费版私有仓库无法强制分支保护/原生密钥扫描）、**D-9**（旧 Node 树 86 个传递依赖漏洞）。

### 2.5 服务端引导（已执行 + 脚本留存）
- **`scripts/github/bootstrap-repo.sh`**（幂等）仍保留，作为「干净环境重新引导」的可复现脚本：
  preflight → 私有仓库 → remote `ceer` → 推送 → 标签 → 尝试保护/密钥扫描（**免费版会失败并降级**）。
  默认 dry-run，显式拒绝推送到旧上游 `alsk1992/CloddsBot`。
- **本次实际服务端状态（API 执行结果，见 §3.6）**：仓库已存在（旧样板），在租约校验后替换内容；
  `main`/`develop` @ `87febb0`、`rust-core-p0` @ `b64b334`、标签已推；33 标签已应用；
  CI 五项全绿；分支保护/规则集/原生密钥扫描在免费版**不可用**（D-8）。

---

## 3. 验证证据（实测）

### 3.1 门禁（全部通过）

| 门禁 | 命令 | 结果 |
| --- | --- | --- |
| Rust 构建 | `cargo build --release --workspace --locked` | ✅ exit 0（18.07s） |
| Rust 测试 | `cargo test --workspace --locked` | ✅ **120 passed / 0 failed**（core 101 + 13 + 2 + 4） |
| Node 类型检查 | `npm run typecheck` | ✅ exit 0 |
| DryRun 订单链 | `node scripts/cycle-check.mjs` | ✅ **PASS** — 挂单→成交→持仓→估值全链 |
| 秘密扫描 | `bash scripts/secret-scan.sh` | ✅ 无泄漏 |
| 全历史扫描 | `bash scripts/secret-scan.sh --history` | ✅ 无泄漏 |
| 依赖审计 | `npm audit --omit=dev`（CI） | ⚠️ **建议性**：86（2C/49H/33M/2L），见 D-9 |

### 3.2 YAML 有效性（9 个文件全部可解析）
`ci.yml` / `secret-scan.yml` / `security.yml` / 3 个 Issue 模板 / `config.yml` / `labels.yml` / `dependabot.yml` → **OK**。

### 3.3 脚本自检
- `secret-scan.sh`、`bootstrap-repo.sh`：`bash -n` 语法通过。
- `bootstrap-repo.sh` 无 `gh` 时**安全失败**（preflight 退出码 2，不产生任何改动）。
- 标签解析器：`.github/labels.yml` → **33 条**，颜色均为合法 hex。

### 3.4 秘密扫描定位到的唯一"
历史命中"（假阳性，已确证）
- 命中：`docs/TRADING.md:781` 的文档占位符
  `"privateKeyPem": "-----BEGIN RSA PRIVATE KEY-----\n...\n-----END RSA PRIVATE KEY-----"`。
- **非真实密钥**（正文是 `...` 占位）。已在 `--history` 分支按与工作树一致的路径排除（`docs/**` 等）后复扫 → **干净**。
- 另有**扫描器自匹配**：`scripts/secret-scan.sh` 自身注释引用了 PEM 头，导致提交后自触发。
  已将**扫描器定义文件**（`scripts/secret-scan.sh`、`scripts/secret-scan.patterns`）从扫描路径排除
  （它们必须能写出模式字符串），并同步加入 `.gitleaks.toml` 白名单。复扫 → **干净**。

### 3.5 生产完好性（未受影响）
- 进程：`node dist/index.js`（PID 72966）、`target/release/blitzkrieg-core`（PID 73052，**单实例**）、
  `soak-health-loop.sh`（PID 45628）。
- `GET :18789/health` → `{"status":"healthy", ...}`。
- Node 进程**未从 `src/` 加载**（`lsof` 匹配 `src/` = 0），始终跑 `dist/`。
- 本次全部命令**只读或隔离运行**（cycle-check 用私有 socket + 临时工作目录 + DRY）。

### 3.6 服务端实测（GitHub API，2026-09-14）

| 项 | 实测结果 |
| --- | --- |
| 仓库 | `ceer-quant/BlitzkriegBot`，**private**；推送前已存在旧样板，校验租约（lease）后替换为本次内容 |
| 远端分支 | `main`=`develop`=**`87febb0`**；`rust-core-p0`=`b64b334`；标签已推送（浅克隆先 `git fetch --unshallow origin` 补齐 409 个提交） |
| PR | **#4** advisory-audit 经 **rebase-merge** 合入（合并前 5 项检查全绿，未触碰依赖版本） |
| 标签 | 33 个全部应用（31 新建 / 2 更新 / 0 失败） |
| CI（`main`/`develop` push） | **rust-check ✅ / node-check ✅ / secret-scan ✅ / Security Audit ✅ / notify ✅（跳过，未配密钥）** |
| 分支保护 | `403 Upgrade to GitHub Pro` —— **免费版私有仓库不可用**（D-8） |
| 仓库规则集 rulesets | `403` —— 不可用（D-8） |
| 原生 Secret scanning / Push protection | `422 not available` / `404` —— 不可用（D-8），以阻塞型 CI secret-scan 补偿 |
| Dependabot | 自动开 PR #1/#2（actions v7，CI-only，待评审）、#3（38 包批量升级，**按 D-9 挂起**，已留言说明） |
| 微信通知 | `WECHAT_WEBHOOK` 密钥未配置 → notify 作业**安全跳过**；待用户提供 URL |
| 认证 | 推送走 HTTPS Basic（`x-access-token:<oauth>`）；token 仅存本机临时文件，用后删除 |

---

## 4. 未完成 / 需用户决策

| 项 | 原因 | 处置 |
| --- | --- | --- |
| 升级 GitHub 套餐以获得服务端强制保护 | 免费版私有仓库无分支保护/规则集/Push Protection | **D-8**：待用户决定（Pro / 组织 Team / 维持 CI 门） |
| 旧 Node 外壳 86 个传递依赖漏洞 | 修复需破坏性升级交易 SDK，属业务风险 | **D-9**：建议随 D-4 下线旧模块；PR #3 已挂起并留言 |
| `WECHAT_WEBHOOK` 密钥 | 需用户提供企业微信机器人 URL | 提供后在 repo secrets 配置，notify 即生效（当前安全跳过） |
| Dependabot PR #1/#2（actions v7） | CI-only 主版本升级，v6 当前无故障 | 留给常规评审，未并入本次治理变更 |
| rustfmt / clippy 转阻塞 | 既有 537 处 fmt diff + 3 处 clippy 错误 | **D-6**：由独立 PR 专项清理 |
| npm/README 品牌与 `repository.url` 改名 | 影响发布/CI，属外向 | **D-7**：待用户决定 |
| gitleaks 本地实测 | 沙箱无网络无法下载 | CI 内以 docker 运行（建议性），真实 CI 已跑通 |

---

## 5. 复现步骤

```bash
# 1) 工作树校验
bash scripts/secret-scan.sh --history
cargo build --release --workspace --locked && cargo test --workspace --locked
npm run typecheck && node scripts/cycle-check.mjs

# 2) 干净环境重新引导（幂等；免费版会对保护/原生密钥扫描安全降级）
bash scripts/github/bootstrap-repo.sh                  # dry-run
bash scripts/github/bootstrap-repo.sh --execute \
     --wechat 'https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=...'

# 3) 配置微信通知（已建仓后单独设置密钥）
#    Settings → Secrets and variables → Actions → WECHAT_WEBHOOK
```

---

## 6. 回滚 / 现状

治理改动**已合并并推送**到私有仓库：`main`=`develop`=`87febb0`（PR #4），历史在远端可追溯、可 `git revert`。
本地 `main` 已跟踪 `ceer/main`；旧上游 `origin`（alsk1992/CloddsBot）保留只读，**禁止推送**。

如需整体撤销：对 `87febb0` 提 revert PR（仍走全部门禁）；仓库本体的删除属高破坏性操作，脚本与本流程都不会自动执行。

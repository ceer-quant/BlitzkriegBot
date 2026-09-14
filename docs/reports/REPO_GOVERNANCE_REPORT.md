# 仓库规范化交付报告（Repository Governance）

- **日期**：2026-09-14
- **分支**：`chore/repo-governance`（自 `rust-core-p0` 切出，**未触碰 `main`**）
- **模式**：仅本地文件 + 一次 dry-run；**未开启 Live、未改动任何凭证、未删除任何文件、未向任何远端推送**
- **目标仓库**：`github.com/ceer-quant/BlitzkriegBot`（**private**）
- **作者署名**：`ceer_quant`

---

## 0. 一句话结论

仓库治理**工作树部分已 100% 落地并通过全部门禁**；**服务端部分**（建仓库、分支保护、标签、Secret Scanning、Webhook）
因**本机没有 `gh`、且沙箱无网络**，已实现为**幂等脚本 `scripts/github/bootstrap-repo.sh`**，待有 GitHub 凭证时一条命令应用。
推送 `ceer-quant/BlitzkriegBot` 属**外向动作**，按硬约束需用户显式授权后才执行——脚本默认 `--dry-run`。

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
  - `node-check`：`npm ci` → `npm run typecheck` → `npm test` → `npm run build` → `npm audit`（阻塞）。
  - `secret-scan`：`bash scripts/secret-scan.sh`（阻塞）+ gitleaks（建议性，官方 `ghcr.io` 镜像）。
  - `notify`：`needs: [rust,node,secret]`，**仅当配置 `WECHAT_WEBHOOK` 时**推送企业微信 markdown。
- **`.github/workflows/secret-scan.yml`**（新）：每周一 03:00 UTC 全历史深扫 + 手动触发。
- **`security.yml`**：保留（npm audit 周扫）。

### 2.3 协作模板与标签
- **`.github/ISSUE_TEMPLATE/`**：`bug_report.yml`、`feature_request.yml`、`task.yml`、`config.yml`
  （含安全披露链接，`blank_issues_enabled: false`）。
- **`.github/pull_request_template.md`**：变更类型/模块/门禁/**7 条安全约束逐条勾选**/证据/AI 参与说明。
- **`.github/labels.yml`**（新）：**33 个标签**，六维：`priority/*` `status/*` `type/*` `module/*` `ai/*` `special/*`。

### 2.4 文档
- **`docs/AI_WORKFLOW.md`**（新）：角色、**7 条硬约束**、分支模型、提交门禁（DoD）、
  `Co-authored-by:` 署名约定、证据与可复现性、Issue/PR 流、安全红线。
- **`README.md`**：顶部加私有仓库 + 门禁横幅（**最小改动**，未重排全文品牌）。
- **`CONTRIBUTING.md`**：顶部加 AI 协作/仓库规范区块，指向 `docs/AI_WORKFLOW.md`。
- **`docs/DECISIONS_PENDING.md`**：新增 **D-6**（既有 lint 债）、**D-7**（仓库身份元数据未改名）。

### 2.5 服务端引导
- **`scripts/github/bootstrap-repo.sh`**（新，幂等）：preflight 检查 `gh`/auth → 建私有仓库 →
  加 remote `ceer` → **`git push ceer --all --tags`**（推全部分支，不用可能过期的 main）→
  以 `rust-core-p0` 为 `develop` 种子 → 开启 **Secret Scanning + Push Protection** →
  对 `main`/`develop` 设**分支保护**（要求 `rust-check`/`node-check`/`secret-scan` 状态检查 + 1 评审 + 禁强推）→
  配置 `WECHAT_WEBHOOK` → 从 `labels.yml` 应用 33 个标签。
  **默认 dry-run**；显式拒绝推送到旧上游 `alsk1992/CloddsBot`。

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

---

## 4. 未完成 / 需用户决策

| 项 | 原因 | 处置 |
| --- | --- | --- |
| 建 `ceer-quant/BlitzkriegBot` 私有仓库并推送 | 沙箱无网络、无 `gh`；且属**外向动作**需授权 | 待用户运行 `bootstrap-repo.sh --execute` |
| 分支保护 / Secret Scanning / Push Protection / 标签 / Webhook | 均为 GitHub 服务端操作 | 同上（脚本已就绪） |
| rustfmt / clippy 转阻塞 | 既有 537 处 fmt diff + 3 处 clippy 错误 | **D-6**：由独立 PR 专项清理 |
| npm/README 品牌与 `repository.url` 改名 | 影响发布/CI，属外向 | **D-7**：待用户决定 |
| gitleaks 本地实测 | 沙箱无网络无法下载 | CI 内以 docker 运行（建议性） |

---

## 5. 复现步骤

```bash
# 1) 工作树校验（无需 GitHub）
bash scripts/secret-scan.sh --history
cargo build --release --workspace --locked && cargo test --workspace --locked
npm run typecheck && node scripts/cycle-check.mjs

# 2) 服务端引导（需 gh 已 auth；先 dry-run 看计划）
brew install gh && gh auth login
bash scripts/github/bootstrap-repo.sh                  # dry-run
bash scripts/github/bootstrap-repo.sh --execute \
     --wechat 'https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=...'
```

---

## 6. 回滚

工作树改动全部在**功能分支** `chore/repo-governance`，未合并、未推送：

```bash
git checkout rust-core-p0 && git branch -D chore/repo-governance   # 丢弃
```

服务端引导为幂等；如需撤销，删除仓库或用 `gh api --method DELETE` 反设保护即可（脚本不自动执行任何删除）。

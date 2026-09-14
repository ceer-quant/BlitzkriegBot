# 描述

<!-- 这个 PR 做了什么？为什么？关联的 Issue：Closes #__ -->

## 变更类型
- [ ] feat 新功能
- [ ] fix 缺陷修复
- [ ] refactor 重构（不改变行为）
- [ ] perf 性能
- [ ] docs 文档
- [ ] chore / ci / build

## 涉及模块
<!-- 勾选受影响范围 -->
- [ ] rust-core (core/blitzkrieg_core)
- [ ] strategy / engine
- [ ] risk / safety
- [ ] shadow-evolution
- [ ] market extension
- [ ] ui-kit / panel
- [ ] node shell / gateway
- [ ] ci / tooling / docs

---

## 🚦 门禁（必须全绿）
- [ ] `cargo build --release` 通过
- [ ] `cargo test` 通过
- [ ] `npm run typecheck` 通过
- [ ] 涉及下单链的改动已跑 DryRun / `node scripts/cycle-check.mjs`

## 🔐 安全与约束（逐条确认）
- [ ] **未开启 Live 交易**，且改动不影响 Live 开关语义
- [ ] **未新增/修改任何真实凭证、私钥、API Key**；无 `.env` 内容进入本 PR
- [ ] **未删除任何未经备份的文件**
- [ ] **未"顺手优化"与本次目标无关的业务逻辑**
- [ ] `bash scripts/secret-scan.sh` 通过
- [ ] 不确定的分歧已写入 `docs/DECISIONS_PENDING.md`（而非在 PR 中悬置）

## 🧪 证据
<!-- 贴上关键测试输出、A/B 数字、日志片段。证据文件请提交到 docs/reports/data/ 并在此引用。 -->

## 🤖 AI 参与说明（如适用）
<!-- 如使用了 AI：写清人工复核范围即可。注意：提交信息中禁止任何 AI 署名（Co-authored-by / Generated with 等），Git 作者一律为 ceer_quant。 -->

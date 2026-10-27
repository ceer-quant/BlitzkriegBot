#!/usr/bin/env node
/**
 * PR 领地对账（Issue #648 第 7 条 / 流程 D-6 的教训：#547 squash 截断需要
 * #549 补遗——DEV_V0_3 §14.3 的模板早就要求「PR 必须附 git diff --name-only
 * 输出」，但一直缺执行门禁）。
 *
 * 合并仪式：squash-merge 前对每个 PR 跑一遍——
 *
 *     node scripts/pr-territory-check.mjs <PR编号>
 *
 * 判定：PR 的每一个实际改动文件，都必须能在 PR 正文里找到它的落点——
 *   · 完整路径（含反引号包裹），或
 *   · 裸文件名，或
 *   · 目录前缀（`dir/` 或 `dir/**`，覆盖其下全部文件）。
 * 正文完全没提的文件 = 领地外改动，exit 1 并点名。空正文 = 全部超领地。
 *
 * 这不是 CI 门禁（CI 无法在合并动作前拦截），是合并者的对账工具：过一遍
 * 的意义在于让「正文声称的领地」与「diff 实际落点」逐文件对上，正文漏写
 * 的改动被迫补进正文或从 PR 里撤掉。
 *
 * Usage: GH_TOKEN=… node scripts/pr-territory-check.mjs 648
 */
import { execFileSync } from 'child_process';

const pr = process.argv[2];
if (!pr || !/^\d+$/.test(pr)) {
  console.error('usage: node scripts/pr-territory-check.mjs <PR编号>');
  process.exit(2);
}

let view;
try {
  view = JSON.parse(
    execFileSync('gh', ['pr', 'view', pr, '--repo', 'ceer-quant/BlitzkriegBot',
      '--json', 'number,title,body,files'], { encoding: 'utf8', maxBuffer: 32 * 1024 * 1024 }),
  );
} catch (e) {
  console.error(`gh pr view ${pr} failed: ${e.message}`);
  process.exit(2);
}

const body = view.body ?? '';
// Body-declared territory: every path-like token (`dir/file.ext`, `dir/`,
// `dir/**`) plus bare filenames. Backticks stripped first so `path` matches.
const flat = body.replace(/`/g, '');
const dirPrefixes = [];
const bareNames = new Set();
for (const m of flat.matchAll(/[\w][\w./-]*(?:\/[\w][\w./-]*)+/g)) {
  const token = m[0].replace(/\*\*/g, '').replace(/\/+$/, '');
  if (!token.includes('/')) continue;
  dirPrefixes.push(`${token}/`);
  bareNames.add(token.split('/').pop());
}
for (const m of flat.matchAll(/[\w][\w.-]*\.(?:rs|toml|mjs|ts|vue|md|json|lua|yml|yaml|lock)\b/g)) {
  bareNames.add(m[0]);
}

const files = (view.files ?? []).map((f) => f.path);
const unaccounted = files.filter((f) => {
  if (flat.includes(f)) return false;
  if (bareNames.has(f.split('/').pop())) return false;
  if (dirPrefixes.some((p) => f.startsWith(p))) return false;
  return true;
});

console.log(`PR #${view.number} ${view.title}`);
console.log(`  改动文件 ${files.length} 个，正文认领 ${files.length - unaccounted.length} 个`);
for (const f of files) {
  const ok = !unaccounted.includes(f);
  console.log(`  ${ok ? '  ok  ' : '  MISS'} ${f}`);
}
if (unaccounted.length > 0) {
  console.error(`\n${unaccounted.length} 个文件在 PR 正文（领地声明）里没有落点 —— 补进正文或撤出 PR。`);
  process.exit(1);
}
console.log('\n领地对账 PASS —— 每个改动文件都在正文里认了领。');

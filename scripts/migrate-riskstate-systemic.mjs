#!/usr/bin/env node
/**
 * migrate-riskstate-systemic — the #965 leftover: a ONE-SHOT migration for the
 * risk-state files that still carry the retired `systemic` cells.
 *
 * Background: PR #990 (a74301db) retired the systemic bounds from the
 * risk-state file (positions.risk-state.json / positions.risk-state.dry.json).
 * Their durable carrier is now `.env` — the five per-account `BK_RISK_*` keys
 * the boot re-arms through `SystemicRiskLimits::from_env` (issue #966; after
 * #983 there are NO global limits at all). A legacy file still loads, but the
 * loader warns loudly on EVERY boot:
 *
 *   core/blitzkrieg_core/src/service/risk_admission/persist.rs
 *   load_risk_state_file(): "risk-state file carries retired systemic cells —
 *   ignored; ..." — the predicate is literally `raw.get("systemic").is_some()`.
 *
 * This script closes that loop: it moves the five per-account values into
 * `.env` (the exact keys the write face `risk.setSystemic` upserts —
 * core/blitzkrieg_core/src/ipc/server/handlers/risk.rs `env_key_for`) and
 * rewrites the file WITHOUT the `systemic` map, so the warning can never fire
 * again. Everything else on the file (entry caps, kill switch, freeze book,
 * auto-frozen stamps) is preserved.
 *
 * KEY MAP (the nine pre-#990 wire spellings; single source here):
 *
 *   per-account — live after #983, migrated to .env:
 *     maxSingleLossUsd        → BK_RISK_MAX_SINGLE_LOSS_USD
 *     maxDailyDrawdownUsd     → BK_RISK_MAX_DAILY_DRAWDOWN_USD
 *     maxPositionSize         → BK_RISK_MAX_POSITION_SIZE
 *     maxConsecutiveLosses    → BK_RISK_MAX_CONSECUTIVE_LOSSES
 *     cooldownMinutes         → BK_RISK_COOLDOWN_MINUTES
 *
 *   global — 已随 #983 退役，丢弃 (the kernel has no process-wide caps;
 *     their old BK_RISK_* keys were deleted from the grammar in #985):
 *     maxTotalPosition          (was BK_RISK_MAX_TOTAL_POSITION)
 *     maxTotalExposureUsd       (was BK_RISK_MAX_TOTAL_EXPOSURE_USD)
 *     maxCorrelationUsd         (was BK_RISK_MAX_CORRELATION_USD)
 *     globalKillSwitchLossUsd   (was BK_RISK_GLOBAL_KILL_SWITCH_LOSS_USD)
 *
 *   anything else — unknown field: discarded and reported (the same posture
 *     the pre-#990 loader had: "unknown systemic field — ignored").
 *
 * Value discipline (mirrors the pre-#990 loader's `parse_decimal_cell`):
 *   - a cell only migrates when its `value` parses as a plain decimal >= 0
 *     (the runtime writes `Decimal::to_string()`, never scientific notation);
 *   - `"0"` is the factory OFF — nothing to arm, reported and skipped (a
 *     `BK_RISK_*=0` line would arm provenance `env` for a bound that is off
 *     either way; the file stays minimal);
 *   - a negative or malformed value is skipped with a warning in the report —
 *     the same fail-closed "the boot value stands" the loader ran.
 *
 * Safety posture (the migrate-config-to-env.sh family, §9.4):
 *   - DRY-RUN by default: nothing is written unless `--apply` is passed.
 *   - Before a write the ORIGINAL risk-state file is backed up beside itself
 *     (`<name>.pre-systemic-migration-<UTC>.bak`) and verified byte-identical.
 *   - `.env` is written FIRST (the new carrier must exist before the old one
 *     is retired); unowned lines are preserved byte-for-byte, managed keys are
 *     replaced in place, new keys appended — the same contract
 *     core/blitzkrieg_core/src/env_file.rs `upsert_env_vars` runs, so a later
 *     runtime upsert replaces these lines rather than duplicating them.
 *   - Writes are atomic (temp file in the same directory + rename) and 0600,
 *     matching env_file.rs. A failed .env write abandons the run with the
 *     risk-state file untouched (fail-closed, nothing lost).
 *   - Idempotent: a file without `systemic` is a no-op that says so — no
 *     backup, no `.env` touch, exit 0. A re-run after a partial failure
 *     (risk-state rewrite failed) resumes safely: the .env upsert lands as
 *     "already present, same value".
 *   - Nothing here reads, prints, or moves credentials: the only values this
 *     script writes are the validated decimal numbers of the five per-account
 *     bounds. Real .env values outside BK_RISK_* are never echoed.
 *
 * Usage:
 *   node scripts/migrate-riskstate-systemic.mjs <risk-state.json> \
 *        [--env <path>] [--apply]
 *     <risk-state.json>  the file to migrate (explicit path — this script
 *                        never guesses at production data)
 *     --env <path>       the .env to upsert (default: ./.env, the launcher's
 *                        read side)
 *     --apply            write for real (default: dry-run, plan only)
 *   node scripts/migrate-riskstate-systemic.mjs --self-test
 *                      fixture-driven self-test in a tmpdir (no real files)
 *
 * Exit: 0 = migrated / planned / nothing to do; 1 = error (missing file,
 * corrupt JSON — fail-closed, nothing written); 2 = usage.
 */

import { parseArgs } from 'node:util';
import assert from 'node:assert';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

// ── the single-source key map (see the header) ───────────────────────────────

/** The five per-account bounds — live keys, migrated to `.env`. */
const PER_ACCOUNT = [
  ['maxSingleLossUsd', 'BK_RISK_MAX_SINGLE_LOSS_USD'],
  ['maxDailyDrawdownUsd', 'BK_RISK_MAX_DAILY_DRAWDOWN_USD'],
  ['maxPositionSize', 'BK_RISK_MAX_POSITION_SIZE'],
  ['maxConsecutiveLosses', 'BK_RISK_MAX_CONSECUTIVE_LOSSES'],
  ['cooldownMinutes', 'BK_RISK_COOLDOWN_MINUTES'],
];

/** The four global bounds — retired by #983, discarded (kernel has no global caps). */
const RETIRED_BY_983 = [
  'maxTotalPosition',
  'maxTotalExposureUsd',
  'maxCorrelationUsd',
  'globalKillSwitchLossUsd',
];

/** The envelope fields `RiskStateFile` carries (persist.rs) — everything else
 * on the file is dropped on rewrite exactly as serde would drop it. */
const ENVELOPE_FIELDS = ['savedAtMs', 'limits', 'kill', 'accountFreezes', 'autoFrozen'];

// ── .env line contract: the read/write halves of env_file.rs ────────────────

/** env_file.rs `line_key`: the key of one .env line, or null for blanks,
 * comments, and invalid key spellings. */
function envLineKey(line) {
  const trimmed = line.trim();
  if (!trimmed || trimmed.startsWith('#')) return null;
  const stripped = trimmed.startsWith('export ') ? trimmed.slice(7).trimStart() : trimmed;
  const eq = stripped.indexOf('=');
  if (eq < 0) return null;
  const key = stripped.slice(0, eq).trim();
  return /^[A-Za-z_][A-Za-z0-9_]*$/.test(key) ? key : null;
}

/** env_file.rs `parse_line`: the value of one .env line (one quote layer off). */
function envLineValue(line) {
  const trimmed = line.trim();
  const stripped = trimmed.startsWith('export ') ? trimmed.slice(7).trimStart() : trimmed;
  const eq = stripped.indexOf('=');
  if (eq < 0) return '';
  let value = stripped.slice(eq + 1).trim();
  if (
    value.length >= 2 &&
    ((value.startsWith('"') && value.endsWith('"')) ||
      (value.startsWith("'") && value.endsWith("'")))
  ) {
    value = value.slice(1, -1);
  }
  return value;
}

/** env_file.rs `render_line`: `KEY="value"` (the numeric bounds never carry
 * quotes themselves, so the default double-quote form always applies). */
function renderEnvLine(key, value) {
  return `${key}="${value}"`;
}

// ── value discipline (the pre-#990 parse_decimal_cell semantics) ────────────

/** Canonical decimal string: /^-?\d+(\.\d+)?$/ normalized (leading zeros and
 * trailing fraction zeros stripped) so `2.0` and `2` compare equal. */
function parseDecimalCell(raw) {
  const s = String(raw).trim();
  if (!/^-?\d+(\.\d+)?$/.test(s)) return null; // malformed
  const neg = s.startsWith('-');
  const [int, frac = ''] = s.replace('-', '').split('.');
  const f = frac.replace(/0+$/, '');
  const norm = `${int.replace(/^0+(?=\d)/, '')}${f ? `.${f}` : ''}`;
  const off = !neg && f === '' && BigInt(int) === 0n; // "0" / "0.0" — the factory OFF
  return { norm: neg ? `-${norm}` : norm, off, negative: neg };
}

// ── the migration plan ───────────────────────────────────────────────────────

/**
 * Plan the migration for one risk-state file against one .env.
 * Pure: reads nothing, writes nothing — the dry-run prints this and --apply
 * commits it, so the two faces cannot drift.
 *
 * Returns { report: string[], envPlan: {kind, key, line, at}[], envLines: string[],
 *           envExisted, migrated: any (the rewritten file text) | null, counts }.
 */
export function planMigration(stateText, envText, { envExisted }) {
  const report = [];
  let doc;
  try {
    doc = JSON.parse(stateText);
  } catch (e) {
    return { error: `risk-state file does not parse (fail-closed, nothing written): ${e.message}` };
  }
  if (doc === null || typeof doc !== 'object' || Array.isArray(doc)) {
    return { error: 'risk-state file is not a JSON object (fail-closed, nothing written)' };
  }
  const systemic = doc.systemic;
  if (systemic === undefined) {
    return { nothingToDo: true };
  }
  if (systemic === null || typeof systemic !== 'object' || Array.isArray(systemic)) {
    return { error: `risk-state "systemic" is ${systemic === null ? 'null' : 'not an object'} — cannot map it (fail-closed, nothing written)` };
  }

  const byField = new Map(Object.entries(systemic));
  const envPlan = [];

  // 1) the five per-account bounds → BK_RISK_* lines.
  for (const [field, envKey] of PER_ACCOUNT) {
    const cell = byField.get(field);
    if (cell === undefined) continue;
    byField.delete(field);
    const raw = cell !== null && typeof cell === 'object' ? cell.value : cell;
    const parsed = raw === undefined ? null : parseDecimalCell(raw);
    if (parsed === null) {
      report.push(`  ! ${field}: 值 ${JSON.stringify(raw ?? cell)} 非法/缺失（旧加载器同样忽略）—— 跳过，不迁移`);
      continue;
    }
    if (parsed.negative) {
      report.push(`  ! ${field}: 负值 ${JSON.stringify(raw)}（旧加载器忽略负值）—— 跳过，不迁移`);
      continue;
    }
    if (parsed.off) {
      report.push(`  = ${field}: 值 0 = 出厂关闭（§4.1）—— 无需武装，跳过`);
      continue;
    }
    envPlan.push({ field, envKey, value: parsed.norm });
  }

  // 2) the retired globals and anything unknown — discarded, never written.
  let discardedRetired = 0;
  let discardedUnknown = 0;
  for (const [field, cell] of byField) {
    if (RETIRED_BY_983.includes(field)) {
      discardedRetired += 1;
      const raw = cell !== null && typeof cell === 'object' ? cell.value : cell;
      report.push(`  - ${field}: 已随 #983 退役（内核无全局限额），丢弃，不迁移（值 ${JSON.stringify(raw ?? cell)}）`);
    } else {
      discardedUnknown += 1;
      report.push(`  - ${field}: 未知字段（旧加载器同样忽略），丢弃，不迁移`);
    }
  }

  // 3) the .env plan: replace in place / append / already the same.
  const envLines = envExisted ? envText.split('\n') : [];
  // A trailing newline yields a spurious '' last element — keep it out of the
  // key scan but remember whether the file ends with one so the rewrite does
  // not glue lines together.
  const endsWithNewline = envLines.length > 0 && envLines[envLines.length - 1] === '';
  if (endsWithNewline) envLines.pop();
  const existingAt = new Map();
  envLines.forEach((line, i) => {
    const key = envLineKey(line);
    if (key !== null && !existingAt.has(key)) existingAt.set(key, i);
  });
  const diff = [];
  let replaced = 0;
  let appended = 0;
  let same = 0;
  for (const item of envPlan) {
    const line = renderEnvLine(item.envKey, item.value);
    if (existingAt.has(item.envKey)) {
      const at = existingAt.get(item.envKey);
      if (envLineValue(envLines[at]) === item.value) {
        same += 1;
        diff.push(`  = ${item.envKey} 已在 .env 且值相同（幂等，不改）`);
        continue;
      }
      // env_file.rs `upsert_env_vars` semantics: EVERY existing line with the
      // key is replaced in place (a hand-duplicated key never survives as a
      // stale shadow).
      const before = envLines[at];
      for (let i = 0; i < envLines.length; i++) {
        if (envLineKey(envLines[i]) === item.envKey) envLines[i] = line;
      }
      diff.push(`  ~ ${item.envKey} 第 ${at + 1} 行原位替换: ${before} → ${line}`);
      replaced += 1;
    } else {
      envLines.push(line);
      appended += 1;
      diff.push(`  + ${item.envKey} 追加: ${line}`);
    }
  }

  // 4) the rewritten risk-state file: the envelope minus `systemic`, in the
  // struct's field order with serde's skip-empty rules (the exact shape the
  // runtime's next save would produce).
  const rewritten = {};
  if (typeof doc.savedAtMs === 'number' && doc.savedAtMs !== 0) rewritten.savedAtMs = doc.savedAtMs;
  if (doc.limits && typeof doc.limits === 'object' && Object.keys(doc.limits).length > 0) rewritten.limits = doc.limits;
  if (doc.kill && typeof doc.kill === 'object') rewritten.kill = { reason: String(doc.kill.reason) };
  if (doc.accountFreezes && typeof doc.accountFreezes === 'object' && Object.keys(doc.accountFreezes).length > 0) rewritten.accountFreezes = doc.accountFreezes;
  if (doc.autoFrozen && typeof doc.autoFrozen === 'object' && Object.keys(doc.autoFrozen).length > 0) rewritten.autoFrozen = doc.autoFrozen;
  const migratedText = `${JSON.stringify(rewritten, null, 2)}\n`;

  return {
    report,
    diff,
    migratedText,
    envText: envLines.join('\n') + (envLines.length > 0 && (endsWithNewline || appended > 0) ? '\n' : ''),
    counts: {
      planned: envPlan.length,
      replaced,
      appended,
      same,
      discardedRetired,
      discardedUnknown,
    },
  };
}

// ── IO faces ─────────────────────────────────────────────────────────────────

/** env_file.rs write posture: atomic temp + rename in the same directory, 0600. */
function atomicWrite(file, text) {
  const tmp = path.join(path.dirname(file), `.${path.basename(file)}.migrate-tmp-${process.pid}`);
  fs.writeFileSync(tmp, text, { mode: 0o600 });
  fs.renameSync(tmp, file);
}

/** The main run. Returns the process exit code. */
function run(statePath, envPath, apply) {
  let stateText;
  try {
    stateText = fs.readFileSync(statePath, 'utf8');
  } catch (e) {
    console.error(`error: cannot read ${statePath}: ${e.message}`);
    return 1;
  }
  let envText = '';
  let envExisted = false;
  try {
    envText = fs.readFileSync(envPath, 'utf8');
    envExisted = true;
  } catch {
    // An absent .env is the factory posture — the upsert creates it.
  }

  const plan = planMigration(stateText, envText, { envExisted });
  if (plan.error) {
    console.error(`error: ${plan.error}`);
    return 1;
  }
  if (plan.nothingToDo) {
    console.log(`nothing to migrate: ${statePath} carries no "systemic" map — 已迁移过或从未有过（幂等 no-op）`);
    return 0;
  }

  console.log(`risk-state: ${statePath}`);
  console.log(`.env      : ${envPath}${envExisted ? '' : ' (不存在，将创建)'}`);
  console.log('');
  console.log('映射报告（旧 systemic 键 → BK_RISK_* .env 键 / 丢弃）:');
  for (const line of plan.report) console.log(line);
  for (const line of plan.diff) console.log(line);
  const c = plan.counts;
  console.log('');
  console.log(
    `计划: 迁移 ${c.planned} 键（追加 ${c.appended} / 原位替换 ${c.replaced} / 已同值 ${c.same}），` +
      `丢弃 ${c.discardedRetired} 个 #983 退役全局键 + ${c.discardedUnknown} 个未知字段`,
  );

  if (!apply) {
    console.log('');
    console.log('dry-run：未写任何文件。确认后加 --apply 执行。');
    return 0;
  }

  // Apply, in the fail-closed order: .env first (the new carrier must exist
  // before the old one is retired), then the backup, then the rewrite.
  try {
    if (c.appended + c.replaced > 0) {
      atomicWrite(envPath, plan.envText);
      fs.chmodSync(envPath, 0o600);
      console.log(`.env 已写入: ${envPath}`);
    } else {
      console.log('.env 无需改动（全部已同值）');
    }
  } catch (e) {
    console.error(`error: .env 写入失败（原 risk-state 文件未动，直接重跑即可）: ${e.message}`);
    return 1;
  }

  const stamp = new Date().toISOString().replace(/[-:]/g, '').replace(/\..+/, '');
  const backup = `${statePath}.pre-systemic-migration-${stamp}.bak`;
  try {
    fs.copyFileSync(statePath, backup);
    if (fs.readFileSync(backup, 'utf8') !== stateText) {
      throw new Error('backup verification failed');
    }
    console.log(`原文件已备份: ${backup}`);
  } catch (e) {
    console.error(`error: 备份失败（.env 已写入，risk-state 原文件未动；重跑安全）: ${e.message}`);
    return 1;
  }
  try {
    atomicWrite(statePath, plan.migratedText);
    console.log(`risk-state 已重写（systemic 段已移除，#990 的加载告警不再触发）: ${statePath}`);
  } catch (e) {
    console.error(`error: risk-state 重写失败（备份与 .env 均在；重跑安全）: ${e.message}`);
    return 1;
  }
  console.log('完成。重启内核后 BK_RISK_* 生效（provenance=env），风险状态文件不再携带退役格式。');
  return 0;
}

// ── the self-test: fixtures in a tmpdir, no real files ──────────────────────

/** A legacy pre-#990 file: entry caps + the whole nine-bound systemic map
 * (two off, one negative, one malformed) + kill switch + freeze book. */
const LEGACY_STATE = JSON.stringify(
  {
    savedAtMs: 1_762_000_000_000,
    limits: { maxOrderNotional: '250', maxShares: '900' },
    systemic: {
      maxSingleLossUsd: { value: '0.75', source: 'flag' },
      maxDailyDrawdownUsd: { value: '5', source: 'env' },
      maxPositionSize: { value: '2', source: 'env' },
      maxConsecutiveLosses: { value: '4', source: 'default' },
      cooldownMinutes: { value: '30', source: 'flag' },
      maxTotalPosition: { value: '5000', source: 'flag' },
      maxTotalExposureUsd: { value: '500', source: 'flag' },
      maxCorrelationUsd: { value: '0', source: 'flag' },
      globalKillSwitchLossUsd: { value: '-1', source: 'flag' },
      someUnknownField: { value: '9', source: 'env' },
    },
    kill: { reason: 'operator pause' },
    accountFreezes: { acct_a: 'manual hold' },
    autoFrozen: { acct_b: 1_762_000_000_001 },
  },
  null,
  2,
);

/** The fixture .env: a BK_TEST_KEY_* keeper (never a credential-shaped key),
 * plus one managed key at a stale value to prove replace-in-place. */
const FIXTURE_ENV = 'BK_TEST_KEY_KEEP_ME=1\nBK_RISK_MAX_POSITION_SIZE="999"\n';

/** Run the script in a scratch dir via the same `run()` the CLI uses. */
function runIn(tmp, stateText, envText, apply, envName = '.env') {
  fs.mkdirSync(tmp, { recursive: true });
  const state = path.join(tmp, 'positions.risk-state.json');
  const env = path.join(tmp, envName);
  fs.writeFileSync(state, stateText);
  if (envText !== null) fs.writeFileSync(env, envText);
  const origConsole = { log: console.log, error: console.error };
  console.log = () => {};
  console.error = () => {};
  let code;
  try {
    code = run(state, env, apply);
  } finally {
    console.log = origConsole.log;
    console.error = origConsole.error;
  }
  return {
    code,
    state: fs.readFileSync(state, 'utf8'),
    env: fs.existsSync(env) ? fs.readFileSync(env, 'utf8') : null,
    statePath: state,
    envPath: env,
    backups: fs.readdirSync(tmp).filter((f) => f.includes('.pre-systemic-migration-')),
  };
}

/** The expected .env diff for LEGACY_STATE against FIXTURE_ENV (the contract
 * the task pins): the stale managed line is replaced IN PLACE (position 2,
 * the env_file.rs upsert posture — position and the rest of the file
 * preserved), the four other keys append at the end, the keeper line is
 * untouched. */
function expectedEnvLines() {
  return [
    'BK_TEST_KEY_KEEP_ME=1',
    'BK_RISK_MAX_POSITION_SIZE="2"', // stale "999" replaced in place
    'BK_RISK_MAX_SINGLE_LOSS_USD="0.75"',
    'BK_RISK_MAX_DAILY_DRAWDOWN_USD="5"',
    'BK_RISK_MAX_CONSECUTIVE_LOSSES="4"',
    'BK_RISK_COOLDOWN_MINUTES="30"',
  ];
}

function selfTest() {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'bk-riskstate-mig-'));
  let pass = 0;
  let fail = 0;
  const check = (name, ok, detail = '') => {
    if (ok) {
      pass += 1;
      console.log(`  ok   ${name}`);
    } else {
      fail += 1;
      console.error(`  FAIL ${name}${detail ? ` — ${detail}` : ''}`);
    }
  };

  // 1. dry-run writes NOTHING (byte-identical files, no backup).
  const dry = runIn(path.join(tmp, 'dry'), LEGACY_STATE, FIXTURE_ENV, false);
  check('dry-run exits 0', dry.code === 0);
  check('dry-run leaves the risk-state file untouched', dry.state === LEGACY_STATE);
  check('dry-run leaves .env untouched', dry.env === FIXTURE_ENV);
  check('dry-run creates no backup', dry.backups.length === 0, dry.backups.join(','));

  // 2. apply: the exact expected .env diff + the rewrite + the backup.
  const apply = runIn(path.join(tmp, 'apply'), LEGACY_STATE, FIXTURE_ENV, true);
  check('apply exits 0', apply.code === 0);
  check(
    '.env diff is exactly the expected lines (keeper preserved, stale value replaced in place)',
    apply.env !== null && apply.env.split('\n').filter((l) => l !== '').join('\n') === expectedEnvLines().join('\n'),
    apply.env,
  );
  check('no BK_RISK_ line was appended twice', (apply.env.match(/BK_RISK_MAX_POSITION_SIZE=/g) || []).length === 1);
  const doc = JSON.parse(apply.state);
  check('rewritten file has NO "systemic" key', !('systemic' in doc));
  check(
    'rewritten file keeps the envelope: savedAtMs / limits / kill / accountFreezes / autoFrozen',
    doc.savedAtMs === 1_762_000_000_000 &&
      doc.limits?.maxOrderNotional === '250' &&
      doc.kill?.reason === 'operator pause' &&
      doc.accountFreezes?.acct_a === 'manual hold' &&
      doc.autoFrozen?.acct_b === 1_762_000_000_001,
    apply.state,
  );
  check('rewritten file keeps the envelope SHAPE the loader parses (no surprises)',
    Object.keys(doc).every((k) => ENVELOPE_FIELDS.includes(k)), Object.keys(doc).join(','));
  check('exactly one backup, byte-identical to the original', apply.backups.length === 1 && fs.readFileSync(path.join(tmp, 'apply', apply.backups[0]), 'utf8') === LEGACY_STATE);

  // 3. the retired globals and the skips NEVER reach .env.
  for (const banned of ['BK_RISK_MAX_TOTAL_POSITION', 'BK_RISK_MAX_TOTAL_EXPOSURE_USD', 'BK_RISK_MAX_CORRELATION_USD', 'BK_RISK_GLOBAL_KILL_SWITCH_LOSS_USD']) {
    check(`retired-by-#983 key absent from .env: ${banned}`, !apply.env.includes(banned));
  }

  // 4. idempotence: the second apply is a no-op (no new backup, byte-identical).
  const again = runIn(path.join(tmp, 'again'), apply.state, apply.env, true);
  check('second run exits 0 (nothing to migrate)', again.code === 0);
  check('second run leaves .env byte-identical', again.env === apply.env);
  check('second run leaves the risk-state file byte-identical', again.state === apply.state);
  check('second run creates NO backup', again.backups.length === 0, again.backups.join(','));

  // 5. corrupt file fails closed: exit 1, nothing written anywhere.
  const corrupt = runIn(path.join(tmp, 'corrupt'), '{"limits": {"maxOrderNotional"', FIXTURE_ENV, true);
  check('corrupt file exits 1', corrupt.code === 1);
  check('corrupt file: .env untouched', corrupt.env === FIXTURE_ENV);
  check('corrupt file: no backup', corrupt.backups.length === 0);

  // 6. the closed loop with #990: the migrated file cannot trip the load
  // warning — the predicate (persist.rs load_risk_state_file) is literally
  // `raw.get("systemic").is_some()` on the parsed doc.
  check('#990 load predicate stays quiet on the migrated file', JSON.parse(apply.state).systemic === undefined);
  check('#990 load predicate DOES fire on the legacy file (the problem is real)', JSON.parse(LEGACY_STATE).systemic !== undefined);

  // 7. absent .env: apply creates it with just the migrated keys, 0600.
  const fresh = runIn(path.join(tmp, 'fresh'), LEGACY_STATE, null, true);
  check('apply creates a missing .env with exactly the five migrated keys',
    fresh.env !== null && fresh.env.split('\n').filter((l) => l !== '').length === 5, fresh.env);
  check('created .env is 0600', (fs.statSync(fresh.envPath).mode & 0o777) === 0o600);

  console.log(fail === 0 ? `\nSELF-TEST OK — ${pass} checks pass.` : `\nSELF-TEST FAILED — ${fail}/${pass + fail} checks wrong.`);
  return fail === 0 ? 0 : 1;
}

// ── CLI ──────────────────────────────────────────────────────────────────────

function main() {
  const { values, positionals } = parseArgs({
    allowPositionals: true,
    options: {
      env: { type: 'string' },
      apply: { type: 'boolean', default: false },
      'self-test': { type: 'boolean', default: false },
      help: { type: 'boolean', default: false },
    },
  });
  if (values.help) {
    console.log(
      'usage: node scripts/migrate-riskstate-systemic.mjs <risk-state.json> [--env <path>] [--apply]\n' +
        '       node scripts/migrate-riskstate-systemic.mjs --self-test',
    );
    return 0;
  }
  if (values['self-test']) return selfTest();
  const statePath = positionals[0];
  if (!statePath) {
    console.error('usage: node scripts/migrate-riskstate-systemic.mjs <risk-state.json> [--env <path>] [--apply]');
    return 2;
  }
  return run(path.resolve(statePath), path.resolve(values.env ?? '.env'), values.apply);
}

// The ci-lane-planner guard style: basename match, so a path with spaces
// (`/Volumes/Hard Disk/...`) still compares equal to the module URL.
if (process.argv[1] && import.meta.url.endsWith(process.argv[1].split('/').pop())) {
  process.exit(main());
}

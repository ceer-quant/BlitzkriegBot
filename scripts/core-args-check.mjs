#!/usr/bin/env node
/**
 * What the core does with an argument it does not recognise (#228).
 *
 * The incident this exists to prevent: the fallback arm of `parse_args` printed
 * `ignoring unknown argument: <token>` and started the core ANYWAY, so
 *
 *   * `--readonly` (misspelled `--read-only`, `--redonly`, …) started a core in
 *     WRITE mode — the one spelling an operator types to make a process safe was
 *     the one that silently removed the safety;
 *   * `--max-order-notional` (and every other limit) fell back to its shipped
 *     default, so a typo turned a deliberate cap into the compiled one;
 *   * a stale binary with no `--version` support started a core when a probe
 *     asked it for its version (the #197 hole, closed there for that one flag).
 *
 * `--help` exited 2 for failing to parse, and nothing in the tree could tell an
 * operator which spellings this build actually accepts.
 *
 * So this gate drives the REAL release binary and asserts the four properties
 * the fix promises, each one on the artifact that ships:
 *
 *   1. `--help` / `-h` — exit 0, and the list names EVERY flag the parser in
 *      `core/blitzkrieg_core/src/main.rs` accepts (both directions: no accepted
 *      flag unlisted, no listed flag unaccepted). A help list that drifts from
 *      the parser is this defect in a new spelling.
 *   2. `--version` / `-V` — exit 0, `<semver>+g<sha>`, and NO boot (#197's
 *      short-circuit, asserted on the shipped binary rather than by reading it).
 *   3. an unknown argument — exit 2 with the argument named and the closest
 *      legal spelling suggested, the process gone on its own, and ZERO files
 *      written: the `--trade-log` / `--order-log` / `--position-log` paths point
 *      into a scratch directory that must still be empty afterwards (#199's
 *      write latch plus the boot path are what actually write).
 *   4. the safety spellings — `--read-only`, `--redonly`, `--max-notinal` — are
 *      refused with the flag that was meant, and the refusal says what leaving it
 *      out would have meant. `--allow-unknown-args` is the documented opt-out for
 *      a benign unknown argument and still does NOT cover these (`--redonly` under
 *      the hatch stays a refusal; the hatch cannot re-open #228).
 *
 * Every run is spawned with `--mode dry`, a socket under the system temp dir and
 * a sandbox cwd, and is killed by process group on timeout (`lib/child-guard`),
 * so a REGRESSION — a binary that boots instead of refusing — is stopped here
 * rather than left serving. That is also why the assertions are on "the process
 * exited by itself" (not just on the exit code): under the old behaviour the
 * child was still alive, which is exactly the state this gate has to catch.
 *
 * Usage:
 *   node scripts/core-args-check.mjs
 *   BK_CORE_BIN=/elsewhere/blitzkrieg-core node scripts/core-args-check.mjs
 *
 * Exit 0 only if every check passes.
 */
import { spawn } from './lib/child-guard.mjs';
import { appendFileSync, mkdtempSync, readdirSync, readFileSync, existsSync } from 'fs';
import { tmpdir } from 'os';
import { dirname, join } from 'path';
import { fileURLToPath } from 'url';
import { coreBinaryPath, checkCoreProvenance, VERSION_RE } from './lib/core-provenance.mjs';
import { createChecks } from './lib/gate-harness.mjs';
import { sleep } from './lib/wait.mjs';

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..');
const BIN = coreBinaryPath(ROOT);
const summaryLines = [];
function summary(text) {
  summaryLines.push(text);
  if (process.env.GITHUB_STEP_SUMMARY) {
    try { appendFileSync(process.env.GITHUB_STEP_SUMMARY, text + '\n'); } catch { /* best effort */ }
  }
}

const gate = createChecks();
const { check } = gate;

/**
 * The flags the parser accepts, read from the source the binary was built from.
 *
 * The extraction mirrors the Rust unit test in `core/blitzkrieg_core/src/cli.rs`
 * (`flags_matched_by_the_parser`): the parse loop's own arms, located by markers
 * rather than by line numbers. If a marker is gone the gate CANNOT verify what it
 * was written to verify, so it fails and says which marker moved — the alternative
 * (comparing the help list against an empty set) would pass forever.
 */
function parserFlags() {
  const src = readFileSync(join(ROOT, 'core', 'blitzkrieg_core', 'src', 'main.rs'), 'utf8');
  const start = src.indexOf('let mut it = argv.iter().cloned();');
  const end = src.indexOf('// ── Resolve the file-settable settings');
  if (start < 0 || end < 0 || end <= start) {
    check('parse-loop markers found in core/blitzkrieg_core/src/main.rs', false,
      `the help/parser comparison cannot run (start=${start}, end=${end})`);
    return [];
  }
  const out = new Set();
  for (const line of src.slice(start, end).split('\n')) {
    const trimmed = line.trimStart();
    if (trimmed.startsWith('"--')) {
      const close = trimmed.indexOf('"', 1);
      if (close > 0) out.add(trimmed.slice(1, close));
    }
    // `--allow-unknown-args` is matched through the shared const, not a literal.
    if (trimmed.includes('blitzkrieg_core::cli::ALLOW_UNKNOWN_ARGS')) out.add('--allow-unknown-args');
  }
  if (out.size < 50) {
    check('the parse-loop extraction found enough flags to be the parse loop', false,
      `found only ${out.size}`);
  }
  return [...out];
}

/** The flags the shipped `--help` output lists: an entry line is `  --flag…`. */
function helpFlags(help) {
  const out = new Set();
  for (const line of help.split('\n')) {
    const m = /^ {2}(--[a-z][a-z0-9-]*)/.exec(line);
    if (m) out.add(m[1]);
  }
  return [...out];
}

/**
 * Run the binary in `cwd`, bounded by `graceMs`. `killed` is the interesting
 * field: a refusal finishes in milliseconds, a boot does not finish at all.
 */
async function run(args, { cwd, graceMs = 5000, env = {} } = {}) {
  const child = spawn(BIN, args, {
    cwd,
    env: { ...process.env, BK_CONFIG: 'none', ...env },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  let out = '';
  let err = '';
  child.stdout?.setEncoding('utf8');
  child.stderr?.setEncoding('utf8');
  child.stdout?.on('data', (d) => { out += d; });
  child.stderr?.on('data', (d) => { err += d; });
  let settle;
  const done = new Promise((res) => { settle = res; });
  child.on('exit', (code, signal) => settle({ code, signal }));
  child.on('error', (e) => settle({ code: null, signal: null, spawnError: String(e?.message ?? e) }));
  const outcome = await Promise.race([done, sleep(graceMs).then(() => null)]);
  let killed = false;
  if (outcome === null) {
    killed = true;
    // The child leads its own group (`lib/child-guard` spawns detached), so the
    // group is what has to go — a core that forked anything would otherwise be
    // left holding its socket while the gate reports a refusal it never got.
    try { process.kill(-child.pid, 'SIGKILL'); } catch { try { child.kill('SIGKILL'); } catch { /* already gone */ } }
    await Promise.race([done, sleep(3000)]);
  }
  await sleep(20); // let a final pipe chunk land before it is read
  return { ...(outcome ?? {}), out, err, killed, args };
}

/** A sandbox with the ledger paths the core would write to if it booted. */
function sandbox(tag) {
  const dir = mkdtempSync(join(tmpdir(), `coreargs-${tag}-`));
  return {
    dir,
    socket: join(dir, 'core.sock'),
    ledger: join(dir, 'ledger'),
  };
}

const ledgerArgs = (box) => [
  '--trade-log', join(box.ledger, 'trades.jsonl'),
  '--order-log', join(box.ledger, 'orders.jsonl'),
  '--position-log', join(box.ledger, 'positions.jsonl'),
];

/** The isolated arguments a refusal run carries, so a REGRESSION cannot touch anything real. */
const isolated = (box) => ['--mode', 'dry', '--socket', box.socket, '--no-discovery'];

/** Everything a sandbox contains — logs, data dir, lock file, socket. Any entry is a write. */
function sandboxEntries(box) {
  const seen = [];
  for (const p of [box.dir, box.ledger]) {
    if (!existsSync(p)) continue;
    for (const e of readdirSync(p)) seen.push(join(p, e));
  }
  return seen.sort();
}

console.log('CORE ARGUMENT HANDLING (#228)');
checkCoreProvenance(BIN, check);
summary(`- binary under test: \`${BIN}\``);

// ── 1. --help / -h: exit 0, and the list is the parser's list ────────────────
{
  const box = sandbox('help');
  // `--help`/`--version` are handled in `main` BEFORE `parse_args` (#197), so the
  // parse loop the extractor reads does not contain them — they are listed by
  // `--help` all the same, and that is the whole set this gate compares.
  const EARLY = ['--help', '--version'];
  const wanted = [...parserFlags(), ...EARLY];
  const bootBanner = (r) => /per-order bounds in force|listening on/.test(r.err);
  for (const flag of ['--help', '-h']) {
    const r = await run([flag], { cwd: box.dir });
    check(`${flag} exits 0`, !r.killed && r.code === 0,
      `code=${r.code} killed=${r.killed} stderr=${JSON.stringify(r.err.slice(-200))}`);
    const listed = helpFlags(r.out);
    const missing = wanted.filter((f) => !listed.includes(f));
    const extra = listed.filter((f) => !wanted.includes(f));
    check(`${flag} lists every flag the parser accepts (${wanted.length})`,
      wanted.length > 0 && missing.length === 0,
      `missing from --help: ${missing.join(', ') || '(none)'}`);
    check(`${flag} lists nothing the parser rejects`, extra.length === 0,
      `listed but not accepted: ${extra.join(', ') || '(none)'}`);
    check(`${flag} does not boot and writes no file`,
      !bootBanner(r) && sandboxEntries(box).length === 0,
      `boot banner in stderr=${bootBanner(r)} wrote=${sandboxEntries(box).join(', ')}`);
  }
  const help = (await run(['--help'], { cwd: box.dir })).out;
  for (const spelling of ['--readonly', '--max-order-notional', '--allow-unknown-args', '--mode', '--config']) {
    check(`--help names ${spelling}`, help.includes(spelling), 'the spelling is not in the list');
  }
  // One sentence each, not a bare name: every entry line is followed by an indented line.
  const entryLines = help.split('\n').filter((l) => /^ {2}--/.test(l)).length;
  const descLines = help.split('\n').filter((l) => /^ {6}\S/.test(l)).length;
  check('every --help entry carries a description', descLines >= entryLines,
    `${entryLines} entries, ${descLines} descriptions`);
  summary(`- \`--help\`: ${entryLines} flags listed, each with a sentence`);
}

// ── 2. --version / -V: exit 0, a revision, no boot ───────────────────────────
{
  const box = sandbox('version');
  for (const flag of ['--version', '-V']) {
    const r = await run([flag], { cwd: box.dir });
    check(`${flag} exits 0 with <semver>+g<sha>`, !r.killed && r.code === 0 && VERSION_RE.test(r.out.trim()),
      `code=${r.code} killed=${r.killed} out=${JSON.stringify(r.out.slice(0, 80))}`);
    check(`${flag} does not boot`, sandboxEntries(box).length === 0 && !r.err.includes('listening'),
      `wrote=${sandboxEntries(box).join(', ')}`);
  }
}

// ── 3. an unknown argument: refused, named, and zero writes ──────────────────
{
  const box = sandbox('unknown');
  const r = await run([...isolated(box), ...ledgerArgs(box), '--definitely-not-a-flag'], { cwd: box.dir });
  check('an unknown argument exits 2 without booting', !r.killed && r.code === 2,
    `code=${r.code} killed=${r.killed} stderr=${JSON.stringify(r.err.slice(-200))}`);
  check('the refusal names the argument', r.err.includes('--definitely-not-a-flag'),
    JSON.stringify(r.err.slice(-200)));
  check('the refusal is a refusal, not a warning', /refusing to start/.test(r.err) && !/ignoring/.test(r.err),
    JSON.stringify(r.err.slice(-200)));
  const wrote = sandboxEntries(box);
  check('an unknown argument writes NO ledger file', wrote.length === 0,
    `wrote: ${wrote.join(', ')}`);

  const positional = await run([...isolated(box), 'start'], { cwd: box.dir });
  check('a stray positional argument exits 2 without booting',
    !positional.killed && positional.code === 2,
    `code=${positional.code} killed=${positional.killed}`);
}

// ── 4. the safety spellings: refused, with the spelling that was meant ───────
{
  const safety = [
    ['--read-only', '--readonly'],
    ['--redonly', '--readonly'],
    ['--max-notinal', '--max-order-notional'],
    ['--max-daily-lost', '--max-daily-loss'],
  ];
  for (const [typo, meant] of safety) {
    const box = sandbox('safety');
    const r = await run([...isolated(box), ...ledgerArgs(box), typo], { cwd: box.dir });
    check(`${typo} is refused without booting`, !r.killed && r.code === 2,
      `code=${r.code} killed=${r.killed} stderr=${JSON.stringify(r.err.slice(-200))}`);
    check(`${typo} is answered with ${meant}`, r.err.includes(meant) && r.err.includes('did you mean'),
      JSON.stringify(r.err.slice(-200)));
    check(`${typo} writes no ledger file`, sandboxEntries(box).length === 0,
      `wrote: ${sandboxEntries(box).join(', ')}`);
  }

  // The risk consequence: an operator has to be able to see, in the refusal, what
  // running without the flag would have meant.
  const box = sandbox('risk');
  const r = await run([...isolated(box), '--redonly'], { cwd: box.dir });
  check('a safety near-miss states what leaving the flag out would have meant',
    /WITHOUT it/.test(r.err) && /WRITE mode/.test(r.err),
    JSON.stringify(r.err.slice(-400)));
  summary('- a safety-flag typo is refused with the flag meant AND the consequence of losing it');
}

// ── 5. the escape hatch: benign unknowns only, never a safety near-miss ──────
{
  const box = sandbox('hatch');
  // `--max-orderbook-stale-ms -1` is an invalid VALUE, so the boot stops after
  // parsing: it proves the ignored argument was parsed and skipped rather than
  // the process booting.
  const r = await run([
    '--allow-unknown-args', '--definitely-not-a-flag', ...isolated(box),
    '--max-orderbook-stale-ms', '-1',
  ], { cwd: box.dir });
  check('--allow-unknown-args lets a benign unknown argument through (warned, not refused)',
    !r.killed && r.code === 2 && /ignoring/.test(r.err) && !/refusing to start/.test(r.err)
      && r.err.includes('--definitely-not-a-flag'),
    `code=${r.code} killed=${r.killed} stderr=${JSON.stringify(r.err.slice(-300))}`);
  check('the ignored argument is reported as NOT in force', /NOT in force/.test(r.err),
    JSON.stringify(r.err.slice(-300)));

  const safe = await run(['--allow-unknown-args', ...isolated(box), '--redonly'], { cwd: box.dir });
  check('--allow-unknown-args does NOT cover a misspelled safety flag',
    !safe.killed && safe.code === 2 && /refusing to start/.test(safe.err) && safe.err.includes('--readonly'),
    `code=${safe.code} killed=${safe.killed} stderr=${JSON.stringify(safe.err.slice(-300))}`);
  check('the hatch cannot write a ledger file', sandboxEntries(box).length === 0,
    `wrote: ${sandboxEntries(box).join(', ')}`);
}

console.log(gate.failures === 0
  ? '\nCORE ARGUMENT HANDLING OK — an unknown argument stops the boot, --help is the flag list, ' +
    'and a safety typo cannot start a core.'
  : `\nCORE ARGUMENT HANDLING FAILED (${gate.failures})`);
if (gate.failures !== 0) summary(`- **argument-handling gate failed (${gate.failures})**`);
process.exit(gate.failures === 0 ? 0 : 1);

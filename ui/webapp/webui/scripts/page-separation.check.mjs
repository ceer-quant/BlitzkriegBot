/**
 * E11/D-32 — the「插件页/策略页内容互不出现」验收, executable and bidirectional.
 *
 * The separation shipped in #147 (plugins page stopped listing strategies), but
 * a one-sided gate would let it silently regress in exactly the two ways it
 * historically broke: a strategy metric creeping back onto the plugins page,
 * or a plugin registry section creeping onto the strategies page. So the gate
 * asserts BOTH directions, with the positive content pinned too — a page that
 * accidentally renders nothing would otherwise also "pass" a negative-only
 * assertion.
 *
 *   cd ui/webapp/webui && npm run check:separation
 */
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const here = dirname(fileURLToPath(import.meta.url))
const read = (...p) => readFileSync(join(here, '..', ...p), 'utf8')

let failures = 0
const check = (label, fn) => {
  try {
    fn()
    console.log(`  ok   ${label}`)
  } catch (e) {
    failures++
    console.log(`  FAIL ${label}\n       ${e.message}`)
  }
}

const plugins = read('src', 'pages', 'Plugins.vue')
const strategies = read('src', 'pages', 'Strategies.vue')

console.log('plugins page — external capabilities ONLY')

check('still lists both plugin kinds (positive content)', () => {
  assert.ok(plugins.includes("label: '扩展插件'"), 'extension section gone')
  assert.ok(plugins.includes("label: '行情插件'"), 'market section gone')
})

check('no strategy rows, no strategy metrics, no strategy toggle', () => {
  // store.strategyRows is the strategy page's whole data source; its presence
  // here is exactly the pre-#147 defect. The metric vocabulary (placed/wins/
  // PnL columns) is what a regression would drag in with it.
  for (const leak of [
    'store.strategyRows',
    'StrategyStatsRow',
    'ordersPlaced',
    'netPnlUsd',
    'closedTrades',
    'winRate',
    '策略表现',
    '拒单',
  ]) {
    assert.ok(!plugins.includes(leak), `strategies vocabulary on plugins page: ${leak}`)
  }
})

check('插件页 header states the single-source-of-truth rule', () => {
  // The E9-g doc comment is the contract; keep it pinned so a rewrite of the
  // page header cannot restate the wrong rule.
  assert.ok(/策略页是策略的唯一入口/.test(plugins), 'rule statement missing')
})

console.log('strategies page — the strategy ledger ONLY')

check('renders the strategy table and its fleet KPIs (positive content)', () => {
  assert.ok(strategies.includes('store.strategyRows'), 'strategy rows not rendered')
  assert.ok(strategies.includes('策略表现'), 'strategy table header gone')
})

check('no plugin registry content, no plugin icons', () => {
  // `Puzzle` is the plugins page's identity icon and `marketPlugins`/
  // `extensions` are its data source; either appearing here means a registry
  // section leaked onto the strategy ledger.
  for (const leak of ['marketPlugins', 'plugins?.extensions', 'store.plugins', 'Puzzle']) {
    assert.ok(!strategies.includes(leak), `plugin vocabulary on strategies page: ${leak}`)
  }
})

console.log('nav — the two pages stay separate entry points')

check('shell nav keeps both pages as distinct tabs', () => {
  const shell = read('src', 'App.vue')
  assert.ok(/id: 'strategies', label: '策略'/.test(shell), 'no strategy tab')
  assert.ok(/id: 'plugins', label: '插件'/.test(shell), 'no plugin tab')
})

console.log(failures === 0 ? '\nRESULT: PASS' : `\nRESULT: FAIL (${failures})`)
process.exit(failures === 0 ? 0 : 1)

/**
 * 绑定即启用 — account↔venue pairing presentation gate.
 *
 * The user's ruling (2026-10-16): a market plugin's enabled-set is DERIVED
 * from the accounts that hold a venue binding — adding an exchange's API
 * IS enabling its plugin. The user's follow-up ruling (2026-10-19): the
 * binding is CREATE-ONLY — an existing account never edits its venue
 * (rebinding mid-life scrambles the data); delete-and-recreate is the only
 * exit. On the panel this surfaces as these contracts:
 *
 *   1. AccountSwitcher shows the binding READ-ONLY and offers delete; the
 *      per-row binding editor is gone, and no panel entry may call the
 *      `account.venue.set` proxy (the verb and its route stay at the
 *      gateway — the UI door is what closed).
 *   2. The 行情 page never grows its own exchange switcher: its venue view
 *      is derived from the ACTIVE ACCOUNT's binding (filterByVenue), the
 *      same philosophy as filterByAccount — unattributed rows stay.
 *   3. The balance tile labels itself with the account id when more than
 *      one account exists — the tile reads the process-active ledger, not
 *      the switched session, and must not masquerade as per-account.
 *
 *   cd ui/webapp/webui && npm run check:pairing
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

console.log('绑定即启用 — the account↔venue pairing contract')

// ── 1. the binding is create-only: the switcher is a read-only face ──────
const switcher = read('src/components/AccountSwitcher.vue')

check('AccountSwitcher has NO per-row binding editor (create-only ruling)', () => {
  // The edit state machine and its entry points are deleted.
  for (const gone of ['venueEditing', 'venueDraft', 'editVenue', 'saveVenue', 'loadVenueOptions']) {
    assert.doesNotMatch(switcher, new RegExp(gone), `${gone} must stay deleted`)
  }
  assert.doesNotMatch(switcher, /accountVenueSet\(/, 'no panel entry may call the account.venue.set proxy')
  // The read-only display survives (venue 只读显示保留).
  assert.match(switcher, /venueLabel\(/, 'the read-only venue display must survive')
})

check('the binding verb stays out of the text command channel too', () => {
  assert.doesNotMatch(switcher, /method:\s*'account\.venue\.set'/, 'JSON-RPC envelope via api.command must not return')
  assert.doesNotMatch(switcher, /account\.venue\.set\s*$/, 'no text-verb call either')
})

check('the binding write path still exists for the CREATE dialog (client level)', () => {
  // The gateway route and client helper stay: AddAccountDialog-era creates
  // bind at creation. Only the per-row edit surface is gone.
  const client = read('src/api/client.ts')
  assert.match(
    client,
    /accountVenueSet[^`]*'\/account-venue\/set'/,
    'client must keep the /account-venue/set proxy route',
  )
})

check('the pairing rationale is stated in prose (绑定即启用 + 删户重建)', () => {
  assert.match(switcher, /绑定即启用/, 'footer must state the derivation rule')
  assert.match(switcher, /删户重建/, 'footer must name delete-and-recreate as the only rebinding exit')
})

// ── 1b. the add-account dialog (#517): 选交易所 → 填 API → account.create ──
const dialog = read('src/components/AddAccountDialog.vue')

check('AddAccountDialog creates the identity through account.create', () => {
  assert.match(dialog, /accountCreate\(/, 'the dialog must call api.accountCreate')
  assert.match(dialog, /venue:\s*venue\.value/, 'the chosen venue must ride the create call (绑定即启用)')
  const client = read('src/api/client.ts')
  assert.match(client, /accountCreate[^`]*'\/account-create'/, 'client must post to /account-create')
})

check('credentials ride market.credentials.set, never the create call', () => {
  // The credential VALUES flow only through market.credentials.set (kernel
  // cell, values never echo); the create call carries identity fields only.
  assert.doesNotMatch(dialog, /accountCreate\(\{[^}]*vars/, 'credentials must not ride account.create')
  assert.match(dialog, /marketCredentialsSet\(/, 'the dialog must call api.marketCredentialsSet for the API keys')
  assert.match(dialog, /type="password"/, 'credential inputs must not echo')
  // The var NAMES come from the kernel readout, not a UI-side table.
  assert.match(dialog, /marketCredentials\(\)/, 'required var names must come from the credentials status readout')
})

check('the venue picker is mandatory before anything can be submitted', () => {
  assert.match(dialog, /canSubmit[^=]*=[^>]*=>[^>]*venue\.value\.trim\(\) !== ''/, 'empty venue must disable the submit button')
})

// ── 1c. account removal (#517 / #535): 只删不改，所有账户都可删 ─────────────
check('account removal is delete-only behind a confirm-word gate', () => {
  assert.match(switcher, /accountRemove\(\{[^}]*id:\s*a\.id/, 'the remove path must name the row account')
  assert.match(switcher, /ConfirmWordDialog/, 'removal must go through the confirm-word dialog')
  assert.match(switcher, /word="removeTarget\?\.id/, 'the confirm word must be the account id itself')
  assert.doesNotMatch(switcher, /editAccount|renameAccount/, 'accounts are delete-only: no edit/rename surface may exist')
})

check('every account is removable — no default interception anywhere', () => {
  // The 2026-10-19 ruling: the last account (not `default`) is what the
  // kernel refuses. No UI-side interception of the default id may remain.
  assert.doesNotMatch(switcher, /id\s*===\s*'default'/, "the askRemove default-id interception must stay deleted")
  assert.doesNotMatch(switcher, /!==\s*'default'/, "no v-if !== 'default' may gate the remove button")
  assert.match(
    switcher,
    /最后一个账户内核会拒绝/,
    'the confirm dialog must name the real refusal face (positions + last account)',
  )
  // The remove button is unconditional (every row gets it).
  const btn = switcher.match(/<button[^>]*@click\.stop="askRemove\(a\)"[^>]*>/)?.[0] ?? ''
  assert.ok(btn.length > 0, 'the per-row remove button must exist')
  assert.doesNotMatch(btn, /v-if/, 'the remove button must not be conditionally gated per row')
})

// ── 2. the 行情 page derives its venue view from the active account ──────
const hft = read('src/pages/HftPage.vue')

check('HftPage venue filter derives from the active account binding', () => {
  assert.match(hft, /filterByVenue/, 'filterByVenue not imported/used')
  assert.match(
    hft,
    /activeAccountVenue/,
    'venue view must come from store.activeAccountVenue (the active account), not a local control',
  )
})

check('HftPage has NO independent exchange switcher', () => {
  // The user's ruling: the account switcher IS the venue switcher. A second
  // control would fork the truth — forbid a dedicated select/combobox.
  const selects = hft.match(/v-model="[^"]*[Vv]enue[^"]*"/g) ?? []
  assert.equal(selects.length, 0, `HftPage binds its own venue control: ${selects}`)
})

check('filter semantics: unattributed rows survive (shared philosophy)', () => {
  const lib = read('src/lib/account-filter.ts')
  const fn = lib.match(/export function filterByVenue[\s\S]*?\n\}/)?.[0] ?? ''
  assert.ok(fn.length > 0, 'filterByVenue missing from account-filter.ts')
  assert.match(fn, /!r\.venue \|\| r\.venue === venue/, 'unattributed rows must stay')
  // No active venue or a venue-less row set = pass-through with attributed:false.
  assert.match(fn, /if \(!venue\) return/, 'empty venue must be a pass-through')
  assert.match(fn, /attributed: false/, 'the honest label must exist')
})

check('the filtered badge names the venue, the unfiltered one does not', () => {
  assert.match(hft, /已按账户绑定的交易所过滤/, 'venue-filter badge copy missing')
})

check('balance tile is labeled with the account id under multi-account', () => {
  // balanceAccountSuffix must append the id, and the note must disclose the
  // tile reads the process-active ledger (not the switched session's book).
  assert.match(hft, /balanceAccountSuffix/, 'suffix helper missing')
  const note = hft.match(/balanceAccountNote[\s\S]*?\n\}\)/)?.[0] ?? ''
  assert.match(note, /accounts\.length <= 1/, 'single-account runs must stay untouched')
  assert.match(note, /进程默认账户|不会改写/, 'the disclosure must say what the tile actually reads')
})

check('empty state under venue filtering names the venue', () => {
  assert.match(hft, /暂无 \$\{activeVenue\} 的持仓/, 'venue-aware empty state missing')
})

// ── 3. Strategy page: fixed action columns (user ruling 2026-10-16) ──────
const strat = read('src/pages/Strategies.vue')

check('strategy-name cell: actions pinned in a fixed column, not flowing after the title', () => {
  // The name cell is a two-track grid (name truncates in track 1; the edit
  // button / incompatible badge live in a fixed-width right track) — so the
  // button's x does not move with the name's length.
  assert.match(
    strat,
    /grid-cols-\[minmax\(0,1fr\)_auto\]/,
    'name cell must be a name|actions grid',
  )
  assert.match(
    strat,
    /justify-end gap-2/,
    'actions must sit in a justify-end track',
  )
})

check('tunables rows: fixed-width grid so inputs do not drift', () => {
  // Each knob row is a three-column fixed grid (name / input / range) —
  // every input lands on the same x regardless of knob name length.
  assert.match(
    strat,
    /grid-cols-\[minmax\(0,9rem\)_5\.5rem_6rem\]/,
    'knob rows must use the fixed 3-column grid',
  )
  assert.doesNotMatch(
    strat,
    /min-w-0 flex-1 truncate text-muted-fg/,
    'the old flex-1 name (input x drifts) is forbidden',
  )
})

if (failures > 0) {
  console.log(`\nRESULT: FAIL — ${failures} check(s) red`)
  process.exit(1)
}
console.log('\nRESULT: PASS — bindings are create-only (read-only face + delete), every account is removable but the last, the 行情 page derives its venue view, the balance tile is honestly labeled, strategy rows are fixed-grid')

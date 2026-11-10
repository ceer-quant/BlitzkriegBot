/**
 * Regression check for the panel's trading-safety banners (#236, #232).
 *
 * The defect #236 pins is a *silent* one, so this check is written against the
 * reading rules rather than against a screenshot: the kernel froze trading, the
 * panel drew nothing, and no layer reported a problem — the UI Kit's
 * `EngineStatsView` dropped the keys between the kernel and the page. The Rust
 * side now has a seam test (`core/blitzkrieg_core/tests/engine_stats_view_seam.rs`)
 * that hands the kernel's real payload to that view type; this check covers what
 * the page does with it once it arrives.
 *
 * It also holds the two TITLE fixes in place, because a title is a claim about
 * the data (#232):
 *
 *   - the trading-error banner may not name a source. The slot it reads carries
 *     a venue refusal, a safety-net failure and the kernel's own leg refusals,
 *     so "（venue）" sent operators to the exchange for a local problem.
 *   - the connection banner may not call the gateway's own "core not reachable"
 *     string an engine error. That one sent operators to the engine log when the
 *     panel could not reach the core at all.
 *
 *   cd ui/webapp/webui && npm run check:safety
 */
import assert from 'node:assert/strict'

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

const {
  connectionBanner, freezeBanner, lastErrorBanner, selfCheckBanner, archiveStoppedBanner,
  accountFreezeBanners, accountFrozen, overdueReceivableBanner,
} = await import('../src/lib/safety.ts')

console.log('trading-safety banners — the three states that used to be invisible')

check('a frozen kernel raises the freeze banner with the kernel\'s reason', () => {
  const b = freezeBanner({
    tradingFrozen: { active: true, reason: '5 consecutive venue rejections; last: auth failed' },
  })
  assert.ok(b, 'the freeze banner is the one thing that must never be silent')
  assert.equal(b.tone, 'error', 'a freeze is red, not a warning')
  assert.match(b.body, /consecutive venue rejections/)
})

check('a live kernel raises no freeze banner', () => {
  assert.equal(freezeBanner({ tradingFrozen: { active: false } }), null)
  // An older core omits the key entirely — absence means "unknown", and the
  // panel shows nothing rather than inventing a freeze.
  assert.equal(freezeBanner({}), null)
  assert.equal(freezeBanner(null), null)
})

check('the refusal banner reads the structured record and shows its code', () => {
  const b = lastErrorBanner({
    lastError: { tsMs: 1_000_000_000_000, code: 'NOT_AUTHENTICATED', message: 'auth failed' },
  })
  assert.ok(b)
  assert.equal(b.body, 'auth failed')
  assert.equal(b.code, 'NOT_AUTHENTICATED', 'the kernel\'s code is DATA on screen')
})

check('the refusal banner title claims no source (#232)', () => {
  const structured = lastErrorBanner({
    lastError: { tsMs: 1, code: 'RISK_REJECTED', message: 'position cap reached' },
  })
  const legacy = lastErrorBanner({ lastVenueError: { tsMs: 1, message: 'RiskRejected: position cap' } })
  // RISK_REJECTED 结构化记录渲染为冻结标题（不命名来源，也不含 venue）
  assert.ok(structured)
  assert.equal(structured.title, '账户交易已经冻结', 'RISK_REJECTED title is the freeze title')
  assert.doesNotMatch(structured.title, /venue/i, `title must not pin a source: ${structured.title}`)
  assert.doesNotMatch(structured.body, /^引擎/, 'and the body is the kernel\'s message, not a claim')
  // legacy 字段走普通 refuse 分支，标题不变，也不命名来源
  assert.ok(legacy)
  assert.equal(legacy.title, '最近交易错误', 'legacy title unchanged')
  assert.doesNotMatch(legacy.title, /venue/i, `title must not pin a source: ${legacy.title}`)
  assert.doesNotMatch(legacy.body, /^引擎/, 'and the body is the kernel\'s message, not a claim')
})

check('an old core falls back to lastVenueError, and only then', () => {
  const b = lastErrorBanner({ lastVenueError: { tsMs: 1, message: 'NotAuthenticated: auth failed' } })
  assert.ok(b, 'a core that predates lastError still has something to show')
  assert.equal(b.code, undefined, 'the legacy string already carries its code as text')
  assert.equal(b.body, 'NotAuthenticated: auth failed')
  // The structured record wins when both are present — it is the same record,
  // and the structured one is the one with the code split out.
  const both = lastErrorBanner({
    lastError: { tsMs: 2, code: 'VENUE_ERROR', message: 'structured' },
    lastVenueError: { tsMs: 2, message: 'VenueError: structured' },
  })
  assert.equal(both.body, 'structured')
  assert.equal(both.code, 'VENUE_ERROR')
})

check('the freeze banner replaces the refusal banner rather than stacking', () => {
  const stats = {
    tradingFrozen: { active: true, reason: 'audit halted' },
    lastError: { tsMs: 1, code: 'VENUE_ERROR', message: 'auth failed' },
  }
  assert.ok(freezeBanner(stats))
  assert.equal(lastErrorBanner(stats, true), null, 'the freeze is the thing to act on')
  assert.ok(lastErrorBanner(stats, false), 'the record itself is not lost when unfrozen')
})

check('a failed self-check lists exactly the failing probes', () => {
  const b = selfCheckBanner({
    selfCheck: {
      ok: false,
      tsMs: 1,
      items: [
        { name: 'venue_balance', ok: false, detail: 'HTTP 401 unauthorized' },
        { name: 'reconcile_sweep', ok: true, detail: 'ok' },
      ],
    },
  })
  assert.ok(b)
  assert.equal(b.tone, 'error')
  assert.deepEqual(b.items.map((i) => i.name), ['venue_balance'], 'the passing probe is not a problem')
  assert.match(b.body, /1 项/)
  // A passing report is not a banner, and neither is a core that never ran one.
  assert.equal(selfCheckBanner({ selfCheck: { ok: true, tsMs: 1, items: [] } }), null)
  assert.equal(selfCheckBanner({}), null)
})

check('the connection banner says what it is: the panel cannot reach the core', () => {
  const b = connectionBanner({ lastError: 'core not reachable on /tmp/blitzkrieg-core.sock' })
  assert.ok(b)
  assert.doesNotMatch(b.title, /引擎/, 'snapshot.lastError is the GATEWAY\'s string, not the engine\'s')
  assert.match(b.title, /连接|内核/)
  assert.equal(connectionBanner({ lastError: null }), null)
})

check('a healthy snapshot raises nothing at all', () => {
  const stats = {
    venueRejected: 0,
    lastError: null,
    lastVenueError: null,
    selfCheck: null,
    tradingFrozen: { active: false },
    reconcile: { consecutiveSweepFailures: 0, freezeThreshold: 3 },
  }
  const snap = { lastError: null, stats }
  assert.equal(connectionBanner(snap), null)
  assert.equal(freezeBanner(stats), null)
  assert.equal(lastErrorBanner(stats), null)
  assert.equal(selfCheckBanner(stats), null)
  assert.equal(archiveStoppedBanner(stats), null)
})

// Issue #669 ②: the archive stopped recording and nothing on the panel said so
// — an external operator found five days of missing backtest corpus by hand.
// The banner is CONTINUOUS (reads stats.archive on every snapshot), names the
// kernel's stopped_reason, and stays dark for an older core (`archive: null`)
// or a recording one.
check('a stopped archive raises the corpus-loss banner with the kernel\'s reason', () => {
  const b = archiveStoppedBanner({
    archive: { path: '/data/events.jsonl', events: 1, bytes: 2, dropped: 3, recording: false, rotateBytes: 0, segmentBytes: 0, segments: 0, freeBytes: 0, stoppedReason: 'disk' },
  })
  assert.ok(b, 'a stopped archive must never be silent on the cockpit')
  assert.equal(b.tone, 'warn')
  assert.match(b.body, /停录/)
  assert.match(b.body, /disk/, 'the kernel\'s stopped_reason is DATA on screen')
  assert.match(b.body, /语料/)
})

check('a recording or absent archive raises no archive banner', () => {
  assert.equal(
    archiveStoppedBanner({
      archive: { path: '/data/events.jsonl', events: 1, bytes: 2, dropped: 0, recording: true, rotateBytes: 0, segmentBytes: 0, segments: 0, freeBytes: 9, stoppedReason: null },
    }),
    null,
  )
  // Older cores: the key is null (no archive configured) or missing.
  assert.equal(archiveStoppedBanner({ archive: null }), null)
  assert.equal(archiveStoppedBanner({}), null)
  assert.equal(archiveStoppedBanner(null), null)
})

// Issue #943: PER-ACCOUNT freezes. The kernel freezes ONE account on its
// venue-rejection streak while every other account keeps trading — the global
// `tradingFrozen` banner stays dark, so without these lines the panel looks
// perfectly normal to the operator of the frozen account. One urgent red line
// per frozen account, nothing for an old core or an empty map, and the
// countdown predicate (`accountFrozen`) gates the HFT page's clock.
	check('a per-account freeze is one urgent red line per frozen account (issue 943)', () => {
	  const many = accountFreezeBanners({
	    accountGates: [
	      { account: 'b-another', blocked: true, reason: '3 consecutive venue rejections' },
	      { account: 'aSome', blocked: true, reason: 'reason for a' },
	    ],
	    tradingFrozen: { active: false },
	  })
	  assert.equal(many.length, 2, 'two frozen accounts = two red lines')
	  for (const b of many) {
	    assert.equal(b.tone, 'error', 'a frozen account is red, not a warning')
	    assert.equal(b.urgent, true, 'its trading is down right now — pinned, no ✕')
	    assert.equal(b.identity, undefined, 'urgent banners carry no dismissal identity')
	  }
	  // accountGates is sorted by account id ascending: 'aSome' < 'b-another'
	  assert.match(many[0].title, /aSome/, 'the account id is in the title')
	  assert.match(many[0].body, /reason for a/, 'the kernel\'s freeze reason is DATA on screen')
	  assert.match(many[1].body, /3 consecutive venue rejections/)
	  assert.ok(
	    many.every((b) => b.hint && /解冻/.test(b.hint)),
	    'every banner names the unfreeze door (设置·风控)',
	  )
	})

	check('no per-account freezes renders nothing — old core, empty map, null stats', () => {
	  assert.deepEqual(accountFreezeBanners({ accountGates: [] }), [])
	  assert.deepEqual(accountFreezeBanners({ accountGates: null }), [])
	  assert.deepEqual(accountFreezeBanners({}), [])
	  assert.deepEqual(accountFreezeBanners(null), [])
	})

	check('the countdown pause predicate answers for the DISPLAYED account only', () => {
	  const stats = { accountGates: [ { account: 'acct-b', blocked: true, reason: 'streak' } ] }
	  assert.equal(accountFrozen(stats, 'acct-b'), true, 'the frozen displayed account pauses')
	  assert.equal(accountFrozen(stats, 'acct-a'), false, 'another account keeps its clock')
	  assert.equal(accountFrozen(stats, null), false, 'no displayed account never freezes')
	  assert.equal(accountFrozen({}, 'acct-b'), false, 'a core without the map cannot freeze')
	  assert.equal(accountFrozen(null, 'acct-b'), false)
})

// Issue #991: a fallback receivable aged past the operator's budget. Amber +
// dismissible + stable identity — an aging claim is ATTENTION, never the red
// pair (red stays reserved for freeze/self-check). Older cores / cash-policy
// kernels (no additive settlement keys, age null) render nothing.
check('an overdue fallback receivable raises the amber aging banner (issue 991)', () => {
  const stats = {
    settlement: {
      receivableUsd: '0',
      fallbackReceivableUsd: '12.34',
      pendingFallbackClaims: 1,
      oldestFallbackClaimAgeMs: 172_800_000,
      unverifiedExpiryPolicy: 'receivable',
    },
  }
  const b = overdueReceivableBanner(stats, 172_800_000)
  assert.ok(b, 'a claim at exactly the budget is overdue — the banner must show')
  assert.equal(b.tone, 'warn', 'aging receivable is attention, not trading-down')
  assert.equal(b.urgent, false, 'never the red pair (red = freeze/self-check)')
  assert.equal(b.identity, 'receivable-overdue', 'stable identity for dismiss-memory')
  assert.match(b.body, /172800|48/, 'the age is DATA on screen (hours)')
  assert.match(b.body, /12\.34/, 'the estimated amount is DATA on screen')
})

check('a young or absent receivable raises no banner', () => {
  const young = {
    settlement: { fallbackReceivableUsd: '5', oldestFallbackClaimAgeMs: 3_600_000 },
  }
  assert.equal(overdueReceivableBanner(young, 172_800_000), null, 'under budget → quiet')
  const noClaim = { settlement: { fallbackReceivableUsd: '0', oldestFallbackClaimAgeMs: null } }
  assert.equal(overdueReceivableBanner(noClaim, 172_800_000), null, 'no open claim → quiet')
  // Older cores omit the additive keys entirely; a cash-policy kernel keeps
  // the age null. Both must render nothing — never an invented receivable.
  assert.equal(overdueReceivableBanner({}, 172_800_000), null)
  assert.equal(overdueReceivableBanner({ settlement: null }, 172_800_000), null)
  assert.equal(overdueReceivableBanner(null, 172_800_000), null)
  // A disabled budget (0) never fires, whatever the age.
  const ancient = {
    settlement: { oldestFallbackClaimAgeMs: 999_999_999_999 },
  }
  assert.equal(overdueReceivableBanner(ancient, 0), null, 'budget 0 = the check is off')
  assert.equal(overdueReceivableBanner(ancient, null), null)
})

// 红色只留给紧急（交易此刻真的停了：冻结 / 自检失败），其余黄色 + 可关闭 +
// 自动消失。tone 已经各自断言过；这里钉住的是 urgent 分级本身——红色横幅
// 必须钉住（不可关、不自动消失），琥珀横幅必须可关且带去重 identity。
check('severity tiers: urgent banners are exactly the trading-is-down pair', () => {
  const frozen = freezeBanner({ tradingFrozen: { active: true, reason: 'x' } })
  assert.ok(frozen)
  assert.equal(frozen.urgent, true, 'a freeze is urgent — red and pinned')
  const selfcheck = selfCheckBanner({
    selfCheck: { ok: false, tsMs: 1, items: [{ name: 'venue_balance', ok: false, detail: 'd' }] },
  })
  assert.ok(selfcheck)
  assert.equal(selfcheck.urgent, true, 'a failed self-check is urgent — red and pinned')
})

check('severity tiers: attention banners are dismissible with a stable identity', () => {
  const frozen = freezeBanner({ tradingFrozen: { active: true, reason: 'x' } })
  const refusal = lastErrorBanner({
    lastError: {
      tsMs: 1,
      code: 'INVALID_PARAMS',
      message: 'duplicate internal key: lua_momentum (balance 13.596906, drift -7.898029)',
    },
  })
  assert.ok(refusal)
  assert.equal(refusal.urgent, false, 'a transient refusal record is amber, not red')
  assert.ok(refusal.identity, 'identity is the dismiss-memory key')
  // The SAME error keeps the identity stable across snapshots (so a dismissal
  // sticks); a DIFFERENT error yields a different identity (so it re-raises).
  const again = lastErrorBanner({
    lastError: {
      tsMs: 2,
      code: 'INVALID_PARAMS',
      message: 'duplicate internal key: lua_momentum (balance 7.898029, drift -0.000000)',
    },
  })
  assert.equal(again?.identity, refusal.identity, 'same failure → same identity')
  const other = lastErrorBanner({
    lastError: { tsMs: 3, code: 'VENUE_ERROR', message: 'different failure' },
  })
  assert.notEqual(other?.identity, refusal.identity, 'a new failure re-raises the banner')
  // Volatile figures inside the message must NOT change the identity: the
  // accounting-audit note embeds balances/drift/timestamps recomputed on every
  // reconcile pass, and an identity that churns with them resurrects a
  // dismissed banner (2026-10-28 operator report: 警告还是关不掉). `again`
  // above already carries churned numbers; a ms-timestamp churn is the other
  // live variant (anchor correction at 1793176336641).
  const churned = lastErrorBanner({
    lastError: {
      tsMs: 4,
      code: 'INVALID_PARAMS',
      message: 'duplicate internal key: lua_momentum (balance 7.898029, drift -7.898029 at 1793176336641)',
    },
  })
  // The conditional "at <ts>" tail is a real wording change (a different
  // realign branch) — same skeleton + churned NUMBERS must collapse, so assert
  // on the balance/drift churn only.
  assert.equal(
    churned?.identity,
    'error:INVALID_PARAMS:duplicate internal key: lua_momentum (balance N drift -N at N)',
    'number churn with identical skeleton → same identity',
  )
  const skeletonStable = lastErrorBanner({
    lastError: {
      tsMs: 5,
      code: 'INVALID_PARAMS',
      message: 'duplicate internal key: lua_momentum (balance 0.000000, drift -7.898029)',
    },
  })
  assert.equal(
    skeletonStable?.identity,
    again?.identity,
    'churned numbers, same skeleton → same identity (stays dismissed)',
  )
  // Dismiss-memory contract: one dismissal covers every churned variant.
  const dismissedSet = new Set([refusal.identity])
  assert.ok(again?.identity && dismissedSet.has(again.identity), 'churned variant stays dismissed')
  assert.ok(
    skeletonStable?.identity && dismissedSet.has(skeletonStable.identity),
    'skeleton-stable variant stays dismissed',
  )
  // Legacy slot and the other amber banners carry identities too.
  assert.ok(lastErrorBanner({ lastVenueError: { tsMs: 1, message: 'x' } })?.identity)
  assert.ok(connectionBanner({ lastError: 'core not reachable' })?.identity)
  assert.ok(archiveStoppedBanner({
    archive: { path: '/p', events: 1, bytes: 2, dropped: 0, recording: false, rotateBytes: 0, segmentBytes: 0, segments: 0, freeBytes: 9, stoppedReason: 'disk' },
  })?.identity)
  // Urgent banners carry NO identity — they cannot be dismissed away at all.
  assert.equal(frozen.identity, undefined)
})

console.log(failures === 0 ? '\nRESULT: PASS' : `\nRESULT: FAIL (${failures})`)
process.exit(failures === 0 ? 0 : 1)

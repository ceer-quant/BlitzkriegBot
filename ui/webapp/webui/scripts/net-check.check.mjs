/**
 * Regression check for 网络自检（设置页的「网络诊断」卡片）的读法规则。
 *
 * 这份检查盯的是「不许撒谎」的三条，而不是某个截图像素：
 *
 *  1. 未知 `status` 必须原样显示 —— 内核将来多一个状态时，页面不能把它藏进
 *     空白或某个更顺眼的词里（Rust 侧 `status_label` 是同一条规则）。
 *  2. `unsupported` / `rejected` 不能读成网络故障：一个是本构建没有探针，一个是
 *     按策略在拨号前拒绝，都不是「交易所挂了」。
 *  3. 空报告、`probing`、报错都不能渲染成「通过」：没有证据就是没有证据。
 *
 *   cd ui/webapp/webui && npm run check:net
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

const { netStatusLabel, netHintTitle, readNetCheck, ageLabel } = await import('../src/lib/net-check.ts')

console.log('network self-check — what the panel may claim about a probe report')

check('every status the core emits has a label', () => {
  for (const s of [
    'ok', 'dns_failed', 'tcp_refused', 'tcp_timeout', 'tls_cert', 'tls_error',
    'timeout', 'http_error', 'transport_error', 'rejected', 'unsupported',
  ]) {
    const label = netStatusLabel(s)
    assert.ok(label && label !== s, `${s} fell through to its raw wire value`)
  }
})

check('an unknown status is echoed, never hidden', () => {
  assert.equal(netStatusLabel('quic_reset'), 'quic_reset')
})

check('非网络故障的状态不写成失败', () => {
  assert.match(netStatusLabel('unsupported'), /无探针/)
  assert.match(netStatusLabel('rejected'), /未拨号/)
  assert.doesNotMatch(netStatusLabel('unsupported'), /失败|故障/)
  assert.doesNotMatch(netStatusLabel('rejected'), /失败|故障/)
})

check('a passing report reads as passing, with the count', () => {
  const v = readNetCheck({
    probing: false,
    ageMs: 1200,
    error: null,
    report: {
      ok: true,
      hintCode: 'ok',
      hint: 'venue-rest 12 ms 内应答',
      items: [
        { name: 'venue-rest', target: 'clob.polymarket.com', ok: true, status: 'ok', ms: 12 },
        { name: 'venue-ws', target: 'ws.polymarket.com', ok: true, status: 'ok', ms: 20, fakeIp: true },
      ],
    },
  })
  assert.equal(v.tone, 'up')
  assert.match(v.summary, /2\/2 条路径通过/)
  assert.equal(v.rows.length, 2)
  assert.equal(v.rows[1].fakeIp, true, 'fake-IP 是解析器的事实，要带出来')
  assert.equal(v.ageNote, '刚刚')
  assert.equal(v.emptyHint, null)
})

check('a failing path is listed with its status label and detail', () => {
  const v = readNetCheck({
    probing: false,
    ageMs: 30_000,
    error: null,
    report: {
      ok: false,
      hintCode: 'tls_blocked',
      hint: 'TLS 在请求前被拦，像是代理拦了 CONNECT',
      proxyEnv: ['HTTPS_PROXY'],
      items: [
        { name: 'venue-rest', target: 'clob.polymarket.com', ok: false, status: 'tls_cert', detail: '证书链被替换' },
      ],
    },
  })
  assert.equal(v.tone, 'down')
  assert.equal(v.title, 'TLS 在请求前被拦')
  assert.equal(v.rows[0].label, 'TLS 证书被拒')
  assert.equal(v.rows[0].detail, '证书链被替换')
  assert.match(String(v.proxyNote), /HTTPS_PROXY/)
  assert.match(String(v.proxyNote), /只报名字/)
  assert.equal(v.ageNote, '30 秒前')
})

check('an unsupported report is not a silent pass', () => {
  const v = readNetCheck({
    probing: false, ageMs: 0, error: null,
    report: {
      ok: false, hintCode: 'unsupported',
      hint: '市场插件 `none` 未实现网络探针',
      items: [{ name: 'venue', target: '', ok: false, status: 'unsupported' }],
    },
  })
  assert.equal(v.tone, 'down')
  assert.equal(v.title, '本构建没有网络探针')
  assert.equal(v.rows[0].label, '无探针（本构建未实现）')
})

check('an empty report is never a pass', () => {
  const v = readNetCheck({ probing: false, ageMs: 0, error: null, report: { ok: false, items: [] } })
  assert.equal(v.tone, 'down')
  assert.match(v.summary, /0\/0/)
  assert.match(v.summary, /不算通过/)
  assert.ok(v.emptyHint, 'an empty report must hand the reader their next step')
})

check('a probe in flight is not rendered as an empty table', () => {
  const v = readNetCheck({ probing: true, ageMs: null, report: null, error: null })
  assert.equal(v.tone, 'default')
  assert.equal(v.title, '正在探测…')
  assert.match(v.emptyHint, /自动刷新/)
})

check('a core that never answered says so, with the reason', () => {
  const v = readNetCheck({ probing: false, ageMs: null, report: null, error: 'core not reachable on /tmp/x.sock' })
  assert.equal(v.tone, 'down')
  assert.match(v.hint, /core not reachable/)
  assert.match(String(v.emptyHint), /指令台/)
})

check('no doc at all is still not a pass', () => {
  const v = readNetCheck(null)
  assert.equal(v.tone, 'down')
  assert.match(v.summary, /无结果/)
})

check('a stale report keeps its age instead of pretending to be fresh', () => {
  assert.equal(ageLabel(4_000), '刚刚')
  assert.equal(ageLabel(42_000), '42 秒前')
  assert.equal(ageLabel(600_000), '10 分钟前')
  assert.equal(ageLabel(null), null)
  assert.equal(ageLabel(Number.NaN), null)
})

check('an unknown hint code falls back to a neutral title', () => {
  assert.equal(netHintTitle('quic_blocked'), '网络自检')
  assert.equal(netHintTitle(undefined), '网络自检')
})

if (failures > 0) {
  console.error(`\n${failures} check(s) failed`)
  process.exit(1)
}
console.log('\nall checks passed')

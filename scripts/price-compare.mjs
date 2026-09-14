#!/usr/bin/env node
/** Compare panel prices vs real Polymarket CLOB; detect staleness. */
import net from 'net';
import { resolveSocketPath } from './lib/core-socket.mjs';

const SOCK = await resolveSocketPath();
const ROUND_SEC = 900;
const timeout = (p, ms, label) => Promise.race([p, new Promise((_, r) => setTimeout(() => r(new Error('timeout ' + label)), ms))]);
const get = async (url) => {
  const r = await fetch(url, { signal: AbortSignal.timeout(7000) });
  return r;
};

function rpc(method, params = {}) {
  return new Promise((res) => {
    const c = net.connect(SOCK); let b = ''; let done = false;
    const fin = (v) => { if (done) return; done = true; try { c.destroy(); } catch {} res(v); };
    c.on('connect', () => c.write(JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }) + '\n'));
    c.on('data', (d) => { b += d.toString(); const n = b.indexOf('\n'); if (n < 0) return;
      try { fin(JSON.parse(b.slice(0, n)).result); } catch { fin(null); } });
    c.on('error', () => fin(null));
    setTimeout(() => fin(null), 4000);
  });
}

const clobMid = async (tok) => {
  try { const r = await get(`https://clob.polymarket.com/midpoint?token_id=${tok}`);
    if (!r.ok) return `HTTP${r.status}`; const j = await r.json(); return j.mid != null ? Number(j.mid) : null; }
  catch (e) { return 'ERR'; }
};

const sleep = (ms) => new Promise(r => setTimeout(r, ms));

(async () => {
  const slot = Math.floor(Date.now() / 1000 / ROUND_SEC);
  const slotStart = slot * ROUND_SEC;
  console.log('local now    :', new Date().toLocaleTimeString('en-GB'));
  const r0 = await rpc('engine.round');
  console.log('core round   : slot', r0?.slot, 'age', r0?.ageSec, 'left', r0?.timeLeftSec, 'canTrade', r0?.canTrade);
  console.log('expected slot:', slot, '(start', new Date(slotStart*1000).toLocaleTimeString('en-GB') + ')');
  const s0 = await rpc('engine.stats');
  console.log('feed         : books', s0?.books, 'spots', s0?.spots);
  console.log('');

  // Real market for this round, per asset
  const real = {};
  for (const a of ['btc','eth','sol','xrp']) {
    const slug = `${a}-updown-15m-${slotStart}`;
    let m = null;
    try { const r = await get(`https://gamma-api.polymarket.com/markets?slug=${encodeURIComponent(slug)}`); const j = await r.json(); m = Array.isArray(j) ? j[0] : null; } catch {}
    if (!m) { console.log(a.toUpperCase(), 'slug', slug, '-> Gamma: no market'); continue; }
    const outcomes = JSON.parse(m.outcomes||'[]'), toks = JSON.parse(m.clobTokenIds||'[]');
    const ui = outcomes.findIndex(o=>/up|yes/i.test(o)), di = outcomes.findIndex(o=>/down|no/i.test(o));
    const [um, dm] = await Promise.all([clobMid(toks[ui]), clobMid(toks[di])]);
    real[a.toUpperCase()] = { up: um, down: dm };
    console.log(`${a.toUpperCase().padEnd(4)} real CLOB mid: UP=${um} DOWN=${dm}`);
  }
  console.log('');
  console.log('PANEL t0     :', (r0?.marketPrices||[]).map(x=>`${x.asset} UP=${x.up} DOWN=${x.down}`).join('  '));

  // sample again to detect freeze
  await sleep(12000);
  const r1 = await rpc('engine.round');
  console.log('PANEL t+12s  :', (r1?.marketPrices||[]).map(x=>`${x.asset} UP=${x.up} DOWN=${x.down}`).join('  '));
  const s1 = await rpc('engine.stats');
  console.log('feed t+12s   : books', s1?.books, '(delta', (s1?.books??0)-(s0?.books??0), ') spots', s1?.spots, '(delta', (s1?.spots??0)-(s0?.spots??0), ')');
  console.log('');
  for (const mp of (r1?.marketPrices||[])) {
    const rr = real[mp.asset];
    if (rr) console.log(`${mp.asset} panel ${mp.up}/${mp.down}  vs  real ${rr.up}/${rr.down}`);
  }
  process.exit(0);
})().catch(e => { console.error('ERR', e.message); process.exit(1); });

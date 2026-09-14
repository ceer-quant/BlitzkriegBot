#!/usr/bin/env node
/** Tight probe: is the panel price feed live, and does it lag real CLOB? */
import net from 'net';
const SOCK = (process.env.TMPDIR || '') + 'clodds-core-fancer.sock';
const RS = 900;
const rpc = (m, p = {}) => new Promise((res) => {
  const c = net.connect(SOCK); let b = ''; let d = false;
  const fin = (v) => { if (d) return; d = true; try { c.destroy(); } catch {} res(v); };
  c.on('connect', () => c.write(JSON.stringify({ jsonrpc: '2.0', id: 1, method: m, params: p }) + '\n'));
  c.on('data', (x) => { b += x.toString(); const n = b.indexOf('\n'); if (n < 0) return; try { fin(JSON.parse(b.slice(0, n)).result); } catch { fin(null); } });
  c.on('error', () => fin(null)); setTimeout(() => fin(null), 3000);
});
const mid = async (t) => { try { const r = await fetch(`https://clob.polymarket.com/midpoint?token_id=${t}`, { signal: AbortSignal.timeout(5000) }); if (!r.ok) return null; const j = await r.json(); return j.mid != null ? Number(j.mid) : null; } catch { return null; } };
const sleep = (ms) => new Promise(r => setTimeout(r, ms));

let tk = {}, tkSlot = null;
async function tokens() {
  const s = Math.floor(Date.now() / 1000 / RS) * RS; const out = {};
  for (const a of ['btc', 'eth', 'sol', 'xrp']) {
    try { const r = await fetch(`https://gamma-api.polymarket.com/markets?slug=${a}-updown-15m-${s}`); const j = await r.json(); const m = j[0];
      if (m) { const o = JSON.parse(m.outcomes || '[]'), t = JSON.parse(m.clobTokenIds || '[]');
        out[a.toUpperCase()] = { up: t[o.findIndex(x => /up|yes/i.test(x))], down: t[o.findIndex(x => /down|no/i.test(x))] }; } } catch {}
  }
  return out;
}
const last = {}; const changes = {}; let totBooks = null;
for (let i = 0; i < 25; i++) {
  const cur = Math.floor(Date.now() / 1000 / RS);
  if (cur !== tkSlot) { tk = await tokens(); tkSlot = cur; }
  const r = await rpc('engine.round'); const s = await rpc('engine.stats');
  if (totBooks != null) { const d = (s?.books ?? 0) - totBooks; if (d === 0 && i > 1) console.log('   !! books 计数未增长 (可能 feed 停滞)'); }
  totBooks = s?.books ?? totBooks;
  const parts = [];
  for (const m of (r?.marketPrices || [])) {
    const key = m.asset;
    if (last[key] !== m.up) { changes[key] = (changes[key] || 0) + 1; last[key] = m.up; }
    const rr = tk[key] ? await mid(tk[key].up) : null;
    const d = rr != null ? Math.abs(m.up - rr) : null;
    parts.push(`${key} p=${m.up} r=${rr} Δ=${d != null ? d.toFixed(3) : '?'} chg=${changes[key] || 0}`);
  }
  console.log(new Date().toLocaleTimeString('en-GB'), 'slot', r?.slot, 'left', r?.timeLeftSec, '| books', s?.books, '|', parts.join(' | '));
  await sleep(4000);
}
process.exit(0);

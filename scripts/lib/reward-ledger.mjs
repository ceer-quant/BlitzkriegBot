/**
 * Platform liquidity rewards (Polymarket REWARD) — the shared ledger surface
 * for #910.
 *
 * Why this exists: the maker-side strategies earn money the trade ledger can
 * never see — the venue's liquidity rewards pay RESTING orders, not fills, so
 * `data/trades/*.jsonl` is structurally blind to that income (measured in the
 * #910 evidence pull: 32 payouts / 4756.9846 USDC on the repo's public wallet,
 * settled ~00:15 UTC for the previous UTC day, NOT every trading day). This
 * module is the one place that knows:
 *
 *   - how a data-api v2 REWARD page is parsed (fail-closed — a drifted shape
 *     is an error, never a silently smaller income figure);
 *   - how a reward row becomes an ARCHIVE cashflow row (#388's exact format,
 *     `{"at":ms,"k":"cashflow","which":"reward","usd":"<decimal string>"}`) —
 *     the format the replay side already credits through
 *     `DataEvent::Cashflow`, so a pulled file can ride an existing replay
 *     corpus with zero core changes;
 *   - how the report side folds reward rows into the account income view;
 *   - the SSRF rule for the one host this surface may ever dial (Target enum +
 *     single constructor + host allowlist — the same shape as the venue's
 *     `validate_host`, but Mimosa-style: exactly ONE host passes).
 *
 * REWARD rows carry no market attribution (`condition_id=""`, `slug=""` in
 * every observed row), so the fold is wallet×time only — per the #910 evidence
 * there is nothing more honest to attribute to.
 */

import { readFileSync } from 'fs';

/** The only host any reward pull may ever dial. */
export const DATA_API_HOST = 'data-api.polymarket.com';
/** The validated base URL the target constructor accepts. */
export const DATA_API_BASE = `https://${DATA_API_HOST}`;
/** v2 path (v1 answers 410 Gone — "Data API v1 has been retired"). */
export const ACTIVITY_PATH = '/v2/activity';
/** Page size the probe measured working (cursor-only paging, see #910). */
export const PAGE_LIMIT = 100;

/**
 * A dial target, or the reason it was refused before any connection attempt.
 * Mirrors the venue net-check's `Target` enum: `ok` carries a URL that has
 * ALREADY passed validation; a refusal carries the raw input and the reason.
 * This is the ONLY constructor that turns raw text into a dialable URL — every
 * request URL is derived from a target, so an unvalidated string can never
 * become a request.
 */
export function makeTarget(raw) {
  // Scheme: read-only GET against the data api — https only, no downgrade.
  const prefix = 'https://';
  if (!raw.startsWith(prefix)) {
    return { ok: false, raw, reason: `must be https://, got: ${raw}` };
  }
  const rest = raw.slice(prefix.length);
  const authority = rest.split(/[/?#]/)[0] ?? '';
  // The allowlisted host is dialled bare: no userinfo, no port games — the
  // single-pass rule has one name and one port.
  if (authority.includes('@')) {
    return { ok: false, raw, reason: `userinfo is not accepted on the allowlisted host: ${raw}` };
  }
  if (authority.includes(':')) {
    return { ok: false, raw, reason: `the allowlisted host is dialled on 443 only, got: ${authority}` };
  }
  const host = authority.toLowerCase();
  if (!host) return { ok: false, raw, reason: 'has no host' };

  // Reserved names, then the literal allowlist — the Mimosa single-pass rule:
  // a private or look-alike host is refused, not dialled.
  const reserved =
    host === 'localhost' ||
    host === 'local' ||
    host.endsWith('.localhost') ||
    host.endsWith('.local') ||
    host.endsWith('.internal') ||
    host.endsWith('.internal.invalid');
  if (reserved) return { ok: false, raw, reason: `points at a reserved host: ${host}` };
  if (host !== DATA_API_HOST) {
    return { ok: false, raw, reason: `host ${host} is not the allowlisted ${DATA_API_HOST}` };
  }
  // A literal-IP spelling is refused even if it matched: the allowlist is a
  // NAME, so an address cannot smuggle itself in as one.
  if (isLiteralIp(host)) {
    return { ok: false, raw, reason: `host ${host} must be a name, not a literal address` };
  }
  // The path rides as given, minus a trailing slash (the request paths below
  // append their own leading slash — '…com//v2/activity' is not a path).
  const path = rest.slice(authority.length).replace(/\/+$/, '');
  return { ok: true, raw, url: `${DATA_API_BASE}${path}` };
}

function isLiteralIp(host) {
  // Bracketed IPv6 has already been lowercased; both spellings are refused.
  const v4 = host.split('.').map(Number);
  if (v4.length === 4 && v4.every((o) => Number.isInteger(o) && o >= 0 && o <= 255)) return true;
  return host.includes(':');
}

/**
 * The activity-page URL for one wallet: built ONLY from a validated target,
 * so the request path cannot point anywhere the allowlist refused.
 * `cursor` rides verbatim when the previous page handed one over.
 */
export function activityUrl(target, wallet, { limit = PAGE_LIMIT, cursor } = {}) {
  if (!target?.ok) throw new Error(`refused target: ${target?.reason ?? target}`);
  if (!/^0x[0-9a-fA-F]{40}$/.test(wallet)) {
    throw new Error(`not a 20-byte hex wallet address: ${wallet}`);
  }
  const q = new URLSearchParams({ user: wallet, type: 'REWARD', limit: String(limit) });
  if (cursor != null && cursor !== '') q.set('cursor', cursor);
  return `${target.url}${ACTIVITY_PATH}?${q.toString()}`;
}

/** The exact decimal string of a JSON number, or null when it cannot be one.
 * A value that only serialises in exponent form (1e-7) is refused rather than
 * rounded: the archive's `usd` feeds rust_decimal, which wants plain digits. */
export function usdTextOf(value) {
  if (typeof value !== 'number' || !Number.isFinite(value)) return null;
  const t = String(value);
  if (/e/i.test(t)) return null;
  return t;
}

/**
 * Parse one v2 activity page. Fail-closed on the envelope (missing `data` or
 * `pagination` is an error); a NON-REWARD or unparsable row inside the page is
 * skipped AND counted — the puller asked for type=REWARD, so anything else is
 * shape drift to report, not money to credit.
 */
export function parseActivityPage(text) {
  let page;
  try {
    page = JSON.parse(text);
  } catch (e) {
    throw new Error(`response is not JSON: ${e.message}`);
  }
  if (!Array.isArray(page.data)) throw new Error('missing `data` array');
  const p = page.pagination ?? {};
  if (typeof p.has_more !== 'boolean') throw new Error('missing `pagination.has_more`');
  const rows = [];
  let skipped = 0;
  for (const r of page.data) {
    if (r == null || r.type !== 'REWARD') {
      skipped += 1;
      continue;
    }
    const atMs = typeof r.timestamp === 'number' ? r.timestamp * 1000 : null;
    const usd = usdTextOf(r.usdc_size);
    if (atMs == null || usd == null) {
      skipped += 1;
      continue;
    }
    rows.push({
      atMs,
      usd,
      txHash: typeof r.transaction_hash === 'string' ? r.transaction_hash : '',
    });
  }
  return {
    rows,
    skipped,
    hasMore: p.has_more,
    nextCursor: typeof p.next_cursor === 'string' ? p.next_cursor : null,
  };
}

/** The #388 archive cashflow row a reward row becomes. Decimal-as-string, so
 * the replay credits exactly what the venue paid — no float round trip. */
export function toArchiveCashflow(row) {
  return { at: row.atMs, k: 'cashflow', which: 'reward', usd: row.usd };
}

/** A pulled payout's identity: second + amount (tx hash is not carried in the
 * archive format; the measured 32 payouts are unique on this pair). */
export function rewardKey(row) {
  return `${row.atMs}|${row.usd}`;
}

/**
 * Walk the cursor-paginated activity endpoint, collecting REWARD rows.
 *
 * `fetchPage(url)` returns the response TEXT (the caller owns transport —
 * real fetch in the CLI, a fixture map in tests). Every page is parsed
 * fail-closed; transport failures retry per page (`retries` attempts,
 * exponential `backoffMs`) and only then surface — the network is flaky, and
 * a pull that gave up on the first hiccup would under-count income; a pull
 * that SILENTLY skipped a failed page would do the same more quietly. So:
 * exhausted retries abort the whole pull with the page index and the last
 * error, and the CLI writes nothing.
 */
export async function collectRewards({
  target,
  wallet,
  fetchPage,
  maxPages = 200,
  retries = 4,
  backoffMs = 2000,
  sleep = (ms) => new Promise((r) => setTimeout(r, ms)),
  onPage,
}) {
  const rows = [];
  const seen = new Set();
  let cursor;
  let pages = 0;
  let skipped = 0;
  let retriesUsed = 0;
  for (;;) {
    if (pages >= maxPages) {
      throw new Error(`pagination did not end within ${maxPages} pages — refusing to spin`);
    }
    const url = activityUrl(target, wallet, { cursor });
    let text;
    let got = false;
    let lastErr;
    let attempt = 0;
    for (;;) {
      try {
        text = await fetchPage(url);
        got = true;
        break;
      } catch (e) {
        lastErr = e;
        attempt += 1;
        if (attempt > retries) break;
        retriesUsed += 1;
        await sleep(backoffMs * 2 ** (attempt - 1));
      }
    }
    if (!got) {
      throw new Error(`page ${pages + 1} failed after ${retries + 1} attempts: ${lastErr?.message ?? lastErr}`);
    }
    const parsed = parseActivityPage(text);
    pages += 1;
    skipped += parsed.skipped;
    for (const row of parsed.rows) {
      const key = rewardKey(row);
      if (seen.has(key)) continue;
      seen.add(key);
      rows.push(row);
    }
    if (onPage) onPage({ page: pages, rows: parsed.rows.length, skipped: parsed.skipped, hasMore: parsed.hasMore });
    if (!parsed.hasMore) {
      if (parsed.nextCursor && onPage) {
        // has_more=false with a cursor is contract drift: the endpoint says
        // it is done AND points somewhere. Trust the boolean, report the oddity.
        onPage({ note: 'has_more=false but next_cursor present — ignored' });
      }
      return { rows, pages, skipped, retriesUsed };
    }
    if (!parsed.nextCursor) {
      throw new Error(`page ${pages} says has_more=true but carries no next_cursor`);
    }
    cursor = parsed.nextCursor;
  }
}

/**
 * Load a rewards JSONL (archive cashflow rows) for the report side. Absent
 * file → `present:false`; every other problem is counted per line, never
 * silently dropped: a rewards ledger that under-reports income is exactly the
 * dishonesty this surface exists to remove.
 */
export function loadRewardRows(path) {
  let text;
  try {
    text = readFileSync(path, 'utf8');
  } catch {
    return { present: false, rows: [], malformed: 0, wrongKind: 0 };
  }
  const rows = [];
  let malformed = 0;
  let wrongKind = 0;
  for (const line of text.split('\n')) {
    if (!line.trim()) continue;
    let v;
    try {
      v = JSON.parse(line);
    } catch {
      malformed += 1;
      continue;
    }
    if (v?.k !== 'cashflow' || v?.which !== 'reward') {
      wrongKind += 1;
      continue;
    }
    const at = typeof v.at === 'number' ? v.at : null;
    const usd = typeof v.usd === 'string' ? v.usd : null;
    if (at == null || usd == null) {
      malformed += 1;
      continue;
    }
    rows.push({ atMs: at, usd });
  }
  return { present: true, rows, malformed, wrongKind };
}

/**
 * The report-side fold: reward rows inside the window, summed EXACTLY in
 * BigInt at the rows' own finest scale (money strings — float-summing them
 * first is how 0.1+0.2 lies), then the total as the 4-dp number the report
 * prints. An empty window is a valid answer ("no rewards paid here"), not
 * missing data.
 */
export function foldRewards(rows, fromMs, toMs) {
  const inWindow = rows.filter((r) => r.atMs >= fromMs && r.atMs <= toMs);
  // One common scale: the finest fractional digit count any row carries, so
  // no digit is ever truncated by the sum itself.
  const scale = inWindow.reduce((m, r) => {
    const frac = r.usd.replace(/^-/, '').split('.')[1] ?? '';
    return Math.max(m, frac.length);
  }, 0);
  let units = 0n;
  for (const r of inWindow) {
    const neg = r.usd.startsWith('-');
    const digits = r.usd.replace(/^-/, '').split('.');
    const frac = ((digits[1] ?? '') + '0'.repeat(scale)).slice(0, scale);
    units += BigInt(`${digits[0]}${frac}`) * (neg ? -1n : 1n);
  }
  const usd = Number(units) / 10 ** scale;
  return {
    windowRows: inWindow.length,
    rewardsUsd: Number((Math.round(usd * 10000) / 10000).toFixed(4)),
  };
}

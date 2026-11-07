#!/usr/bin/env python3
"""Rebuild backtest settlement labels from OFFICIAL Polymarket resolutions.

WHY (issue #859): every backtest label in this repo was self-built from the
Binance spot tape — build_realbook.py emits `k=resolution` rows as
sign(spot(expires) - spot(open)), and the A2 lane's labels_*.jsonl carry the
same point rule (`p_win`). The venue does NOT settle that way: the official
market description (clob.polymarket.com/markets/<conditionId>) resolves against
the Chainlink BTC/USD **TWAP** data stream, live since 2026-08-07. A2 measured
the damage: 5.77% of 15m and 13.06% of 5m labels flip vs a spot-TWAP proxy
(bk-session/deep-audit-1102/a2-twap/verdict.md). This tool replaces those
self-built labels with the official settlement truth and quantifies the diff.

OFFICIAL SOURCES (https only, host allowlist, see ALLOWED_HOSTS):
  CLOB  GET /markets/<conditionId>  → tokens[].winner (the market_resolved
        state), market_slug, seconds_delay (taker-delay marker), closed.
  Gamma GET /events?slug=<market_slug> → markets[0].outcomePrices /
        clobTokenIds / umaResolutionStatus. NOTE: /markets?slug= returns
        EMPTY for archived rounds and ?condition_id= is silently IGNORED by
        Gamma (it once returned an unrelated market) — never trust those;
        slug comes from the CLOB payload, never guessed.
  (Data API /trades is tape, not settlement — allowed host, unused here.)

MODES
  --relabel LABELS.jsonl... --out OUT.jsonl
      Read-only over backtest-label rows (A2 labels_15m/5m shape: {cond, fam,
      asset, open, exp, p_win, t_win, ok, ...}; any row with "cond" counts).
      Fetches the official winner per conditionId (CLOB) and writes each input
      row back UNCHANGED plus additive official_* fields — history is never
      rewritten, `p_win`/`t_win` stay as they were. Prints a diff report
      (agreement of official vs p_win / t_win, per settlement-rule period,
      taker-delay counts). Per-round evidence IS the output file.
  --verify RELABELED.jsonl --sample N
      Acceptance gate. Deterministic sample (seeded) of N rows; re-fetches
      official truth INDEPENDENTLY for each: CLOB winner AND Gamma
      outcomePrices (slug taken from the CLOB payload). `agreement` = sampled
      rows where stored official_win == CLOB == Gamma. Exit 0 iff agreement
      >= --threshold (default 0.995). Below 0.99 the report says STOP: do not
      use the relabeled corpus for downstream evaluation (issue #859).
  --apply-corpus CORPUS.jsonl
      Rewrite `k=resolution` rows in a replayable corpus to the official
      winner (token-form p, mapped through the k=round rows' upTokenId/
      downTokenId — same shape fix_resolutions.py writes). Rows whose official
      resolution cannot be fetched stay byte-identical and are counted;
      sibling temp file + atomic rename; idempotent. Exit 1 if any row is
      left unresolved.

SETTLEMENT-RULE PERIODS (report segmentation only — historical numbers are
  never altered, per the issue's 禁止项): rounds are bucketed by open time —
  pre-0807 / 0807-0814 / post-0814 (UTC dates, --d0807/--d0814 to override).
  2026-08-07 = official Chainlink-TWAP settlement went live (stated in every
  up/down market description; whale_relabel.py precedent). 2026-08-14 = the
  second rule marker named by issue #859 (taker-delay era). The per-round
  CLOB `seconds_delay` field is recorded as direct taker-delay evidence.

Self-test (offline, stdlib only, fake fetcher): test_relabel_official.py.
"""
import argparse
import calendar
import concurrent.futures as fut
import json
import os
import random
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

UA = {"User-Agent": "Mozilla/5.0 (BlitzkriegBot-research; issue-859)"}

# --- SSRF discipline: host enumeration + the ONE url constructor -------------
ALLOWED_HOSTS = frozenset({
    "clob.polymarket.com",       # /markets/<cid> — market_resolved state
    "gamma-api.polymarket.com",  # /events?slug= — outcomePrices truth
    "data-api.polymarket.com",   # /trades?market= — tape (unused, reserved)
})


def safe_write_path(path: str) -> str:
    """拒绝含穿越组件(`..`)的输出路径，否则原样返回。

    该工具只落盘到调用方显式指定的文件；不允许通过 `../` 向上
    穿越写入仓库树之外的任意文件（path traversal，高危）。
    """
    parts = path.replace("\\", "/").split("/")
    if ".." in parts:
        raise ValueError(f"refusing path-traversal output path: {path!r}")
    return path


def safe_open(path: str, mode: str = "r"):
    """以给定模式打开文件；写入/追加/读写模式前先做路径穿越校验。

    所有落盘入口统一走此函数，避免直接接收未校验的调用方路径
    （path traversal，高危）。
    """
    if any(c in mode for c in "wax+"):
        path = safe_write_path(path)
    return open(path, mode)


def validate_url(url):
    """Re-validate an already-built URL against the allowlist."""
    u = urllib.parse.urlsplit(url)
    if u.scheme != "https" or u.hostname not in ALLOWED_HOSTS:
        raise ValueError(f"url failed allowlist: {url!r}")
    return url


def build_url(host, path, params=None):
    """The only place a request URL is ever constructed."""
    if host not in ALLOWED_HOSTS:
        raise ValueError(f"host not allowlisted: {host!r}")
    if not path.startswith("/"):
        raise ValueError(f"path must start with '/': {path!r}")
    return validate_url(urllib.parse.urlunsplit(
        ("https", host, path, urllib.parse.urlencode(params or {}), "")))


class FetchError(Exception):
    pass


def fetch_json(url, tries=4, timeout=20, sleep=time.sleep):
    """GET + parse JSON, retrying 429/5xx/network with backoff."""
    last = None
    for i in range(tries):
        if i:
            sleep(1.0 + i)
        try:
            req = urllib.request.Request(url, headers=UA)
            with urllib.request.urlopen(req, timeout=timeout) as r:
                return json.loads(r.read().decode())
        except urllib.error.HTTPError as e:
            last = f"HTTP {e.code}"
            if e.code not in (429, 500, 502, 503, 504):
                break
        except Exception as e:  # URLError, timeout, JSON decode
            last = str(e)[:200]
    raise FetchError(f"{last} :: {url}")


# --- official winner extraction ----------------------------------------------

def clob_truth(payload):
    """{winner, slug, seconds_delay, closed} from a CLOB /markets/<cid> body."""
    toks = payload.get("tokens") or []
    wins = [t.get("outcome") for t in toks if t.get("winner")]
    winner = wins[0] if len(wins) == 1 else None
    return {"winner": winner,
            "slug": payload.get("market_slug"),
            "seconds_delay": payload.get("seconds_delay"),
            "closed": payload.get("closed")}


def _jload(s):
    try:
        return json.loads(s or "[]")
    except Exception:
        return []


def gamma_truth(payload):
    """Winner outcome name from a Gamma market (events?slug= → markets[0])."""
    outcomes, prices = _jload(payload.get("outcomes")), _jload(payload.get("outcomePrices"))
    if len(outcomes) != 2 or len(prices) != 2:
        return None
    wins = [o for o, p in zip(outcomes, prices) if str(p) == "1"]
    return wins[0] if len(wins) == 1 else None


def official_via_clob(cid, fetch=fetch_json):
    return clob_truth(fetch(build_url("clob.polymarket.com", f"/markets/{cid}")))


def official_via_gamma(slug, fetch=fetch_json):
    ev = fetch(build_url("gamma-api.polymarket.com", "/events",
                         {"slug": slug}))
    if not ev:
        return None
    mkts = ev[0].get("markets") or []
    return gamma_truth(mkts[0]) if mkts else None


# --- settlement-rule periods --------------------------------------------------

def iso_utc_ms(date_str):
    """UTC midnight of YYYY-MM-DD as epoch ms."""
    y, m, d = (int(x) for x in date_str.split("-"))
    return calendar.timegm((y, m, d, 0, 0, 0)) * 1000


def period_of(open_ms, d0807_ms, d0814_ms):
    if open_ms is None:
        return "unknown"
    if open_ms < d0807_ms:
        return "pre-0807"
    if open_ms < d0814_ms:
        return "0807-0814"
    return "post-0814"


PERIODS = ("pre-0807", "0807-0814", "post-0814", "unknown")


# --- parallel official fetch ---------------------------------------------------

def fetch_many(cids, fetch=fetch_json, workers=6, progress=None):
    """conditionId -> clob_truth dict (or {"error": ...})."""
    out = {}

    def one(cid):
        try:
            return official_via_clob(cid, fetch)
        except Exception as e:
            return {"error": str(e)[:120]}

    with fut.ThreadPoolExecutor(max_workers=workers) as ex:
        for i, (cid, tr) in enumerate(zip(cids, ex.map(one, cids)), 1):
            out[cid] = tr
            if progress and i % progress == 0:
                print(f"  fetched {i}/{len(cids)}", file=sys.stderr, flush=True)
    return out


def aggregate(rows):
    """Report counters over relabeled rows. Agreement denominators count only
    rows that actually carry the reference label (p_win/t_win not None) — a
    row the A2 lane marked label_ok=False has no Binance label to disagree
    with, and counting it as disagreement would fabricate error."""
    stats = {p: {"n": 0, "p": 0, "pn": 0, "t": 0, "tn": 0, "delay": 0}
             for p in PERIODS}
    d0807, d0814 = iso_utc_ms("2026-08-07"), iso_utc_ms("2026-08-14")
    for r in rows:
        period = r.get("period") or period_of(r.get("open"), d0807, d0814)
        s = stats[period]
        s["n"] += 1
        s["delay"] += 1 if (r.get("official_seconds_delay") or 0) > 0 else 0
        ap, at = r.get("agree_p"), r.get("agree_t")
        if r.get("p_win") is not None and r.get("official_win") is not None:
            s["pn"] += 1
            s["p"] += 1 if ap else 0
        if r.get("t_win") is not None and r.get("official_win") is not None:
            s["tn"] += 1
            s["t"] += 1 if at else 0
    return stats


def print_report(rows, stats, title):
    def pct(a, n):
        return f"{a / n * 100:.2f}%" if n else "n/a"

    tp = sum(stats[p]["p"] for p in PERIODS)
    tpn = sum(stats[p]["pn"] for p in PERIODS)
    tt = sum(stats[p]["t"] for p in PERIODS)
    ttn = sum(stats[p]["tn"] for p in PERIODS)
    print(f"{title} rounds={len(rows)}")
    print(f"agreement official_vs_p_win(binance point)={pct(tp, tpn)} "
          f"(n={tpn})")
    print(f"agreement official_vs_t_win(spot-TWAP proxy)={pct(tt, ttn)} "
          f"(n={ttn})")
    for p in PERIODS:
        s = stats[p]
        print(f"period {p}: rounds={s['n']} "
              f"agree_p={pct(s['p'], s['pn'])} agree_t={pct(s['t'], s['tn'])} "
              f"taker_delay_rounds={s['delay']}")


def write_report_file(path, rows, args, unresolved=0, errors=0):
    path = safe_write_path(path)
    stats = aggregate(rows)
    with safe_open(path, "w") as fh:
        fh.write("sources: clob.polymarket.com/markets/<cid>"
                 " (market_resolved winner) + gamma-api /events?slug=\n")
        fh.write(f"periods: {args.d0807} = Chainlink-TWAP settlement live;"
                 f" {args.d0814} = taker-delay era marker (issue #859)\n")
        fh.write(f"rows={len(rows)} unresolved={unresolved} errors={errors}\n")
        fh.write("agreement denominators count only rows carrying the"
                 " reference label (p_win/t_win not None)\n")
        for p in PERIODS:
            s = stats[p]
            fh.write(f"period {p}: rounds={s['n']} agree_p={s['p']}/{s['pn']}"
                     f" agree_t={s['t']}/{s['tn']}"
                     f" taker_delay_rounds={s['delay']}\n")


# --- mode 1: relabel backtest-label rows ---------------------------------------

def read_label_rows(paths):
    """Yield (path, lineno, row) for every JSON line with a cond field."""
    n = 0
    for path in paths:
        with open(path) as fh:
            for i, raw in enumerate(fh, 1):
                raw = raw.strip()
                if not raw:
                    continue
                try:
                    o = json.loads(raw)
                except Exception:
                    continue
                if isinstance(o, dict) and o.get("cond"):
                    n += 1
                    yield path, i, o
    if n == 0:
        raise FetchError("no label rows with a cond field found")


def relabel(paths, out_path, args, fetch=fetch_json):
    rows = list(read_label_rows(paths))
    if args.max_rounds:
        rows = rows[:args.max_rounds]
    cids = sorted({r["cond"] for _, _, r in rows})
    print(f"rows={len(rows)} distinct_conditions={len(cids)}", file=sys.stderr)
    truth = fetch_many(cids, fetch, workers=args.workers, progress=200)

    d0807, d0814 = iso_utc_ms(args.d0807), iso_utc_ms(args.d0814)
    unresolved = errors = 0
    out_path = safe_write_path(out_path)
    with safe_open(out_path, "w") as out:
        for path, lineno, r in rows:
            tr = truth.get(r["cond"], {})
            win = tr.get("winner")
            if tr.get("error") or not win:
                unresolved += 1
                if tr.get("error"):
                    errors += 1
            o = dict(r)
            o["official_win"] = win
            o["official_src"] = "clob" if win else None
            o["official_slug"] = tr.get("slug")
            o["official_seconds_delay"] = tr.get("seconds_delay")
            o["official_error"] = tr.get("error")
            o["period"] = period_of(r.get("open"), d0807, d0814)
            if win:
                if r.get("p_win") is not None:
                    o["agree_p"] = (r.get("p_win") == win)
                if r.get("t_win") is not None:
                    o["agree_t"] = (r.get("t_win") == win)
            out.write(json.dumps(o) + "\n")

    print(f"rows={len(rows)} official_resolved={len(rows) - unresolved} "
          f"unresolved={unresolved} fetch_errors={errors}")
    out_rows = [json.loads(l) for l in open(out_path)]
    print_report(out_rows, aggregate(out_rows), "relabel:")
    print(f"out={out_path}")
    if args.report:
        write_report_file(args.report, out_rows, args,
                          unresolved=unresolved, errors=errors)
    return 2 if errors > len(rows) // 2 else 0


# --- mode 2: verify (acceptance gate) ------------------------------------------

def verify(relabeled_path, args, fetch=fetch_json):
    rows = [o for _, _, o in read_label_rows([relabeled_path])]
    rows = [r for r in rows if r.get("official_win")]
    if not rows:
        print("no relabeled rows to verify", file=sys.stderr)
        return 2
    rng = random.Random(args.seed)
    sample = rows if len(rows) <= args.sample else rng.sample(rows, args.sample)
    print(f"population={len(rows)} sampled={len(sample)} seed={args.seed}",
          file=sys.stderr)

    n_ok = 0
    mismatches = []
    for r in sample:
        cid = r["cond"]
        try:
            tr = official_via_clob(cid, fetch)
        except Exception as e:
            mismatches.append({"cond": cid, "stage": "clob", "error": str(e)[:120]})
            continue
        if not tr["winner"]:
            mismatches.append({"cond": cid, "stage": "clob", "error": "no winner"})
            continue
        try:
            gw = official_via_gamma(tr["slug"], fetch) if tr["slug"] else None
        except Exception as e:
            mismatches.append({"cond": cid, "stage": "gamma", "error": str(e)[:120]})
            gw = None
        stored = r.get("official_win")
        ok = (stored == tr["winner"] and gw == tr["winner"])
        n_ok += ok
        if not ok:
            mismatches.append({"cond": cid, "slug": tr.get("slug"),
                               "stored": stored, "clob": tr["winner"],
                               "gamma": gw})
    agree = n_ok / len(sample)
    print(f"sampled={len(sample)} agreed={n_ok} agreement={agree * 100:.2f}% "
          f"threshold={args.threshold * 100:.2f}%")
    for m in mismatches[:50]:
        print("MISMATCH", json.dumps(m))
    if args.out_evidence:
        out_evidence = safe_write_path(args.out_evidence)
        with safe_open(out_evidence, "w") as fh:
            for r in sample:
                fh.write(json.dumps({
                    "cond": r["cond"], "asset": r.get("asset"),
                    "fam": r.get("fam"), "open": r.get("open"),
                    "period": r.get("period"), "stored_official_win": r.get("official_win"),
                    "binance_p_win": r.get("p_win"), "twap_proxy_t_win": r.get("t_win"),
                }) + "\n")
    if agree < 0.99:
        print(f"STOP: agreement {agree * 100:.2f}% < 99% — halt downstream "
              f"evaluation per issue #859")
    verdict = "PASS" if agree >= args.threshold else "FAIL"
    print(f"verdict={verdict}")
    return 0 if agree >= args.threshold else 1


# --- mode 3: apply official resolutions to a replayable corpus ------------------

def collect_tokens(path):
    """conditionId -> (upTokenId, downTokenId) from k=round rows."""
    toks = {}
    for raw in open(path):
        try:
            o = json.loads(raw)
        except Exception:
            continue
        if not o or o.get("k") != "round":
            continue
        for m in o.get("m", []):
            c, up, down = m.get("conditionId"), m.get("upTokenId"), m.get("downTokenId")
            if c and up and down:
                toks[c] = (up, down)
    return toks


def apply_corpus(path, args, fetch=fetch_json):
    toks = collect_tokens(path)
    res_cids = []
    for raw in open(path):
        try:
            o = json.loads(raw)
        except Exception:
            continue
        if o and o.get("k") == "resolution" and o.get("c"):
            res_cids.append(o["c"])
    res_cids = sorted(set(res_cids))
    print(f"round_rows_with_tokens={len(toks)} resolutions={len(res_cids)}",
          file=sys.stderr)
    truth = fetch_many(res_cids, fetch, workers=args.workers)

    tmp = safe_write_path(path + ".tmp")
    nlines = rewritten = unresolved = 0
    with safe_open(tmp, "w") as out:
        for raw in open(path):
            nlines += 1
            try:
                o = json.loads(raw)
            except Exception:
                out.write(raw)
                continue
            if o and o.get("k") == "resolution":
                tr = truth.get(o.get("c"), {})
                win, pair = tr.get("winner"), toks.get(o.get("c"))
                if win and pair:
                    up, down = pair
                    o["p"] = [[up, "1" if win == "Up" else "0"],
                              [down, "1" if win == "Down" else "0"]]
                    o["n"] = False
                    out.write(json.dumps(o, separators=(",", ":")) + "\n")
                    rewritten += 1
                    continue
                unresolved += 1
            out.write(raw)
    os.replace(tmp, path)
    print(f"lines={nlines} resolutions_rewritten={rewritten} "
          f"resolutions_unresolved={unresolved}")
    return 1 if unresolved else 0


# --- mode 4: summarize a relabeled file (no network) ----------------------------

def summarize(relabeled_path, args):
    """Recompute the diff report from a --relabel output file, offline."""
    rows = [o for _, _, o in read_label_rows([relabeled_path])]
    print_report(rows, aggregate(rows), "summarize:")
    if args.report:
        write_report_file(args.report, rows, args)
    return 0


# --- cli ------------------------------------------------------------------------

def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    g = ap.add_mutually_exclusive_group(required=True)
    g.add_argument("--relabel", nargs="+", metavar="LABELS.jsonl",
                   help="rebuild labels from official truth (read-only, additive)")
    g.add_argument("--verify", metavar="RELABELED.jsonl",
                   help="acceptance gate: independent re-fetch + agreement")
    g.add_argument("--apply-corpus", metavar="CORPUS.jsonl",
                   help="rewrite corpus resolution rows to official winners")
    g.add_argument("--summarize", metavar="RELABELED.jsonl",
                   help="recompute the diff report offline (no fetch)")
    ap.add_argument("--out", default="", help="--relabel output jsonl")
    ap.add_argument("--report", default="", help="also write the summary report here")
    ap.add_argument("--max-rounds", type=int, default=0)
    ap.add_argument("--sample", type=int, default=500, help="--verify sample size")
    ap.add_argument("--seed", type=int, default=20261102, help="--verify rng seed")
    ap.add_argument("--threshold", type=float, default=0.995)
    ap.add_argument("--out-evidence", default="", help="--verify per-round evidence")
    ap.add_argument("--d0807", default="2026-08-07", help="TWAP rule live date (UTC)")
    ap.add_argument("--d0814", default="2026-08-14", help="taker-delay era date (UTC)")
    ap.add_argument("--workers", type=int, default=6)
    args = ap.parse_args(argv)

    if args.relabel:
        if not args.out:
            ap.error("--relabel requires --out")
        return relabel(args.relabel, args.out, args)
    if args.verify:
        return verify(args.verify, args)
    if args.summarize:
        return summarize(args.summarize, args)
    return apply_corpus(args.apply_corpus, args)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except FetchError as e:
        print(f"OPERATIONAL FAILURE (exit 2): {e}", file=sys.stderr)
        sys.exit(2)
    except BrokenPipeError:
        sys.exit(2)

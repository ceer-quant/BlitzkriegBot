#!/usr/bin/env python3
"""late_favorite frequency grid on settlement truth.

Replay rules are the ones in docs/late-favorite.md,
docs/reports/lua-momentum-holdout-2026-09.md and
docs/reports/lua-momentum-oos-2026-10.md:

  * Gamma outcomePrices is 0/1 settlement. Anything else is dropped.
  * One taker entry per market, first print whose implied ask is inside the
    band while time_left is inside the window, then hold to settlement.
  * Implied ask: BUY print = price; SELL print = price + 0.01.
    Ask must sit in (0.01, 0.99).
  * Fee is 10 * 0.07 * ask * (1-ask), charged on entry. Redemption is free.
  * BTC 5m and 15m unless --asset says otherwise. No re-entry.

The prior reports kept only the latest 200 prints. On liquid BTC markets
those 200 prints often end before 45 seconds remain, so a wider time window
would be invisible. This script's grid walks backward until time_left >= 240
or the tape is exhausted (`cover` tape). `latest200` is only a calibration
against the published cap.

Selection is locked in `lock_candidates` and does not read the unused window.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from concurrent.futures import ThreadPoolExecutor, as_completed
from datetime import datetime, timezone

UA = {"User-Agent": "Mozilla/5.0 (BlitzkriegBot-research; late-favorite-frequency)"}
GAMMA = "gamma-api.polymarket.com"
DATA = "data-api.polymarket.com"

SHARES = 10.0
FEE_RATE = 0.07
ASK_MICRO = 1_000_000
CENT = 10_000  # 0.01 in micro-units
# (0.01, 0.99) exclusive
ASK_LO = CENT
ASK_HI = 99 * CENT

LOS = (0.70, 0.75, 0.80)
HIS = (0.90, 0.92, 0.95)
TMINS = (20, 30, 45)
TMAXS = (120, 150, 180, 240)
BASELINE = (0.80, 0.90, 45, 120)

# Already examined. Selection only. Not the fresh window.
IN_SAMPLE = ("2026-10-12", "2026-10-26")
# Published late_favorite holdout. Calibration only, never selection, never OOS.
CALIB = ("2026-09-26", "2026-09-30")
# Unused by late_favorite. Nov 5-12 was already seen (holdout doc). Sep 26-30
# is the holdout itself. This block sits immediately before that holdout.
OOS = ("2026-09-12", "2026-09-26")


def day_start(iso: str) -> int:
    dt = datetime.fromisoformat(iso).replace(tzinfo=timezone.utc)
    return int(dt.timestamp())


def allowed(url: str) -> str:
    parts = urllib.parse.urlsplit(url)
    if parts.scheme != "https" or parts.hostname not in (GAMMA, DATA):
        raise ValueError(f"refusing url {url}")
    return url


class Limiter:
    def __init__(self, rps: float) -> None:
        self.interval = 1.0 / rps
        self.lock = threading.Lock()
        self.next_at = 0.0

    def wait(self) -> None:
        with self.lock:
            now = time.monotonic()
            delay = self.next_at - now
            if delay < 0:
                delay = 0.0
                self.next_at = now
            self.next_at += self.interval
        if delay:
            time.sleep(delay)


LIMITER = Limiter(20.0)


def get_json(url: str, timeout: float = 40.0):
    url = allowed(url)
    last = None
    for attempt in range(6):
        LIMITER.wait()
        req = urllib.request.Request(url, headers=UA)
        try:
            with urllib.request.urlopen(req, timeout=timeout) as resp:
                return json.loads(resp.read().decode("utf-8"))
        except urllib.error.HTTPError as exc:
            last = exc
            body = exc.read()[:240]
            if exc.code in (429, 500, 502, 503, 504):
                time.sleep(min(30.0, 1.5 ** attempt))
                continue
            raise RuntimeError(f"HTTP {exc.code} {url} {body!r}") from exc
        except (urllib.error.URLError, TimeoutError, json.JSONDecodeError) as exc:
            last = exc
            time.sleep(min(30.0, 1.5 ** attempt))
    raise RuntimeError(f"gave up {url}: {last}")


def decode_list(value):
    if isinstance(value, list):
        return value
    if isinstance(value, str):
        return json.loads(value)
    return []


def settlement_winner(market: dict):
    outcomes = [str(x) for x in decode_list(market.get("outcomes"))]
    prices = decode_list(market.get("outcomePrices"))
    if len(outcomes) < 2 or len(prices) < 2:
        return None
    parsed = []
    for raw in prices[:2]:
        try:
            parsed.append(float(raw))
        except (TypeError, ValueError):
            return None
    # Decisive 0/1 only. A still-open book is not a label.
    if parsed[0] >= 0.99 and parsed[1] <= 0.01:
        return outcomes[0]
    if parsed[1] >= 0.99 and parsed[0] <= 0.01:
        return outcomes[1]
    return None


def cache_file(root: str, tape: str, asset: str, dur: int, slot: int) -> str:
    directory = os.path.join(root, tape, asset.upper(), str(dur))
    os.makedirs(directory, exist_ok=True)
    return os.path.join(directory, f"{slot}.json")


def write_json(path: str, obj: dict) -> None:
    tmp = path + ".tmp"
    with open(tmp, "w", encoding="utf-8") as fh:
        json.dump(obj, fh, separators=(",", ":"))
    os.replace(tmp, path)


def fetch_one(root: str, tape: str, asset: str, dur: int, slot: int) -> str:
    path = cache_file(root, tape, asset, dur, slot)
    if os.path.exists(path):
        return "cached"
    label = "5m" if dur == 300 else "15m"
    slug = f"{asset.lower()}-updown-{label}-{slot}"
    end = slot + dur
    try:
        events = get_json(f"https://{GAMMA}/events?slug={urllib.parse.quote(slug)}")
    except Exception as exc:  # noqa: BLE001 — recorded, retried next run
        return f"error {exc}"
    market = None
    if isinstance(events, list):
        for event in events:
            for item in event.get("markets") or []:
                if item.get("slug") == slug:
                    market = item
                    break
    if market is None:
        write_json(path, {"slug": slug, "missing": True})
        return "missing"
    winner = settlement_winner(market)
    cid = market.get("conditionId")
    if not cid or winner not in ("Up", "Down"):
        write_json(
            path,
            {
                "slug": slug,
                "slot": slot,
                "dur": dur,
                "asset": asset.upper(),
                "unresolved": True,
                "winner": winner,
            },
        )
        return "unresolved"
    prints = []
    cursor = None
    covered = False
    scanned = 0
    limit = 200 if tape == "latest200" else 1000
    max_pages = 1 if tape == "latest200" else 6
    oldest_tl = None
    for _page in range(max_pages):
        query = {
            "condition": cid,
            "limit": str(limit),
            "taker_only": "true",
        }
        if cursor:
            query["cursor"] = cursor
        url = f"https://{DATA}/v2/trades?{urllib.parse.urlencode(query)}"
        try:
            body = get_json(url)
        except Exception as exc:  # noqa: BLE001
            return f"error {exc}"
        rows = body.get("data") or []
        if not rows:
            covered = True
            break
        page_oldest = None
        for row in rows:
            scanned += 1
            try:
                ts = int(row["timestamp"])
                price = float(row["price"])
            except (KeyError, TypeError, ValueError):
                continue
            tl = end - ts
            if page_oldest is None or tl > page_oldest:
                page_oldest = tl
            keep = tape == "latest200" or 20 <= tl <= 240
            if not keep:
                continue
            side = row.get("side")
            outcome = row.get("outcome")
            if side not in ("BUY", "SELL") or outcome not in ("Up", "Down"):
                continue
            prints.append(
                [
                    ts,
                    "B" if side == "BUY" else "S",
                    "U" if outcome == "Up" else "D",
                    int(round(price * ASK_MICRO)),
                ]
            )
        if page_oldest is not None:
            oldest_tl = page_oldest if oldest_tl is None else max(oldest_tl, page_oldest)
        pagination = body.get("pagination") or {}
        if tape == "latest200":
            covered = oldest_tl is not None and oldest_tl >= 240
            # The cap is the method. `covered` says whether 240s was inside it.
            break
        if oldest_tl is not None and oldest_tl >= 240:
            covered = True
            break
        cursor = pagination.get("next_cursor")
        if not pagination.get("has_more") or not cursor:
            covered = True
            break
    else:
        covered = oldest_tl is not None and oldest_tl >= 240
    # Stable de-dupe. The same taker print should appear once.
    uniq = []
    seen = set()
    for item in prints:
        key = tuple(item)
        if key in seen:
            continue
        seen.add(key)
        uniq.append(item)
    write_json(
        path,
        {
            "slug": slug,
            "slot": slot,
            "dur": dur,
            "asset": asset.upper(),
            "end": end,
            "winner": winner,
            "covered": covered,
            "scanned": scanned,
            "prints": uniq,
        },
    )
    return "ok"


def iter_slots(start_iso: str, end_iso: str):
    start = day_start(start_iso)
    end = day_start(end_iso)
    for dur in (300, 900):
        slot = start
        while slot < end:
            yield dur, slot
            slot += dur


def fetch_window(root: str, tape: str, asset: str, start: str, end: str, workers: int) -> None:
    jobs = list(iter_slots(start, end))
    counts = {"ok": 0, "cached": 0, "missing": 0, "unresolved": 0, "error": 0}
    lock = threading.Lock()
    done = 0
    print(f"fetch {tape} {asset} {start} -> {end} markets={len(jobs)}", flush=True)
    with ThreadPoolExecutor(max_workers=workers) as pool:
        futures = [
            pool.submit(fetch_one, root, tape, asset, dur, slot) for dur, slot in jobs
        ]
        for fut in as_completed(futures):
            status = fut.result()
            key = status if status in counts else "error"
            with lock:
                counts[key] += 1
                done += 1
                if done % 200 == 0 or done == len(jobs):
                    print(f"  {done}/{len(jobs)} {counts}", flush=True)
    print(f"done {tape} {asset} {start} {counts}", flush=True)


def load_markets(root: str, tape: str, asset: str, start: str, end: str):
    markets = []
    missing = unresolved = uncovered = absent = 0
    for dur, slot in iter_slots(start, end):
        path = cache_file(root, tape, asset, dur, slot)
        if not os.path.exists(path):
            absent += 1
            continue
        with open(path, encoding="utf-8") as fh:
            row = json.load(fh)
        if row.get("missing"):
            missing += 1
            continue
        if row.get("unresolved") or row.get("winner") not in ("Up", "Down"):
            unresolved += 1
            continue
        if not row.get("covered"):
            uncovered += 1
        prepared = []
        end_ts = int(row["end"])
        winner_up = row["winner"] == "Up"
        for ts, side, outcome, px in row.get("prints") or []:
            ask = px if side == "B" else px + CENT
            if ask <= ASK_LO or ask >= ASK_HI:
                continue
            tl = end_ts - int(ts)
            if tl < 20 or tl > 240:
                continue
            up = outcome == "U"
            prepared.append((int(ts), -ask, tl, ask, up, winner_up == up))
        prepared.sort()
        markets.append(
            {
                "slot": int(row["slot"]),
                "dur": int(row["dur"]),
                "end": end_ts,
                "prints": prepared,
            }
        )
    meta = {
        "markets": len(markets),
        "missing": missing,
        "unresolved": unresolved,
        "uncovered": uncovered,
        "absent": absent,
        "days": (day_start(end) - day_start(start)) / 86400,
    }
    return markets, meta


def first_trade(market: dict, lo: float, hi: float, tmin: int, tmax: int):
    lo_u = int(round(lo * ASK_MICRO))
    hi_u = int(round(hi * ASK_MICRO))
    for ts, _neg, tl, ask, _up, won in market["prints"]:
        if tl < tmin or tl > tmax:
            continue
        if ask < lo_u or ask > hi_u:
            continue
        p = ask / ASK_MICRO
        fee = SHARES * FEE_RATE * p * (1.0 - p)
        pnl = SHARES * ((1.0 if won else 0.0) - p) - fee
        return {
            "ts": ts,
            "end": market["end"],
            "dur": market["dur"],
            "slot": market["slot"],
            "pnl": pnl,
            "won": won,
        }
    return None


def summarize(trades: list, days: float) -> dict:
    ordered = sorted(trades, key=lambda t: (t["end"], t["dur"], t["slot"], t["ts"]))
    wins = [t for t in ordered if t["pnl"] > 0]
    losses = [t for t in ordered if t["pnl"] < 0]
    gross_win = sum(t["pnl"] for t in wins)
    gross_loss = -sum(t["pnl"] for t in losses)
    decided = len(wins) + len(losses)
    equity = peak = max_dd = 0.0
    for trade in ordered:
        equity += trade["pnl"]
        if equity > peak:
            peak = equity
        dd = peak - equity
        if dd > max_dd:
            max_dd = dd
    by_day = {}
    for trade in ordered:
        day = datetime.fromtimestamp(trade["ts"], timezone.utc).strftime("%Y-%m-%d")
        by_day[day] = by_day.get(day, 0) + 1
    n = len(ordered)
    pf = None if gross_loss == 0 else gross_win / gross_loss
    return {
        "trades": n,
        "trades_per_day": n / days if days else None,
        "max_day": max(by_day.values()) if by_day else 0,
        "wins": len(wins),
        "losses": len(losses),
        "wr": (len(wins) / decided) if decided else None,
        "pf": pf,
        "net": sum(t["pnl"] for t in ordered),
        "max_dd": max_dd,
    }


def run_config(markets, days, cfg, dur_filter=None):
    lo, hi, tmin, tmax = cfg
    trades = []
    for market in markets:
        if dur_filter is not None and market["dur"] != dur_filter:
            continue
        trade = first_trade(market, lo, hi, tmin, tmax)
        if trade is not None:
            trades.append(trade)
    stats = summarize(trades, days)
    stats["config"] = {
        "min_px": lo,
        "max_px": hi,
        "min_time_left": tmin,
        "max_time_left": tmax,
        "dur": dur_filter,
    }
    return stats


def grid(markets, days):
    rows = []
    for lo in LOS:
        for hi in HIS:
            for tmin in TMINS:
                for tmax in TMAXS:
                    rows.append(run_config(markets, days, (lo, hi, tmin, tmax)))
    return rows


def passes(row: dict, min_trades: int) -> bool:
    return (
        row["trades"] >= min_trades
        and row["wr"] is not None
        and row["wr"] >= 0.80
        and row["pf"] is not None
        and row["pf"] >= 1.2
    )


def lock_candidates(rows: list, min_trades: int = 200):
    """In-sample only. Higher trade count first. OOS is not an input."""
    base = next(r for r in rows if _cfg_key(r) == BASELINE)
    eligible = [
        r
        for r in rows
        if passes(r, min_trades) and r["trades"] > base["trades"] and _cfg_key(r) != BASELINE
    ]
    eligible.sort(key=_rank_frequency)
    picked = []
    seen = set()

    def take(row):
        key = _cfg_key(row)
        if key in seen:
            return
        seen.add(key)
        picked.append(row)

    for row in eligible[:5]:
        take(row)
    if eligible:
        take(max(eligible, key=_rank_pf))
        take(max(eligible, key=_rank_net))
    return base, picked


def _cfg_key(row):
    c = row["config"]
    return (c["min_px"], c["max_px"], c["min_time_left"], c["max_time_left"])


def _rank_frequency(row):
    return (-row["trades"], -(row["pf"] or 0), -row["net"], row["max_dd"])


def _rank_pf(row):
    pf = row["pf"] if row["pf"] is not None else 1e18
    return (pf, row["net"], -row["max_dd"])


def _rank_net(row):
    pf = row["pf"] if row["pf"] is not None else 1e18
    return (row["net"], pf, -row["max_dd"])


def recommend(base_oos, candidate_oos_in_lock_order):
    """First locked candidate that beats baseline frequency and clears the gates.

    Lock order is the in-sample order. The unused window is not re-ranked.
    """
    for row in candidate_oos_in_lock_order:
        if row["trades"] > base_oos["trades"] and passes(row, 200):
            return row
    return None


def duration_view(markets, days, cfg):
    return {
        "both": run_config(markets, days, cfg, None),
        "5m": run_config(markets, days, cfg, 300),
        "15m": run_config(markets, days, cfg, 900),
    }


def self_test() -> None:
    # BUY print is the ask. SELL print plus one cent is the ask.
    market = {
        "slot": 1,
        "dur": 300,
        "end": 1000,
        "prints": [
            # too early
            (700, -800_000, 300, 800_000, True, True),
            # SELL at 0.79 -> ask 0.80, time_left 100, winner
            (900, -800_000, 100, 800_000, True, True),
            # later and ignored
            (910, -900_000, 90, 900_000, False, False),
        ],
    }
    trade = first_trade(market, 0.80, 0.90, 45, 120)
    assert trade is not None and trade["ts"] == 900 and trade["won"]
    p = 0.80
    fee = 10 * 0.07 * p * (1 - p)
    assert abs(trade["pnl"] - (10 * (1 - p) - fee)) < 1e-9
    # Outside the band is not a trade. 0.70 ask with a 0.80 floor.
    market["prints"] = [(900, -700_000, 100, 700_000, True, True)]
    assert first_trade(market, 0.80, 0.90, 45, 120) is None
    # Ask of 0.99 is outside (0.01, 0.99) and never stored. A stored 0.95 loses.
    market["prints"] = [(900, -950_000, 100, 950_000, True, False)]
    loss = first_trade(market, 0.90, 0.95, 45, 120)
    assert loss is not None and loss["pnl"] < 0
    stats = summarize(
        [
            {"ts": 100, "end": 200, "dur": 300, "slot": 1, "pnl": 1.5, "won": True},
            {"ts": 300, "end": 400, "dur": 300, "slot": 2, "pnl": -0.5, "won": False},
        ],
        2,
    )
    assert stats["trades"] == 2
    assert abs(stats["pf"] - 3.0) < 1e-9
    assert abs(stats["max_dd"] - 0.5) < 1e-9
    assert stats["wr"] == 0.5
    print("self-test ok")


def dump(obj) -> None:
    json.dump(obj, sys.stdout, indent=2, sort_keys=True)
    sys.stdout.write("\n")


def main(argv: list[str]) -> int:
    common = argparse.ArgumentParser(add_help=False)
    common.add_argument("--cache", default="/tmp/lf-freq-cache")
    common.add_argument("--rps", type=float, default=20.0)
    common.add_argument("--workers", type=int, default=24)
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="cmd", required=True)

    sub.add_parser("self-test", parents=[common])

    fetch = sub.add_parser("fetch", parents=[common])
    fetch.add_argument("--tape", choices=("cover", "latest200"), default="cover")
    fetch.add_argument("--asset", default="BTC")
    fetch.add_argument("--start", required=True)
    fetch.add_argument("--end", required=True)

    ev = sub.add_parser("eval", parents=[common])
    ev.add_argument("--tape", choices=("cover", "latest200"), default="cover")
    ev.add_argument("--asset", default="BTC")
    ev.add_argument("--start", required=True)
    ev.add_argument("--end", required=True)
    ev.add_argument("--grid", action="store_true")
    ev.add_argument("--configs", default="", help="lo,hi,tmin,tmax;... empty = baseline")

    args = parser.parse_args(argv)
    global LIMITER
    LIMITER = Limiter(args.rps)

    if args.cmd == "self-test":
        self_test()
        return 0
    if args.cmd == "fetch":
        fetch_window(args.cache, args.tape, args.asset.upper(), args.start, args.end, args.workers)
        return 0

    markets, meta = load_markets(args.cache, args.tape, args.asset.upper(), args.start, args.end)
    days = meta["days"]
    if args.grid:
        rows = grid(markets, days)
        base, picked = lock_candidates(rows)
        dump({"meta": meta, "baseline": base, "candidates": picked, "grid": rows})
        return 0
    cfgs = [BASELINE]
    if args.configs:
        cfgs = []
        for chunk in args.configs.split(";"):
            lo, hi, tmin, tmax = chunk.split(",")
            cfgs.append((float(lo), float(hi), int(tmin), int(tmax)))
    out = {"meta": meta, "rows": []}
    for cfg in cfgs:
        out["rows"].append(duration_view(markets, days, cfg))
    dump(out)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

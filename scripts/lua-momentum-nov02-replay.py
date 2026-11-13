#!/usr/bin/env python3
"""Replay lua_momentum as of 2026-11-02 on the full trade tape.

Pinned strategy tree: 717d0e2305c3d9de947ad069cd368d2146512a36
(lua_momentum 0.2.1, the package on main during the 2026-11-02 cloud
capture). Entry rules are the down-leg maker gate from
d9a6c19be11f2df2df9310f9f1f20a5ec9b095c5; exits are the 0.2.1 defaults.

What the old code did
  * classify() computed move_pct = (mid-prev)/prev*100 and compared it with
    threshold 0.04. A 0.04% mid drop armed. #1169 later compared the fraction,
    so the manifest's 0.04 means 4%. This replay uses the old comparison.
  * Down token only. A non-negative or sub-threshold move clears the arm.
  * One maker bid per round, at that tick's best bid, posted only while
    time_left > 60. The kernel refused a second order on the same internal
    key, so the first bid rests until it fills or the round ends. It does
    not chase.
  * Fill only when a later print trades strictly through the bid (price <
    bid). A print at the bid is a touch and is not a fill. Fill price is
    the resting bid. Maker entry fee is 0.
  * Exits (mode=exits) follow strategy.lua: take-profit 40 and trail
    arm 20 / giveback 15, judged on best_bid minus 0.07*p*(1-p). The time
    rule default is 0 (off). A position that hits neither redeems at 0/1.
  * mode=settle never sells. That is what the 2026-11-02 engine replay
    actually booked (every close was Settlement): the Lua file emitted
    exits, and the author wrote that the PolySim ladder skips holders.
    The kernel also had a 50% holder stop (HOLDS_STOP_PCT). It is not
    applied here, because that replay's losers paid the full entry.

Tape
  * Gamma outcomePrices must be 0/1. Anything else is dropped.
  * Data API v2 trades, taker_only, paginated until the cursor ends. This is
    not the latest-200 cap, and it does not stop at 240s left the way
    scripts/late-favorite-frequency.py's cover cache does. Momentum can arm
    any time the round still has more than 60s.
  * Implied book, same cent as the late-favorite scripts: BUY print is the
    ask and the bid is one cent under; SELL print is the bid and the ask is
    one cent over. Quotes outside (0.01, 0.99) are not a book. The print
    itself can still trade through a resting bid.
  * 10 shares. Redemption is free. A taker exit pays shares*0.07*p*(1-p).
    The fee-0 + rebate column adds 0.20 * that curve on the maker entry,
    which is the 2026-11-02 cloud report's accounting.
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

UA = {"User-Agent": "Mozilla/5.0 (BlitzkriegBot-research; lua-momentum-nov02)"}
GAMMA = "gamma-api.polymarket.com"
DATA = "data-api.polymarket.com"

SHARES = 10.0
FEE_RATE = 0.07
REBATE = 0.20
# Old classify() compared a percent-point move with manifest 0.04.
THRESHOLD_PCT_POINTS = 0.04
TP_PCT = 40.0
TRAIL_ARM = 20.0
TRAIL_GIVE = 15.0
ENTRY_TIME_LEFT = 60
MICRO = 1_000_000
CENT = 10_000  # 0.01


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


LIMITER = Limiter(12.0)


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
    if parsed[0] >= 0.99 and parsed[1] <= 0.01:
        return outcomes[0]
    if parsed[1] >= 0.99 and parsed[0] <= 0.01:
        return outcomes[1]
    return None


def cache_file(root: str, asset: str, dur: int, slot: int) -> str:
    directory = os.path.join(root, asset.upper(), str(dur))
    os.makedirs(directory, exist_ok=True)
    return os.path.join(directory, f"{slot}.json")


def write_json(path: str, obj: dict) -> None:
    tmp = path + ".tmp"
    with open(tmp, "w", encoding="utf-8") as fh:
        json.dump(obj, fh, separators=(",", ":"))
    os.replace(tmp, path)


def fetch_one(root: str, asset: str, dur: int, slot: int) -> str:
    path = cache_file(root, asset, dur, slot)
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
                "unresolved": True,
                "winner": winner,
            },
        )
        return "unresolved"
    prints = []
    cursor = None
    covered = False
    scanned = 0
    oldest = None
    for _page in range(30):
        query = {"condition": cid, "limit": "1000", "taker_only": "true"}
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
        for row in rows:
            scanned += 1
            try:
                ts = int(row["timestamp"])
                price = float(row["price"])
            except (KeyError, TypeError, ValueError):
                continue
            if oldest is None or ts < oldest:
                oldest = ts
            if row.get("outcome") != "Down":
                continue
            if row.get("side") not in ("BUY", "SELL"):
                continue
            if ts < slot or ts >= end:
                continue
            px = int(round(price * MICRO))
            if px <= 0 or px >= MICRO:
                continue
            prints.append([ts, "B" if row["side"] == "BUY" else "S", px])
        pagination = body.get("pagination") or {}
        cursor = pagination.get("next_cursor")
        if not pagination.get("has_more") or not cursor:
            covered = True
            break
        if oldest is not None and oldest <= slot:
            covered = True
            break
    else:
        covered = oldest is not None and oldest <= slot
    uniq = []
    seen = set()
    for item in prints:
        key = tuple(item)
        if key in seen:
            continue
        seen.add(key)
        uniq.append(item)
    uniq.sort()
    write_json(
        path,
        {
            "slug": slug,
            "slot": slot,
            "dur": dur,
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


def fetch_window(root: str, asset: str, start: str, end: str, workers: int) -> None:
    jobs = list(iter_slots(start, end))
    counts = {"ok": 0, "cached": 0, "missing": 0, "unresolved": 0, "error": 0}
    lock = threading.Lock()
    done = 0
    print(f"fetch {asset} {start} -> {end} markets={len(jobs)}", flush=True)
    with ThreadPoolExecutor(max_workers=workers) as pool:
        futures = [pool.submit(fetch_one, root, asset, dur, slot) for dur, slot in jobs]
        for fut in as_completed(futures):
            status = fut.result()
            key = status if status in counts else "error"
            with lock:
                counts[key] += 1
                done += 1
                if done % 200 == 0 or done == len(jobs):
                    print(f"  {done}/{len(jobs)} {counts}", flush=True)
    print(f"done {asset} {start} {counts}", flush=True)


def book(side: str, px: int):
    """Implied bid/ask in micro-units. None when the cent book is not two-sided."""
    if side == "B":
        ask = px
        bid = px - CENT
    else:
        bid = px
        ask = px + CENT
    if bid <= CENT or ask >= 99 * CENT or bid >= ask:
        return None
    return bid, ask


def fee_per_share(price: float) -> float | None:
    if price <= 0.0 or price >= 1.0:
        return None
    return FEE_RATE * price * (1.0 - price)


def net_pct(proceeds: float, entry: float) -> float:
    return (proceeds - entry) / entry * 100.0


def replay(prints: list, end: int, dur: int, down_wins: bool, mode: str):
    """One round. Returns a trade dict or None when the bid never fills."""
    last_mid = None
    rest = None
    entry = None
    entry_ts = None
    peak = None
    exit_px = None
    exit_ts = None
    reason = None
    for ts, side, px in prints:
        tl = end - int(ts)
        if tl <= 0 or tl > dur:
            continue
        if rest is not None and entry is None and px < rest:
            entry = rest / MICRO
            entry_ts = int(ts)
        quote = book(side, px)
        if entry is not None and exit_px is None and mode == "exits" and quote is not None:
            bid = quote[0] / MICRO
            fee = fee_per_share(bid)
            if fee is not None and entry > 0:
                proceeds = bid - fee
                if peak is None or proceeds > peak:
                    peak = proceeds
                if net_pct(proceeds, entry) >= TP_PCT:
                    exit_px = bid
                    exit_ts = int(ts)
                    reason = "take_profit"
                elif (
                    peak is not None
                    and net_pct(peak, entry) >= TRAIL_ARM
                    and (peak - proceeds) / entry * 100.0 >= TRAIL_GIVE
                ):
                    exit_px = bid
                    exit_ts = int(ts)
                    reason = "trailing_stop"
        if entry is None and rest is None and quote is not None and tl > ENTRY_TIME_LEFT:
            mid = (quote[0] + quote[1]) / 2.0 / MICRO
            prev = last_mid
            last_mid = mid
            if prev is not None and prev > 0:
                move = (mid - prev) / prev * 100.0
                if move < 0 and abs(move) >= THRESHOLD_PCT_POINTS:
                    rest = quote[0]
        elif entry is None and rest is None and quote is not None:
            # Inside the last minute the strategy still observes, it just
            # does not post. Keep the mid so a later round is not this one.
            last_mid = (quote[0] + quote[1]) / 2.0 / MICRO
    if entry is None:
        return None
    if exit_px is None:
        payoff = 1.0 if down_wins else 0.0
        gross = SHARES * (payoff - entry)
        fee = 0.0
        reason = "settlement"
        exit_ts = end
        exit_px = payoff
    else:
        gross = SHARES * (exit_px - entry)
        fee = SHARES * fee_per_share(exit_px)
    rebate = REBATE * SHARES * FEE_RATE * entry * (1.0 - entry)
    return {
        "ts": entry_ts,
        "exit_ts": exit_ts,
        "end": end,
        "dur": dur,
        "reason": reason,
        "entry": entry,
        "exit": exit_px,
        "gross": gross,
        "fee": fee,
        "rebate": rebate,
        "won_settle": down_wins,
    }


def load_markets(root: str, asset: str, start: str, end: str):
    markets = []
    missing = unresolved = uncovered = absent = 0
    for dur, slot in iter_slots(start, end):
        path = cache_file(root, asset, dur, slot)
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
            continue
        markets.append(
            {
                "slot": int(row["slot"]),
                "dur": int(row["dur"]),
                "end": int(row["end"]),
                "down_wins": row["winner"] == "Down",
                "prints": row.get("prints") or [],
            }
        )
    days = (day_start(end) - day_start(start)) / 86400
    meta = {
        "markets": len(markets),
        "missing": missing,
        "unresolved": unresolved,
        "uncovered": uncovered,
        "absent": absent,
        "days": days,
    }
    return markets, meta


def wilson(wins: int, n: int, z: float = 1.96) -> tuple[float | None, float | None]:
    if n <= 0:
        return None, None
    p = wins / n
    den = 1.0 + z * z / n
    centre = (p + z * z / (2 * n)) / den
    margin = z * ((p * (1.0 - p) / n + z * z / (4 * n * n)) ** 0.5) / den
    return centre - margin, centre + margin


def summarize(trades: list, days: float, pnl_key: str) -> dict:
    ordered = sorted(trades, key=lambda t: (t["exit_ts"], t["dur"], t["end"], t["ts"]))
    wins = [t for t in ordered if t[pnl_key] > 0]
    losses = [t for t in ordered if t[pnl_key] < 0]
    gross_win = sum(t[pnl_key] for t in wins)
    gross_loss = -sum(t[pnl_key] for t in losses)
    decided = len(wins) + len(losses)
    equity = peak = max_dd = 0.0
    for trade in ordered:
        equity += trade[pnl_key]
        if equity > peak:
            peak = equity
        dd = peak - equity
        if dd > max_dd:
            max_dd = dd
    n = len(ordered)
    lo, hi = wilson(len(wins), decided)
    reasons = {}
    for trade in ordered:
        reasons[trade["reason"]] = reasons.get(trade["reason"], 0) + 1
    pf = None if gross_loss == 0 else gross_win / gross_loss
    return {
        "trades": n,
        "trades_per_day": (n / days) if days else None,
        "wins": len(wins),
        "losses": len(losses),
        "wr": (len(wins) / decided) if decided else None,
        "wilson95": [lo, hi],
        "pf": pf,
        "net": sum(t[pnl_key] for t in ordered),
        "max_dd": max_dd,
        "reasons": reasons,
    }


def run_window(markets, days, dur_filter, mode: str) -> dict:
    trades = []
    for market in markets:
        if dur_filter is not None and market["dur"] != dur_filter:
            continue
        trade = replay(
            market["prints"],
            market["end"],
            market["dur"],
            market["down_wins"],
            mode,
        )
        if trade is None:
            continue
        trade["pnl_off"] = trade["gross"]
        trade["pnl_on"] = trade["gross"] - trade["fee"]
        trade["pnl_rebate"] = trade["gross"] + trade["rebate"]
        trades.append(trade)
    out = {"dur": dur_filter, "mode": mode}
    for key, label in (
        ("pnl_off", "fee_off"),
        ("pnl_on", "fee_on"),
        ("pnl_rebate", "fee_off_rebate"),
    ):
        out[label] = summarize(trades, days, key)
    return out


def dump(obj) -> None:
    json.dump(obj, sys.stdout, indent=2, sort_keys=True)
    sys.stdout.write("\n")


def self_test() -> None:
    def u(price: float) -> int:
        return int(round(price * MICRO))

    # Any down move of the reconstructed mid arms (0.04% is below one cent).
    # SELL 0.50 then SELL 0.48: mids 0.505 -> 0.485, bid rests at 0.48.
    # A later print at 0.48 only touches. 0.47 trades through. Down redeems.
    end = 1_000
    prints = [
        (700, "S", u(0.50)),
        (710, "S", u(0.48)),
        (720, "S", u(0.48)),
        (730, "B", u(0.47)),
    ]
    trade = replay(prints, end, 300, True, "settle")
    assert trade is not None, "through-fill missing"
    assert abs(trade["entry"] - 0.48) < 1e-9
    assert trade["reason"] == "settlement"
    assert abs(trade["gross"] - SHARES * (1 - 0.48)) < 1e-9
    assert trade["fee"] == 0.0

    # Touch never fills, and the round ends with the bid still resting.
    touch = replay(prints[:3], end, 300, True, "settle")
    assert touch is None, "a print at the bid must not fill"

    # 0.03 percentage points does not arm. 0.05 does. Constructed mids via
    # SELL prints (mid = price + 0.005).
    prev = int(round((0.50 - 0.005) * MICRO))
    tiny = int(round((0.50 * (1 - 0.0003) - 0.005) * MICRO))
    no = replay([(700, "S", prev), (710, "S", tiny), (720, "B", tiny - CENT)], end, 300, True, "settle")
    assert no is None, "0.03% must stay under the old 0.04 threshold"
    enough = int(round((0.50 * (1 - 0.0005) - 0.005) * MICRO))
    yes = replay(
        [(700, "S", prev), (710, "S", enough), (720, "B", enough - 1)],
        end,
        300,
        False,
        "settle",
    )
    assert yes is not None and yes["reason"] == "settlement"
    assert yes["gross"] < 0

    # Last minute does not post.
    late = replay(
        [(950, "S", u(0.50)), (960, "S", u(0.40)), (970, "B", u(0.30))],
        end,
        300,
        True,
        "settle",
    )
    assert late is None

    # Take profit: rest 0.40, trade through, then a 0.60 bid clears 40% net.
    tp_prints = [
        (700, "S", u(0.50)),
        (710, "S", u(0.40)),
        (720, "B", u(0.39)),
        (800, "S", u(0.60)),
    ]
    tp = replay(tp_prints, end, 300, False, "exits")
    assert tp is not None and tp["reason"] == "take_profit"
    assert abs(tp["exit"] - 0.60) < 1e-9
    fee = SHARES * FEE_RATE * 0.60 * 0.40
    assert abs(tp["fee"] - fee) < 1e-9
    assert abs(tp["gross"] - SHARES * (0.60 - 0.40)) < 1e-9
    held = replay(tp_prints, end, 300, False, "settle")
    assert held is not None and held["reason"] == "settlement"
    assert held["gross"] < 0

    # Trail: +20% net at bid 0.50, then a giveback of 15 points, before +40%.
    trail_prints = [
        (700, "S", u(0.50)),
        (710, "S", u(0.40)),
        (720, "B", u(0.39)),
        (800, "S", u(0.50)),
        (860, "S", u(0.43)),
    ]
    trail = replay(trail_prints, end, 300, True, "exits")
    assert trail is not None and trail["reason"] == "trailing_stop", trail
    assert abs(trail["exit"] - 0.43) < 1e-9

    # Up winning the round is a full loss if nothing sells.
    up = replay(prints, end, 300, False, "settle")
    assert up is not None and abs(up["gross"] - SHARES * (0 - 0.48)) < 1e-9
    print("self-test ok")


def main(argv: list[str]) -> int:
    common = argparse.ArgumentParser(add_help=False)
    common.add_argument("--cache", default="/tmp/lm-nov02-cache")
    common.add_argument("--rps", type=float, default=12.0)
    common.add_argument("--workers", type=int, default=16)
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="cmd", required=True)
    sub.add_parser("self-test", parents=[common])
    fetch = sub.add_parser("fetch", parents=[common])
    fetch.add_argument("--asset", default="BTC")
    fetch.add_argument("--start", required=True)
    fetch.add_argument("--end", required=True)
    ev = sub.add_parser("eval", parents=[common])
    ev.add_argument("--asset", default="BTC")
    ev.add_argument("--start", required=True)
    ev.add_argument("--end", required=True)
    args = parser.parse_args(argv)
    global LIMITER
    LIMITER = Limiter(args.rps)
    if args.cmd == "self-test":
        self_test()
        return 0
    if args.cmd == "fetch":
        fetch_window(args.cache, args.asset.upper(), args.start, args.end, args.workers)
        return 0
    markets, meta = load_markets(args.cache, args.asset.upper(), args.start, args.end)
    rows = []
    for mode in ("exits", "settle"):
        for dur in (300, 900, None):
            rows.append(run_window(markets, meta["days"], dur, mode))
    dump(
        {
            "sha": "717d0e2305c3d9de947ad069cd368d2146512a36",
            "threshold_effective": "0.04 percentage points (0.04%)",
            "start": args.start,
            "end": args.end,
            "meta": meta,
            "rows": rows,
        }
    )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

#!/usr/bin/env python3
"""Spot-confirmation grid for the full-tape late_favorite replay.

The tape is the cover cache from scripts/late-favorite-frequency.py: every
print with 20-240s left, not the latest-200 cap. Entry is still the first
print inside 45-120s whose implied ask is inside the band. One entry per
market. The spot filter accepts or rejects that single attempt. It does not
walk forward to a later print.

Settlement reference for these markets is the Chainlink BTC/USD 60s TWAP
stream. Polymarket's proxy of that stream refuses timestamps older than 30
days, which drops the whole Sep 12-26 window and Oct 12-14. The series used
here is Binance BTCUSDT 1-second klines (data.binance.vision): the open of
the round-start second is the strike, the close of a second is the spot.

Momentum "agrees" means the N-second simple return is strictly on the
favorite's side. That is threshold 0 in the #1169 fraction units, where
0.04 would mean 4%. A 4% gate is not in this grid.

`lock` reads only the in-sample window and writes the three configs.
`oos` scores those configs plus the unfiltered baseline once.
"""

from __future__ import annotations

import argparse
import hashlib
import io
import json
import os
import sys
import urllib.request
import zipfile
from concurrent.futures import ThreadPoolExecutor, as_completed
from datetime import datetime, timedelta, timezone

import importlib.util

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
spec = importlib.util.spec_from_file_location(
    "lf", os.path.join(ROOT, "scripts", "late-favorite-frequency.py")
)
lf = importlib.util.module_from_spec(spec)
spec.loader.exec_module(lf)

IN_SAMPLE = ("2026-10-12", "2026-10-26")
OOS = ("2026-09-12", "2026-09-26")
BANDS = ((0.80, 0.90), (0.75, 0.92))
XS = (0.0, 0.0002, 0.0005, 0.001)
LOOKBACKS = (None, 30, 60)
TMIN, TMAX = 45, 120
FEE = 0.07
UA = {"User-Agent": "Mozilla/5.0 (BlitzkriegBot-research; late-favorite-spot)"}
VISION = "https://data.binance.vision/data/spot/daily/klines/BTCUSDT/1s/BTCUSDT-1s-{day}.zip"


def vision_url(day: str) -> str:
    """The pinned data.binance.vision kline URL for one `YYYY-MM-DD` day.

    SSRF containment (CWE-918): the URL is built from the pinned VISION
    template and a day that must match the `YYYY-MM-DD` grammar before
    interpolation, so no caller-supplied string can steer the fetch at an
    internal / metadata address. The day grammar check below is the
    containment; the sole consumer (`one`) passes nothing else.
    """
    if len(day) != 10 or day[4] != "-" or day[7] != "-":
        raise RuntimeError(f"refusing non-day archive key: {day!r}")
    return VISION.format(day=day)


def day_range():
    days = []
    for start, end in (("2026-09-12", "2026-09-27"), ("2026-10-12", "2026-10-27")):
        cur = datetime.fromisoformat(start).replace(tzinfo=timezone.utc)
        stop = datetime.fromisoformat(end).replace(tzinfo=timezone.utc)
        while cur < stop:
            days.append(cur.strftime("%Y-%m-%d"))
            cur += timedelta(days=1)
    return days


def fetch_bytes(url: str) -> bytes:
    req = urllib.request.Request(url, headers=UA)
    with urllib.request.urlopen(req, timeout=60) as resp:
        return resp.read()


def ensure_spot(cache: str) -> dict:
    os.makedirs(cache, exist_ok=True)
    days = day_range()

    def one(day: str):
        path = os.path.join(cache, f"{day}.json")
        if os.path.exists(path):
            return day, "cached"
        blob = fetch_bytes(vision_url(day))
        check = fetch_bytes(vision_url(day) + ".CHECKSUM").decode().split()[0]
        digest = hashlib.sha256(blob).hexdigest()
        if digest != check:
            raise RuntimeError(f"checksum mismatch {day}")
        with zipfile.ZipFile(io.BytesIO(blob)) as zf:
            name = zf.namelist()[0]
            text = zf.read(name).decode()
        opens = [0.0] * 86400
        closes = [0.0] * 86400
        filled = 0
        day_start = int(datetime.fromisoformat(day).replace(tzinfo=timezone.utc).timestamp())
        for line in text.splitlines():
            if not line or line[0] not in "0123456789":
                continue
            parts = line.split(",")
            raw = int(parts[0])
            # Binance vision 1s files are microseconds. Milliseconds still parse.
            if raw > 10**15:
                ts = raw // 1_000_000
            elif raw > 10**12:
                ts = raw // 1_000
            else:
                ts = raw
            if ts < day_start or ts >= day_start + 86400:
                continue
            idx = ts - day_start
            opens[idx] = float(parts[1])
            closes[idx] = float(parts[4])
            filled += 1
        if filled < 80000:
            raise RuntimeError(f"{day} only {filled} seconds")
        from pathlib import Path

        Path(path).write_text(
            json.dumps({"open": opens, "close": closes, "n": filled}, separators=(",", ":")),
            encoding="utf-8",
        )
        return day, "ok"

    out = {}
    with ThreadPoolExecutor(max_workers=6) as pool:
        futs = [pool.submit(one, day) for day in days]
        for fut in as_completed(futs):
            day, status = fut.result()
            out[day] = status
            print(f"spot {day} {status}", flush=True)
    return out


class Spot:
    def __init__(self, cache: str) -> None:
        from pathlib import Path

        self.days = {}
        for day in day_range():
            path = os.path.join(cache, f"{day}.json")
            row = json.loads(Path(path).read_text(encoding="utf-8"))
            self.days[day] = (row["open"], row["close"])

    def price(self, ts: int, field: str) -> float | None:
        # Walk back at most 5s. A longer hole is a missing quote, not a price.
        for back in range(6):
            moment = datetime.fromtimestamp(ts - back, timezone.utc)
            day = moment.strftime("%Y-%m-%d")
            book = self.days.get(day)
            if book is None:
                continue
            px = book[0 if field == "open" else 1][moment.hour * 3600 + moment.minute * 60 + moment.second]
            if px > 0:
                return px
        return None


def entries(markets, lo: float, hi: float):
    lo_u = int(round(lo * lf.ASK_MICRO))
    hi_u = int(round(hi * lf.ASK_MICRO))
    found = []
    for market in markets:
        for ts, _neg, tl, ask, up, won in market["prints"]:
            if tl < TMIN or tl > TMAX or ask < lo_u or ask > hi_u:
                continue
            p = ask / lf.ASK_MICRO
            fee = lf.SHARES * FEE * p * (1.0 - p)
            pnl = lf.SHARES * ((1.0 if won else 0.0) - p) - fee
            found.append(
                {
                    "ts": ts,
                    "end": market["end"],
                    "dur": market["dur"],
                    "slot": market["slot"],
                    "pnl": pnl,
                    "won": won,
                    "ask": p,
                    "up": up,
                    "fee_ps": FEE * p * (1.0 - p),
                }
            )
            break
    return found


def accept(trade, spot: Spot, x: float, lookback):
    strike = spot.price(trade["slot"], "open")
    now = spot.price(trade["ts"], "close")
    if strike is None or now is None or strike <= 0 or now <= 0:
        return False
    edge = (now / strike - 1.0) if trade["up"] else (1.0 - now / strike)
    if edge < x:
        return False
    if lookback is None:
        return True
    prev = spot.price(trade["ts"] - lookback, "close")
    if prev is None or prev <= 0:
        return False
    ret = now / prev - 1.0
    return ret > 0 if trade["up"] else ret < 0


def stats_of(trades, days: float):
    base = lf.summarize(trades, days)
    if trades:
        base["be_wr"] = sum(t["ask"] + t["fee_ps"] for t in trades) / len(trades)
        base["avg_ask"] = sum(t["ask"] for t in trades) / len(trades)
    else:
        base["be_wr"] = None
        base["avg_ask"] = None
    return base


def score(row):
    wr = row["wr"] or 0.0
    pf = 3.0 if row["pf"] is None else min(row["pf"], 3.0)
    return min(wr / 0.80, pf / 1.2)


def rank_key(row):
    pf = row["pf"] if row["pf"] is not None else 1e9
    return (score(row), pf, row["net"], -row["max_dd"], row["trades"])


def lock_three(rows):
    # Highest joint score, at least 50 trades. An empty cell has an undefined
    # profit factor and stays out of the ranking.
    pool = [r for r in rows if r["trades"] >= 50]
    pool = sorted(pool, key=rank_key, reverse=True)
    picked = []
    seen = set()
    for row in pool:
        key = json.dumps(row["config"], sort_keys=True)
        if key in seen:
            continue
        seen.add(key)
        picked.append(row["config"])
        if len(picked) == 3:
            break
    return picked


def apply_config(prepared, spot, days, cfg, dur=None):
    raw = prepared[(cfg["min_px"], cfg["max_px"])]
    if cfg.get("x") is None:
        kept = list(raw)
    else:
        kept = [t for t in raw if accept(t, spot, cfg["x"], cfg["lookback_sec"])]
    if dur is not None:
        kept = [t for t in kept if t["dur"] == dur]
    row = stats_of(kept, days)
    row["config"] = cfg
    return row


def fmt(row):
    c = row["config"]
    look = "off" if c.get("lookback_sec") in (None, 0) else f"{c['lookback_sec']}s"
    x = c.get("x")
    label = "unfiltered" if x is None else f"{c['min_px']:.2f}-{c['max_px']:.2f} x={x * 100:.3f}% mom={look}"
    pf = "inf" if row["pf"] is None else f"{row['pf']:.3f}"
    wr = "n/a" if row["wr"] is None else f"{row['wr'] * 100:.2f}%"
    be = "n/a" if row["be_wr"] is None else f"{row['be_wr'] * 100:.2f}%"
    return (
        f"{label:42} n={row['trades']:5} /day={row['trades_per_day']:.2f} "
        f"WR={wr:8} BE={be:8} PF={pf:8} net={row['net']:9.2f} dd={row['max_dd']:.2f} "
        f"score={score(row):.3f}"
    )


def load_prepared(cache_tape, spot, start, end):
    markets, meta = lf.load_markets(cache_tape, "cover", "BTC", start, end)
    prepared = {band: entries(markets, *band) for band in BANDS}
    return markets, meta, prepared


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--tape-cache", default="/tmp/lf-freq-cache")
    parser.add_argument("--spot-cache", default="/tmp/lf-spot")
    parser.add_argument("--lock", default="/tmp/lf-spot-lock.json")
    sub = parser.add_subparsers(dest="cmd", required=True)
    sub.add_parser("fetch-spot")
    sub.add_parser("lock")
    sub.add_parser("oos")
    args = parser.parse_args(argv)

    if args.cmd == "fetch-spot":
        ensure_spot(args.spot_cache)
        return 0

    ensure_spot(args.spot_cache)
    spot = Spot(args.spot_cache)

    if args.cmd == "lock":
        _markets, meta, prepared = load_prepared(args.tape_cache, spot, *IN_SAMPLE)
        rows = []
        for band, raw in prepared.items():
            for x in XS:
                for look in LOOKBACKS:
                    kept = [t for t in raw if accept(t, spot, x, look)]
                    row = stats_of(kept, meta["days"])
                    row["config"] = {
                        "min_px": band[0],
                        "max_px": band[1],
                        "x": x,
                        "lookback_sec": look,
                    }
                    rows.append(row)
        base_cfg = {"min_px": 0.80, "max_px": 0.90, "x": None, "lookback_sec": None}
        baseline = apply_config(prepared, spot, meta["days"], base_cfg)
        picked = lock_three(rows)
        payload = {
            "window": {"start": IN_SAMPLE[0], "end": IN_SAMPLE[1], "role": "in-sample"},
            "meta": meta,
            "baseline": baseline,
            "picked": picked,
            "grid": rows,
        }
        from pathlib import Path

        Path(args.lock).write_text(json.dumps(payload), encoding="utf-8")
        print("IN-SAMPLE baseline")
        print(fmt(baseline))
        print("IN-SAMPLE grid, best first")
        for row in sorted(rows, key=rank_key, reverse=True):
            mark = " *" if row["config"] in picked else ""
            print(fmt(row) + mark)
        print("LOCKED", json.dumps(picked))
        return 0

    from pathlib import Path

    locked = json.loads(Path(args.lock).read_text(encoding="utf-8"))
    _markets, meta, prepared = load_prepared(args.tape_cache, spot, *OOS)
    base_cfg = {"min_px": 0.80, "max_px": 0.90, "x": None, "lookback_sec": None}
    configs = [base_cfg, *locked["picked"]]
    report = {"window": {"start": OOS[0], "end": OOS[1], "role": "oos"}, "meta": meta, "rows": []}
    print("OOS")
    for cfg in configs:
        both = apply_config(prepared, spot, meta["days"], cfg)
        five = apply_config(prepared, spot, meta["days"], cfg, dur=300)
        fifteen = apply_config(prepared, spot, meta["days"], cfg, dur=900)
        report["rows"].append({"both": both, "5m": five, "15m": fifteen})
        print("BOTH ", fmt(both))
        print("5m   ", fmt(five))
        print("15m  ", fmt(fifteen))
    out = args.lock.replace(".json", "-oos.json")
    from pathlib import Path

    Path(out).write_text(json.dumps(report), encoding="utf-8")
    print("wrote", out)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

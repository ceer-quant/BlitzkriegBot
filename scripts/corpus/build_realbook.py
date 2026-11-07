#!/usr/bin/env python3
"""Build a kernel-replayable archive from the PRODUCTION event archive.

PROVENANCE & FIX (issue #890): ported from
bk-session/live-readiness/realbook/build-realbook.py (session scratch, not
committed) with ONE fix — `k=resolution` rows now carry outcome TOKEN ids,
not the human-readable payout labels "Up"/"Down". The original wrote
`p: [["Up","1"],["Down","0"]]`; the kernel's settlement matches payouts by
exact token id, so every label-form row never triggered: positions stayed
open and the replay closed 0 trades (A4 verdict §5.1,
scripts/corpus/fix_resolutions.py --check is the gate for this class of bug).
Rounds lacking upTokenId/downTokenId are dropped (`no_tokens`), never
emitted as unmatchable resolutions. build-realbook-multi.py already wrote
token ids and needed no fix.

Input : /Volumes/Hard Disk/BlitzkriegBot/data/archive/events.*.jsonl
        (live recording: k in {spot, book, books, round})

WHY THIS EXISTS
  Every prior strategy test in this repo ran on one of two corpora:
    * `*-act-*.jsonl`  — on-chain wallet activity, whose book is SYNTHESISED
      with bid == ask (100% zero spread, tools/onchain_pull/src/lib.rs:1633).
      A post-only strategy always crosses there, so maker results are fiction.
    * `tape-archive*.jsonl` — built from public prints, also synthesised
      (bid = px-0.01, ask = px).
  The production archive is the only corpus that carries the VENUE'S OWN
  two-sided book. Ladder order, verified on the tape:
      ask `a` is DESCENDING (a[0] = 0.999 … a[-1] = best ask)
      bid `b` is ASCENDING  (b[0] = 0.001 … b[-1] = best bid)
  Real spreads run 0.001–0.02 with a 0.01 mode, so a post-only quote is
  genuinely tested instead of being handed a crossed book.

WHAT THIS CONVERTER ADDS
  1. `resolution` events derived from the BTC spot tape. The production
     archive records no resolution rows (settlement is a live concern), so a
     replay would hold every position to the tail and never close it.
     The round's winner is sign(spot(expiresAt) - spot(open)), and the open
     instant comes from `roundSlot * roundDurationSec` — verified exact on the
     tape: 1992771 * 900 == expiresAtMs/1000.
  2. `roundDurationSec` filled (the recorder stamps 0 — it does not know the
     family), and `archiveVerdict` set so the dry synthesis never races the
     resolution.
  3. Dedupe: the recorder re-emits the same round markets on every refresh;
     only the first sighting per (conditionId, roundSlot) is kept.
  4. Rounds not fully bracketed by the spot tape are DROPPED, never guessed.
"""
import argparse, json, os, sys
from decimal import Decimal


def safe_write_path(path: str) -> str:
    """拒绝含穿越组件(`..`)的输出路径（path traversal，高危）。"""
    parts = path.replace("\\", "/").split("/")
    if ".." in parts:
        raise ValueError(f"refusing path-traversal output path: {path!r}")
    return path


def safe_open(path: str, mode: str = "r"):
    if any(c in mode for c in "wax+"):
        path = safe_write_path(path)
    return open(path, mode)


ARCHIVE = "/Volumes/Hard Disk/BlitzkriegBot/data/archive"
FAMILIES = (300, 900, 3600, 14400)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--src", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--meta", default="")
    ap.add_argument("--asset", default="BTC")
    ap.add_argument("--max-rounds", type=int, default=0)
    ap.add_argument("--window-min", type=int, default=0)
    args = ap.parse_args()

    src = args.src if os.path.isabs(args.src) else os.path.join(ARCHIVE, args.src)

    spots, rounds, order, events = [], {}, [], []
    t0 = None
    n_spot = n_book = n_books = n_round = 0

    for line in open(src):
        line = line.strip()
        if not line:
            continue
        try:
            o = json.loads(line)
        except Exception:
            continue
        at = o.get("at")
        if at is None:
            continue
        if t0 is None:
            t0 = at
        if args.window_min and at - t0 > args.window_min * 60_000:
            break
        k = o.get("k")
        if k == "spot":
            if o.get("s") != args.asset:
                continue
            try:
                spots.append((at, Decimal(o["p"])))
            except Exception:
                continue
            n_spot += 1
        elif k in ("book", "books"):
            events.append(o)
            if k == "book": n_book += 1
            else: n_books += 1
        elif k == "round":
            n_round += 1
            for m in o.get("m", []):
                if m.get("asset") != args.asset:
                    continue
                key = (m.get("conditionId"), m.get("roundSlot"))
                if key in rounds:
                    continue
                rounds[key] = m
                order.append(key)

    spots.sort()
    if not spots:
        print("no spot rows for asset", args.asset, file=sys.stderr)
        return 2
    spot_first, spot_last = spots[0][0], spots[-1][0]

    def spot_at(ts_ms):
        lo, hi, best = 0, len(spots) - 1, None
        while lo <= hi:
            mid = (lo + hi) // 2
            if spots[mid][0] <= ts_ms:
                best = spots[mid][1]; lo = mid + 1
            else:
                hi = mid - 1
        return best

    kept, drop_span, drop_nofam, drop_flat, drop_tokens = [], 0, 0, 0, 0
    for key in order:
        m = rounds[key]
        exp_s = m.get("expiresAtMs")
        slot = m.get("roundSlot")
        if not exp_s or slot is None:
            drop_span += 1; continue
        if not m.get("upTokenId") or not m.get("downTokenId"):
            # a resolution without token ids can never match — same failure
            # class as the label bug (#890); drop, never emit it
            drop_tokens += 1; continue
        exp_ms = int(exp_s)
        dur = next((f for f in FAMILIES if abs(slot * f * 1000 - exp_ms) < 2000), None)
        if dur is None:
            drop_nofam += 1; continue
        open_ms = (slot - 1) * dur * 1000
        if open_ms < spot_first or exp_ms > spot_last:
            drop_span += 1; continue
        p0, p1 = spot_at(open_ms), spot_at(exp_ms)
        if p0 is None or p1 is None or p0 == p1:
            drop_flat += 1; continue
        out = dict(m)
        out["roundDurationSec"] = dur
        out["archiveVerdict"] = True
        kept.append((out, "Up" if p1 > p0 else "Down", p0, p1, open_ms, exp_ms))
        if args.max_rounds and len(kept) >= args.max_rounds:
            break

    out_rows = []
    for m, winner, p0, p1, open_ms, exp_ms in kept:
        out_rows.append((open_ms, {"at": open_ms, "k": "round", "m": [m]}))
        out_rows.append((exp_ms, {
            "at": exp_ms, "k": "resolution", "c": m["conditionId"],
            "p": [[m["upTokenId"], "1" if winner == "Up" else "0"],
                  [m["downTokenId"], "1" if winner == "Down" else "0"]],
            "n": False}))
    for o in events:
        out_rows.append((o["at"], o))
    out_rows.sort(key=lambda x: x[0])

    os.makedirs(os.path.dirname(os.path.abspath(args.out)) or ".", exist_ok=True)
    with safe_open(args.out, "w") as fh:
        for _, o in out_rows:
            fh.write(json.dumps(o) + "\n")

    meta = {
        "src": src, "asset": args.asset, "out": args.out,
        "rows": {"spot": n_spot, "book": n_book, "books": n_books, "round_raw": n_round},
        "rounds_seen": len(rounds), "rounds_kept": len(kept),
        "dropped": {"span": drop_span, "no_family": drop_nofam, "flat": drop_flat,
                    "no_tokens": drop_tokens},
        "emitted_rows": len(out_rows),
        "spot_span_ms": [spot_first, spot_last],
        "winner_mix": {w: sum(1 for _, ww, _, _, _, _ in kept if ww == w) for w in ("Up", "Down")},
        "rounds": [{"cond": m["conditionId"], "slot": m["roundSlot"],
                    "dur": m["roundDurationSec"], "open_ms": om, "expires": em,
                    "winner": w, "spot0": str(p0), "spot1": str(p1),
                    "question": m.get("question", "")}
                   for m, w, p0, p1, om, em in kept],
    }
    if args.meta:
        with safe_open(args.meta, "w") as fh:
            json.dump(meta, fh, indent=1)
    print(json.dumps({k: v for k, v in meta.items() if k != "rounds"}, indent=1))
    return 0


if __name__ == "__main__":
    sys.exit(main())

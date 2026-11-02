#!/usr/bin/env python3
"""Fix / gate corpus settlement rows that carry payout LABELS instead of TOKEN ids.

BUG (issue #890, A4 verdict §5.1): the corpus builder emitted `k=resolution`
rows whose `p` field held human-readable labels ("Up"/"Down") instead of the
outcome token ids the schema demands (`p: [["<token_id>","<payout>"]]`). The
kernel's settlement matches payouts by exact token id, so every label-form row
never triggered: positions stayed open and the replay closed 0 trades.

HOW THE FIX WORKS (ported from
bk-session/deep-audit-1102/a4-livereplay/corpus/fix-resolutions.py, which
repaired 22,683 lines / 16 resolutions, ok=16 bad=0):
  pass 1  collect conditionId -> (upTokenId, downTokenId) from `k=round` rows
  pass 2  for each `k=resolution` row still in LABEL form, rewrite `p` to
          [[upTokenId, payout_of_Up], [downTokenId, payout_of_Down]],
          preserving each side's own payout string. Token-form rows and every
          other line are copied through byte-for-byte (so --apply is
          idempotent, down to the counters); rewritten rows are dumped compact
          (the original tool's format). With --apply the file is rewritten
          in place via a sibling temp file + atomic rename.

MODES
  --check  read-only. Report `lines=` and `resolutions_ok=/resolutions_bad=`.
           Exit 1 if any label-form resolution exists — use as a gate after
           every corpus build.
  --apply  fix in place. Prints the original tool's counters
           (`lines= resolutions_rewritten= resolutions_without_round=` /
           `round_rows_with_tokens=`). Exits 1 if label-form rows remain
           (their conditionId had no round row, so they are unfixable).

Self-test with an embedded fixture: test_fix_resolutions.py (stdlib only).
"""
import argparse
import json
import os
import sys


def read_rows(path):
    """Yield (raw_line, obj_or_None) for every line (original tool counted
    every line, JSON or not)."""
    with open(path, "r") as fh:
        for raw in fh:
            try:
                yield raw, json.loads(raw)
            except Exception:
                yield raw, None


def collect_tokens(path):
    """pass 1: conditionId -> (upTokenId, downTokenId) from round rows."""
    tokens = {}
    for _, o in read_rows(path):
        if not o or o.get("k") != "round":
            continue
        for m in o.get("m", []):
            c = m.get("conditionId")
            up, down = m.get("upTokenId"), m.get("downTokenId")
            if c and up and down:
                tokens[c] = (up, down)
    return tokens


def res_is_label(o):
    """A resolution row is label-form when any payout key is a label."""
    return any(lab in ("Up", "Down") for lab, _ in o.get("p", []))


def check(path):
    nlines = ok = bad = 0
    for raw, o in read_rows(path):
        nlines += 1
        if not o or o.get("k") != "resolution":
            continue
        if res_is_label(o):
            bad += 1
        else:
            ok += 1
    print(f"lines={nlines} resolutions_ok={ok} resolutions_bad={bad}")
    return 1 if bad else 0


def apply_fix(path):
    tokens = collect_tokens(path)
    tmp = path + ".tmp"
    nlines = rewritten = missing = 0
    with open(tmp, "w") as out:
        for raw, o in read_rows(path):
            nlines += 1
            if (o and o.get("k") == "resolution" and res_is_label(o)):
                if o.get("c") in tokens:
                    up, down = tokens[o["c"]]
                    # map payout by LABEL (Up/Down -> token id), preserving
                    # each side's own payout string.
                    by_label = {lab: pay for lab, pay in o.get("p", [])}
                    o["p"] = [[up, by_label.get("Up", "0")],
                              [down, by_label.get("Down", "0")]]
                    out.write(json.dumps(o, separators=(",", ":")) + "\n")
                    rewritten += 1
                    continue
                missing += 1  # no round row -> unfixable, stays as-is
            out.write(raw)
    os.replace(tmp, path)
    print(f"lines={nlines} resolutions_rewritten={rewritten} "
          f"resolutions_without_round={missing}")
    print(f"round_rows_with_tokens={len(tokens)}")
    return 1 if missing else 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    g = ap.add_mutually_exclusive_group(required=True)
    g.add_argument("--check", action="store_true",
                   help="report only; exit non-zero on label-form resolutions")
    g.add_argument("--apply", action="store_true",
                   help="fix label-form resolutions in place")
    ap.add_argument("corpus", help="corpus .jsonl path (never commit it)")
    args = ap.parse_args()
    return check(args.corpus) if args.check else apply_fix(args.corpus)


if __name__ == "__main__":
    sys.exit(main())

#!/usr/bin/env python3
"""Filter a replay corpus to ONE venue family (issue #862 reconciliation prep).

The 2026-10-27 live archive records the same up/down slots on TWO venues
(polymarket `0x…` conditions + kalshi `KX…` conditions) as separate k=round
rows, and the strategy races them — a replay on the dual-family corpus may
enter either venue's token. A trade-by-trade reconciliation against a shadow
ledger can only compare prices on the SAME instrument, so the harness needs a
corpus slice restricted to the venue family the shadow account traded.

What this does (byte-preserving on everything it keeps):
  - keeps k=round rows whose markets are ALL `--venue` (each row is one
    venue's view of one slot in these corpora);
  - collects the kept rounds' condition ids AND outcome token ids;
  - keeps k=resolution rows whose condition id is kept;
  - keeps k=book rows whose token id is kept;
  - filters k=books rows PER ENTRY: one batch row snapshots several assets'
    books together (2 of this corpus's tokens + 6 foreign ones in these
    archives), so atomic dropping would lose the whole L2 stream. Entries
    whose token is kept survive (row re-serialized — the only place bytes
    change); a row with no kept entry is dropped whole and counted;
  - every other row (spot, trade, cashflow, …) passes through untouched.

Output is a sibling file (`--out`), never in place. Deterministic: a filtered
corpus is a pure subset in the original line order, so a re-run reproduces the
same sha256.

Self-test (offline, stdlib only): python3 scripts/corpus/filter_family.py --self-test
"""
import argparse
import json
import sys

TOKENED_KINDS = {"book", "books"}


def filter_corpus(src, out, venue):
    # Pass 1 collects the venue family's identity; pass 2 filters. Books
    # precede their k=round row in file order (the archive is time-ordered),
    # so a one-pass filter would drop every book of the first rounds.
    kept_tokens = set()
    kept_conds = set()
    for raw in open(src):
        try:
            o = json.loads(raw)
        except Exception:
            continue
        if o.get("k") == "round":
            markets = o.get("m") or []
            if markets and all(m.get("venue") == venue for m in markets):
                for m in markets:
                    kept_conds.add(m.get("conditionId"))
                    for key in ("upTokenId", "downTokenId"):
                        if m.get(key):
                            kept_tokens.add(m[key])
    lines = 0
    kept = 0
    dropped_rounds = 0
    dropped_books = 0
    dropped_resolutions = 0
    with open(out, "w") as w:
        for raw in open(src):
            lines += 1
            try:
                o = json.loads(raw)
            except Exception:
                w.write(raw)
                kept += 1
                continue
            k = o.get("k")
            if k == "round":
                markets = o.get("m") or []
                if markets and all(m.get("venue") == venue for m in markets):
                    for m in markets:
                        kept_conds.add(m.get("conditionId"))
                        for key in ("upTokenId", "downTokenId"):
                            if m.get(key):
                                kept_tokens.add(m[key])
                    w.write(raw)
                    kept += 1
                else:
                    dropped_rounds += 1
                continue
            if k == "resolution":
                if o.get("c") in kept_conds:
                    w.write(raw)
                    kept += 1
                else:
                    dropped_resolutions += 1
                continue
            if k == "book":
                if o.get("t") in kept_tokens:
                    w.write(raw)
                    kept += 1
                else:
                    dropped_books += 1
                continue
            if k == "books":
                batch = o.get("books") or []

                def tok_of(b):
                    # The archive's batch entry is a dict {t, b, a}; the Rust
                    # BatchBook tuple form [t, b, a] is accepted for safety.
                    return b["t"] if isinstance(b, dict) else b[0]

                kept_entries = [b for b in batch if tok_of(b) in kept_tokens]
                if kept_entries:
                    if len(kept_entries) == len(batch):
                        w.write(raw)
                    else:
                        # A batch row mixes several assets' books inside ONE
                        # row, so it is filtered per entry — the only place
                        # this filter changes bytes.
                        slim = dict(o)
                        slim["books"] = kept_entries
                        w.write(json.dumps(slim, separators=(", ", ": ")) + "\n")
                    kept += 1
                else:
                    dropped_books += 1
                continue
            w.write(raw)
            kept += 1
    print(
        f"lines={lines} kept={kept} dropped_rounds={dropped_rounds} "
        f"dropped_books={dropped_books} dropped_resolutions={dropped_resolutions} "
        f"kept_tokens={len(kept_tokens)} venue={venue}"
    )
    # A slice with no rounds at all is a mistake (wrong venue spelling), not a
    # corpus: refuse it rather than hand the replay an empty stream.
    return 1 if not kept_conds else 0


def self_test():
    import tempfile
    import os

    src = os.path.join(tempfile.gettempdir(), "bkff-selftest.jsonl")
    out = src + ".out"
    rows = [
        # A book BEFORE its round row: the archive is time-ordered, so the
        # token set must be collected in a first pass (a one-pass filter drops
        # every book of the first rounds).
        {"at": 1, "k": "book", "t": "T1", "b": [], "a": []},
        {"at": 1, "k": "round", "m": [{"venue": "polymarket", "conditionId": "0xA",
                                       "upTokenId": "T1", "downTokenId": "T2"}]},
        {"at": 1, "k": "round", "m": [{"venue": "kalshi", "conditionId": "KX1",
                                       "upTokenId": "T3", "downTokenId": "T4"}]},
        {"at": 2, "k": "book", "t": "T1", "b": [], "a": []},
        {"at": 2, "k": "book", "t": "T3", "b": [], "a": []},
        {"at": 3, "k": "books", "ts": 3, "books": [["T1", [], []], ["T2", [], []]]},
        {"at": 3, "k": "books", "ts": 3, "books": [["T1", [], []], ["T4", [], []]]},
        {"at": 4, "k": "resolution", "c": "0xA", "p": [["T1", "1"], ["T2", "0"]]},
        {"at": 4, "k": "resolution", "c": "KX1", "p": [["T3", "1"], ["T4", "0"]]},
        {"at": 5, "k": "spot", "asset": "BTC", "price": "1", "now_ms": 5},
    ]
    with open(src, "w") as f:
        for r in rows:
            f.write(json.dumps(r, separators=(",", ":")) + "\n")
    assert filter_corpus(src, out, "polymarket") == 0
    kept = [json.loads(l) for l in open(out)]
    kinds = [r["k"] for r in kept]
    assert kinds == ["book", "round", "book", "books", "books", "resolution", "spot"], kinds
    # The book written BEFORE its round row must have survived the first pass.
    assert kept[0] == {"at": 1, "k": "book", "t": "T1", "b": [], "a": []}, kept[0]
    # A batch row mixing a foreign token is filtered PER ENTRY, not dropped.
    mixed = [r for r in kept if r.get("k") == "books"][1]
    assert mixed["books"] == [["T1", [], []]], mixed
    assert all("T4" not in json.dumps(r) for r in kept), "foreign token leaked"
    first_round = next(r for r in kept if r.get("k") == "round")
    assert first_round["m"][0]["venue"] == "polymarket"
    os.remove(src)
    os.remove(out)
    print("self-test: ALL PASS")
    return 0


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--corpus", help="input corpus JSONL (read-only)")
    ap.add_argument("--out", help="filtered output JSONL (sibling file)")
    ap.add_argument("--venue", default="polymarket", help="venue family to keep")
    ap.add_argument("--self-test", action="store_true")
    args = ap.parse_args()
    if args.self_test:
        sys.exit(self_test())
    if not args.corpus or not args.out:
        ap.error("--corpus and --out are required (or use --self-test)")
    sys.exit(filter_corpus(args.corpus, args.out, args.venue))
    sys.exit(filter_corpus(args.corpus, args.out, args.venue))


if __name__ == "__main__":
    main()

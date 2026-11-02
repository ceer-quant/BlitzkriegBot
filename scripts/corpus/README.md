# corpus/

Corpus settlement schema tools. The corpus JSONL itself (tens of MB, live
recording + derived resolutions) never lives in this repo — it stays in
`bk-session/`; these tools are the repo's memory of the schema and the fix.

## The bug (#890, A4 verdict §5.1)

`build-realbook.py` emitted `k=resolution` rows with human-readable payout
LABELS instead of outcome TOKEN ids:

```json
{"k":"resolution","c":"<conditionId>","p":[["Up","1"],["Down","0"]]}   ← label form (BUG)
{"k":"resolution","c":"<conditionId>","p":[["<upTokenId>","1"],["<downTokenId>","0"]]}  ← token form (schema)
```

The kernel's settlement (`booking_for` → `MarketResolution::payout_per_share`)
matches by exact token id, so every label-form row never triggered: positions
stayed "prices no outcome — left open" and the replay closed **0 trades**
(空 sha `e3b0c442…`). Repairing 22,683 lines / 16 resolutions (ok=16/bad=0)
turned nine replay configs positive — the bug was data, not conservatism.

## Tools

- `fix_resolutions.py` — gate + repair for resolution rows:
  - `--check <corpus.jsonl>`: read-only. Prints `lines= resolutions_ok= resolutions_bad=`;
    **exit 1 when any label-form resolution exists**. Run as a gate after every
    corpus build — a non-zero exit means do not replay on that corpus.
  - `--apply <corpus.jsonl>`: fix in place (sibling temp file + atomic rename).
    Rewrites only label-form rows (`p` mapped Up/Down → the round row's
    upTokenId/downTokenId, each side keeping its own payout); token-form and all
    other lines pass through byte-for-byte, so it is idempotent. Prints the
    original repair run's counters (`resolutions_rewritten=`,
    `resolutions_without_round=`, `round_rows_with_tokens=`); exits 1 if an
    unfixable row remains (its conditionId had no round row).
- `build_realbook.py` — the single-asset corpus generator, ported from
  `bk-session/live-readiness/realbook/build-realbook.py` **with the root fix**:
  resolution rows are written with token ids from the start, and rounds without
  token ids are dropped (`no_tokens`), never emitted as unmatchable resolutions.
  (`build-realbook-multi.py` already wrote token ids and stays in bk-session.)
- `test_fix_resolutions.py` — self-test, `python3` direct, stdlib only, embedded
  fixture with fake ids:

```bash
python3 scripts/corpus/test_fix_resolutions.py   # ALL PASS
```

## How recurrence is prevented

1. Build corpora only with `scripts/corpus/build_realbook.py` (writes token ids).
2. Gate every corpus with `fix_resolutions.py --check` before replaying; wire it
   into any script that consumes a corpus. Exit 1 ⇒ label-form present ⇒ fix
   (`--apply`) or rebuild before use.
3. Corolla of #890: a corpus bug announces itself as "0 closed trades / empty
   sha", which looks like a strategy problem. `--check` turns it into a schema
   failure at the door instead.

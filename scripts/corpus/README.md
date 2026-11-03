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
- `relabel_official.py` — rebuild settlement labels from the OFFICIAL
  Polymarket resolution (issue #859). The generator above (and the A2 lane's
  `labels_15m/5m.jsonl`) derives winners from the Binance spot tape, but the
  venue settles against the **Chainlink TWAP** stream (live 2026-08-07, per
  every up/down market description); A2 measured 5.77% (15m) / 13.06% (5m)
  label flips from that gap. Three modes:
  - `--relabel labels.jsonl... --out relabeled.jsonl`: fetch the official
    winner per conditionId (CLOB `/markets/<cid>` `tokens[].winner` — the
    market_resolved state) and write each input row back UNCHANGED plus
    additive `official_win`/`official_slug`/`official_seconds_delay`/`period`
    fields; prints the diff report (official vs Binance-point `p_win` and vs
    spot-TWAP proxy `t_win`, per settlement-rule period). History is never
    rewritten.
  - `--verify relabeled.jsonl --sample N`: the acceptance gate. Deterministic
    sample, re-fetch each round independently (CLOB winner AND Gamma
    `outcomePrices`, slug taken from the CLOB payload); `agreement` = stored
    label == CLOB == Gamma. Exit 0 iff agreement >= 99.5%; below 99% prints
    STOP — halt downstream evaluation (issue #859).
  - `--apply-corpus corpus.jsonl`: rewrite `k=resolution` rows to the official
    winner in token form (mapped through the round rows' up/downTokenId);
    unfetchable rows stay byte-identical; exit 1 if any remain. Temp file +
    atomic rename, idempotent.
  Network discipline: https only, host allowlist
  (`clob.polymarket.com`, `gamma-api.polymarket.com`, `data-api.polymarket.com`),
  one URL constructor that re-validates before every dial. Known Gamma traps
  are pinned in the docstring: `/markets?slug=` returns EMPTY for archived
  rounds and `?condition_id=` is silently ignored. Self-test:
  `python3 scripts/corpus/test_relabel_official.py` (offline, fake fetcher).
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

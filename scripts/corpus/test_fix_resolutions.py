#!/usr/bin/env python3
"""Self-test for fix_resolutions.py with an embedded fixture (issue #890).

Run: python3 scripts/corpus/test_fix_resolutions.py   (stdlib only, direct)

Asserts the three acceptance teeth:
  1. --check exits non-zero on label-form resolutions, zero on token-form.
  2. --apply output is byte-for-byte equal to the expected fixed corpus
     (untouched lines raw, rewritten rows compact) with correct counters.
  3. apply is idempotent, and unfixable rows (no round row) stay byte-intact
     and make --apply exit non-zero.

All token ids here are FAKE constants (this repo is Mimosa security-scanned;
real token ids must never be committed).
"""
import json
import pathlib
import subprocess
import sys
import tempfile

TOOL = pathlib.Path(__file__).resolve().parent / "fix_resolutions.py"

FAKE_UP = "111111111111111111111111111111111111111111111111111111111111111111"
FAKE_DOWN = "222222222222222222222222222222222222222222222222222222222222222222"
FAKE_UP2 = "333333333333333333333333333333333333333333333333333333333333333333"
FAKE_DOWN2 = "444444444444444444444444444444444444444444444444444444444444444444"


def d(o):  # default json.dumps, like build-realbook.py wrote the buggy corpus
    return json.dumps(o)


def c(o):  # compact, like the fixer rewrites resolution rows
    return json.dumps(o, separators=(",", ":"))


ROUND_A = {"at": 1000, "k": "round",
           "m": [{"conditionId": "0xfakeconday", "upTokenId": FAKE_UP,
                  "downTokenId": FAKE_DOWN}]}
# winner Up, loser Down
LABEL_RES_A = {"at": 2000, "k": "resolution", "c": "0xfakeconday",
               "p": [["Up", "1"], ["Down", "0"]], "n": False}
TOKEN_RES_A = {"at": 2000, "k": "resolution", "c": "0xfakeconday",
               "p": [[FAKE_UP, "1"], [FAKE_DOWN, "0"]], "n": False}
# a book row that mentions "Up" must never be touched by the fixer
BOOK_ROW = {"at": 1500, "k": "book",
            "a": [["0.999", "5"]], "b": [["0.001", "7"]], "note": "Up"}
ROUND_B = {"at": 3000, "k": "round",
           "m": [{"conditionId": "0xfakecondbz", "upTokenId": FAKE_UP2,
                  "downTokenId": FAKE_DOWN2}]}
# winner Down, loser Up
LABEL_RES_B = {"at": 4000, "k": "resolution", "c": "0xfakecondbz",
               "p": [["Down", "1"], ["Up", "0"]], "n": False}
TOKEN_RES_B = {"at": 4000, "k": "resolution", "c": "0xfakecondbz",
               "p": [[FAKE_UP2, "0"], [FAKE_DOWN2, "1"]], "n": False}
# label-form row with NO matching round row -> unfixable, must stay byte-intact
ORPHAN_RES = {"at": 5000, "k": "resolution", "c": "0xfakecondorph",
              "p": [["Up", "1"], ["Down", "0"]], "n": False}


def write(path, rows):
    path.write_text("".join(d(o) + "\n" for o in rows))


def run(*a):
    return subprocess.run([sys.executable, str(TOOL), *a],
                          capture_output=True, text=True)


def expect(cond, msg):
    if not cond:
        sys.exit(f"FAIL: {msg}")
    print(f"ok: {msg}")


def main() -> int:
    with tempfile.TemporaryDirectory() as td:
        t = pathlib.Path(td)

        # 1. --check: label-form -> non-zero; token-form -> zero
        label = t / "label.jsonl"
        write(label, [ROUND_A, BOOK_ROW, LABEL_RES_A, ROUND_B, LABEL_RES_B])
        r = run("--check", str(label))
        expect(r.returncode != 0, "--check exits non-zero on label-form corpus")
        expect("resolutions_bad=2" in r.stdout and "resolutions_ok=0" in r.stdout,
               f"--check counts ok=0 bad=2 (got {r.stdout.strip()!r})")

        token = t / "token.jsonl"
        write(token, [ROUND_A, BOOK_ROW, TOKEN_RES_A, ROUND_B, TOKEN_RES_B])
        r = run("--check", str(token))
        expect(r.returncode == 0, "--check exits 0 on token-form corpus")
        expect("resolutions_bad=0" in r.stdout,
               f"--check counts bad=0 (got {r.stdout.strip()!r})")

        # 2. --apply: byte-equal to the expected fixed corpus + counters
        fixed = t / "fixed.jsonl"
        write(fixed, [ROUND_A, BOOK_ROW, LABEL_RES_A, ROUND_B, LABEL_RES_B])
        expected = (d(ROUND_A) + "\n" + d(BOOK_ROW) + "\n" + c(TOKEN_RES_A)
                    + "\n" + d(ROUND_B) + "\n" + c(TOKEN_RES_B) + "\n")
        r = run("--apply", str(fixed))
        expect(r.returncode == 0, "--apply exits 0 when every row is fixable")
        expect(fixed.read_text() == expected,
               "--apply output byte-equal to expected fixed corpus")
        expect("lines=5 resolutions_rewritten=2 resolutions_without_round=0"
               in r.stdout and "round_rows_with_tokens=2" in r.stdout,
               f"--apply counters correct (got {r.stdout.splitlines()!r})")

        # 2b. idempotent: second apply rewrites nothing, file unchanged
        r = run("--apply", str(fixed))
        expect(r.returncode == 0 and fixed.read_text() == expected
               and "resolutions_rewritten=0" in r.stdout,
               "--apply is idempotent (rewritten=0, bytes unchanged)")

        # 2c. the untouched book row kept its exact bytes
        expect(d(BOOK_ROW) in fixed.read_text(),
               "non-resolution rows (even ones mentioning 'Up') stay byte-intact")

        # 3. unfixable orphan row: stays as-is, --apply exits non-zero
        orphan = t / "orphan.jsonl"
        write(orphan, [ROUND_A, LABEL_RES_A, ORPHAN_RES])
        orphan_expected = (d(ROUND_A) + "\n" + c(TOKEN_RES_A) + "\n"
                           + d(ORPHAN_RES) + "\n")
        r = run("--apply", str(orphan))
        expect(r.returncode != 0, "--apply exits non-zero with an unfixable row")
        expect(orphan.read_text() == orphan_expected,
               "unfixable label row stays byte-intact")
        expect("resolutions_rewritten=1 resolutions_without_round=1" in r.stdout,
               "orphan counted as resolutions_without_round=1")
        r = run("--check", str(orphan))
        expect(r.returncode != 0, "--check still flags the surviving orphan")

    print("ALL PASS")
    return 0


if __name__ == "__main__":
    sys.exit(main())

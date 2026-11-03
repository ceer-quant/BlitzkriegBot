#!/usr/bin/env python3
"""Offline self-test for relabel_official.py (stdlib only, zero network).

Every mode runs against an injected FAKE fetcher, so the suite proves the
pipeline logic (winner extraction, token mapping, period segmentation,
threshold exit codes, idempotence) without touching polymarket. The fake
fetcher itself re-validates every URL through validate_url() — the SSRF
allowlist is exercised on the exact path the real fetch takes. Condition ids
and token ids below are FAKE constants (the repo never carries real ones).
"""
import importlib.util
import json
import os
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location(
    "relabel_official", os.path.join(HERE, "relabel_official.py"))
R = importlib.util.module_from_spec(spec)
spec.loader.exec_module(R)

FAILED = []


def check(name, cond):
    print(("PASS " if cond else "FAIL ") + name)
    if not cond:
        FAILED.append(name)


CID = "0x" + "a1" * 32
CID2 = "0x" + "b2" * 32
UP = "111" * 22
DOWN = "222" * 22
SLUG = "btc-updown-15m-1790731800"
SLUG2 = "btc-updown-15m-1780000000"

CLOB_BODY_UP = {
    "closed": True, "market_slug": SLUG, "seconds_delay": 0,
    "tokens": [{"token_id": UP, "outcome": "Up", "winner": True},
               {"token_id": DOWN, "outcome": "Down", "winner": False}],
}
CLOB_BODY_DOWN = {
    "closed": True, "market_slug": SLUG, "seconds_delay": 2,
    "tokens": [{"token_id": UP, "outcome": "Up", "winner": False},
               {"token_id": DOWN, "outcome": "Down", "winner": True}],
}
GAMMA_UP = [{"markets": [{"closed": True, "umaResolutionStatus": "resolved",
                          "outcomes": '["Up","Down"]', "outcomePrices": '["1","0"]'}]}]
GAMMA_DOWN = [{"markets": [{"closed": True, "umaResolutionStatus": "resolved",
                            "outcomes": '["Up","Down"]', "outcomePrices": '["0","1"]'}]}]


def fake_fetch_factory(clob_map=None, gamma_map=None, fail_cids=()):
    """A fetch(url) double that validates every URL before answering."""

    def fetch(url):
        R.validate_url(url)  # same gate the real fetch sits behind
        if "/markets/" in url:
            cid = url.rsplit("/", 1)[1]
            if cid in fail_cids:
                raise R.FetchError("boom")
            return (clob_map or {}).get(cid, CLOB_BODY_UP)
        if url.startswith("https://gamma-api.polymarket.com/events?"):
            slug = dict(p.split("=", 1) for p in url.split("?", 1)[1].split("&"))["slug"]
            return (gamma_map or {}).get(slug, GAMMA_UP)
        raise AssertionError("unexpected url " + url)

    return fetch


class Args:
    max_rounds = 0
    workers = 2
    d0807 = "2026-08-07"
    d0814 = "2026-08-14"
    report = ""
    sample = 500
    seed = 1
    threshold = 0.995
    out_evidence = ""


def write_tmp(lines):
    fd, path = tempfile.mkstemp(suffix=".jsonl")
    with os.fdopen(fd, "w") as fh:
        fh.write("\n".join(lines) + "\n")
    return path


def main():
    # 1. SSRF allowlist: the only URL constructor refuses everything else.
    check("build_url https allowlisted host ok",
          R.build_url("clob.polymarket.com", "/markets/x").startswith("https://clob.polymarket.com/markets/x"))
    for host, path in (("evil.example.com", "/x"),
                       ("clob.polymarket.com.evil.com", "/x"),
                       ("data-api.polymarket.com", "http-nope")):
        try:
            R.build_url(host, path)
            check(f"build_url rejects {host}{path}", False)
        except ValueError:
            check(f"build_url rejects {host}{path}", True)
    try:
        R.validate_url("http://clob.polymarket.com/markets/x")
        check("validate_url rejects http scheme", False)
    except ValueError:
        check("validate_url rejects http scheme", True)
    try:
        R.validate_url("https://clob.polymarket.com.evil.com/markets/x")
        check("validate_url rejects lookalike host", False)
    except ValueError:
        check("validate_url rejects lookalike host", True)

    # 2. winner extraction
    check("clob_truth single winner -> Up", R.clob_truth(CLOB_BODY_UP)["winner"] == "Up")
    check("clob_truth slug+delay carried",
          R.clob_truth(CLOB_BODY_DOWN)["slug"] == SLUG and R.clob_truth(CLOB_BODY_DOWN)["seconds_delay"] == 2)
    check("clob_truth two winners -> None",
          R.clob_truth({"tokens": [{"outcome": "Up", "winner": True},
                                   {"outcome": "Down", "winner": True}]})["winner"] is None)
    check("gamma_truth ['1','0'] -> Up",
          R.gamma_truth({"outcomes": '["Up","Down"]', "outcomePrices": '["1","0"]'}) == "Up")
    check("gamma_truth ['0','1'] -> Down",
          R.gamma_truth({"outcomes": '["Up","Down"]', "outcomePrices": '["0","1"]'}) == "Down")
    check("gamma_truth unresolved -> None",
          R.gamma_truth({"outcomes": '["Up","Down"]', "outcomePrices": '["0.5","0.5"]'}) is None)

    # 3. period segmentation boundaries (UTC midnights)
    d0807, d0814 = R.iso_utc_ms("2026-08-07"), R.iso_utc_ms("2026-08-14")
    check("period pre-0807", R.period_of(d0807 - 1, d0807, d0814) == "pre-0807")
    check("period 0807-0814", R.period_of(d0807, d0807, d0814) == "0807-0814")
    check("period post-0814", R.period_of(d0814, d0807, d0814) == "post-0814")
    check("period unknown when open missing", R.period_of(None, d0807, d0814) == "unknown")

    # 4. --relabel: additive fields, history (p_win/t_win) untouched, stats right
    rows = [
        {"fam": "15m", "cond": CID, "asset": "BTC", "open": 1790731800000,
         "p_win": "Up", "t_win": "Down", "ok": True},          # official Down: p flips, t agrees
        {"fam": "15m", "cond": "0x" + "b2" * 32, "asset": "BTC", "open": 1780000000000,
         "p_win": "Up", "t_win": "Up", "ok": True},            # pre-0807 bucket, official Up
        {"fam": "15m", "cond": "0x" + "c3" * 32, "asset": "ETH", "open": 1790731800000,
         "p_win": "Down", "t_win": "Down", "ok": False},       # fetch fails -> unresolved
    ]
    src = write_tmp(json.dumps(r) for r in rows)
    out = os.path.join(tempfile.mkdtemp(), "relabeled.jsonl")
    args = Args()
    rc = R.relabel([src], out, args, fetch=fake_fetch_factory(
        clob_map={CID: CLOB_BODY_DOWN,
                  CID2: {"closed": True, "market_slug": SLUG2, "seconds_delay": 0,
                         "tokens": [{"token_id": UP, "outcome": "Up", "winner": True},
                                    {"token_id": DOWN, "outcome": "Down", "winner": False}]}},
        fail_cids={"0x" + "c3" * 32}))
    got = [json.loads(l) for l in open(out)]
    check("relabel exit 0 with minority errors", rc == 0)
    check("relabel keeps row count", len(got) == 3)
    check("relabel preserves p_win/t_win (history untouched)",
          got[0]["p_win"] == "Up" and got[0]["t_win"] == "Down")
    check("relabel adds official winner", got[0]["official_win"] == "Down")
    check("relabel agree flags: p flipped, t agrees",
          got[0]["agree_p"] is False and got[0]["agree_t"] is True)
    check("relabel period post-0814", got[0]["period"] == "post-0814")
    check("relabel period pre-0807 bucket", got[1]["period"] == "pre-0807")
    check("relabel failed cid -> unresolved, official_error set",
          got[2]["official_win"] is None and got[2]["official_error"] == "boom")

    # 5. --verify: agreement + threshold exit codes (fetch injected)
    rv = R.verify(out, args, fetch=fake_fetch_factory(
        clob_map={CID: CLOB_BODY_DOWN,
                  CID2: {"closed": True, "market_slug": SLUG2, "seconds_delay": 0,
                         "tokens": [{"token_id": UP, "outcome": "Up", "winner": True},
                                    {"token_id": DOWN, "outcome": "Down", "winner": False}]}},
        gamma_map={SLUG: GAMMA_DOWN, SLUG2: GAMMA_UP}))
    check("verify PASS (all sources agree) exit 0", rv == 0)
    # one sampled row's gamma disagrees with clob -> below 99.5% threshold
    rv2 = R.verify(out, args, fetch=fake_fetch_factory(
        clob_map={CID: CLOB_BODY_DOWN,
                  CID2: {"closed": True, "market_slug": SLUG2, "seconds_delay": 0,
                         "tokens": [{"token_id": UP, "outcome": "Up", "winner": True},
                                    {"token_id": DOWN, "outcome": "Down", "winner": False}]}},
        gamma_map={SLUG: GAMMA_UP, SLUG2: GAMMA_UP}))
    check("verify FAIL (gamma disagrees) exit 1", rv2 == 1)

    # 5b. --summarize: offline report recomputation from the relabeled file
    report = os.path.join(tempfile.mkdtemp(), "report.txt")
    args3 = Args()
    args3.report = report
    rc5 = R.summarize(out, args3)
    rep = open(report).read()
    check("summarize exit 0", rc5 == 0)
    check("summarize report carries period lines and denominators",
          "period post-0814" in rep and "agree_p=0/1" in rep)

    # 6. --apply-corpus: official winner lands in token-form resolution rows
    corpus = [
        json.dumps({"at": 1, "k": "round",
                    "m": [{"conditionId": CID, "roundSlot": 7, "asset": "BTC",
                           "upTokenId": UP, "downTokenId": DOWN}]}),
        json.dumps({"at": 2, "k": "resolution", "c": CID,
                    "p": [["Up", "1"], ["Down", "0"]], "n": False}),
        json.dumps({"at": 3, "k": "book", "b": []}),
        json.dumps({"at": 4, "k": "resolution", "c": "0x" + "ff" * 32,
                    "p": [["Up", "1"], ["Down", "0"]], "n": False}),  # no tokens -> unresolved
    ]
    cpath = write_tmp(corpus)
    args2 = Args()
    rc3 = R.apply_corpus(cpath, args2, fetch=fake_fetch_factory(
        clob_map={CID: CLOB_BODY_DOWN}))
    body = open(cpath).read()
    got_rows = [json.loads(l) for l in body.splitlines()]
    check("apply exits 1 when a resolution stays unresolved", rc3 == 1)
    check("apply rewrites resolvable row to official winner (Down)",
          got_rows[1]["p"] == [[UP, "0"], [DOWN, "1"]])
    check("apply leaves non-resolution lines byte-identical",
          got_rows[2] == {"at": 3, "k": "book", "b": []})
    # idempotence: second run with the same truth changes nothing
    rc4 = R.apply_corpus(cpath, args2, fetch=fake_fetch_factory(
        clob_map={CID: CLOB_BODY_DOWN, "0x" + "ff" * 32: CLOB_BODY_UP}))
    check("apply idempotent (same bytes on rerun)", open(cpath).read() == body)
    check("apply rerun still exits 1 on the orphan row", rc4 == 1)

    print()
    if FAILED:
        print(f"{len(FAILED)} FAILED: {FAILED}")
        return 1
    print("ALL PASS")
    return 0


if __name__ == "__main__":
    sys.exit(main())

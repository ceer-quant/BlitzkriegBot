# user_layer/strategies — the strategy drop-point

**The kernel ships no trading strategy, and this directory ships one measurement
fixture.** The five strategies that used to live here (`dog`, `trend_follow`,
`mean_reversion`, `pair_arb`, and `spread_arb` as a *traded* strategy) were
deleted; see `CHANGELOG.md` for that entry and the coverage that went with them.

`spread_arb/` was restored on its own, as a **fixture, not a product**: the
economic gate `scripts/exit-economics-check.mjs` (#272) puts a money number on
exit reachability by replaying the sha256-pinned frozen corpus through a real
strategy, and a strategy has to come from a cdylib. Nothing enables it by
default — it registers DISABLED like any other loaded library — and no shipped
configuration names it.

The directory is kept as a directory — rather than removed — because three
things point at it by name:

- the kernel's default `--strategy-dir` (`core/blitzkrieg_core/src/service.rs`);
- `loader::APPROVED_ROOT`, the primary entry in the dylib trust allowlist
  (`core/blitzkrieg_core/src/strategy_engine/loader.rs`) — a strategy built
  outside an approved root is refused at load time;
- the `DYLIB_DIRS` staging tree in `scripts/upgrade.sh`.

So a strategy you write yourself can be dropped in here and found by the kernel
with no flag, exactly as before.

## Writing one

The kernel is a strategy host: it ships zero strategy implementations and
reaches every strategy through C ABI v2 via `dlopen`. To add one:

1. Write a crate against `user_layer/strategy_api` (the C ABI), `crate-type =
   ["cdylib"]`, with its own `Cargo.lock` — mirroring how an external author
   builds. `user_layer/parity_strategy/` is the reference implementation: it is
   the fixture `core/blitzkrieg_core/tests/foreign_parity.rs` loads, and it is
   deliberately tiny.
2. Build it into a `target/release/` tree under an approved root.
3. Load it with `--enable-strategy <name>`, or let startup auto-load it.

`docs/rust-core/STRATEGY_GUIDE.md` is the full walkthrough; the ABI itself is
specified in `docs/rust-core/ABI_V2_DESIGN.md`.

## The nested workspace

`Cargo.toml` here is a nested workspace with `spread_arb` as its only member,
excluded from the root workspace so it carries its own `Cargo.lock` like a real
external author's checkout. Add your crate as a member, or give it its own
`[workspace]` — either is fine, as long as the cdylib lands under an approved
root.

Build the fixture with:

    (cd user_layer/strategies && cargo build --release --locked)

`scripts/lib/strategy-dylib-freshness.mjs` reads this manifest's `members` to
tell a **stale** cdylib from an **orphan** one, so a gate that drives a cdylib
fails loudly when the library is older than the sources it was built from.

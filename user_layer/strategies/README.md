# user_layer/strategies — the strategy drop-point

**This directory ships no strategies.** Every strategy that used to live here
(`dog`, `spread_arb`, `trend_follow`, `mean_reversion`, `pair_arb`) has been
deleted; see `CHANGELOG.md` for the entry and the coverage that went with them.

It is kept as a directory — rather than removed — because three things point at
it by name:

- the kernel's default `--strategy-dir` (`core/blitzkrieg_core/src/service.rs`);
- `loader::APPROVED_ROOT`, the primary entry in the dylib trust allowlist
  (`core/blitzkrieg_core/src/strategy_engine/loader.rs`) — a strategy built
  outside an approved root is refused at load time;
- the `DYLIB_DIRS` staging tree in `scripts/upgrade.sh`.

So a strategy you write yourself can be dropped in here and found by the kernel
with no flag, exactly as before.

## Writing one

The kernel is a strategy host: it ships zero strategy implementations and
reaches every strategy through C ABI v2 via `dlopen`. Nothing about that changed.
To add one:

1. Write a crate against `user_layer/strategy_api` (the C ABI), `crate-type =
   ["cdylib"]`, with its own `Cargo.lock` — mirroring how an external author
   builds. `user_layer/parity_strategy/` is the reference implementation: it is
   the fixture `core/blitzkrieg_core/tests/foreign_parity.rs` loads, and it is
   deliberately tiny.
2. Build it into a `target/release/` tree under an approved root.
3. Load it with `--enable-strategy <name>`, or let startup auto-load it.

`docs/rust-core/STRATEGY_GUIDE.md` is the full walkthrough; the ABI itself is
specified in `docs/rust-core/ABI_V2_DESIGN.md`.

## Why this directory has no `Cargo.toml`

It used to be a nested workspace (`[workspace]` with the five strategies as
members, excluded from the root workspace so it carried its own `Cargo.lock`).
With no members left the workspace file was deleted with them; a new strategy
crate declares its own `[workspace]` and `Cargo.lock`, as the guide describes.

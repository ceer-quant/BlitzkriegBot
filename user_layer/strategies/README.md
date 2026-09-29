# user_layer/strategies — the strategy drop-point

**The kernel ships no trading strategy, and this directory ships no strategy at
all.** The five strategies that used to live here (`dog`, `trend_follow`,
`mean_reversion`, `pair_arb`, and `spread_arb` as a *traded* strategy) were
deleted; see `CHANGELOG.md` for that entry and the coverage that went with them.
`spread_arb` was later restored as a measurement fixture for the economic gate,
and on 2026-09-29 that fixture moved to Lua too — **the repo's official
`spread_arb` is now the Lua package
[`user_layer/strategies_lua/spread_arb/`](../strategies_lua/spread_arb/)**, an
exact port of the Rust implementation proven bit-identical on the economic
gate's frozen corpus (BASELINE rows verbatim) before the cdylib was removed.

The directory is kept as a directory — rather than removed — because two things
point at it by name:

- the kernel's default `--strategy-dir` (`core/blitzkrieg_core/src/main.rs`,
  `default_strategy_dir()`) — every library found here is loaded at startup;
- `loader::APPROVED_ROOT`, the primary entry in the dylib trust allowlist
  (`core/blitzkrieg_core/src/strategy_engine/loader.rs`) — a strategy built
  outside an approved root is refused at load time.

So a cdylib strategy YOU write can be dropped in here and the kernel finds it
with no flag. **Being found is not being switched on.** Loading happens to every
library in the directory; enabling does not — startup enables only the persisted
set (`data/strategy-state.json`) plus whatever `--enable-strategy` names, and the
directory is never consulted for that (`install_engine`, `service.rs`).

⚠️ **Stale-build note (the 0.3.0 upgrade lesson):** a `*.dylib` left in
`target/release/` here keeps loading even after its crate is gone from the tree
— the kernel scans this directory for libraries, and a stale build can collide
with a same-named strategy from another stack (e.g. the Lua `spread_arb`). If a
crate is removed, remove its build output too:
`rm -rf user_layer/strategies/target`.

`scripts/upgrade.sh` deliberately does **not** stage this tree: `DYLIB_DIRS` is
`user_layer/parity_strategy/target/release`, and the script carries a comment
saying why — this tree ships no strategies, so there is nothing to stage. A
release carries the drop-point as a directory, not as contents.

## Writing a strategy

Two stacks, one adjudicated kernel:

- **Lua (what the repo ships its own strategies in)** — a package directory
  under `user_layer/strategies_lua/` (`manifest.json` with a mandatory sha256 +
  entry script + README). `spread_arb/` and `lua_momentum/` are the reference
  packages. See `docs/rust-core/STRATEGY_GUIDE.md` §3.7.
- **Rust cdylib (bring your own)** — write a crate against
  `user_layer/strategy_api` (C ABI v2), `crate-type = ["cdylib"]`, with its own
  `Cargo.lock` — mirroring how an external author builds.
  `user_layer/parity_strategy/` is the reference implementation: it is the
  fixture `core/blitzkrieg_core/tests/foreign_parity.rs` loads, and it is
  deliberately tiny. Build it into a `target/release/` tree under an approved
  root here or elsewhere; enable with `--enable-strategy <name>` or the panel.
  The ABI itself is specified in `docs/rust-core/ABI_V2_DESIGN.md`.

Either way: loading is not enabling — the startup scan registers, the persisted
set (or `--enable-strategy`) switches on.

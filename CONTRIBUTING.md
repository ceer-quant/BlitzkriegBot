# Contributing to BlitzkriegBot

Thanks for your interest in contributing! (`ceer-quant/BlitzkriegBot`, MIT).

## Architecture first

This is a **Rust-only** project: all money, order, risk and state-consistency
logic lives in Rust. Node.js appears only in zero-dependency gate scripts
(bare Node stdlib) and is not part of the production runtime.

| Directory | What it is |
| --- | --- |
| `core/blitzkrieg_core/` | Market-agnostic trading engine (OME, orders, positions, risk, ledger, reconciliation) |
| `core/market_api/` | Extension contract crate: `DataFeed` / `Discovery` / `Executor` / `MarketPlugin` traits + DTOs |
| `extensions/polymarket/` | Official Polymarket plugin (CLOB, WS feed, Gamma discovery) |
| `user_layer/strategy_api/` | User strategy trait / FFI stable surface (cdylib hot-loading) |
| `ui/` | Rust UI kit: `ui_kit_web` (panel server, port 51888), `ui_kit_panel` (terminal), Vue webapp |
| `scripts/lib/` | Zero-dependency bare-Node IPC client used by the local acceptance gates |

Key architecture constraints — do not violate them:

1. **Zero market code in the core** — `core/blitzkrieg_core` must not depend on
   any exchange SDK; venues are wired through `core/market_api` contracts.
2. **Extensions must not depend on the core** — otherwise Cargo loops. They
   depend only on `core/market_api`.
3. **Risk hard limits are not strategy-exemptible**, and the strategy ABI
   vtable is frozen (`BK_ABI_VERSION = 2`; new capabilities go through
   optional symbols only).

## Development

```bash
# Rust core
cargo build --release --workspace --locked
cargo test --workspace --locked

# DryRun order-chain end-to-end (spawns an isolated temporary core)
node scripts/cycle-check.mjs

# Core behavior / ledger parity gates (spawns dry+live twin cores)
node scripts/core-parity.mjs
node scripts/account-parity.mjs

# Zero-dependency secret scan
bash scripts/secret-scan.sh
```

The panel frontend (`ui/webapp/webui/`) has its own checks:

```bash
cd ui/webapp/webui && npm run check:all
```

## Adding a Market Extension

1. Create a new crate under `extensions/your-venue/`.
2. Depend on `core/market_api` (never on `blitzkrieg-core` itself).
3. Implement `DataFeed`, `MarketDiscovery`, `OrderExecutor`, and pack them
   into a `MarketPlugin`.
4. Register via a Cargo feature in the root workspace. See
   [docs/rust-core/EXTENSION_GUIDE.md](docs/rust-core/EXTENSION_GUIDE.md).
   The core does not change.

## Writing a Strategy

Built-in strategies live in `core/blitzkrieg_core/src/strategies/`.
External strategies are cdylibs built against `user_layer/strategy_api`
and loaded at runtime. See
[docs/rust-core/STRATEGY_GUIDE.md](docs/rust-core/STRATEGY_GUIDE.md) and
[docs/rust-core/ABI_V2_DESIGN.md](docs/rust-core/ABI_V2_DESIGN.md).

## Pull Request Guidelines

1. Run the full gate set above before submitting.
2. Keep changes focused and atomic.
3. Describe what changed and why; attach gate output where relevant.
4. Live-trading changes are **not** accepted from outside contributors.

## Code Style

- Rust: `cargo fmt` / `clippy` clean; match the surrounding idiom.
- Gate scripts (`scripts/*.mjs`): bare Node stdlib only — no npm dependencies.

## Security Guidelines

1. Never commit credentials, private keys or API keys — secrets go through
   environment variables (see `.env.example`).
2. Report vulnerabilities via GitHub Security Advisories, not public issues.
3. Run `bash scripts/secret-scan.sh` before submitting.

> Note: no third-party security audit has been performed on this project.

## Reporting Issues

Open an issue at https://github.com/ceer-quant/BlitzkriegBot/issues with
Rust version, steps to reproduce, expected vs actual behavior, and error
messages/logs.

## License

By contributing, you agree that your contributions will be licensed under MIT.

# Extensions

Two kinds of plugin live in the kernel, and they are **not** the same thing:

| | Market plugin (`MarketPlugin`) | General extension (`Extension`) |
|:---|:---|:---|
| Purpose | Integrate a **market** (orders + market data + discovery) | Audit / event hook only |
| Contract | `DataFeed` / `MarketDiscovery` / `OrderExecutor` | `on_load` / `on_unload` / `on_event` |
| Capability | Pushes data + takes orders via `MarketHost` | No trading ability (emit/log only) |
| Crate | `extensions/<name>/` (own crate, rlib+cdylib) | compiled into the kernel |
| Example | `extensions/polymarket/` | `extension/builtins.rs::BinanceSpotExtension` |

A market is linked in by a Cargo feature and selected at runtime with
`--market-plugin <name>`. It must depend on `blitzkrieg-market-api` only — **never
on `blitzkrieg-core`** (that would form a dependency cycle).

> Note: `config.toml` files under `extensions/` are **documentation only** — the
> kernel does not parse them. Assembly is by Cargo feature; selection is by CLI.

See `docs/blitzkrieg/EXTENSION_GUIDE.md` for the full contract (it lives under
`docs/blitzkrieg/`, not `docs/`).

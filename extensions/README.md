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

> `config.toml` under `extensions/<name>/` **is read** (KI-11). The kernel parses
> `[meta]` at startup and checks it against the linked extension — a name/version/
> type drift is reported. `[market]`, `[risk]` and `[dependencies]` are recognised
> but have no adapter: they are reported as declared-but-inert rather than acted on
> (market wiring is by Cargo feature + `--market-plugin`, risk limits come from
> `RiskConfig`, dependencies resolve at build time). Assembly is still by Cargo
> feature; selection is still by CLI. There is no hot reload.

See `docs/rust-core/EXTENSION_GUIDE.md` for the full contract.

# Security Policy

## Supported Versions

| Version | Supported          |
| ------- | ------------------ |
| 0.2.x   | :white_check_mark: |

## Reporting a Vulnerability

**Please do NOT report security vulnerabilities through public GitHub issues.**

Instead, please report them via GitHub Security Advisories:

1. Go to the [Security tab](https://github.com/ceer-quant/BlitzkriegBot/security/advisories)
2. Click "Report a vulnerability"
3. Fill out the form with details

### What to Include

- Type of issue (e.g., command injection, credential exposure, order-state corruption)
- Location of the affected source code (tag/branch/commit or direct URL)
- Step-by-step instructions to reproduce the issue
- Proof-of-concept or exploit code (if possible)
- Impact assessment, especially for any path that could touch real funds

### Response Timeline

- **Initial response:** Within 48 hours
- **Status update:** Within 7 days
- **Fix timeline:** Depends on severity
  - Critical: 24-72 hours
  - High: 1-2 weeks
  - Medium: 2-4 weeks
  - Low: Next release

## Security Best Practices for Users

### Credential Safety

1. **Never commit credentials** — secrets go through environment variables
   (see `.env.example` for the expected shape)
2. **Keep `.env` in `.gitignore`** — it holds live keys and wallet material
   and must never leave your machine
3. **Rotate API keys** regularly and use separate wallets for trading bots
4. **Never enable live trading unless you fully understand the consequences** —
   the default run mode is `dry`

### Deployment

1. **Keep Rust dependencies updated** — review `Cargo.lock` changes in PRs,
   run `cargo audit` where available
2. **Use HTTPS** — never expose the panel HTTP server (port 51888) beyond
   localhost or an authenticated tunnel
3. **Run the secret scan** — `bash scripts/secret-scan.sh` (zero-dependency)
4. **Review logs** — monitor for unexpected order or position activity

### Trading Safety

1. **Start with dry-run mode** — validate the full order chain before any
   live deployment
2. **Set loss limits** — the engine enforces hard risk limits that strategies
   cannot override
3. **Use separate wallets** — dedicated funds only, never your primary wallet
4. **Monitor positions** — set up alerts for large or unexpected trades

## Known Security Considerations

### Process Architecture

- The trading engine (`blitzkrieg-core`) and the panel server (`ui_kit_web`)
  communicate over a Unix-domain socket with newline-framed JSON-RPC 2.0.
  The panel has **no** direct market or wallet access; all order flow goes
  through the engine.
- Market extensions (`extensions/*`) are compiled plugins depending only on
  the `core/market_api` contract crate — they are not hot-swappable without
  a rebuild.
- External strategies are cdylibs loaded via a **frozen ABI**
  (`BK_ABI_VERSION = 2`). Risk hard limits are enforced in the engine and
  cannot be exempted by any strategy.

### No Third-Party Audit

No third-party security audit has been performed on this project. Use it at
your own risk, especially with live funds.

## Further Reading

Security-relevant design decisions are documented in
[docs/rust-core/ARCHITECTURE.md](./docs/rust-core/ARCHITECTURE.md)
(process boundaries, risk enforcement).

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
   and must never leave your machine. The repository ignores `.env`, `.env.local`
   and `.env.*` while keeping the `.env.example` template tracked; verify with
   `git check-ignore -v .env`.
3. **`.env` must be owner-only: `chmod 600 .env`** — a fresh copy inherits the
   shell's umask (`-rw-r--r--`, i.e. 0644), which lets every other account on
   the machine read the wallet key, the API key and the panel password.
   `ls -l .env` must print `-rw-------`. Check the backups, LaunchAgent plists,
   editor swap files and shell history for copies too.
4. **Rotate anything that may have been echoed** — if a secret ever reached a
   log, a terminal recording or an issue, treat it as leaked. Rotation is a
   *user* action: this repository cannot invalidate a venue-side key for you.
5. **Inspections print key names, never values** — when scripting around `.env`,
   report presence and shape only (`grep -c '^KEY=' .env`, `grep -o '^[A-Z_]*='`),
   never the value. The same rule applies to audit tooling, bug reports and
   pasted logs. `scripts/unified-launcher-check.mjs` already asserts the launcher
   never echoes a `.env` credential.
6. **Never enable live trading unless you fully understand the consequences** —
   the default run mode is `dry`

### Deployment

1. **Keep Rust dependencies updated** — review `Cargo.lock` changes in PRs,
   run `cargo audit` where available
2. **Keep the panel on loopback** — it binds `127.0.0.1:51888` by default, so an
   unconfigured panel is never reachable from another host. `BLITZKRIEG_PANEL_ADDR`
   (or `--addr`) overrides that, and a non-loopback address serves the panel over
   plain HTTP to everyone who can reach the port; the kernel prints a red warning
   at startup and the panel shows the same warning in its own page. If you need
   remote access, put an authenticated TLS proxy in front of it rather than
   exposing the port.
3. **Treat the panel password as a network credential** once the bind is not
   loopback — `BLITZKRIEG_PANEL_USER` / `BLITZKRIEG_PANEL_PASSWORD` arm the login
   gate. After 10 failed attempts from one client address the panel refuses that
   address for 30 s, doubling up to a 15 min ceiling, and the refusal looks
   exactly like a wrong password (`429` + `Retry-After`), so it never reveals
   whether a password was correct.
4. **Run the secret scan** — `bash scripts/secret-scan.sh` (zero-dependency)
5. **Review logs** — monitor for unexpected order or position activity

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

### IPC Socket

- The socket (`$TMPDIR/blitzkrieg-core-$USER.sock` by default) is chmodded to
  **0600** immediately after `bind` (`SOCKET_MODE` in
  `core/blitzkrieg_core/src/ipc/server.rs`). `bind` alone leaves
  `0777 & !umask`, which is world-connectable, so the chmod is not optional.
- Every accepted connection is classified by **peer credentials**: Linux
  `SO_PEERCRED`, macOS `LOCAL_PEERCRED`. The kernel's own uid is accepted; a
  different uid is refused *before a single byte of JSON is read*, and the
  refusal is logged with both uids.
- Where a platform cannot report peer credentials, the session is accepted with
  a one-time warning and the 0600 socket mode is the only control — a graceful
  degradation, so an unsupported platform still runs instead of refusing every
  connection. The handshake reports `peerVerified`, `peerUid`, `peerAuth` and
  `socketMode`, so a client can tell which case it is in.
- The kernel refuses to unlink a socket file it does not own, so a planted path
  cannot redirect the bind.

### Panel HTTP Surface

- Default bind is `127.0.0.1:51888`; `--addr` beats `BLITZKRIEG_PANEL_ADDR`,
  and the built-in default is loopback. When the address actually bound is not
  loopback, the process prints a multi-line warning at startup and the panel
  renders a red banner; the JSON snapshot carries a `security` block
  (`loopbackOnly`, `bind`, `warning`, `login`) so tooling can assert it.
- Login throttling is keyed by **client address**, never by username: a
  username-keyed lock would let any stranger lock the operator out. State is
  bounded (oldest entry evicted) so an attacker cannot grow it without limit.
- All panel traffic is plain HTTP. Sessions are bearer tokens, so a non-loopback
  bind without a TLS proxy exposes both the token and the password on the wire.

### Strategy Libraries (cdylibs)

- Loading is `dlopen` in the **same process** as the kernel. There is no
  sandbox, no seccomp filter and no separate address space: an approved library
  runs with the kernel's full authority, including order flow. The controls
  below decide *where a library may come from and who approved it*; they do not
  limit what an approved library can do.
- **Approved roots**: `<repo>/user_layer/strategies` and
  `<repo>/user_layer/parity_strategy` (the C ABI v2 reference implementation CI
  builds), plus anything listed in `BLITZKRIEG_STRATEGY_ALLOW_DIRS`
  (`:`-separated).
- **Never approved**: shared scratch space and download folders — `/tmp`,
  `/var/tmp`, `$TMPDIR`, `/Users/Shared`, `~/Downloads`, `~/Desktop`. A library
  there is refused unless it also sits inside an approved root.
- **World-writable libraries are refused** outright: any local user could
  rewrite one between approval and load.
- **Approval manifest**: one `sha256 <path>` line per entry, default
  `user_layer/strategies/approved.manifest`, overridable with
  `BLITZKRIEG_STRATEGY_MANIFEST`. A library outside every approved root, or one
  under a machine-generated tree (`data/`, `shadow_evolution/`, any `shadow*` /
  `evolution*` component), loads only when the manifest lists it **and** the
  on-disk digest matches. A digest mismatch is reported as "re-review the
  library and update the manifest" rather than loading it.
- **Shadow Evolution output is machine-generated by definition**, so it needs a
  human to add it to the manifest: evolution can propose a candidate, only an
  operator can approve it for loading. See
  [docs/rust-core/SHADOW_EVOLUTION.md](./docs/rust-core/SHADOW_EVOLUTION.md).
- **Residual risk**: an attacker who can write into an approved directory, or
  edit the manifest, is inside the trust boundary — the next load executes their
  code with kernel authority. Keep approved directories owner-writable only and
  review manifest changes like code.

### Supervisor Restart Budget

- A supervised core that crashes is restarted with backoff, up to
  `max_attempts` (5 by default, `0` = unlimited). When the budget is spent the
  supervisor **stops trying** — the kernel stays down — and says so loudly
  instead of going quiet:
  - a banner on stderr naming the attempt count, the last exit report and the
    tail of the core's own stderr;
  - a `giveUp` object in the panel snapshot (`attempts`, `atMs`, `message`,
    `stderrTail`) plus a red banner in the panel page;
  - an opt-in desktop notification when `BLITZKRIEG_ALERT_NOTIFY=1` is set.
- The alert is raised once per give-up and cleared automatically when a core
  comes back, so it always describes the current state rather than the last
  incident.

### Verifying a Deployment

```sh
lsof -nP -iTCP:51888 -sTCP:LISTEN     # must show 127.0.0.1:51888, never *:51888
ls -l .env                            # must show -rw-------  (chmod 600 .env)
stat -f '%Lp' "$SOCKET"               # macOS: 600
stat -c '%a'  "$SOCKET"               # Linux: 600
sudo -u nobody nc -U "$SOCKET"        # foreign uid: connection closed, refusal logged
```

A strategy library under `/tmp` is refused — `cargo test -p blitzkrieg-core
--lib strategy_engine::loader` covers the temp-directory rule, the
manifest-digest rule and the world-writable rule.

### No Third-Party Audit

No third-party security audit has been performed on this project. Use it at
your own risk, especially with live funds.

## Further Reading

Security-relevant design decisions are documented in
[docs/rust-core/ARCHITECTURE.md](./docs/rust-core/ARCHITECTURE.md)
(process boundaries, risk enforcement).

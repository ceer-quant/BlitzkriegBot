# Assets

Repository-level visual assets.

## `logo.png`

The source brand mark, retained from the pre-takeover tree (recorded in
`DECISIONS_PENDING.md` D-24 as a non-code file kept for provenance/audit — it is
not derived from any deleted Blitzkrieg source).

**Not the file the panel serves.** The running UI Kit web panel bundles its logo
from `ui/webapp/webui/src/assets/logo.png` (Vite hashes it into
`dist/assets/logo-<hash>.png`; `scripts/webapp-check.mjs` asserts the served
bytes match). If you change the panel's brand mark, change that file.

The previous version of this README described `demo.gif` plus Telegram / WebChat
/ arbitrage / portfolio screenshots. Those belonged to the retired Node product
(the gateway on port 18789, the messaging channels) and were never committed, so
the directories they named do not exist. Removed rather than left as a checklist
for a product that is gone.

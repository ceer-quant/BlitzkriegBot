/**
 * Process bootstrap for the legacy→canonical brand rename (E1-c).
 *
 * Imported for its side effects as the FIRST import of every entrypoint
 * (`src/index.ts`, `src/cli/index.ts`, `src/bin/worker.ts`). ES module
 * initialisers run in source order, so placing this first guarantees that by
 * the time any other module reads the environment:
 *
 *   1. `.env` files from the state directory have been loaded.
 *   2. Every legacy `CLODDS_*` variable has been mirrored onto its canonical
 *      `BLITZKRIEG_*` name (canonical wins; a deprecation line is emitted).
 *
 * The state directory is computed here without depending on `brand-paths`,
 * because that resolver itself reads branded variables — this module is the
 * layer that makes those names consistent first.
 */

import { config as dotenvConfig } from 'dotenv';
import { existsSync } from 'fs';
import { homedir } from 'os';
import { join } from 'path';
import { adoptLegacyEnv } from './env';

function envTrim(name: string): string | undefined {
  const v = process.env[name];
  return v !== undefined && v.trim() !== '' ? v.trim() : undefined;
}

/** Candidate state directories, highest precedence first, that actually exist. */
function stateEnvCandidates(): string[] {
  const out: string[] = [];
  const explicit = envTrim('BLITZKRIEG_STATE_DIR') ?? envTrim('CLODDS_STATE_DIR');
  if (explicit) {
    out.push(explicit);
  } else {
    const home = homedir();
    if (existsSync(join(home, '.blitzkrieg'))) out.push(join(home, '.blitzkrieg'));
    if (existsSync(join(home, '.clodds'))) out.push(join(home, '.clodds'));
  }
  return out;
}

let bootstrapped = false;

/** Load `.env` candidates and mirror legacy variable names. Idempotent. */
export function bootstrapBrandEnv(): void {
  if (bootstrapped) return;
  bootstrapped = true;

  for (const dir of stateEnvCandidates()) {
    dotenvConfig({ path: join(dir, '.env') });
  }
  dotenvConfig(); // CWD fallback; dotenv never overrides variables already set.

  adoptLegacyEnv();
}

bootstrapBrandEnv();

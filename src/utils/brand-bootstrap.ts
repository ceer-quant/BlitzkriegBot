/**
 * Process bootstrap for the canonical brand.
 *
 * Imported for its side effects as the FIRST import of every entrypoint
 * (`src/index.ts`, `src/cli/index.ts`, `src/bin/worker.ts`). ES module
 * initialisers run in source order, so placing this first guarantees that by
 * the time any other module reads the environment, `.env` files from the state
 * directory have been loaded.
 *
 * The state directory is computed here without depending on `brand-paths`,
 * because that resolver itself reads branded variables.
 */

import { config as dotenvConfig } from 'dotenv';
import { homedir } from 'os';
import { join } from 'path';

function envTrim(name: string): string | undefined {
  const v = process.env[name];
  return v !== undefined && v.trim() !== '' ? v.trim() : undefined;
}

let bootstrapped = false;

/** Load `.env` candidates. Idempotent. */
export function bootstrapBrandEnv(): void {
  if (bootstrapped) return;
  bootstrapped = true;

  const explicit = envTrim('BLITZKRIEG_STATE_DIR');
  if (explicit) {
    dotenvConfig({ path: join(explicit, '.env') });
  } else {
    dotenvConfig({ path: join(homedir(), '.blitzkrieg', '.env') });
  }
  dotenvConfig(); // CWD fallback; dotenv never overrides variables already set.
}

bootstrapBrandEnv();

/**
 * Branded environment variables — canonical `BLITZKRIEG_*` only.
 *
 * The legacy `CLODDS_*` names are retired: no aliasing, no mirroring, no
 * deprecation warnings. Every environment variable the gateway reads carries
 * the canonical prefix.
 */

/** Canonical prefix for every branded environment variable. */
export const CANONICAL_ENV_PREFIX = 'BLITZKRIEG';

/** The canonical name for one variable suffix (e.g. `STATE_DIR`). */
export function brandEnvNames(suffix: string): { canonical: string } {
  return {
    canonical: `${CANONICAL_ENV_PREFIX}_${suffix}`,
  };
}

type Warner = (message: string) => void;

const defaultWarn: Warner = (message) => {
  // Deliberately not the structured logger: this module is imported from the
  // logger's own startup path, so it must stay cycle-free.
  console.warn(`[config] ${message}`);
};

let warn: Warner = defaultWarn;

const warnedOnce = new Set<string>();

/** Replace the deprecation sink (tests capture it; hosts may route it to logs). */
export function setLegacyEnvWarner(next: Warner | null): void {
  warn = next ?? defaultWarn;
  warnedOnce.clear();
}

/** Emit a line once per key, so hot read paths cannot flood logs. */
export function warnLegacyOnce(key: string, message: string): void {
  if (warnedOnce.has(key)) return;
  warnedOnce.add(key);
  warn(message);
}

/**
 * Read a canonical branded variable. Returns `undefined` when it is unset
 * (an empty value is returned as-is so callers can distinguish "unset" from
 * "deliberately empty").
 */
export function readBrandEnv(
  suffix: string,
  env: NodeJS.ProcessEnv = process.env,
): string | undefined {
  return env[`${CANONICAL_ENV_PREFIX}_${suffix}`];
}

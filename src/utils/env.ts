/**
 * Branded environment variables — canonical `BLITZKRIEG_*`, with the legacy
 * `CLODDS_*` names accepted for one release (E1-c).
 *
 * Two mechanisms, deliberately:
 *
 * - [`adoptLegacyEnv`] mirrors a set legacy name onto its canonical counterpart
 *   once at process start. Every read site in the codebase can therefore use the
 *   canonical name only, while an existing deployment that still exports the old
 *   names keeps working unchanged.
 * - [`readBrandEnv`] resolves a single variable on demand, for call sites that
 *   want the precedence rule inline (config path resolution, for example).
 *
 * In both, the canonical name wins when both are present, and nothing here ever
 * moves or rewrites user data.
 */

/** Canonical prefix for every branded environment variable. */
export const CANONICAL_ENV_PREFIX = 'BLITZKRIEG';
/** Legacy prefix, accepted for one release. */
export const LEGACY_ENV_PREFIX = 'CLODDS';

/** The canonical and legacy names for one variable suffix (e.g. `STATE_DIR`). */
export function brandEnvNames(suffix: string): { canonical: string; legacy: string } {
  return {
    canonical: `${CANONICAL_ENV_PREFIX}_${suffix}`,
    legacy: `${LEGACY_ENV_PREFIX}_${suffix}`,
  };
}

/** True when `key` carries the legacy brand prefix. */
export function isLegacyEnvName(key: string): boolean {
  return key.startsWith(`${LEGACY_ENV_PREFIX}_`);
}

/** Canonical name for a key; a non-branded key is returned unchanged. */
export function canonicalEnvName(key: string): string {
  return isLegacyEnvName(key)
    ? `${CANONICAL_ENV_PREFIX}_${key.slice(LEGACY_ENV_PREFIX.length + 1)}`
    : key;
}

/** Legacy name for a key; a key that is not canonically branded is unchanged. */
export function legacyEnvName(key: string): string {
  return key.startsWith(`${CANONICAL_ENV_PREFIX}_`)
    ? `${LEGACY_ENV_PREFIX}_${key.slice(CANONICAL_ENV_PREFIX.length + 1)}`
    : key;
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

/** Emit a deprecation line once per key, so hot read paths cannot flood logs. */
export function warnLegacyOnce(key: string, message: string): void {
  if (warnedOnce.has(key)) return;
  warnedOnce.add(key);
  warn(message);
}

/**
 * Mirror legacy `CLODDS_*` values onto their canonical `BLITZKRIEG_*` names.
 *
 * Called once at process start. The canonical name always wins: a mirror only
 * happens when the canonical variable is unset or empty. Returns the suffixes
 * that were adopted (already sorted), which is what the single deprecation line
 * reports and what tests assert on.
 */
export function adoptLegacyEnv(env: NodeJS.ProcessEnv = process.env): string[] {
  const adopted: string[] = [];

  for (const key of Object.keys(env)) {
    if (!isLegacyEnvName(key)) continue;
    const canonical = canonicalEnvName(key);
    const legacyValue = env[key];
    if (legacyValue === undefined || legacyValue.trim() === '') continue;

    const canonicalValue = env[canonical];
    if (canonicalValue !== undefined && canonicalValue.trim() !== '') continue;

    env[canonical] = legacyValue;
    adopted.push(key.slice(LEGACY_ENV_PREFIX.length + 1));
  }

  if (adopted.length > 0) {
    adopted.sort();
    warnLegacyOnce(
      'adopt',
      `deprecated environment variable names are in use: ${adopted.map((s) => `${LEGACY_ENV_PREFIX}_${s}`).join(', ')}. ` +
        `Rename them to ${CANONICAL_ENV_PREFIX}_* — the old names stop working in the next release.`,
    );
  }

  return adopted;
}

/**
 * Read a branded variable: canonical first, legacy as a deprecation alias.
 * Returns `undefined` when neither is set (an empty canonical value is returned
 * as-is so callers can distinguish "unset" from "deliberately empty").
 */
export function readBrandEnv(suffix: string, env: NodeJS.ProcessEnv = process.env): string | undefined {
  const { canonical, legacy } = brandEnvNames(suffix);

  const preferred = env[canonical];
  if (preferred !== undefined && preferred.trim() !== '') return preferred;

  const fallback = env[legacy];
  if (fallback !== undefined && fallback.trim() !== '') {
    warnLegacyOnce(
      legacy,
      `${legacy} is deprecated; use ${canonical} instead (the old name stops working in the next release)`,
    );
    return fallback;
  }

  return preferred;
}

/** True when the variable came from the legacy name only (for diagnostics). */
export function isLegacyEnvSuffix(suffix: string, env: NodeJS.ProcessEnv = process.env): boolean {
  const { canonical, legacy } = brandEnvNames(suffix);
  const preferred = env[canonical];
  if (preferred !== undefined && preferred.trim() !== '') return false;
  return env[legacy] !== undefined && (env[legacy] as string).trim() !== '';
}

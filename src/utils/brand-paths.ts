/**
 * Branded filesystem paths — canonical `~/.blitzkrieg` with the legacy
 * `~/.clodds` layout kept working (E1-c).
 *
 * The rename must never move or discard user data. So the rule is
 * "canonical unless only the legacy location exists":
 *
 * - Both state directories absent        → use the canonical one (fresh install).
 * - Canonical present                    → use the canonical one.
 * - Only the legacy directory present    → keep using it, warn once.
 *
 * The same "act on what exists" rule picks the config file name inside a state
 * directory, so an existing `clodds.json` is not orphaned by a new default.
 * Moving data is an explicit operator action (`BLITZKRIEG_STATE_DIR`), never a
 * silent side effect of upgrading.
 */

import { existsSync } from 'fs';
import { homedir } from 'os';
import { join, resolve } from 'path';
import { readBrandEnv, warnLegacyOnce } from './env';

/** Canonical state directory name under `$HOME`. */
export const CANONICAL_STATE_DIR_NAME = '.blitzkrieg';
/** Legacy state directory name, honoured while it is the one on disk. */
export const LEGACY_STATE_DIR_NAME = '.clodds';
/** Canonical config file name inside the state directory. */
export const CANONICAL_CONFIG_FILE = 'blitzkrieg.json';
/** Legacy config file name, honoured while it is the one on disk. */
export const LEGACY_CONFIG_FILE = 'clodds.json';
/** Canonical workspace directory name under `$HOME`. */
export const CANONICAL_WORKSPACE_DIR_NAME = 'blitzkrieg';
/** Legacy workspace directory name, honoured while it is the one on disk. */
export const LEGACY_WORKSPACE_DIR_NAME = 'clodds';

/** Resolve `~` against the home directory. */
function resolveUserPath(input: string): string {
  const trimmed = input.trim();
  if (!trimmed) return trimmed;
  if (trimmed.startsWith('~')) {
    return resolve(trimmed.replace(/^~(?=$|[\\/])/, homedir()));
  }
  return resolve(trimmed);
}

/**
 * Pick between a canonical and a legacy directory under `$HOME`.
 * Canonical wins unless it is absent and the legacy one exists.
 */
function preferExistingHomeDir(
  canonicalName: string,
  legacyName: string,
  legacyKey: string,
  home = homedir(),
): string {
  const canonical = join(home, canonicalName);
  if (existsSync(canonical)) return canonical;

  const legacy = join(home, legacyName);
  if (existsSync(legacy)) {
    warnLegacyOnce(
      legacyKey,
      `using the existing ${legacy} directory; the canonical location is now ${canonical}. ` +
        `Nothing was moved — set BLITZKRIEG_STATE_DIR to relocate it deliberately.`,
    );
    return legacy;
  }

  return canonical;
}

/** State directory for mutable data. */
export function resolveStateDir(env: NodeJS.ProcessEnv = process.env, home = homedir()): string {
  const override = readBrandEnv('STATE_DIR', env)?.trim();
  if (override) return resolveUserPath(override);
  return preferExistingHomeDir(CANONICAL_STATE_DIR_NAME, LEGACY_STATE_DIR_NAME, 'state-dir', home);
}

/** Config file path inside the state directory (or an explicit override). */
export function resolveConfigPath(env: NodeJS.ProcessEnv = process.env, home = homedir()): string {
  const override = readBrandEnv('CONFIG_PATH', env)?.trim();
  if (override) return resolveUserPath(override);

  const stateDir = resolveStateDir(env, home);
  const canonical = join(stateDir, CANONICAL_CONFIG_FILE);
  if (existsSync(canonical)) return canonical;

  const legacy = join(stateDir, LEGACY_CONFIG_FILE);
  if (existsSync(legacy)) {
    warnLegacyOnce(
      'config-file',
      `using the existing ${legacy} config file; new installs use ${CANONICAL_CONFIG_FILE}.`,
    );
    return legacy;
  }

  return canonical;
}

/** Credentials directory. */
export function resolveCredentialsDir(env: NodeJS.ProcessEnv = process.env, home = homedir()): string {
  return join(resolveStateDir(env, home), 'credentials');
}

/** Logs directory. */
export function resolveLogsDir(env: NodeJS.ProcessEnv = process.env, home = homedir()): string {
  return join(resolveStateDir(env, home), 'logs');
}

/** Workspace directory for agent file output. */
export function resolveWorkspaceDir(env: NodeJS.ProcessEnv = process.env, home = homedir()): string {
  const override = readBrandEnv('WORKSPACE', env)?.trim();
  if (override) return resolveUserPath(override);
  return preferExistingHomeDir(
    CANONICAL_WORKSPACE_DIR_NAME,
    LEGACY_WORKSPACE_DIR_NAME,
    'workspace-dir',
    home,
  );
}

/**
 * A path inside the state directory: `statePath('plugins')` →
 * `<state dir>/plugins`.
 *
 * Every extension that persists under `$HOME` should build its path this way
 * instead of joining `~/.clodds` directly, so a relocated state directory
 * relocates all of it at once and no component keeps writing to the old tree.
 */
export function statePath(...segments: string[]): string {
  return join(resolveStateDir(), ...segments);
}

/** Build a path inside an explicit state directory (testable seam). */
export function statePathFrom(stateDir: string, ...segments: string[]): string {
  return join(stateDir, ...segments);
}

/** Canonical SQLite database file name inside the state directory. */
export const CANONICAL_DB_FILE = 'blitzkrieg.db';
/** Legacy database file name, honoured while it is the one on disk. */
export const LEGACY_DB_FILE = 'clodds.db';

/**
 * Database path inside the state directory.
 *
 * A database is user data, so an existing `clodds.db` keeps being used rather
 * than abandoned next to a fresh empty file.
 */
export function resolveDbPath(env: NodeJS.ProcessEnv = process.env): string {
  const dir = resolveStateDir(env);

  const canonical = join(dir, CANONICAL_DB_FILE);
  if (existsSync(canonical)) return canonical;

  const legacy = join(dir, LEGACY_DB_FILE);
  if (existsSync(legacy)) {
    warnLegacyOnce(
      'db-file',
      `using the existing ${legacy} database; new installs use ${CANONICAL_DB_FILE}. Nothing was moved.`,
    );
    return legacy;
  }

  return canonical;
}

/** Canonical per-project workspace config file name. */
export const CANONICAL_WORKSPACE_CONFIG_FILE = '.blitzkrieg.json';
/** Legacy per-project workspace config file name. */
export const LEGACY_WORKSPACE_CONFIG_FILE = '.clodds.json';

/** Recognised workspace config file names, canonical first. */
export const WORKSPACE_CONFIG_FILES = [
  CANONICAL_WORKSPACE_CONFIG_FILE,
  LEGACY_WORKSPACE_CONFIG_FILE,
] as const;

/** The workspace config file present in `dir`, preferring the canonical name. */
export function resolveWorkspaceConfigFile(dir: string): string {
  const canonical = join(dir, CANONICAL_WORKSPACE_CONFIG_FILE);
  if (existsSync(canonical)) return canonical;

  const legacy = join(dir, LEGACY_WORKSPACE_CONFIG_FILE);
  if (existsSync(legacy)) {
    warnLegacyOnce(
      'workspace-config',
      `reading the existing ${legacy}; new files are written as ${CANONICAL_WORKSPACE_CONFIG_FILE}.`,
    );
    return legacy;
  }

  return canonical;
}

/** Canonical user-level service definition file name. */
export const CANONICAL_SERVICE_FILE = 'blitzkrieg.service';
/** Legacy user-level service definition file name. */
export const LEGACY_SERVICE_FILE = 'clodds.service';

/** Canonical launchd label / service identifier. */
export const CANONICAL_SERVICE_NAME = 'com.blitzkrieg.gateway';
/** Legacy launchd label, still removed during uninstall. */
export const LEGACY_SERVICE_NAME = 'com.clodds.gateway';

/** Directory under `$HOME/.config` for XDG-style config (MCP descriptor, …). */
export const CANONICAL_XDG_CONFIG_DIR_NAME = 'blitzkrieg';
/** Legacy XDG config directory name. */
export const LEGACY_XDG_CONFIG_DIR_NAME = 'clodds';

/**
 * A config-managed file under `$HOME/.config`. Same "canonical unless only the
 * legacy location exists" rule as the state directory, so an existing MCP
 * descriptor keeps being read.
 */
export function resolveUserConfigPath(...segments: string[]): string {
  return resolveUserConfigPathFor(homedir(), ...segments);
}

/** Testable variant of {@link resolveUserConfigPath} with an injected home. */
export function resolveUserConfigPathFor(home: string, ...segments: string[]): string {
  const canonical = join(home, '.config', CANONICAL_XDG_CONFIG_DIR_NAME, ...segments);
  if (existsSync(canonical)) return canonical;

  const legacy = join(home, '.config', LEGACY_XDG_CONFIG_DIR_NAME, ...segments);
  if (existsSync(legacy)) {
    const key = `xdg-${segments.join('/')}`;
    warnLegacyOnce(
      key,
      `using the existing ${legacy}; new files live under ~/.config/${CANONICAL_XDG_CONFIG_DIR_NAME}. Nothing was moved.`,
    );
    return legacy;
  }

  return canonical;
}

/** The XDG config directory itself (canonical unless only the legacy one exists). */
export function resolveUserConfigDir(home: string = homedir()): string {
  const canonical = join(home, '.config', CANONICAL_XDG_CONFIG_DIR_NAME);
  if (existsSync(canonical)) return canonical;

  const legacy = join(home, '.config', LEGACY_XDG_CONFIG_DIR_NAME);
  if (existsSync(legacy)) {
    warnLegacyOnce(
      'xdg-dir',
      `using the existing ${legacy} config directory; the canonical location is ${canonical}. Nothing was moved.`,
    );
    return legacy;
  }

  return canonical;
}

/**
 * Project-local managed-skills directories to scan, canonical first.
 *
 * These live under the current working directory (one project per checkout),
 * not the user state directory. A pre-rename `.clodds/skills` keeps being read;
 * new installs only create `.blitzkrieg/skills`.
 */
export function projectManagedSkillsDirs(cwd: string = process.cwd()): string[] {
  const canonical = join(cwd, CANONICAL_STATE_DIR_NAME, 'skills');
  const legacy = join(cwd, LEGACY_STATE_DIR_NAME, 'skills');
  // Scan the legacy directory too while it physically exists, so skills
  // installed before the rename are not silently dropped.
  return existsSync(legacy) ? [canonical, legacy] : [canonical];
}

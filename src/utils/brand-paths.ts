/**
 * Branded filesystem paths — canonical `~/.blitzkrieg` only.
 *
 * The CloddsBot era is over: legacy `~/.clodds` locations are no longer
 * consulted anywhere. Every mutable file lives under the canonical names, and
 * relocating user data is an explicit operator action
 * (`BLITZKRIEG_STATE_DIR`), never a side effect.
 */

import { homedir } from 'os';
import { join, resolve } from 'path';
import { readBrandEnv } from './env';

/** Canonical state directory name under `$HOME`. */
export const CANONICAL_STATE_DIR_NAME = '.blitzkrieg';
/** Canonical config file name inside the state directory. */
export const CANONICAL_CONFIG_FILE = 'blitzkrieg.json';
/** Canonical workspace directory name under `$HOME`. */
export const CANONICAL_WORKSPACE_DIR_NAME = 'blitzkrieg';

/** Resolve `~` against the home directory. */
function resolveUserPath(input: string): string {
  const trimmed = input.trim();
  if (!trimmed) return trimmed;
  if (trimmed.startsWith('~')) {
    return resolve(trimmed.replace(/^~(?=$|[\\/])/, homedir()));
  }
  return resolve(trimmed);
}

/** State directory for mutable data. */
export function resolveStateDir(env: NodeJS.ProcessEnv = process.env, home = homedir()): string {
  const override = readBrandEnv('STATE_DIR', env)?.trim();
  if (override) return resolveUserPath(override);
  return join(home, CANONICAL_STATE_DIR_NAME);
}

/** Config file path inside the state directory (or an explicit override). */
export function resolveConfigPath(env: NodeJS.ProcessEnv = process.env, home = homedir()): string {
  const override = readBrandEnv('CONFIG_PATH', env)?.trim();
  if (override) return resolveUserPath(override);
  return join(resolveStateDir(env, home), CANONICAL_CONFIG_FILE);
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
  return join(home, CANONICAL_WORKSPACE_DIR_NAME);
}

/**
 * A path inside the state directory: `statePath('plugins')` →
 * `<state dir>/plugins`.
 *
 * Every extension that persists under `$HOME` should build its path this way
 * instead of joining `~/.blitzkrieg` directly, so a relocated state directory
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

/** Database path inside the state directory. */
export function resolveDbPath(env: NodeJS.ProcessEnv = process.env): string {
  return join(resolveStateDir(env), CANONICAL_DB_FILE);
}

/** Canonical per-project workspace config file name. */
export const CANONICAL_WORKSPACE_CONFIG_FILE = '.blitzkrieg.json';

/** Recognised workspace config file names. */
export const WORKSPACE_CONFIG_FILES = [CANONICAL_WORKSPACE_CONFIG_FILE] as const;

/** The workspace config file in `dir`. */
export function resolveWorkspaceConfigFile(dir: string): string {
  return join(dir, CANONICAL_WORKSPACE_CONFIG_FILE);
}

/** Canonical user-level service definition file name. */
export const CANONICAL_SERVICE_FILE = 'blitzkrieg.service';

/** Canonical launchd label / service identifier. */
export const CANONICAL_SERVICE_NAME = 'com.blitzkrieg.gateway';

/** Directory under `$HOME/.config` for XDG-style config (MCP descriptor, …). */
export const CANONICAL_XDG_CONFIG_DIR_NAME = 'blitzkrieg';

/** A config-managed file under `$HOME/.config`. */
export function resolveUserConfigPath(...segments: string[]): string {
  return join(homedir(), '.config', CANONICAL_XDG_CONFIG_DIR_NAME, ...segments);
}

/** Testable variant of {@link resolveUserConfigPath} with an injected home. */
export function resolveUserConfigPathFor(home: string, ...segments: string[]): string {
  return join(home, '.config', CANONICAL_XDG_CONFIG_DIR_NAME, ...segments);
}

/** The XDG config directory itself. */
export function resolveUserConfigDir(home: string = homedir()): string {
  return join(home, '.config', CANONICAL_XDG_CONFIG_DIR_NAME);
}

/**
 * Project-local managed-skills directory to scan.
 *
 * These live under the current working directory (one project per checkout),
 * not the user state directory.
 */
export function projectManagedSkillsDirs(cwd: string = process.cwd()): string[] {
  return [join(cwd, CANONICAL_STATE_DIR_NAME, 'skills')];
}

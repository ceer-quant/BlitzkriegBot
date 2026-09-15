import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, mkdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import {
  CANONICAL_CONFIG_FILE,
  CANONICAL_WORKSPACE_CONFIG_FILE,
  resolveConfigPath,
  resolveDbPath,
  resolveStateDir,
  resolveUserConfigDir,
  resolveUserConfigPathFor,
  resolveWorkspaceConfigFile,
  resolveWorkspaceDir,
  statePathFrom,
  projectManagedSkillsDirs,
  WORKSPACE_CONFIG_FILES,
} from '../../src/utils/brand-paths';

function tempHome(): string {
  return mkdtempSync(join(tmpdir(), 'bk-home-'));
}

test('resolveStateDir uses the canonical directory', () => {
  const home = tempHome();
  mkdirSync(join(home, '.clodds'));
  assert.equal(resolveStateDir({} as NodeJS.ProcessEnv, home), join(home, '.blitzkrieg'));
});

test('resolveStateDir honours BLITZKRIEG_STATE_DIR', () => {
  const home = tempHome();
  const env = { BLITZKRIEG_STATE_DIR: '/tmp/custom-state' } as NodeJS.ProcessEnv;
  assert.equal(resolveStateDir(env, home), '/tmp/custom-state');
});

test('resolveStateDir ignores the legacy CLODDS_STATE_DIR alias', () => {
  const home = tempHome();
  const env = { CLODDS_STATE_DIR: '/tmp/legacy-state' } as NodeJS.ProcessEnv;
  assert.equal(resolveStateDir(env, home), join(home, '.blitzkrieg'));
});

test('resolveConfigPath returns the canonical config file name', () => {
  const home = tempHome();
  assert.equal(resolveConfigPath({} as NodeJS.ProcessEnv, home), join(home, '.blitzkrieg', CANONICAL_CONFIG_FILE));
});

test('resolveDbPath returns the canonical blitzkrieg.db, never clodds.db', () => {
  const dir = mkdtempSync(join(tmpdir(), 'bk-db-'));
  const env = { BLITZKRIEG_STATE_DIR: dir } as NodeJS.ProcessEnv;
  assert.equal(resolveDbPath(env), join(dir, 'blitzkrieg.db'));
});

test('resolveWorkspaceDir defaults to the canonical workspace', () => {
  const legacyHome = tempHome();
  mkdirSync(join(legacyHome, 'clodds'));
  assert.equal(resolveWorkspaceDir({} as NodeJS.ProcessEnv, legacyHome), join(legacyHome, 'blitzkrieg'));
});

test('statePath builds inside the resolved state directory', () => {
  const home = tempHome();
  const state = resolveStateDir({} as NodeJS.ProcessEnv, home);
  assert.equal(statePathFrom(state, 'plugins', 'x.json'), join(home, '.blitzkrieg', 'plugins', 'x.json'));
});

test('XDG user config uses the canonical directory only', () => {
  const home = tempHome();
  mkdirSync(join(home, '.config', 'clodds'), { recursive: true });
  assert.equal(resolveUserConfigPathFor(home, 'mcp.json'), join(home, '.config', 'blitzkrieg', 'mcp.json'));
  assert.equal(resolveUserConfigDir(home), join(home, '.config', 'blitzkrieg'));
});

test('workspace config files are canonical-only', () => {
  assert.deepEqual(WORKSPACE_CONFIG_FILES, [CANONICAL_WORKSPACE_CONFIG_FILE]);
  const cwd = mkdtempSync(join(tmpdir(), 'bk-proj-'));
  assert.equal(resolveWorkspaceConfigFile(cwd), join(cwd, '.blitzkrieg.json'));
  assert.deepEqual(projectManagedSkillsDirs(cwd), [join(cwd, '.blitzkrieg', 'skills')]);
});

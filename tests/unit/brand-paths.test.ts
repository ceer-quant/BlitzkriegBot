import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, mkdirSync, writeFileSync, existsSync } from 'node:fs';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import {
  CANONICAL_CONFIG_FILE,
  LEGACY_CONFIG_FILE,
  resolveConfigPath,
  resolveDbPath,
  resolveStateDir,
  resolveUserConfigDir,
  resolveUserConfigPathFor,
  resolveWorkspaceConfigFile,
  resolveWorkspaceDir,
  statePathFrom,
  projectManagedSkillsDirs,
} from '../../src/utils/brand-paths';

function tempHome(): string {
  return mkdtempSync(join(tmpdir(), 'bk-home-'));
}

test('resolveStateDir uses the canonical directory on a fresh machine', () => {
  const home = tempHome();
  assert.equal(resolveStateDir({} as NodeJS.ProcessEnv, home), join(home, '.blitzkrieg'));
});

test('resolveStateDir keeps using an existing legacy directory (no silent migration)', () => {
  const home = tempHome();
  mkdirSync(join(home, '.clodds'));
  assert.equal(resolveStateDir({} as NodeJS.ProcessEnv, home), join(home, '.clodds'));
});

test('resolveStateDir prefers the canonical directory when both exist', () => {
  const home = tempHome();
  mkdirSync(join(home, '.blodds'));
  mkdirSync(join(home, '.blitzkrieg'));
  assert.equal(resolveStateDir({} as NodeJS.ProcessEnv, home), join(home, '.blitzkrieg'));
});

test('resolveStateDir honours BLITZKRIEG_STATE_DIR', () => {
  const home = tempHome();
  const env = { BLITZKRIEG_STATE_DIR: '/tmp/custom-state' } as NodeJS.ProcessEnv;
  assert.equal(resolveStateDir(env, home), '/tmp/custom-state');
});

test('resolveStateDir honours the legacy CLODDS_STATE_DIR alias', () => {
  const home = tempHome();
  mkdirSync(join(home, '.blitzkrieg'));
  const env = { CLODDS_STATE_DIR: '/tmp/legacy-state' } as NodeJS.ProcessEnv;
  assert.equal(resolveStateDir(env, home), '/tmp/legacy-state');
});

test('resolveStateDir lets the canonical env name win over the legacy one', () => {
  const home = tempHome();
  const env = {
    BLITZKRIEG_STATE_DIR: '/tmp/new-state',
    CLODDS_STATE_DIR: '/tmp/old-state',
  } as NodeJS.ProcessEnv;
  assert.equal(resolveStateDir(env, home), '/tmp/new-state');
});

test('resolveConfigPath picks the canonical config file when present', () => {
  const home = tempHome();
  const state = join(home, '.blitzkrieg');
  mkdirSync(state, { recursive: true });
  writeFileSync(join(state, CANONICAL_CONFIG_FILE), '{}');
  writeFileSync(join(state, LEGACY_CONFIG_FILE), '{}');
  assert.equal(resolveConfigPath({} as NodeJS.ProcessEnv, home), join(state, CANONICAL_CONFIG_FILE));
});

test('resolveConfigPath keeps reading an existing legacy clodds.json', () => {
  const home = tempHome();
  const state = join(home, '.blitzkrieg');
  mkdirSync(state, { recursive: true });
  writeFileSync(join(state, LEGACY_CONFIG_FILE), '{}');
  assert.equal(resolveConfigPath({} as NodeJS.ProcessEnv, home), join(state, LEGACY_CONFIG_FILE));
});

test('resolveConfigPath returns the canonical name on a fresh machine', () => {
  const home = tempHome();
  assert.equal(resolveConfigPath({} as NodeJS.ProcessEnv, home), join(home, '.blitzkrieg', CANONICAL_CONFIG_FILE));
});

test('resolveDbPath reuses an existing legacy clodds.db rather than orphaning it', () => {
  const dir = mkdtempSync(join(tmpdir(), 'bk-db-'));
  writeFileSync(join(dir, 'clodds.db'), '');
  const env = { BLITZKRIEG_STATE_DIR: dir } as NodeJS.ProcessEnv;
  assert.equal(resolveDbPath(env), join(dir, 'clodds.db'));
});

test('resolveDbPath prefers blitzkrieg.db when both database files exist', () => {
  const dir = mkdtempSync(join(tmpdir(), 'bk-db2-'));
  writeFileSync(join(dir, 'clodds.db'), '');
  writeFileSync(join(dir, 'blitzkrieg.db'), '');
  const env = { BLITZKRIEG_STATE_DIR: dir } as NodeJS.ProcessEnv;
  assert.equal(resolveDbPath(env), join(dir, 'blitzkrieg.db'));
});

test('resolveDbPath returns the canonical name for a fresh install', () => {
  const dir = mkdtempSync(join(tmpdir(), 'bk-db3-'));
  const env = { BLITZKRIEG_STATE_DIR: dir } as NodeJS.ProcessEnv;
  assert.equal(resolveDbPath(env), join(dir, 'blitzkrieg.db'));
});

test('resolveWorkspaceDir keeps an existing legacy workspace, defaults canonical otherwise', () => {
  const legacyHome = tempHome();
  mkdirSync(join(legacyHome, 'clodds'));
  assert.equal(resolveWorkspaceDir({} as NodeJS.ProcessEnv, legacyHome), join(legacyHome, 'clodds'));

  const freshHome = tempHome();
  assert.equal(resolveWorkspaceDir({} as NodeJS.ProcessEnv, freshHome), join(freshHome, 'blitzkrieg'));
});

test('statePath builds inside the resolved state directory', () => {
  const home = tempHome();
  const state = resolveStateDir({} as NodeJS.ProcessEnv, home);
  assert.equal(statePathFrom(state, 'plugins', 'x.json'), join(home, '.blitzkrieg', 'plugins', 'x.json'));
});

test('XDG user config reuses an existing legacy ~/.config/clodds directory', () => {
  const home = tempHome();
  mkdirSync(join(home, '.config', 'clodds'), { recursive: true });
  writeFileSync(join(home, '.config', 'clodds', 'mcp.json'), '{}');
  assert.equal(resolveUserConfigPathFor(home, 'mcp.json'), join(home, '.config', 'clodds', 'mcp.json'));
  assert.equal(resolveUserConfigDir(home), join(home, '.config', 'clodds'));
});

test('XDG user config uses the canonical directory on a fresh machine', () => {
  const home = tempHome();
  assert.equal(resolveUserConfigPathFor(home, 'mcp.json'), join(home, '.config', 'blitzkrieg', 'mcp.json'));
  assert.equal(resolveUserConfigDir(home), join(home, '.config', 'blitzkrieg'));
});

test('projectManagedSkillsDirs scans the legacy project dir while it exists', () => {
  const cwd = mkdtempSync(join(tmpdir(), 'bk-proj-'));
  assert.deepEqual(projectManagedSkillsDirs(cwd), [join(cwd, '.blitzkrieg', 'skills')]);
  mkdirSync(join(cwd, '.clodds', 'skills'), { recursive: true });
  const dirs = projectManagedSkillsDirs(cwd);
  assert.ok(dirs.includes(join(cwd, '.blitzkrieg', 'skills')));
  assert.ok(dirs.includes(join(cwd, '.clodds', 'skills')));
});

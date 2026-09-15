import { test } from 'node:test';
import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import { resolveConfigPath, resolveStateDir, resolveWorkspaceDir } from '../../src/utils/config';

test('resolveStateDir uses the BLITZKRIEG_STATE_DIR override', () => {
  const env = { BLITZKRIEG_STATE_DIR: '/tmp/bk-state' } as NodeJS.ProcessEnv;
  assert.equal(resolveStateDir(env), resolve('/tmp/bk-state'));
});

test('resolveStateDir ignores the deprecated CLODDS_STATE_DIR alias', () => {
  const env = { CLODDS_STATE_DIR: '/tmp/clodds-state' } as NodeJS.ProcessEnv;
  assert.notEqual(resolveStateDir(env), resolve('/tmp/clodds-state'));
});

test('resolveConfigPath uses the BLITZKRIEG_CONFIG_PATH override', () => {
  const env = { BLITZKRIEG_CONFIG_PATH: '/tmp/blitzkrieg.json' } as NodeJS.ProcessEnv;
  assert.equal(resolveConfigPath(env), resolve('/tmp/blitzkrieg.json'));
});

test('resolveConfigPath ignores the deprecated CLODDS_CONFIG_PATH alias', () => {
  const env = { CLODDS_CONFIG_PATH: '/tmp/clodds.json' } as NodeJS.ProcessEnv;
  assert.notEqual(resolveConfigPath(env), resolve('/tmp/clodds.json'));
});

test('resolveWorkspaceDir uses the BLITZKRIEG_WORKSPACE override', () => {
  const env = { BLITZKRIEG_WORKSPACE: '/tmp/bk-workspace' } as NodeJS.ProcessEnv;
  assert.equal(resolveWorkspaceDir(env), resolve('/tmp/bk-workspace'));
});

test('resolveWorkspaceDir ignores the deprecated CLODDS_WORKSPACE alias', () => {
  const env = { CLODDS_WORKSPACE: '/tmp/clodds-workspace' } as NodeJS.ProcessEnv;
  assert.notEqual(resolveWorkspaceDir(env), resolve('/tmp/clodds-workspace'));
});

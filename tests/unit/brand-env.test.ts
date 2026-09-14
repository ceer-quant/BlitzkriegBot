import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  adoptLegacyEnv,
  brandEnvNames,
  canonicalEnvName,
  isLegacyEnvName,
  readBrandEnv,
  setLegacyEnvWarner,
} from '../../src/utils/env';

test('brandEnvNames maps a suffix to canonical and legacy names', () => {
  assert.deepEqual(brandEnvNames('STATE_DIR'), {
    canonical: 'BLITZKRIEG_STATE_DIR',
    legacy: 'CLODDS_STATE_DIR',
  });
});

test('name helpers only rewrite branded keys', () => {
  assert.equal(isLegacyEnvName('CLODDS_MODEL'), true);
  assert.equal(isLegacyEnvName('BLITZKRIEG_MODEL'), false);
  assert.equal(canonicalEnvName('CLODDS_MODEL'), 'BLITZKRIEG_MODEL');
  assert.equal(canonicalEnvName('PORT'), 'PORT');
});

test('readBrandEnv returns the canonical value when both are set', () => {
  const env = { BLITZKRIEG_MODEL: 'new', CLODDS_MODEL: 'old' } as NodeJS.ProcessEnv;
  assert.equal(readBrandEnv('MODEL', env), 'new');
});

test('readBrandEnv falls back to the legacy value when canonical is unset', () => {
  const env = { CLODDS_MODEL: 'old' } as NodeJS.ProcessEnv;
  assert.equal(readBrandEnv('MODEL', env), 'old');
});

test('readBrandEnv treats a blank canonical value as unset', () => {
  const env = { BLITZKRIEG_MODEL: '   ', CLODDS_MODEL: 'old' } as NodeJS.ProcessEnv;
  assert.equal(readBrandEnv('MODEL', env), 'old');
});

test('readBrandEnv returns undefined when neither name is set', () => {
  assert.equal(readBrandEnv('MODEL', {} as NodeJS.ProcessEnv), undefined);
});

test('readBrandEnv warns through the captured sink on a legacy read', () => {
  const messages: string[] = [];
  setLegacyEnvWarner((m) => messages.push(m));
  try {
    readBrandEnv('MODEL', { CLODDS_MODEL: 'old' } as NodeJS.ProcessEnv);
    readBrandEnv('MODEL', { CLODDS_MODEL: 'old' } as NodeJS.ProcessEnv);
  } finally {
    setLegacyEnvWarner(null);
  }
  assert.equal(messages.length, 1, 'warned exactly once per key');
  assert.match(messages[0], /CLODDS_MODEL is deprecated/);
  assert.match(messages[0], /BLITZKRIEG_MODEL/);
});

test('adoptLegacyEnv mirrors legacy names onto canonical names', () => {
  const env = { CLODDS_MODEL: 'old', CLODDS_LOCALE: 'zh', UNRELATED: 'x' } as NodeJS.ProcessEnv;
  const warnings: string[] = [];
  setLegacyEnvWarner((m) => warnings.push(m));
  try {
    const adopted = adoptLegacyEnv(env);
    assert.deepEqual(adopted, ['LOCALE', 'MODEL']);
    assert.equal(env.BLITZKRIEG_MODEL, 'old');
    assert.equal(env.BLITZKRIEG_LOCALE, 'zh');
    assert.equal((env as Record<string, unknown>).UNRELATED, 'x');
  } finally {
    setLegacyEnvWarner(null);
  }
  assert.equal(warnings.length, 1, 'one consolidated deprecation line');
});

test('adoptLegacyEnv never overrides an existing canonical value', () => {
  const env = { BLITZKRIEG_MODEL: 'new', CLODDS_MODEL: 'old' } as NodeJS.ProcessEnv;
  adoptLegacyEnv(env);
  assert.equal(env.BLITZKRIEG_MODEL, 'new');
});

test('adoptLegacyEnv ignores empty legacy values', () => {
  const env = { CLODDS_MODEL: '  ' } as NodeJS.ProcessEnv;
  const adopted = adoptLegacyEnv(env);
  assert.deepEqual(adopted, []);
  assert.equal(env.BLITZKRIEG_MODEL, undefined);
});

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { brandEnvNames, readBrandEnv } from '../../src/utils/env';

test('brandEnvNames maps a suffix to the canonical name', () => {
  assert.deepEqual(brandEnvNames('STATE_DIR'), {
    canonical: 'BLITZKRIEG_STATE_DIR',
  });
});

test('readBrandEnv reads only the canonical name', () => {
  assert.equal(readBrandEnv('MODEL', { BLITZKRIEG_MODEL: 'x' } as NodeJS.ProcessEnv), 'x');
  assert.equal(readBrandEnv('MODEL', { CLODDS_MODEL: 'x' } as NodeJS.ProcessEnv), undefined);
  assert.equal(readBrandEnv('MODEL', {} as NodeJS.ProcessEnv), undefined);
});

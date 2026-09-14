import { test } from 'node:test';
import assert from 'node:assert/strict';
import { existsSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { createTempStateDir } from '../helpers/state';

async function withStateDir<T>(dir: string, body: () => Promise<T>): Promise<T> {
  const previousStateDir = process.env.BLITZKRIEG_STATE_DIR;
  process.env.BLITZKRIEG_STATE_DIR = dir;
  try {
    return await body();
  } finally {
    if (previousStateDir === undefined) {
      delete process.env.BLITZKRIEG_STATE_DIR;
    } else {
      process.env.BLITZKRIEG_STATE_DIR = previousStateDir;
    }
  }
}

test('database uses BLITZKRIEG_STATE_DIR and the canonical file name', async () => {
  const tempState = createTempStateDir();
  const tempDir = tempState.dir;
  try {
    await withStateDir(tempDir, async () => {
      const { createDatabase } = await import('../../src/db/index.ts');
      const db = createDatabase();
      await db.getVersion();
      const dbPath = join(tempDir, 'blitzkrieg.db');
      assert.ok(existsSync(dbPath));
      db.close();
    });
  } finally {
    tempState.cleanup();
  }
});

test('database keeps using a pre-existing legacy clodds.db', async () => {
  const tempState = createTempStateDir();
  const tempDir = tempState.dir;
  try {
    // A zero-byte file is enough for the "reuse what exists" rule; the database
    // module must pick this path instead of creating a fresh blitzkrieg.db.
    writeFileSync(join(tempDir, 'clodds.db'), '');
    await withStateDir(tempDir, async () => {
      const { resolveDbPath } = await import('../../src/utils/brand-paths.ts');
      assert.equal(resolveDbPath(), join(tempDir, 'clodds.db'));
      assert.equal(existsSync(join(tempDir, 'blitzkrieg.db')), false);
    });
  } finally {
    tempState.cleanup();
  }
});

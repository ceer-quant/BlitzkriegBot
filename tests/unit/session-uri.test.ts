import { describe, it } from 'node:test';
import assert from 'node:assert';
import {
  SESSION_URI_SCHEME,
  buildSessionUri,
  parseSessionUri,
} from '../../src/session/uri';

describe('session share URIs', () => {
  it('builds canonical blitzkrieg:// links', () => {
    const uri = buildSessionUri('abc123', 'deadbeef');
    assert.equal(uri, 'blitzkrieg://session/abc123?key=deadbeef');
    assert.ok(uri.startsWith(SESSION_URI_SCHEME));
  });

  it('parses canonical links round-trip', () => {
    const uri = buildSessionUri('sess-1', 'key-2');
    const parsed = parseSessionUri(uri);
    assert.deepEqual(parsed, { sessionId: 'sess-1', key: 'key-2' });
  });

  it('accepts links without a key', () => {
    assert.equal(parseSessionUri('blitzkrieg://session/x')?.key, undefined);
  });

  it('rejects foreign schemes and malformed paths', () => {
    assert.equal(parseSessionUri('clodds://session/oldsess'), null);
    assert.equal(parseSessionUri('https://blitzkrieg.com/session/x'), null);
    assert.equal(parseSessionUri('blitzkrieg://other/x'), null);
    assert.equal(parseSessionUri('not a uri'), null);
  });
});

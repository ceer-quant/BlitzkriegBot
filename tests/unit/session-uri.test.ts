import { describe, it } from 'node:test';
import assert from 'node:assert';
import {
  SESSION_URI_SCHEME,
  LEGACY_SESSION_URI_SCHEME,
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
    assert.deepEqual(parsed, { sessionId: 'sess-1', key: 'key-2', legacy: false });
  });

  it('still accepts legacy clodds:// links and flags them', () => {
    const parsed = parseSessionUri('clodds://session/oldsess?key=0123abcd');
    assert.deepEqual(parsed, { sessionId: 'oldsess', key: '0123abcd', legacy: true });
  });

  it('accepts links without a key on both schemes', () => {
    assert.equal(parseSessionUri('blitzkrieg://session/x')?.key, undefined);
    assert.equal(parseSessionUri('clodds://session/x')?.legacy, true);
  });

  it('rejects foreign schemes and malformed paths', () => {
    assert.equal(parseSessionUri('https://clodds.com/session/x'), null);
    assert.equal(parseSessionUri('blitzkrieg://other/x'), null);
    assert.equal(parseSessionUri('not a uri'), null);
  });

  it('keeps the legacy scheme constant available during the compatibility window', () => {
    assert.equal(LEGACY_SESSION_URI_SCHEME, 'clodds://');
  });
});

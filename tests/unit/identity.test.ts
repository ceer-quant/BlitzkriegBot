import { describe, it } from 'node:test';
import assert from 'node:assert';
import {
  PRODUCT_NAME,
  PRODUCT_VERSION,
  PRODUCT_CONTACT_URL,
  userAgent,
  COPILOT_INTEGRATION_ID,
} from '../../src/utils/identity';

describe('outbound product identity', () => {
  it('builds the canonical user agent with and without detail', () => {
    assert.equal(userAgent(), `Blitzkrieg/${PRODUCT_VERSION}`);
    assert.equal(userAgent('News Aggregator'), `Blitzkrieg/${PRODUCT_VERSION} (News Aggregator)`);
  });

  it('uses the blitzkrieg product name everywhere', () => {
    assert.equal(PRODUCT_NAME, 'Blitzkrieg');
    assert.equal(COPILOT_INTEGRATION_ID, 'blitzkrieg');
    assert.doesNotMatch(userAgent(), /clodds/i);
    assert.doesNotMatch(PRODUCT_CONTACT_URL, /clodds/i);
  });
});

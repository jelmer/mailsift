const test = require('node:test');
const assert = require('node:assert/strict');
const { loadAddon } = require('./fakes.js');

const acme = { name: 'acme', order: 10, from_domains: ['acme.example'] };
const dhl = { name: 'dhl', order: 20, from_domains: ['dhl.example'] };

const names = (manifests) => Array.from(manifests, (m) => m.name);

test('getExtractors fetches once, then reuses the list while it is fresh', () => {
  const addon = loadAddon({ threads: [], manifests: [acme] });

  assert.deepEqual(names(addon.context.getExtractors()), ['acme']);
  addon.sleep(60 * 1000);
  assert.deepEqual(names(addon.context.getExtractors()), ['acme']);

  assert.deepEqual(addon.extractorRequests, [null]);
});

test('getExtractors revalidates a stale list with its ETag', () => {
  const addon = loadAddon({ threads: [], manifests: [acme] });
  addon.context.getExtractors();
  const etag = addon.context.getCachedExtractors().etag;

  addon.sleep(10 * 60 * 1000);
  assert.deepEqual(names(addon.context.getExtractors()), ['acme']);

  addon.server.manifests = [acme, dhl];
  addon.sleep(10 * 60 * 1000);
  assert.deepEqual(names(addon.context.getExtractors()), ['acme', 'dhl']);

  assert.deepEqual(addon.extractorRequests, [null, etag, etag]);
});

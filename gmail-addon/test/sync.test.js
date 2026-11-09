const test = require('node:test');
const assert = require('node:assert/strict');
const { loadAddon } = require('./fakes.js');

// Card actions are killed after this long.
const CARD_ACTION_LIMIT_MS = 30000;

const catchAll = { name: 'schema-ld', order: 10, requires: ['html'] };
const narrow = { name: 'dhl', order: 20, from_domains: ['dhl.example'] };

function messageIds(threadId, count) {
  return Array.from({ length: count }, (_, i) => `${threadId}-m${i}`);
}

test('a long thread is spread over several steps, each within the card action limit', () => {
  const addon = loadAddon({
    threads: [{ id: 't0', messages: 12 }],
    manifests: [catchAll],
  });

  let reset = true;
  let steps = 0;
  while (true) {
    const { value: state, elapsedMs } = addon.timed(() =>
      addon.context.runInteractiveSyncStep(reset)
    );
    reset = false;
    steps++;
    assert.ok(
      elapsedMs < CARD_ACTION_LIMIT_MS,
      `step ${steps} took ${elapsedMs}ms`
    );
    if (state.done) {
      break;
    }
    assert.ok(steps < 20, 'sync made no progress');
  }

  assert.ok(steps > 1);
  assert.deepEqual(addon.uploads, messageIds('t0', 12));
  assert.deepEqual(addon.labelled(), ['t0']);
});

test('a step handles a message even when setup used up the time budget', () => {
  const addon = loadAddon({
    threads: [{ id: 't0', messages: 1 }],
    manifests: [narrow],
    cost: { search: 6000 },
  });

  const state = addon.context.runInteractiveSyncStep(true);

  assert.equal(state.threadsDone, 1);
  assert.equal(state.scannedMessages, 1);
});

test('a message the server rejects does not block the rest of the sync', () => {
  const addon = loadAddon({
    threads: [
      { id: 't0', messages: 1 },
      { id: 't1', messages: 1 },
    ],
    manifests: [catchAll],
    cost: { upload: 100 },
    failIds: new Set(['t0-m0']),
  });

  addon.context.runInteractiveSyncStep(true);

  const state = addon.syncState();
  assert.equal(state.done, true);
  assert.equal(state.uploadedMessages, 1);
  assert.deepEqual(state.failedIds, ['t0-m0']);
  assert.deepEqual(state.failures, [
    {
      subject: 'Order t0 update 0',
      extractors: 'schema-ld',
      error: 'POST /api/messages failed with HTTP 500: extractor crashed',
    },
  ]);
  assert.deepEqual(addon.labelled(), ['t1']);
  assert.deepEqual(addon.uploads, ['t0-m0', 't1-m0']);

  addon.context.runInteractiveSyncStep(true);
  assert.deepEqual(addon.uploads, ['t0-m0', 't1-m0', 't0-m0']);
});

test('background sync uploads the remaining messages and then reports failures', () => {
  const addon = loadAddon({
    threads: [
      { id: 't0', messages: 1 },
      { id: 't1', messages: 1 },
    ],
    manifests: [catchAll],
    cost: { upload: 100 },
    failIds: new Set(['t0-m0']),
  });

  assert.throws(() => addon.context.syncMatchingEmails(), {
    message:
      '1 message(s) failed to upload:\n' +
      't0-m0: POST /api/messages failed with HTTP 500: extractor crashed',
  });
  assert.deepEqual(addon.uploads, ['t0-m0', 't1-m0']);
  assert.deepEqual(addon.labelled(), ['t1']);

  assert.throws(() => addon.context.syncMatchingEmails());
  assert.deepEqual(addon.uploads, ['t0-m0', 't1-m0', 't0-m0']);
});

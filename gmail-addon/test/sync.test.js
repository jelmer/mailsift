const test = require('node:test');
const assert = require('node:assert/strict');
const { loadAddon, START_MS, HOUR_MS } = require('./fakes.js');

// Card actions are killed after this long.
const CARD_ACTION_LIMIT_MS = 30000;

const catchAll = { name: 'any-html', order: 10, requires: ['html'] };
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
      extractors: 'any-html',
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

test('schema-ld mail without ld+json is not uploaded', () => {
  const schemaLd = { name: 'schema-ld', order: 10, requires: ['html'] };
  const addon = loadAddon({
    threads: [
      { id: 't0', messages: 1 },
      { id: 't1', messages: 1, html: '<script type="application/ld+json">{}</script>' },
    ],
    manifests: [schemaLd],
  });

  addon.context.runInteractiveSyncStep(true);

  assert.deepEqual(addon.uploads, ['t1-m0']);
  assert.deepEqual(addon.labelled(), ['t1']);
});

const narrowShop = { name: 'shop', order: 20, from_domains: ['shop.example'] };

function threadsReceived(count) {
  return Array.from({ length: count }, (_, i) => ({
    id: `t${i}`,
    hoursAgo: [count - i],
  }));
}

test('background sync covers every thread since the last run, oldest first', () => {
  const addon = loadAddon({
    threads: threadsReceived(40),
    manifests: [narrowShop],
    cost: { upload: 100 },
  });

  addon.context.syncMatchingEmails();

  assert.deepEqual(
    addon.uploads,
    Array.from({ length: 40 }, (_, i) => `t${i}-m0`)
  );
  assert.equal(addon.props.get('MAILSIFT_SYNC_CURSOR'), String(START_MS / 1000));
});

test('background sync resumes where a timed-out run stopped', () => {
  const addon = loadAddon({
    threads: threadsReceived(100),
    manifests: [narrowShop],
  });

  addon.context.syncMatchingEmails();
  const firstRun = addon.uploads.length;
  assert.ok(firstRun > 0 && firstRun < 100, `uploaded ${firstRun}`);
  assert.equal(
    addon.props.get('MAILSIFT_SYNC_CURSOR'),
    String((START_MS - (100 - firstRun + 1) * HOUR_MS) / 1000)
  );

  while (addon.uploads.length < 100) {
    addon.sleep(HOUR_MS);
    addon.context.syncMatchingEmails();
  }
  assert.deepEqual(
    addon.uploads,
    Array.from({ length: 100 }, (_, i) => `t${i}-m0`)
  );
});

test('background sync picks up replies in threads it already labelled', () => {
  const addon = loadAddon({
    threads: [{ id: 't0', hoursAgo: [5] }],
    manifests: [narrowShop],
    cost: { upload: 100 },
  });
  addon.context.syncMatchingEmails();
  assert.deepEqual(addon.labelled(), ['t0']);

  addon.sleep(HOUR_MS);
  addon.deliver('t0');
  addon.sleep(HOUR_MS);
  addon.context.syncMatchingEmails();

  assert.deepEqual(addon.uploads, ['t0-m0', 't0-m1']);
  assert.equal(
    addon.searches.at(-1),
    `after:${(START_MS - HOUR_MS) / 1000} from:(shop.example)`
  );
});

test('background sync does not redo messages before the cursor once the cache expires', () => {
  const addon = loadAddon({
    threads: [{ id: 't0', hoursAgo: [5] }],
    manifests: [narrowShop],
    cost: { upload: 100 },
  });
  addon.context.syncMatchingEmails();

  addon.sleep(7 * HOUR_MS);
  addon.deliver('t0');
  addon.expireCache();
  addon.context.syncMatchingEmails();

  assert.deepEqual(addon.uploads, ['t0-m0', 't0-m1']);
});

test('background sync rejects a corrupt cursor', () => {
  const addon = loadAddon({ threads: [], manifests: [narrowShop] });
  addon.props.set('MAILSIFT_SYNC_CURSOR', 'yesterday');

  assert.throws(() => addon.context.syncMatchingEmails(), {
    message: 'Invalid MAILSIFT_SYNC_CURSOR property: "yesterday"',
  });
});

test('background sync stops retrying a failed message once it leaves the lookback window', () => {
  const addon = loadAddon({
    threads: [{ id: 't0', hoursAgo: [24] }],
    manifests: [narrowShop],
    cost: { upload: 100 },
    failIds: new Set(['t0-m0']),
  });
  assert.throws(() => addon.context.syncMatchingEmails());
  addon.sleep(HOUR_MS);
  assert.throws(() => addon.context.syncMatchingEmails());
  assert.deepEqual(addon.uploads, ['t0-m0', 't0-m0']);

  addon.sleep(7 * 24 * HOUR_MS);
  assert.throws(() => addon.context.syncMatchingEmails(), {
    message:
      'Gave up on 1 message(s) older than the 7-day lookback:\n' +
      't0-m0: POST /api/messages failed with HTTP 500: extractor crashed',
  });

  addon.sleep(HOUR_MS);
  addon.context.syncMatchingEmails();
  assert.deepEqual(addon.uploads, ['t0-m0', 't0-m0']);
});

test('a step keeps going past five threads while within its time budget', () => {
  const addon = loadAddon({
    threads: Array.from({ length: 20 }, (_, i) => ({ id: `t${i}`, messages: 1 })),
    manifests: [narrow],
  });

  addon.context.onSyncNow();

  const state = addon.syncState();
  assert.equal(state.threadsDone, 20);
  assert.equal(state.done, true);
});

test('Sync Next Batch continues using commonEventObject parameters', () => {
  const addon = loadAddon({
    threads: [{ id: 't0', messages: 12 }],
    manifests: [catchAll],
  });
  addon.context.onSyncNow();
  const first = addon.uploads.length;

  addon.context.onSyncNow({
    commonEventObject: { parameters: { continueBatch: 'true' } },
  });

  assert.ok(addon.uploads.length > first);
  // A restarted sync would have reset the counters.
  assert.equal(addon.syncState().scannedMessages, addon.uploads.length);
});

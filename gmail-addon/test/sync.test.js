const test = require('node:test');
const assert = require('node:assert/strict');
const { loadAddon, START_MS, HOUR_MS } = require('./fakes.js');

const catchAll = { name: 'any-html', order: 10, requires: ['html'] };

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

  addon.context.syncMatchingEmails();

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

test('Start Sync queues one background run and shows it as queued', () => {
  const addon = loadAddon({ threads: [], manifests: [narrowShop] });

  addon.context.onSyncNow();
  addon.context.onSyncNow();

  assert.deepEqual(addon.triggers(), ['runScheduledSync']);
  assert.equal(addon.syncStatus().state, 'queued');
});

test('the scheduled run removes its trigger and records the outcome', () => {
  const addon = loadAddon({
    threads: threadsReceived(3),
    manifests: [narrowShop],
    cost: { upload: 100 },
  });
  addon.context.onSyncNow();

  addon.context.runScheduledSync();

  assert.deepEqual(addon.triggers(), []);
  const status = addon.syncStatus();
  assert.deepEqual(
    [status.state, status.total, status.handled, status.scanned, status.uploaded],
    ['finished', 3, 3, 3, 3]
  );
  assert.equal(addon.lock.held, false);
});

test('a run that runs out of time queues a follow-up run', () => {
  const addon = loadAddon({
    threads: threadsReceived(100),
    manifests: [narrowShop],
  });

  addon.context.syncMatchingEmails();

  assert.deepEqual(addon.triggers(), ['runScheduledSync']);
  assert.equal(addon.syncStatus().state, 'queued');
});

test('a sync leaves the work to a run already in progress', () => {
  const addon = loadAddon({
    threads: threadsReceived(3),
    manifests: [narrowShop],
  });
  addon.lock.held = true;

  assert.equal(addon.context.syncMatchingEmails(), null);
  assert.deepEqual(addon.uploads, []);
});

test('failed uploads are shown in the sync status', () => {
  const addon = loadAddon({
    threads: threadsReceived(2),
    manifests: [narrowShop],
    cost: { upload: 100 },
    failIds: new Set(['t0-m0']),
  });

  assert.throws(() => addon.context.syncMatchingEmails());

  const status = addon.syncStatus();
  assert.equal(status.state, 'failed');
  assert.equal(status.uploaded, 1);
  assert.equal(
    status.error,
    '1 message(s) failed to upload:\n' +
      't0-m0: POST /api/messages failed with HTTP 500: extractor crashed'
  );
});

test('Start Sync can queue a new run once a queued one has gone stale', () => {
  const addon = loadAddon({ threads: [], manifests: [narrowShop] });
  addon.context.onSyncNow();
  addon.context.ScriptApp.deleteTrigger(addon.context.ScriptApp.getProjectTriggers()[0]);

  addon.sleep(11 * 60 * 1000);
  addon.context.onSyncNow();

  assert.deepEqual(addon.triggers(), ['runScheduledSync']);
});

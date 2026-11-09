// Loads Code.js and Matcher.js into a VM context with fake Apps Script
// services. Fake service calls advance a virtual clock by `cost[...]` ms,
// which `Date.now()` inside the context reports.

const crypto = require('node:crypto');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const START_MS = Date.UTC(2026, 10, 9, 12, 0, 0);
const HOUR_MS = 3600 * 1000;

const DEFAULT_COST = {
  search: 500,
  extractors: 500,
  getMessages: 200,
  body: 300,
  raw: 200,
  upload: 3000,
};

// Stands in for CardService and ScriptApp: every property is a callable
// that returns the stub again, so builder chains work.
const stub = new Proxy(function () {}, {
  get: () => stub,
  apply: () => stub,
});

function response(code, text, headers) {
  return {
    getResponseCode: () => code,
    getContentText: () => text,
    getHeaders: () => headers || {},
  };
}

/**
 * Each thread gives either `messages` (that many messages, received a day
 * before the fake clock starts) or `hoursAgo` (one message per entry,
 * received that many hours before the clock starts).
 *
 * @param {Object} opts
 * @param {Array<{id: string, messages?: number, hoursAgo?: number[],
 *   html?: string}>} opts.threads
 * @param {Array<Object>} opts.manifests
 * @param {Object=} opts.cost
 * @param {Set<string>=} opts.failIds message IDs the server rejects
 * @param {Function=} opts.onUpload called on each upload
 */
function loadAddon({ threads, manifests, cost, failIds, onUpload }) {
  cost = Object.assign({}, DEFAULT_COST, cost || {});
  failIds = failIds || new Set();
  let now = START_MS;
  const tick = (ms) => {
    now += ms;
  };
  const cache = new Map();
  const props = new Map([['MAILSIFT_SERVER_URL', 'https://mailsift.example']]);
  const labels = new Map();
  const uploads = [];
  const searches = [];
  const server = { manifests };
  const triggers = [];
  let nextTriggerId = 1;
  const lock = { held: false };
  const extractorRequests = [];
  const mailbox = threads.map((t) => ({
    id: t.id,
    dates: (t.hoursAgo || Array(t.messages).fill(24)).map(
      (h) => START_MS - h * HOUR_MS
    ),
    html: t.html || '<p>Your order</p>',
    labels: new Set(),
  }));

  const fakeMessage = (t, i) => ({
    getId: () => `${t.id}-m${i}`,
    getFrom: () => 'Example Shop <orders@shop.example>',
    getSubject: () => `Order ${t.id} update ${i}`,
    getDate: () => new Date(t.dates[i]),
    getThread: () => fakeThread(t),
    getAttachments: () => {
      tick(cost.body);
      return [];
    },
    getBody: () => t.html,
    getPlainBody: () => 'Your order',
    getRawContent: () => {
      tick(cost.raw);
      return 'Subject: Order\r\n\r\nYour order';
    },
  });

  const fakeThread = (t) => ({
    getId: () => t.id,
    getMessages: () => {
      tick(cost.getMessages);
      return t.dates.map((_, i) => fakeMessage(t, i));
    },
    addLabel: (label) => t.labels.add(label.name),
  });

  // Understands the `after:`, `newer_than:Nd` and `-label:` terms the
  // add-on generates; other terms are ignored.
  function search(query, start, max) {
    tick(cost.search);
    searches.push(query);
    const after = /\bafter:(\d+)/.exec(query);
    const newerThan = /\bnewer_than:(\d+)d/.exec(query);
    const excluded = /-label:(\S+)/.exec(query);
    const sinceMs = after
      ? Number(after[1]) * 1000
      : newerThan
        ? now - Number(newerThan[1]) * 24 * HOUR_MS
        : -Infinity;
    return mailbox
      .filter((t) => !excluded || !t.labels.has(excluded[1]))
      .filter((t) => t.dates.some((d) => d > sinceMs))
      .sort((a, b) => Math.max(...b.dates) - Math.max(...a.dates))
      .slice(start, start + max)
      .map(fakeThread);
  }

  const context = {
    console: { log() {}, error() {} },
    PropertiesService: {
      getUserProperties: () => ({
        getProperty: (k) => (props.has(k) ? props.get(k) : null),
        setProperty: (k, v) => props.set(k, String(v)),
        setProperties: (kv) => {
          for (const [k, v] of Object.entries(kv)) {
            props.set(k, String(v));
          }
        },
      }),
    },
    CacheService: {
      getUserCache: () => ({
        get: (k) => (cache.has(k) ? cache.get(k) : null),
        put: (k, v) => cache.set(k, v),
        remove: (k) => cache.delete(k),
      }),
    },
    UrlFetchApp: {
      fetch: (url, opts) => {
        if (url.endsWith('/api/extractors')) {
          tick(cost.extractors);
          const body = JSON.stringify(server.manifests);
          const etag = `"${crypto.createHash('sha256').update(body).digest('hex')}"`;
          const ifNoneMatch = (opts.headers || {})['If-None-Match'] || null;
          extractorRequests.push(ifNoneMatch);
          return ifNoneMatch === etag
            ? response(304, '', { ETag: etag })
            : response(200, body, { ETag: etag });
        }
        tick(cost.upload);
        const id = decodeURIComponent(/source=([^&]+)/.exec(url)[1]).replace(
          /^gmail:/,
          ''
        );
        uploads.push(id);
        if (onUpload) {
          onUpload(id);
        }
        if (failIds.has(id)) {
          return response(500, 'extractor crashed');
        }
        return response(200, JSON.stringify({ matched: [], failed: [] }));
      },
    },
    GmailApp: {
      search,
      getMessagesForThreads: (ts) => ts.map((t) => t.getMessages()),
      getMessageById: (id) => {
        const [, threadId, i] = /^(.*)-m(\d+)$/.exec(id);
        return fakeMessage(
          mailbox.find((t) => t.id === threadId),
          Number(i)
        );
      },
      getUserLabelByName: (name) => labels.get(name) || null,
      createLabel: (name) => {
        const label = { name };
        labels.set(name, label);
        return label;
      },
    },
    CardService: stub,
    ScriptApp: {
      getProjectTriggers: () => triggers.slice(),
      deleteTrigger: (t) => triggers.splice(triggers.indexOf(t), 1),
      newTrigger: (handler) => ({
        timeBased: () => {
          const builder = {
            after: () => builder,
            everyHours: () => builder,
            create: () => {
              const id = `trigger-${nextTriggerId++}`;
              const t = { getHandlerFunction: () => handler, getUniqueId: () => id };
              triggers.push(t);
              return t;
            },
          };
          return builder;
        },
      }),
    },
    LockService: {
      getUserLock: () => ({
        tryLock: () => {
          if (lock.held) {
            return false;
          }
          lock.held = true;
          return true;
        },
        releaseLock: () => {
          lock.held = false;
        },
      }),
    },
    __now: () => now,
  };
  vm.createContext(context);
  vm.runInContext('Date.now = () => __now();', context);
  for (const file of ['Matcher.js', 'Format.js', 'Code.js']) {
    const src = fs.readFileSync(path.join(__dirname, '..', file), 'utf8');
    vm.runInContext(src, context, { filename: file });
  }

  return {
    context,
    uploads,
    searches,
    server,
    /** The `If-None-Match` header (or null) of each `/api/extractors` request. */
    extractorRequests,
    props,
    labelled: () => mailbox.filter((t) => t.labels.size > 0).map((t) => t.id),
    lock,
    /** Handler names of the project's triggers. */
    triggers: () => triggers.map((t) => t.getHandlerFunction()),
    syncStatus: () => JSON.parse(cache.get('MAILSIFT_SYNC_STATUS_V1') || 'null'),
    /** Drop everything from the user cache, as its TTLs would. */
    expireCache: () => cache.clear(),
    /** Let `ms` of virtual time pass. */
    sleep: tick,
    /** Deliver a new message into thread `id`, received now. */
    deliver(id) {
      mailbox.find((t) => t.id === id).dates.push(now);
    },
    /** Run `fn`, returning its result and the virtual time it took. */
    timed(fn) {
      const before = now;
      const value = fn();
      return { value, elapsedMs: now - before };
    },
  };
}

module.exports = { loadAddon, START_MS, HOUR_MS };

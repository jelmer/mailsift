// Loads Code.js and Matcher.js into a VM context with fake Apps Script
// services. Fake service calls advance a virtual clock by `cost[...]` ms,
// which `Date.now()` inside the context reports.

const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

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

function response(code, text) {
  return { getResponseCode: () => code, getContentText: () => text };
}

/**
 * @param {Object} opts
 * @param {Array<{id: string, messages: number}>} opts.threads
 * @param {Array<Object>} opts.manifests
 * @param {Object=} opts.cost
 * @param {Set<string>=} opts.failIds message IDs the server rejects
 */
function loadAddon({ threads, manifests, cost, failIds }) {
  cost = Object.assign({}, DEFAULT_COST, cost || {});
  failIds = failIds || new Set();
  let clock = 0;
  const tick = (ms) => {
    clock += ms;
  };
  const cache = new Map();
  const labels = new Map();
  const uploads = [];
  const mailbox = threads.map((t) => ({
    id: t.id,
    messages: t.messages,
    labels: new Set(),
  }));

  const fakeMessage = (t, i) => ({
    getId: () => `${t.id}-m${i}`,
    getFrom: () => 'Example Shop <orders@shop.example>',
    getSubject: () => `Order ${t.id} update ${i}`,
    getDate: () => new Date(Date.UTC(2026, 10, 1)),
    getAttachments: () => {
      tick(cost.body);
      return [];
    },
    getBody: () => '<p>Your order</p>',
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
      return Array.from({ length: t.messages }, (_, i) => fakeMessage(t, i));
    },
    addLabel: (label) => t.labels.add(label.name),
  });

  const context = {
    console: { log() {}, error() {} },
    PropertiesService: {
      getUserProperties: () => ({
        getProperty: (k) =>
          k === 'MAILSIFT_SERVER_URL' ? 'https://mailsift.example' : null,
      }),
    },
    CacheService: {
      getUserCache: () => ({
        get: (k) => (cache.has(k) ? cache.get(k) : null),
        put: (k, v) => cache.set(k, v),
      }),
    },
    UrlFetchApp: {
      fetch: (url) => {
        if (url.endsWith('/api/extractors')) {
          tick(cost.extractors);
          return response(200, JSON.stringify(manifests));
        }
        tick(cost.upload);
        const id = decodeURIComponent(/source=([^&]+)/.exec(url)[1]).replace(
          /^gmail:/,
          ''
        );
        uploads.push(id);
        if (failIds.has(id)) {
          return response(500, 'extractor crashed');
        }
        return response(200, JSON.stringify({ matched: [], failed: [] }));
      },
    },
    GmailApp: {
      search: (query, start, max) => {
        tick(cost.search);
        return mailbox
          .filter((t) => !t.labels.has('Mailsift'))
          .slice(start, start + max)
          .map(fakeThread);
      },
      getUserLabelByName: (name) => labels.get(name) || null,
      createLabel: (name) => {
        const label = { name };
        labels.set(name, label);
        return label;
      },
    },
    CardService: stub,
    ScriptApp: stub,
    __clock: () => clock,
  };
  vm.createContext(context);
  vm.runInContext('Date.now = () => __clock();', context);
  for (const file of ['Matcher.js', 'Code.js']) {
    const src = fs.readFileSync(path.join(__dirname, '..', file), 'utf8');
    vm.runInContext(src, context, { filename: file });
  }

  return {
    context,
    uploads,
    labelled: () => mailbox.filter((t) => t.labels.size > 0).map((t) => t.id),
    syncState: () => JSON.parse(cache.get('MAILSIFT_SYNC_STATE_V2') || 'null'),
    /** Run `fn` from a fresh virtual clock, returning its elapsed ms. */
    timed(fn) {
      clock = 0;
      const value = fn();
      return { value, elapsedMs: clock };
    },
  };
}

module.exports = { loadAddon };

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

/**
 * Load Code.js and Format.js with fake Apps Script services. `respond`
 * answers each fetch with `{ code, body }`. Card widgets are recorded as
 * the list of builder calls that set some text on them.
 */
function loadAddon(respond) {
  const fetches = [];
  const texts = [];
  const recordingStub = new Proxy(function () {}, {
    get: (_, name) =>
      name === 'Icon'
        ? {}
        : new Proxy(function () {}, {
            apply: (__, ___, args) => {
              if (/^set(Text|TopLabel|BottomLabel)$/.test(name)) {
                texts.push([name, args[0]]);
              }
              return recordingStub;
            },
            get: (__, inner) => recordingStub[inner],
          }),
    apply: () => recordingStub,
  });
  const context = vm.createContext({
    PropertiesService: {
      getUserProperties: () => ({
        getProperty: (key) =>
          ({
            MAILSIFT_SERVER_URL: 'https://mailsift.example/',
            MAILSIFT_AUTH_HEADER: 'tok',
          })[key] || null,
      }),
    },
    UrlFetchApp: {
      fetch: (url, opts) => {
        fetches.push({ url, headers: opts.headers });
        const { code, body } = respond(url);
        return {
          getResponseCode: () => code,
          getContentText: () => JSON.stringify(body),
        };
      },
    },
    CardService: recordingStub,
  });
  for (const file of ['Format.js', 'Code.js']) {
    vm.runInContext(
      fs.readFileSync(path.join(__dirname, '..', file), 'utf8'),
      context,
      { filename: file }
    );
  }
  return { context, fetches, texts };
}

test('lookupMessage asks for the Message-ID and returns the entry', () => {
  const entry = { message_id: 'a@example.org', extractors: [] };
  const addon = loadAddon(() => ({ code: 200, body: entry }));
  const result = addon.context.lookupMessage(
    addon.context.getSettings(),
    '<a+b@example.org>'
  );

  assert.deepEqual(JSON.parse(JSON.stringify(result)), entry);
  assert.deepEqual(JSON.parse(JSON.stringify(addon.fetches)), [
    {
      url: 'https://mailsift.example/api/messages?message_id=%3Ca%2Bb%40example.org%3E',
      headers: { Authorization: 'Bearer tok' },
    },
  ]);
});

test('lookupMessage returns null for an unknown message', () => {
  const addon = loadAddon(() => ({ code: 404, body: {} }));
  assert.equal(
    addon.context.lookupMessage(addon.context.getSettings(), '<a@example.org>'),
    null
  );
});

test('lookupMessage throws on a server error', () => {
  const addon = loadAddon(() => ({ code: 500, body: {} }));
  assert.throws(
    () => addon.context.lookupMessage(addon.context.getSettings(), '<a@example.org>'),
    /HTTP 500/
  );
});

test('buildMessageSection lists records and failures', () => {
  const addon = loadAddon(() => ({
    code: 200,
    body: {
      message_id: 'a@example.org',
      processed_at: 1,
      extractors: [
        {
          extractor: 'dhl',
          records: [
            {
              kind: 'parcel',
              filename: 'x.parcel.json',
              content_type: 'application/json',
              data: { trackingNumber: 'DHL999', deliveryStatus: 'OnItsWay' },
            },
            {
              kind: 'event',
              filename: 'x.event.ics',
              content_type: 'text/calendar',
              data: {
                summary: 'Delivery',
                start: { datetime: '2026-12-01T09:30:00Z' },
              },
            },
            { kind: 'ticket', filename: 'label.ticket.pdf', content_type: 'application/pdf' },
          ],
        },
        { extractor: 'broken', error: 'exit status 3' },
      ],
    },
  }));
  addon.context.buildMessageSection(addon.context.getSettings(), '<a@example.org>');

  assert.deepEqual(addon.texts, [
    ['setTopLabel', 'Parcel'],
    ['setText', 'DHL999'],
    ['setBottomLabel', 'On its way'],
    ['setTopLabel', 'Event'],
    ['setText', 'Delivery'],
    ['setBottomLabel', 'Tue 1 Dec 2026, 09:30 UTC'],
    ['setTopLabel', 'Ticket'],
    ['setText', 'label.ticket.pdf'],
    ['setTopLabel', 'broken failed'],
    ['setText', 'exit status 3'],
    ['setText', 'Open Mailsift'],
    ['setText', 'Settings'],
  ]);
});

test('buildMessageSection says when nothing was extracted', () => {
  const addon = loadAddon(() => ({
    code: 200,
    body: { extractors: [{ extractor: 'quiet', records: [] }] },
  }));
  addon.context.buildMessageSection(addon.context.getSettings(), '<a@example.org>');
  assert.deepEqual(addon.texts[0], [
    'setText',
    'quiet found nothing to extract from this message.',
  ]);
});

test('buildMessageSection says when the message is unknown', () => {
  const addon = loadAddon(() => ({ code: 404, body: {} }));
  addon.context.buildMessageSection(addon.context.getSettings(), '<a@example.org>');
  assert.deepEqual(addon.texts[0], [
    'setText',
    'Mailsift has not extracted anything from this message.',
  ]);
});

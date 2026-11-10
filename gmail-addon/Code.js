/**
 * Google Workspace Gmail Add-on for Mailsift.
 *
 * Fetches extractor definitions from `<serverUrl>/api/extractors`,
 * matches Gmail messages against each extractor's dispatch rules,
 * uploads matching raw RFC822 messages to `<serverUrl>/api/messages`,
 * and displays extracted artifact details with direct links to Mailsift.
 */

const PROP_SERVER_URL = 'MAILSIFT_SERVER_URL';
const PROP_AUTH_HEADER = 'MAILSIFT_AUTH_HEADER';
const PROP_AUTO_UPLOAD = 'MAILSIFT_AUTO_UPLOAD';
const PROP_PROCESSED_LABEL = 'MAILSIFT_PROCESSED_LABEL';
const CACHE_EXTRACTORS_KEY = 'MAILSIFT_EXTRACTORS_V2';
const CACHE_CHECKED_IDS_KEY = 'MAILSIFT_CHECKED_IDS_V1';
const CACHE_SYNC_STATUS_KEY = 'MAILSIFT_SYNC_STATUS_V1';
const CACHE_MSG_RESULT_PREFIX = 'MAILSIFT_MSG_RES_';
const CACHE_MAX_TTL_SECONDS = 21600;
// How long to use the cached extractor list before revalidating it.
const EXTRACTORS_FRESH_MS = 5 * 60 * 1000;
const MSG_RESULT_TTL_SECONDS = 21600; // 6 hours
const DEFAULT_PROCESSED_LABEL = 'Mailsift';
const PROP_SYNC_CURSOR = 'MAILSIFT_SYNC_CURSOR';
const PROP_SYNC_RETRIES = 'MAILSIFT_SYNC_RETRIES';
const SYNC_LOOKBACK_SECONDS = 7 * 86400;
// Gmail can take a while to index new mail, so the background sync
// searches this far before its cursor; checkedIds skips repeats.
const SYNC_OVERLAP_SECONDS = 3600;
const SEARCH_PAGE_SIZE = 100;
// Time budgets for a sync run from the hourly trigger (executions are
// killed at 6 minutes) and from a card action (killed at 30 seconds).
const SYNC_MAX_DURATION_MS = 240000;
const UI_SYNC_BUDGET_MS = 20000;
// A running status older than this belongs to a run that was killed.
const SYNC_STALE_MS = 10 * 60 * 1000;

/**
 * Load user settings from `PropertiesService`.
 *
 * @return {Object}
 */
function getSettings() {
  const props = PropertiesService.getUserProperties();
  return {
    serverUrl: (props.getProperty(PROP_SERVER_URL) || '').replace(/\/+$/, ''),
    authHeader: (props.getProperty(PROP_AUTH_HEADER) || '').trim(),
    autoUpload: props.getProperty(PROP_AUTO_UPLOAD) === 'true',
    processedLabel:
      (props.getProperty(PROP_PROCESSED_LABEL) || '').trim() ||
      DEFAULT_PROCESSED_LABEL,
  };
}

/**
 * Build request headers for `UrlFetchApp`, including optional external
 * reverse-proxy `Authorization` header when configured.
 *
 * @param {Object} settings
 * @param {Object=} extraHeaders
 * @return {Object}
 */
function buildHeaders(settings, extraHeaders) {
  const headers = Object.assign({}, extraHeaders || {});
  if (settings.authHeader) {
    headers['Authorization'] = settings.authHeader.includes(' ')
      ? settings.authHeader
      : 'Bearer ' + settings.authHeader;
  }
  return headers;
}

/**
 * Return the cached extractor manifests with the ETag they were served
 * with and when that was last confirmed, or `null` if the cache is cold.
 *
 * @return {{manifests: Array<Object>, etag: (string|null), checkedAt: number}|null}
 */
function getCachedExtractors() {
  const cached = CacheService.getUserCache().get(CACHE_EXTRACTORS_KEY);
  return cached ? JSON.parse(cached) : null;
}

/**
 * Look up a response header case-insensitively.
 *
 * @param {GoogleAppsScript.URL_Fetch.HTTPResponse} resp
 * @param {string} name
 * @return {string|null}
 */
function getResponseHeader(resp, name) {
  const headers = resp.getHeaders();
  const key = Object.keys(headers).find(
    (k) => k.toLowerCase() === name.toLowerCase()
  );
  return key === undefined ? null : headers[key];
}

/**
 * Return the extractor manifests from `GET <serverUrl>/api/extractors`.
 *
 * A cached list is used as-is for `EXTRACTORS_FRESH_MS`; after that it is
 * revalidated with `If-None-Match`, which costs a round trip but no body
 * when nothing changed.
 *
 * @return {Array<Object>}
 */
function getExtractors() {
  const settings = getSettings();
  if (!settings.serverUrl) {
    throw new Error('Mailsift server URL is not configured.');
  }

  const cached = getCachedExtractors();
  const nowMs = Date.now();
  if (cached && nowMs - cached.checkedAt < EXTRACTORS_FRESH_MS) {
    return cached.manifests;
  }

  const url = settings.serverUrl + '/api/extractors';
  const extraHeaders = { Accept: 'application/json' };
  if (cached && cached.etag) {
    extraHeaders['If-None-Match'] = cached.etag;
  }
  const resp = UrlFetchApp.fetch(url, {
    method: 'get',
    headers: buildHeaders(settings, extraHeaders),
    muteHttpExceptions: true,
  });

  const status = resp.getResponseCode();
  let manifests;
  if (status === 304 && cached) {
    manifests = cached.manifests;
  } else if (status >= 200 && status < 300) {
    manifests = JSON.parse(resp.getContentText());
    if (!Array.isArray(manifests)) {
      throw new Error('Expected JSON array from /api/extractors');
    }
  } else {
    throw new Error(
      `GET ${url} failed with HTTP ${status}: ${resp.getContentText().slice(0, 200)}`
    );
  }

  CacheService.getUserCache().put(
    CACHE_EXTRACTORS_KEY,
    JSON.stringify({
      manifests,
      etag: getResponseHeader(resp, 'ETag'),
      checkedAt: nowMs,
    }),
    CACHE_MAX_TTL_SECONDS
  );
  return manifests;
}

/**
 * The Mailsift server answered a request with a non-2xx status. Network
 * failures surface as plain `Error`s from `UrlFetchApp` instead.
 */
class MailsiftHttpError extends Error {
  constructor(status, message) {
    super(message);
    this.name = 'MailsiftHttpError';
    this.status = status;
  }
}

/**
 * Upload a raw RFC822 message to `POST <serverUrl>/api/messages`.
 *
 * @param {string} rawRfc822
 * @param {string} messageId
 * @param {number=} receivedAtEpoch
 * @param {boolean=} dryRun
 * @return {Object} parsed JSON response from the Mailsift server
 */
function uploadMessageToMailsift(
  rawRfc822,
  messageId,
  receivedAtEpoch,
  dryRun
) {
  const settings = getSettings();
  if (!settings.serverUrl) {
    throw new Error('Mailsift server URL is not configured.');
  }

  const params = [`source=${encodeURIComponent('gmail:' + messageId)}`];
  if (typeof receivedAtEpoch === 'number' && !Number.isNaN(receivedAtEpoch)) {
    params.push(`received_at=${Math.floor(receivedAtEpoch)}`);
  }
  if (dryRun) {
    params.push('dry_run=true');
  }

  const url = `${settings.serverUrl}/api/messages?${params.join('&')}`;
  const resp = UrlFetchApp.fetch(url, {
    method: 'post',
    contentType: 'message/rfc822',
    headers: buildHeaders(settings),
    payload: rawRfc822,
    muteHttpExceptions: true,
  });

  const status = resp.getResponseCode();
  if (status < 200 || status >= 300) {
    throw new MailsiftHttpError(
      status,
      `POST /api/messages failed with HTTP ${status}: ${resp.getContentText().slice(0, 200)}`
    );
  }

  const result = JSON.parse(resp.getContentText());
  if (!dryRun && messageId) {
    saveCachedMessageResult(messageId, result);
  }
  return result;
}

/**
 * Cache the `POST /api/messages` result for a Gmail message ID so
 * reopening the email shows the extracted summary immediately.
 *
 * @param {string} messageId
 * @param {Object} result
 */
function saveCachedMessageResult(messageId, result) {
  try {
    CacheService.getUserCache().put(
      CACHE_MSG_RESULT_PREFIX + messageId,
      JSON.stringify(result),
      MSG_RESULT_TTL_SECONDS
    );
  } catch (_) {}
}

/**
 * Load a previously cached `POST /api/messages` result for a Gmail message ID.
 *
 * @param {string} messageId
 * @return {Object|null}
 */
function getCachedMessageResult(messageId) {
  try {
    const raw = CacheService.getUserCache().get(
      CACHE_MSG_RESULT_PREFIX + messageId
    );
    return raw ? JSON.parse(raw) : null;
  } catch (_) {
    return null;
  }
}

/**
 * Render a text progress bar, e.g. `██████░░░░ 60% (6 / 10)`.
 *
 * @param {number} current
 * @param {number} total
 * @return {string}
 */
function renderProgressBar(current, total) {
  const safeTotal = Math.max(total, 1);
  const clamped = Math.min(Math.max(current, 0), safeTotal);
  const ratio = clamped / safeTotal;
  const width = 12;
  const filled = Math.round(ratio * width);
  const bar = '█'.repeat(filled) + '░'.repeat(width - filled);
  const pct = Math.round(ratio * 100);
  return `${bar} ${pct}% (${clamped} / ${total})`;
}

/**
 * Inspect a `GmailMessage` lazily:
 * 1. Check `from` and `subject` headers first (fast, no MIME body fetch).
 * 2. Only fetch body/attachments if a header-matched manifest has `requires`.
 * 3. Only fetch `getRawContent()` if a matched manifest has `require_dkim`
 *    or `fetchRawOnMatch` is true.
 *
 * @param {Array<Object>} extractors
 * @param {GoogleAppsScript.Gmail.GmailMessage} message
 * @param {boolean=} fetchRawOnMatch
 * @return {Object} `{ info, matching }`
 */
function evaluateGmailMessage(extractors, message, fetchRawOnMatch) {
  const dateObj = message.getDate();
  const receivedAtEpoch = dateObj
    ? Math.floor(dateObj.getTime() / 1000)
    : undefined;

  const info = {
    id: message.getId(),
    from: message.getFrom(),
    subject: message.getSubject(),
    receivedAtEpoch,
    bodyParts: null,
    rawRfc822: null,
  };

  let candidates = matchHeaders(extractors, info);
  if (candidates.length === 0) {
    return { info, matching: [] };
  }

  const needsBody = candidates.some(
    (m) => m.requires && m.requires.length > 0
  );
  if (needsBody) {
    info.bodyParts = inspectMessageBodyParts(message);
    candidates = matchMessage(candidates, info);
    if (candidates.length === 0) {
      return { info, matching: [] };
    }
  }

  const needsDkim = candidates.some(
    (m) => m.require_dkim && m.require_dkim.length > 0
  );
  if (needsDkim || fetchRawOnMatch) {
    info.rawRfc822 = message.getRawContent();
    if (needsDkim) {
      candidates = matchMessage(candidates, info);
    }
  }

  return { info, matching: candidates };
}

/**
 * Extract body and attachment metadata for `requires:` checks.
 *
 * @param {GoogleAppsScript.Gmail.GmailMessage} message
 * @return {Object}
 */
function inspectMessageBodyParts(message) {
  const attachments = message.getAttachments({
    includeInlineImages: true,
    includeAttachments: true,
  });
  const htmlBody = message.getBody() || '';
  const plainBody = message.getPlainBody() || '';
  const mimeTypes = [];
  if (htmlBody) {
    mimeTypes.push('text/html');
  }
  if (plainBody) {
    mimeTypes.push('text/plain');
  }
  const attachmentFilenames = [];
  for (const att of attachments) {
    const ct = (att.getContentType() || '').split(';')[0].trim().toLowerCase();
    if (ct) {
      mimeTypes.push(ct);
    }
    const name = att.getName();
    if (name) {
      attachmentFilenames.push(name);
    }
  }
  return {
    hasHtml: Boolean(htmlBody),
    hasText: Boolean(plainBody),
    hasLdJson: hasLdJsonBlock(htmlBody),
    mimeTypes,
    attachmentFilenames,
  };
}

/**
 * Ensure the configured Gmail label exists and apply it to `thread`.
 *
 * @param {GoogleAppsScript.Gmail.GmailThread} thread
 * @param {string} labelName
 */
function markThreadProcessed(thread, labelName) {
  if (!labelName) {
    return;
  }
  let label = GmailApp.getUserLabelByName(labelName);
  if (!label) {
    label = GmailApp.createLabel(labelName);
  }
  thread.addLabel(label);
}

/**
 * Homepage trigger handler. Loads the extractor list (usually from the
 * cache), which also shows whether the server is reachable.
 *
 * @param {string=} statusMessage Optional status banner to display
 * @return {GoogleAppsScript.Card_Service.Card}
 */
function onHomepage(statusMessage) {
  const settings = getSettings();
  if (!settings.serverUrl) {
    return onSettings();
  }

  const card = CardService.newCardBuilder().setHeader(
    CardService.newCardHeader()
      .setTitle('Mailsift')
      .setSubtitle(settings.serverUrl)
  );

  const section = CardService.newCardSection();
  if (typeof statusMessage === 'string' && statusMessage) {
    section.addWidget(
      CardService.newTextParagraph().setText(statusMessage)
    );
  }

  try {
    const extractors = getExtractors();
    section.addWidget(
      CardService.newDecoratedText()
        .setTopLabel('Extractors')
        .setText(`${extractors.length} active extractor(s)`)
        .setBottomLabel('Ready to match and sync emails')
    );
  } catch (err) {
    section.addWidget(
      CardService.newDecoratedText()
        .setTopLabel('Extractors')
        .setText('Could not load extractors from the server')
        .setBottomLabel(err.message)
        .setWrapText(true)
    );
  }

  const syncStatus = getSyncStatus();
  const { text, detail } = describeSyncStatus(syncStatus);
  section.addWidget(
    CardService.newDecoratedText()
      .setTopLabel('Sync')
      .setText(text)
      .setBottomLabel(detail)
      .setWrapText(true)
  );
  const syncButtons = CardService.newButtonSet();
  if (!isSyncRunning(syncStatus)) {
    syncButtons.addButton(
      CardService.newTextButton()
        .setText('Start Sync')
        .setTextButtonStyle(CardService.TextButtonStyle.FILLED)
        .setOnClickAction(
          CardService.newAction()
            .setFunctionName('onSyncNow')
            .setLoadIndicator(CardService.LoadIndicator.SPINNER)
        )
    );
  }
  syncButtons.addButton(
    CardService.newTextButton()
      .setText('Refresh')
      .setOnClickAction(
        CardService.newAction()
          .setFunctionName('onRefreshHomepage')
          .setLoadIndicator(CardService.LoadIndicator.SPINNER)
      )
  );
  section.addWidget(syncButtons);

  section.addWidget(
    CardService.newButtonSet()
      .addButton(
        CardService.newTextButton()
          .setText('Open Mailsift')
          .setOpenLink(
            CardService.newOpenLink().setUrl(settings.serverUrl)
          )
      )
      .addButton(
        CardService.newTextButton()
          .setText('Settings')
          .setOnClickAction(
            CardService.newAction().setFunctionName('onSettings')
          )
      )
  );

  card.addSection(section);
  return card.build();
}

/**
 * Contextual trigger handler when a user opens an email in Gmail.
 *
 * @param {Object} e Gmail contextual event object
 * @return {GoogleAppsScript.Card_Service.Card}
 */
function onGmailMessageOpen(e) {
  const settings = getSettings();
  if (!settings.serverUrl) {
    return onSettings();
  }

  if (e && e.gmail && e.gmail.accessToken) {
    GmailApp.setCurrentMessageAccessToken(e.gmail.accessToken);
  }

  const messageId = e.gmail.messageId;
  const message = GmailApp.getMessageById(messageId);

  const card = CardService.newCardBuilder().setHeader(
    CardService.newCardHeader()
      .setTitle('Mailsift')
      .setSubtitle(message.getSubject() || '(no subject)')
  );

  const section = CardService.newCardSection();
  let extractors;
  try {
    extractors = getExtractors();
  } catch (err) {
    section.addWidget(
      CardService.newTextParagraph().setText(
        `Could not load extractors from the server: ${err.message}`
      )
    );
    section.addWidget(
      CardService.newButtonSet()
        .addButton(
          CardService.newTextButton()
            .setText('Upload Anyway')
            .setOnClickAction(
              CardService.newAction()
                .setFunctionName('onUploadCurrentMessage')
                .setParameters({ messageId, dryRun: 'false' })
            )
        )
    );
    card.addSection(section);
    return card.build();
  }

  let matching = [];
  let info = null;
  try {
    const evalResult = evaluateGmailMessage(
      extractors,
      message,
      settings.autoUpload
    );
    info = evalResult.info;
    matching = evalResult.matching;
  } catch (err) {
    section.addWidget(
      CardService.newTextParagraph().setText(
        `Error inspecting message: ${err.message}`
      )
    );
    card.addSection(section);
    return card.build();
  }

  let uploadResult = getCachedMessageResult(info.id);

  if (matching.length > 0) {
    const names = matching.map((m) => m.name).join(', ');
    section.addWidget(
      CardService.newKeyValue()
        .setTopLabel('Matching Extractors')
        .setContent(names)
    );

    if (settings.autoUpload && !uploadResult) {
      try {
        if (!info.rawRfc822) {
          info.rawRfc822 = message.getRawContent();
        }
        uploadResult = uploadMessageToMailsift(
          info.rawRfc822,
          info.id,
          info.receivedAtEpoch,
          false
        );
        markThreadProcessed(message.getThread(), settings.processedLabel);
      } catch (err) {
        section.addWidget(
          CardService.newTextParagraph().setText(`Upload failed: ${err.message}`)
        );
      }
    }
  } else {
    section.addWidget(
      CardService.newTextParagraph().setText(
        'No extractor manifest hints matched this message.'
      )
    );
  }

  section.addWidget(
    CardService.newButtonSet()
      .addButton(
        CardService.newTextButton()
          .setText(uploadResult ? 'Re-upload to Mailsift' : 'Upload to Mailsift')
          .setTextButtonStyle(CardService.TextButtonStyle.FILLED)
          .setOnClickAction(
            CardService.newAction()
              .setFunctionName('onUploadCurrentMessage')
              .setParameters({ messageId: info.id, dryRun: 'false' })
          )
      )
      .addButton(
        CardService.newTextButton()
          .setText('Dry Run')
          .setOnClickAction(
            CardService.newAction()
              .setFunctionName('onUploadCurrentMessage')
              .setParameters({ messageId: info.id, dryRun: 'true' })
          )
      )
  );

  card.addSection(section);

  if (uploadResult) {
    card.addSection(
      buildExtractedInfoSection(settings, uploadResult, 'Extracted by Mailsift')
    );
  }

  return card.build();
}

/**
 * Build a CardSection listing what Mailsift extracted from a message,
 * from the `records` in a `POST /api/messages` response, with links to
 * the records in Mailsift.
 *
 * @param {Object} settings
 * @param {Object} result `POST /api/messages` response
 * @param {string} heading
 * @return {GoogleAppsScript.Card_Service.CardSection}
 */
function buildExtractedInfoSection(settings, result, heading) {
  const section = CardService.newCardSection().setHeader(heading);
  let widgets = 0;
  const link = (path) =>
    CardService.newOpenLink().setUrl(settings.serverUrl + path);

  for (const m of result.matched || []) {
    const records = m.records || [];
    for (const record of records) {
      const summary = summarizeRecord(record.kind, record.data);
      const widget = CardService.newDecoratedText()
        .setTopLabel(summary.label)
        .setText(summary.title)
        .setWrapText(true)
        .setOpenLink(link(summary.path));
      if (summary.subtitle) {
        widget.setBottomLabel(summary.subtitle);
      }
      // Cosmetic: leave the icon out if this Icon name isn't available.
      if (CardService.Icon[summary.icon]) {
        widget.setStartIcon(
          CardService.newIconImage().setIcon(CardService.Icon[summary.icon])
        );
      }
      section.addWidget(widget);
      widgets++;
    }

    // Artifacts that come without a JSON record (calendar events, ticket
    // files), and everything from servers that don't send `records`.
    for (const kind of UNRECORDED_KINDS) {
      const shown = records.filter((r) => r.kind === kind.record).length;
      const extra = kind.counted ? m[kind.key] - shown : m[kind.key];
      if (extra > 0 && !(kind.key === 'tickets' && shown > 0)) {
        section.addWidget(
          CardService.newDecoratedText()
            .setTopLabel(m.extractor)
            .setText(`${extra} ${extra === 1 ? kind.one : kind.many}`)
            .setOpenLink(link(kind.listPath))
        );
        widgets++;
      }
    }
  }

  for (const f of result.failed || []) {
    section.addWidget(
      CardService.newDecoratedText()
        .setTopLabel(`${f.extractor} failed`)
        .setText(f.error)
        .setWrapText(true)
    );
    widgets++;
  }

  if (widgets === 0) {
    section.addWidget(
      CardService.newTextParagraph().setText(
        'Nothing was extracted from this message.'
      )
    );
  }
  section.addWidget(
    CardService.newButtonSet().addButton(
      CardService.newTextButton()
        .setText('Open Mailsift')
        .setOpenLink(link('/all'))
    )
  );
  return section;
}

// `counted`: the count in the response covers exactly the JSON records
// of that kind, so only the difference needs a separate line.
const UNRECORDED_KINDS = [
  { key: 'events', record: null, counted: false, one: 'calendar event', many: 'calendar events', listPath: '/events' },
  { key: 'tickets', record: 'ticket', counted: false, one: 'ticket file', many: 'ticket files', listPath: '/tickets' },
  { key: 'reservations', record: 'reservation', counted: true, one: 'reservation', many: 'reservations', listPath: '/reservations' },
  { key: 'bills', record: 'bill', counted: true, one: 'bill', many: 'bills', listPath: '/bills' },
  { key: 'parcels', record: 'parcel', counted: true, one: 'parcel', many: 'parcels', listPath: '/parcels' },
  { key: 'receipts', record: 'receipt', counted: true, one: 'receipt', many: 'receipts', listPath: '/receipts' },
  { key: 'subscriptions', record: 'subscription', counted: true, one: 'subscription', many: 'subscriptions', listPath: '/subscriptions' },
];

/**
 * Format the JSON response from `POST /api/messages` into a compact
 * human-readable summary.
 *
 * @param {Object} result
 * @return {string}
 */
function formatUploadOutcome(result) {
  if (!result || !Array.isArray(result.matched) || result.matched.length === 0) {
    return 'Processed (no artifacts emitted)';
  }
  const summaries = [];
  for (const m of result.matched) {
    const counts = [];
    for (const key of [
      'events',
      'reservations',
      'bills',
      'parcels',
      'receipts',
      'tickets',
      'subscriptions',
    ]) {
      if (m[key] > 0) {
        counts.push(`${m[key]} ${key}`);
      }
    }
    if (counts.length > 0) {
      summaries.push(`${m.extractor}: ${counts.join(', ')}`);
    }
  }
  return summaries.length > 0
    ? summaries.join('; ')
    : 'Matched, produced 0 artifacts';
}

/**
 * Action handler for the "Upload to Mailsift" / "Dry Run" button.
 *
 * @param {Object} e
 * @return {GoogleAppsScript.Card_Service.ActionResponse}
 */
function onUploadCurrentMessage(e) {
  if (e && e.gmail && e.gmail.accessToken) {
    GmailApp.setCurrentMessageAccessToken(e.gmail.accessToken);
  }
  const params = getActionParameters(e);
  const messageId = params.messageId;
  if (!messageId) {
    throw new Error('Upload action is missing its messageId parameter');
  }
  const dryRun = params.dryRun === 'true';
  const message = GmailApp.getMessageById(messageId);
  const dateObj = message.getDate();
  const receivedAtEpoch = dateObj
    ? Math.floor(dateObj.getTime() / 1000)
    : undefined;

  try {
    const rawRfc822 = message.getRawContent();
    const res = uploadMessageToMailsift(
      rawRfc822,
      messageId,
      receivedAtEpoch,
      dryRun
    );
    if (!dryRun) {
      const settings = getSettings();
      markThreadProcessed(message.getThread(), settings.processedLabel);
    }
    const builder = CardService.newActionResponseBuilder().setNotification(
      CardService.newNotification().setText(formatUploadOutcome(res))
    );
    if (dryRun) {
      const preview = CardService.newCardBuilder()
        .setHeader(
          CardService.newCardHeader()
            .setTitle('Dry run')
            .setSubtitle(message.getSubject() || '(no subject)')
        )
        .addSection(
          buildExtractedInfoSection(getSettings(), res, 'Would extract')
        )
        .build();
      builder.setNavigation(CardService.newNavigation().pushCard(preview));
    } else {
      builder.setNavigation(
        CardService.newNavigation().updateCard(onGmailMessageOpen(e))
      );
    }
    return builder.build();
  } catch (err) {
    return CardService.newActionResponseBuilder()
      .setNotification(
        CardService.newNotification().setText(`Error: ${err.message}`)
      )
      .build();
  }
}

/**
 * Return the parameters set on the action that triggered `e`, or `{}`
 * when there are none (e.g. the universal "Sync Matching Emails" action).
 * Workspace add-ons pass them in `commonEventObject.parameters`; older
 * Gmail add-on events used `parameters`.
 *
 * @param {Object=} e
 * @return {Object<string, string>}
 */
function getActionParameters(e) {
  if (!e) {
    return {};
  }
  return (
    (e.commonEventObject && e.commonEventObject.parameters) ||
    e.parameters ||
    {}
  );
}

/**
 * Load the set of already-checked Gmail message IDs from `CacheService`
 * so repeated sync clicks skip already-inspected non-matching messages.
 *
 * @return {Set<string>}
 */
function loadCheckedMessageIds() {
  try {
    const raw = CacheService.getUserCache().get(CACHE_CHECKED_IDS_KEY);
    return raw ? new Set(JSON.parse(raw)) : new Set();
  } catch (_) {
    return new Set();
  }
}

/**
 * Save the set of checked Gmail message IDs to `CacheService`.
 *
 * @param {Set<string>} ids
 */
function saveCheckedMessageIds(ids) {
  try {
    const arr = Array.from(ids).slice(-500);
    CacheService.getUserCache().put(
      CACHE_CHECKED_IDS_KEY,
      JSON.stringify(arr),
      MSG_RESULT_TTL_SECONDS
    );
  } catch (_) {}
}

/**
 * Evaluate one message during sync and upload it if it matches.
 *
 * An HTTP error from the Mailsift server is returned as `error` rather
 * than thrown, so one message the server rejects doesn't block every
 * message after it. Network failures still propagate.
 *
 * @param {Array<Object>} extractors
 * @param {GoogleAppsScript.Gmail.GmailMessage} message
 * @return {Object} `{ info, matching, result?, error? }`
 */
function syncMessage(extractors, message) {
  const { info, matching } = evaluateGmailMessage(extractors, message, true);
  if (matching.length === 0) {
    return { info, matching };
  }
  try {
    const result = uploadMessageToMailsift(
      info.rawRfc822,
      info.id,
      info.receivedAtEpoch,
      false
    );
    return { info, matching, result };
  } catch (err) {
    if (!(err instanceof MailsiftHttpError)) {
      throw err;
    }
    console.error(`Uploading Gmail message ${info.id} failed: ${err.message}`);
    return { info, matching, error: err };
  }
}

/**
 * Read the status of the current or most recent sync run, or `null`.
 *
 * @return {Object|null} `{ state: 'running'|'finished'|'failed',
 *   updatedAt, total?, handled?, scanned?, matched?, uploaded?, error? }`
 */
function getSyncStatus() {
  const raw = CacheService.getUserCache().get(CACHE_SYNC_STATUS_KEY);
  return raw ? JSON.parse(raw) : null;
}

/**
 * @param {Object} status see `getSyncStatus`
 */
function saveSyncStatus(status) {
  status.updatedAt = Date.now();
  CacheService.getUserCache().put(
    CACHE_SYNC_STATUS_KEY,
    JSON.stringify(status),
    CACHE_MAX_TTL_SECONDS
  );
}

/**
 * Whether `status` describes a run that is still going, ignoring ones
 * that have gone quiet for too long to still be alive.
 *
 * @param {Object|null} status
 * @return {boolean}
 */
function isSyncRunning(status) {
  return Boolean(
    status &&
      status.state === 'running' &&
      Date.now() - status.updatedAt < SYNC_STALE_MS
  );
}

/**
 * Run a sync with the card action budget and summarise it for a
 * notification. Errors are recorded in the sync status by `runSync`.
 *
 * @return {string}
 */
function syncFromCard() {
  let outcome;
  try {
    outcome = runSync(UI_SYNC_BUDGET_MS);
  } catch (err) {
    return `Sync failed: ${err.message}`;
  }
  if (outcome === null) {
    return 'A sync is already running.';
  }
  if (outcome.problems.length > 0) {
    return 'Sync finished with errors.';
  }
  if (outcome.remaining > 0) {
    return `${outcome.remaining} message(s) left to sync.`;
  }
  return 'Sync finished.';
}

/**
 * "Start Sync" action: sync as much as fits in a card action and show
 * the result on the homepage. Pressing it again continues where it
 * stopped, as does the hourly sync.
 *
 * @return {GoogleAppsScript.Card_Service.ActionResponse}
 */
function onSyncNow() {
  const note = syncFromCard();
  return CardService.newActionResponseBuilder()
    .setNavigation(CardService.newNavigation().updateCard(onHomepage()))
    .setNotification(CardService.newNotification().setText(note))
    .build();
}

/**
 * Universal action wrapper for `onSyncNow`.
 *
 * @return {GoogleAppsScript.Card_Service.UniversalActionResponse}
 */
function onSyncNowUniversal() {
  syncFromCard();
  return CardService.newUniversalActionResponseBuilder()
    .displayAddOnCards([onHomepage()])
    .build();
}

/**
 * Universal action wrapper for `onSettings`.
 *
 * @return {GoogleAppsScript.Card_Service.UniversalActionResponse}
 */
function onSettingsUniversal() {
  return CardService.newUniversalActionResponseBuilder()
    .displayAddOnCards([onSettings()])
    .build();
}

/**
 * "Refresh" action: re-render the homepage with the latest sync status.
 *
 * @return {GoogleAppsScript.Card_Service.ActionResponse}
 */
function onRefreshHomepage() {
  return CardService.newActionResponseBuilder()
    .setNavigation(CardService.newNavigation().updateCard(onHomepage()))
    .build();
}

/**
 * Describe `status` for the homepage as `{ text, detail }`.
 *
 * @param {Object|null} status
 * @return {{text: string, detail: string}}
 */
function describeSyncStatus(status) {
  if (!status) {
    return { text: 'No recent sync', detail: '' };
  }
  const counts = `Scanned ${status.scanned} · Matched ${status.matched} · Uploaded ${status.uploaded}`;
  const ago = Math.round((Date.now() - status.updatedAt) / 60000);
  const when = ago < 1 ? 'just now' : `${ago} min ago`;
  switch (status.state) {
    case 'running':
      if (!isSyncRunning(status)) {
        return {
          text: 'Sync stopped responding',
          detail: `Last update ${when}; the run was probably killed.`,
        };
      }
      return {
        text: renderProgressBar(status.handled, status.total),
        detail: counts,
      };
    case 'finished': {
      const remaining = status.total - status.handled;
      if (remaining > 0) {
        return {
          text: `Synced ${status.handled} of ${status.total} message(s) ${when}`,
          detail: `${counts}\nPress Start Sync to continue; otherwise the next hourly sync does.`,
        };
      }
      return { text: `Sync finished ${when}`, detail: counts };
    }
    case 'failed':
      return {
        text: `Sync failed ${when}`,
        detail:
          status.scanned === undefined
            ? status.error
            : `${counts}\n${status.error}`,
      };
    default:
      throw new Error(`Unknown sync state ${status.state}`);
  }
}

/**
 * Read the background sync cursor: the unix time (seconds) up to which
 * mail has been synced, or `null` before the first run.
 *
 * @param {GoogleAppsScript.Properties.Properties} props
 * @return {number|null}
 */
function loadSyncCursor(props) {
  const raw = props.getProperty(PROP_SYNC_CURSOR);
  if (raw === null) {
    return null;
  }
  if (!/^\d+$/.test(raw)) {
    throw new Error(
      `Invalid ${PROP_SYNC_CURSOR} property: ${JSON.stringify(raw)}`
    );
  }
  return Number(raw);
}

/**
 * Read the messages whose upload failed in an earlier background run.
 *
 * @param {GoogleAppsScript.Properties.Properties} props
 * @return {Array<{id: string, error: string}>}
 */
function loadSyncRetries(props) {
  const raw = props.getProperty(PROP_SYNC_RETRIES);
  if (raw === null) {
    return [];
  }
  const retries = JSON.parse(raw);
  if (!Array.isArray(retries)) {
    throw new Error(`Invalid ${PROP_SYNC_RETRIES} property: ${raw}`);
  }
  return retries;
}

/**
 * List the messages in threads matching `query` that were received after
 * `sinceEpoch`.
 *
 * @param {string} query
 * @param {number} sinceEpoch unix seconds
 * @return {Array<{message: GoogleAppsScript.Gmail.GmailMessage, date: number}>}
 */
function listMessagesSince(query, sinceEpoch) {
  const threads = [];
  for (let start = 0; ; start += SEARCH_PAGE_SIZE) {
    const page = GmailApp.search(query, start, SEARCH_PAGE_SIZE);
    threads.push(...page);
    if (page.length < SEARCH_PAGE_SIZE) {
      break;
    }
  }
  const entries = [];
  for (const messages of GmailApp.getMessagesForThreads(threads)) {
    for (const message of messages) {
      const date = message.getDate().getTime() / 1000;
      if (date > sinceEpoch) {
        entries.push({ message, date });
      }
    }
  }
  return entries;
}

/**
 * Time-driven trigger entrypoint for the hourly sync.
 *
 * Throws if any message failed, so the execution is marked failed in the
 * Apps Script log. Add-on triggers don't send failure emails; the
 * homepage's sync status is where failures show up.
 *
 * @return {Object|null} see `runSync`
 */
function syncMatchingEmails() {
  const outcome = runSync(SYNC_MAX_DURATION_MS);
  if (outcome && outcome.problems.length > 0) {
    throw new Error(outcome.problems.join('\n'));
  }
  return outcome;
}

/**
 * Sync mail received since the cursor saved by the previous run (at most
 * `SYNC_LOOKBACK_SECONDS` back), oldest first, for up to `budgetMs`.
 *
 * Progress (the cursor, checked messages and retry list) is saved after
 * every message, so a run that is stopped early or killed is continued
 * by the next one. Labelled threads are searched too, so replies to them
 * aren't missed. Messages the server rejects are kept in a retry list and
 * tried again by later runs until they fall out of the lookback window.
 *
 * Only one run happens at a time; a run that finds another in progress
 * leaves the work to it.
 *
 * @param {number} budgetMs stop starting new messages after this long
 * @return {Object|null} `{ scanned, matched, uploaded, remaining,
 *   problems }`, or `null` if another run was in progress
 */
function runSync(budgetMs) {
  const lock = LockService.getUserLock();
  if (!lock.tryLock(0)) {
    console.log('Another sync run is in progress; leaving the work to it.');
    return null;
  }
  try {
    return syncWithLock(budgetMs);
  } catch (err) {
    saveSyncStatus(
      Object.assign({}, getSyncStatus(), { state: 'failed', error: err.message })
    );
    throw err;
  } finally {
    lock.releaseLock();
  }
}

/**
 * The body of `runSync`, run while holding its lock.
 *
 * @param {number} budgetMs
 * @return {Object} see `runSync`
 */
function syncWithLock(budgetMs) {
  const startMs = Date.now();
  const startEpoch = Math.floor(startMs / 1000);
  const oldestEpoch = startEpoch - SYNC_LOOKBACK_SECONDS;
  const settings = getSettings();
  const props = PropertiesService.getUserProperties();
  const extractors = getExtractors();
  const cursor = Math.max(loadSyncCursor(props) || 0, oldestEpoch);
  const sinceEpoch = cursor - SYNC_OVERLAP_SECONDS;
  const query = buildGmailSearchQuery(extractors, { afterEpoch: sinceEpoch });

  const entries = listMessagesSince(query, sinceEpoch);
  const listed = new Set(entries.map((e) => e.message.getId()));
  const abandoned = [];
  const retries = [];
  for (const retry of loadSyncRetries(props)) {
    if (listed.has(retry.id)) {
      retries.push(retry);
      continue;
    }
    const message = GmailApp.getMessageById(retry.id);
    if (!message) {
      abandoned.push(`${retry.id}: message no longer exists`);
      continue;
    }
    const date = message.getDate().getTime() / 1000;
    if (date < oldestEpoch) {
      abandoned.push(`${retry.id}: ${retry.error}`);
      continue;
    }
    retries.push(retry);
    entries.push({ message, date });
  }
  entries.sort((a, b) => a.date - b.date);

  const checkedIds = loadCheckedMessageIds();
  const handledIds = new Set();
  const failed = [];
  const labelledThreads = new Set();
  let newCursor = cursor;
  const persist = () => {
    saveCheckedMessageIds(checkedIds);
    props.setProperty(PROP_SYNC_CURSOR, String(newCursor));
    // Retries this run hasn't got to yet stay on the list.
    const pending = retries.filter((r) => !handledIds.has(r.id));
    props.setProperty(PROP_SYNC_RETRIES, JSON.stringify(pending.concat(failed)));
  };
  const status = {
    state: 'running',
    total: entries.length,
    handled: 0,
    scanned: 0,
    matched: 0,
    uploaded: 0,
  };
  saveSyncStatus(status);

  try {
    for (const { message, date } of entries) {
      if (status.handled > 0 && Date.now() - startMs > budgetMs) {
        break;
      }
      const msgId = message.getId();
      status.handled++;
      const isNew = !checkedIds.has(msgId);
      if (isNew) {
        status.scanned++;
        const { matching, result, error } = syncMessage(extractors, message);
        if (matching.length > 0) {
          status.matched++;
        }
        if (error) {
          failed.push({ id: msgId, error: error.message });
        } else {
          checkedIds.add(msgId);
          if (result) {
            status.uploaded++;
            const thread = message.getThread();
            if (!labelledThreads.has(thread.getId())) {
              markThreadProcessed(thread, settings.processedLabel);
              labelledThreads.add(thread.getId());
            }
          }
        }
      }
      handledIds.add(msgId);
      newCursor = Math.max(newCursor, Math.floor(date));
      if (isNew || status.handled % 20 === 0) {
        persist();
        saveSyncStatus(status);
      }
    }
    if (status.handled === entries.length) {
      newCursor = Math.max(newCursor, startEpoch);
    }
  } finally {
    persist();
  }

  const problems = [];
  if (failed.length > 0) {
    const lines = failed.map((r) => `${r.id}: ${r.error}`);
    problems.push(
      `${failed.length} message(s) failed to upload:\n${lines.join('\n')}`
    );
  }
  if (abandoned.length > 0) {
    const days = SYNC_LOOKBACK_SECONDS / 86400;
    problems.push(
      `Gave up on ${abandoned.length} message(s) older than the ` +
        `${days}-day lookback:\n${abandoned.join('\n')}`
    );
  }

  status.state = problems.length > 0 ? 'failed' : 'finished';
  status.error = problems.join('\n');
  saveSyncStatus(status);
  return {
    scanned: status.scanned,
    matched: status.matched,
    uploaded: status.uploaded,
    remaining: entries.length - status.handled,
    problems,
  };
}

/**
 * Render the Settings card.
 *
 * @return {GoogleAppsScript.Card_Service.Card}
 */
function onSettings() {
  const settings = getSettings();
  const hasTrigger = ScriptApp.getProjectTriggers().some(
    (t) => t.getHandlerFunction() === 'syncMatchingEmails'
  );

  const section = CardService.newCardSection()
    .addWidget(
      CardService.newTextInput()
        .setFieldName('serverUrl')
        .setTitle('Mailsift Server URL')
        .setHint('https://mailsift.example.org')
        .setValue(settings.serverUrl)
    )
    .addWidget(
      CardService.newTextInput()
        .setFieldName('authHeader')
        .setTitle('Authorization Header / Token (optional)')
        .setHint('Bearer <token> or Basic <credentials>')
        .setValue(settings.authHeader)
    )
    .addWidget(
      CardService.newTextInput()
        .setFieldName('processedLabel')
        .setTitle('Processed Gmail Label')
        .setValue(settings.processedLabel)
    )
    .addWidget(
      CardService.newDecoratedText()
        .setText('Auto-upload matching emails on open')
        .setSwitchControl(
          CardService.newSwitch()
            .setFieldName('autoUpload')
            .setValue('true')
            .setSelected(settings.autoUpload)
        )
    )
    .addWidget(
      CardService.newDecoratedText()
        .setText('Hourly background sync trigger')
        .setSwitchControl(
          CardService.newSwitch()
            .setFieldName('hourlySync')
            .setValue('true')
            .setSelected(hasTrigger)
        )
    )
    .addWidget(
      CardService.newTextButton()
        .setText('Save Settings')
        .setTextButtonStyle(CardService.TextButtonStyle.FILLED)
        .setOnClickAction(
          CardService.newAction().setFunctionName('onSaveSettings')
        )
    );

  return CardService.newCardBuilder()
    .setHeader(CardService.newCardHeader().setTitle('Mailsift Settings'))
    .addSection(section)
    .build();
}

/**
 * Save user settings and configure the background hourly trigger.
 * Returns immediately to the homepage card without blocking on a network
 * fetch so saving settings is instant.
 *
 * @param {Object} e
 * @return {GoogleAppsScript.Card_Service.ActionResponse}
 */
function onSaveSettings(e) {
  const inputs = (e && e.formInput) || {};
  const serverUrl = (inputs.serverUrl || '').trim().replace(/\/+$/, '');
  const authHeader = (inputs.authHeader || '').trim();
  const processedLabel =
    (inputs.processedLabel || '').trim() || DEFAULT_PROCESSED_LABEL;
  const autoUpload = Boolean(inputs.autoUpload);
  const hourlySync = Boolean(inputs.hourlySync);

  const props = PropertiesService.getUserProperties();
  props.setProperties({
    [PROP_SERVER_URL]: serverUrl,
    [PROP_AUTH_HEADER]: authHeader,
    [PROP_PROCESSED_LABEL]: processedLabel,
    [PROP_AUTO_UPLOAD]: autoUpload ? 'true' : 'false',
  });

  CacheService.getUserCache().remove(CACHE_EXTRACTORS_KEY);

  const existing = ScriptApp.getProjectTriggers().filter(
    (t) => t.getHandlerFunction() === 'syncMatchingEmails'
  );
  if (hourlySync && existing.length === 0) {
    ScriptApp.newTrigger('syncMatchingEmails').timeBased().everyHours(1).create();
  } else if (!hourlySync && existing.length > 0) {
    for (const t of existing) {
      ScriptApp.deleteTrigger(t);
    }
  }

  return CardService.newActionResponseBuilder()
    .setNavigation(
      CardService.newNavigation().updateCard(
        onHomepage('Settings saved.')
      )
    )
    .setNotification(
      CardService.newNotification().setText('Mailsift settings saved.')
    )
    .build();
}

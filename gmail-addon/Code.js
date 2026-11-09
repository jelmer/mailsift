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
const CACHE_SYNC_STATE_KEY = 'MAILSIFT_SYNC_STATE_V2';
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
// Stop starting new messages after this long. Card actions are killed at
// 30s, and one message (Gmail fetches plus the server running the
// extraction pipeline) has to fit in the remainder.
const UI_BATCH_MAX_DURATION_MS = 10000;
const UI_SYNC_MAX_THREADS = 50;

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

  section.addWidget(
    CardService.newButtonSet()
      .addButton(
        CardService.newTextButton()
          .setText('Start Sync')
          .setTextButtonStyle(CardService.TextButtonStyle.FILLED)
          .setOnClickAction(
            CardService.newAction()
              .setFunctionName('onSyncNow')
              .setLoadIndicator(CardService.LoadIndicator.SPINNER)
          )
      )
  );

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

  if (uploadResult || matching.length > 0) {
    const extractedSection = buildExtractedInfoSection(
      settings,
      info,
      matching,
      uploadResult
    );
    if (extractedSection) {
      card.addSection(extractedSection);
    }
  }

  return card.build();
}

/**
 * Build a CardSection showing the extracted artifact details and links
 * to Mailsift for the current email.
 *
 * @param {Object} settings
 * @param {Object} info
 * @param {Array<Object>} matching
 * @param {Object|null} uploadResult
 * @return {GoogleAppsScript.Card_Service.CardSection|null}
 */
function buildExtractedInfoSection(settings, info, matching, uploadResult) {
  const section = CardService.newCardSection().setHeader('Extracted in Mailsift');
  let addedWidgets = 0;

  if (uploadResult && Array.isArray(uploadResult.matched)) {
    section.addWidget(
      CardService.newKeyValue()
        .setTopLabel('Extraction Result')
        .setContent(formatUploadOutcome(uploadResult))
    );
    addedWidgets++;
  }

  const kindMeta = [
    {
      key: 'events',
      label: 'Calendar Events',
      listPath: '/events',
      apiPath: null,
    },
    {
      key: 'reservations',
      label: 'Reservations',
      listPath: '/reservations',
      apiPath: '/api/reservations.json',
      viewPath: (item) =>
        item._year && item._slug
          ? `/reservations/${encodeURIComponent(item._year)}/${encodeURIComponent(item._slug)}/view`
          : '/reservations',
      formatItem: (item) => {
        const provider = item.provider || item.reservationFor || 'Reservation';
        const ref = item.reservationNumber || item.identifier || '';
        const status = item.reservationStatus || '';
        return {
          title: ref ? `${provider} (${ref})` : String(provider),
          subtitle: status ? `Status: ${status}` : '',
        };
      },
    },
    {
      key: 'bills',
      label: 'Bills',
      listPath: '/bills',
      apiPath: '/api/bills.json',
      viewPath: (item) =>
        item._year && item._slug
          ? `/bills/${encodeURIComponent(item._year)}/${encodeURIComponent(item._slug)}/view`
          : '/bills',
      formatItem: (item) => {
        const payee = item.payee || item.provider || 'Bill';
        const amt = formatMoney(item.totalPaymentDue || item.amount, item.currency);
        const due = item.paymentDueDate ? `Due ${String(item.paymentDueDate).slice(0, 10)}` : '';
        return {
          title: amt ? `${payee} — ${amt}` : String(payee),
          subtitle: [item.invoiceNumber, due].filter(Boolean).join(' · '),
        };
      },
    },
    {
      key: 'parcels',
      label: 'Parcels',
      listPath: '/parcels',
      apiPath: '/api/parcels.json',
      viewPath: (item) =>
        item.trackingNumber
          ? `/parcels?q=${encodeURIComponent(item.trackingNumber)}`
          : '/parcels',
      formatItem: (item) => {
        const carrier = item.carrier || 'Parcel';
        const tn = item.trackingNumber || '';
        const status = item.status || item.deliveryStatus || '';
        return {
          title: tn ? `${carrier}: ${tn}` : String(carrier),
          subtitle: [item.sender, status].filter(Boolean).join(' · '),
        };
      },
    },
    {
      key: 'receipts',
      label: 'Receipts',
      listPath: '/receipts',
      apiPath: '/api/receipts.json',
      viewPath: (item) =>
        item._year && item._slug
          ? `/receipts/${encodeURIComponent(item._year)}/${encodeURIComponent(item._slug)}/view`
          : '/receipts',
      formatItem: (item) => {
        const merchant = item.merchant || item.seller || item.provider || 'Receipt';
        const amt = formatMoney(item.total || item.totalPrice || item.amount, item.currency);
        return {
          title: amt ? `${merchant} — ${amt}` : String(merchant),
          subtitle: [item.orderNumber, item.date ? String(item.date).slice(0, 10) : '']
            .filter(Boolean)
            .join(' · '),
        };
      },
    },
    {
      key: 'tickets',
      label: 'Tickets',
      listPath: '/tickets',
      apiPath: null,
    },
    {
      key: 'subscriptions',
      label: 'Subscriptions',
      listPath: '/subscriptions',
      apiPath: '/api/subscriptions.json',
      viewPath: (item) =>
        item.provider
          ? `/subscriptions?q=${encodeURIComponent(item.provider)}`
          : '/subscriptions',
      formatItem: (item) => {
        const provider = item.provider || item.name || 'Subscription';
        const amt = formatMoney(item.price || item.amount, item.currency);
        return {
          title: amt ? `${provider} — ${amt}` : String(provider),
          subtitle: [item.billingPeriod, item.nextBillingDate]
            .filter(Boolean)
            .join(' · '),
        };
      },
    },
  ];

  const activeKinds = new Set();
  if (uploadResult && Array.isArray(uploadResult.matched)) {
    for (const m of uploadResult.matched) {
      for (const km of kindMeta) {
        if (m[km.key] > 0) {
          activeKinds.add(km.key);
        }
      }
    }
  }

  for (const km of kindMeta) {
    if (!activeKinds.has(km.key)) {
      continue;
    }

    let items = [];
    if (km.apiPath) {
      items = fetchMatchingArtifacts(settings, km.apiPath, info, matching);
    }

    if (items.length > 0 && km.formatItem) {
      for (const item of items.slice(0, 3)) {
        const formatted = km.formatItem(item);
        const targetUrl = settings.serverUrl + km.viewPath(item);
        const widget = CardService.newDecoratedText()
          .setTopLabel(km.label)
          .setText(formatted.title)
          .setOpenLink(CardService.newOpenLink().setUrl(targetUrl));
        if (formatted.subtitle) {
          widget.setBottomLabel(formatted.subtitle);
        }
        section.addWidget(widget);
        addedWidgets++;
      }
    } else {
      const targetUrl = settings.serverUrl + km.listPath;
      section.addWidget(
        CardService.newDecoratedText()
          .setTopLabel(km.label)
          .setText(`View ${km.label.toLowerCase()} in Mailsift`)
          .setOpenLink(CardService.newOpenLink().setUrl(targetUrl))
      );
      addedWidgets++;
    }
  }

  const buttonSet = CardService.newButtonSet().addButton(
    CardService.newTextButton()
      .setText('Open Mailsift Dashboard')
      .setOpenLink(CardService.newOpenLink().setUrl(settings.serverUrl + '/all'))
  );
  section.addWidget(buttonSet);
  addedWidgets++;

  return addedWidgets > 0 ? section : null;
}

/**
 * Format a numeric or object money value with optional currency.
 *
 * @param {*} amount
 * @param {string=} currency
 * @return {string}
 */
function formatMoney(amount, currency) {
  if (amount === null || amount === undefined || amount === '') {
    return '';
  }
  if (typeof amount === 'object') {
    const val = amount.value || amount.price || amount.amount;
    const cur = amount.currency || amount.priceCurrency || currency || '';
    return val !== undefined ? `${val} ${cur}`.trim() : '';
  }
  return currency ? `${amount} ${currency}` : String(amount);
}

/**
 * Fetch artifacts from a Mailsift `/api/<kind>.json` endpoint that
 * correspond to the current email's received date or extractor name.
 *
 * @param {Object} settings
 * @param {string} apiPath
 * @param {Object} info
 * @param {Array<Object>} matching
 * @return {Array<Object>}
 */
function fetchMatchingArtifacts(settings, apiPath, info, matching) {
  try {
    const datePrefix = info.receivedAtEpoch
      ? new Date(info.receivedAtEpoch * 1000).toISOString().slice(0, 10)
      : null;
    const yearParam = datePrefix ? `?year=${datePrefix.slice(0, 4)}` : '';
    const url = settings.serverUrl + apiPath + yearParam;
    const resp = UrlFetchApp.fetch(url, {
      method: 'get',
      headers: buildHeaders(settings, { Accept: 'application/json' }),
      muteHttpExceptions: true,
    });
    if (resp.getResponseCode() !== 200) {
      return [];
    }
    const all = JSON.parse(resp.getContentText());
    if (!Array.isArray(all)) {
      return [];
    }
    if (datePrefix) {
      const byDate = all.filter(
        (item) =>
          typeof item.receivedAt === 'string' &&
          item.receivedAt.startsWith(datePrefix)
      );
      if (byDate.length > 0) {
        return byDate;
      }
    }
    const extractorNames = matching.map((m) => m.name.toLowerCase());
    if (extractorNames.length > 0) {
      const bySlug = all.filter((item) => {
        const text = JSON.stringify(item).toLowerCase();
        return extractorNames.some((n) => text.includes(n));
      });
      if (bySlug.length > 0) {
        return bySlug;
      }
    }
    return all.slice(0, 2);
  } catch (_) {
    return [];
  }
}

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
    if (!dryRun) {
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
 * Load or initialize interactive sync session state from `CacheService`.
 *
 * @param {boolean=} reset
 * @return {Object}
 */
function getSyncState(reset) {
  if (!reset) {
    try {
      const raw = CacheService.getUserCache().get(CACHE_SYNC_STATE_KEY);
      if (raw) {
        return JSON.parse(raw);
      }
    } catch (_) {}
  }
  return {
    offset: 0,
    totalThreads: 0,
    threadsDone: 0,
    scannedMessages: 0,
    matchedMessages: 0,
    uploadedMessages: 0,
    recentFinds: [],
    failures: [],
    failedIds: [],
    currentThread: null,
    done: false,
  };
}

/**
 * Save interactive sync session state to `CacheService`.
 *
 * @param {Object} state
 */
function saveSyncState(state) {
  try {
    CacheService.getUserCache().put(
      CACHE_SYNC_STATE_KEY,
      JSON.stringify(state),
      1800
    );
  } catch (_) {}
}

/**
 * Run one time-bounded interactive sync step and return
 * an updated Progress Card showing a visual progress bar, live counters,
 * and a list of matched messages with links to Mailsift.
 *
 * @param {Object=} e
 * @return {GoogleAppsScript.Card_Service.ActionResponse}
 */
function onSyncNow(e) {
  const reset = getActionParameters(e).continueBatch !== 'true';
  try {
    const state = runInteractiveSyncStep(reset);
    const card = buildSyncProgressCard(state);
    return CardService.newActionResponseBuilder()
      .setNavigation(CardService.newNavigation().updateCard(card))
      .build();
  } catch (err) {
    return CardService.newActionResponseBuilder()
      .setNotification(
        CardService.newNotification().setText(`Sync failed: ${err.message}`)
      )
      .build();
  }
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
 * Execute one bounded step of interactive sync and update `SyncState`.
 *
 * Card actions are killed after 30 seconds, losing anything not yet
 * saved, so the time budget is checked before every message rather than
 * every thread. A thread that doesn't fit is resumed in the next step;
 * `checkedIds` skips the messages already handled.
 *
 * @param {boolean} reset
 * @return {Object} updated sync state
 */
function runInteractiveSyncStep(reset) {
  const startMs = Date.now();
  const settings = getSettings();
  const extractors = getExtractors();
  const query = buildGmailSearchQuery(extractors, {
    newerThanDays: 7,
    excludeLabel: settings.processedLabel,
  });

  const state = getSyncState(reset);
  const checkedIds = loadCheckedMessageIds();

  if (reset || state.totalThreads === 0) {
    const allCandidateThreads = GmailApp.search(query, 0, UI_SYNC_MAX_THREADS);
    state.totalThreads = allCandidateThreads.length;
    state.offset = 0;
    state.threadsDone = 0;
  }

  // How many threads fit in this step is up to the time budget.
  const batchThreads = GmailApp.search(
    query,
    state.offset,
    Math.max(state.totalThreads - state.threadsDone, 1)
  );
  if (batchThreads.length === 0) {
    state.done = true;
    state.threadsDone = state.totalThreads;
    saveSyncState(state);
    return state;
  }

  const failedIds = new Set(state.failedIds);
  // Always handle at least one message, even if setup used up the budget.
  let handledAny = false;
  try {
    for (const thread of batchThreads) {
      const threadId = thread.getId();
      if (!state.currentThread || state.currentThread.id !== threadId) {
        state.currentThread = { id: threadId, matched: false, failed: false };
      }
      let finished = true;
      for (const message of thread.getMessages()) {
        const msgId = message.getId();
        if (checkedIds.has(msgId) || failedIds.has(msgId)) {
          continue;
        }
        if (handledAny && Date.now() - startMs > UI_BATCH_MAX_DURATION_MS) {
          finished = false;
          break;
        }
        handledAny = true;
        state.scannedMessages++;
        const { info, matching, result, error } = syncMessage(
          extractors,
          message
        );
        const subject = info.subject || '(no subject)';
        const names = matching.map((m) => m.name).join(', ');
        if (matching.length > 0) {
          state.matchedMessages++;
        }
        if (error) {
          failedIds.add(msgId);
          state.currentThread.failed = true;
          state.failures.unshift({ subject, extractors: names, error: error.message });
          state.failures = state.failures.slice(0, 8);
          continue;
        }
        checkedIds.add(msgId);
        if (result) {
          state.uploadedMessages++;
          state.currentThread.matched = true;
          state.recentFinds.unshift({
            subject,
            extractors: names,
            outcome: formatUploadOutcome(result),
          });
          state.recentFinds = state.recentFinds.slice(0, 8);
        }
      }
      if (!finished) {
        break;
      }
      // A labelled thread drops out of the search results, so only
      // advance the offset past threads that stay in them. Threads with
      // a failed upload stay unlabelled so a later sync retries them.
      if (state.currentThread.matched && !state.currentThread.failed) {
        markThreadProcessed(thread, settings.processedLabel);
      } else {
        state.offset++;
      }
      state.currentThread = null;
      state.threadsDone = Math.min(state.threadsDone + 1, state.totalThreads);
    }

    if (state.threadsDone >= state.totalThreads) {
      state.done = true;
    }
  } finally {
    state.failedIds = Array.from(failedIds);
    saveCheckedMessageIds(checkedIds);
    saveSyncState(state);
  }
  return state;
}

/**
 * Build the visual Sync Progress Card showing a progress bar, live
 * counters, extracted items, and a "Next Batch" button.
 *
 * @param {Object} state
 * @return {GoogleAppsScript.Card_Service.Card}
 */
function buildSyncProgressCard(state) {
  const settings = getSettings();
  const bar = renderProgressBar(state.threadsDone, state.totalThreads);
  const statusTitle = state.done ? 'Sync Complete' : 'Sync in Progress';

  const card = CardService.newCardBuilder().setHeader(
    CardService.newCardHeader()
      .setTitle('Mailsift Sync')
      .setSubtitle(statusTitle)
  );

  const progressSection = CardService.newCardSection()
    .setHeader('Progress')
    .addWidget(
      CardService.newDecoratedText()
        .setTopLabel('Threads Processed')
        .setText(bar)
        .setBottomLabel(
          `Scanned ${state.scannedMessages} msg(s) · Matched ${state.matchedMessages} · Uploaded ${state.uploadedMessages}`
        )
    );

  const buttons = CardService.newButtonSet();
  if (!state.done && state.totalThreads > state.threadsDone) {
    const remaining = state.totalThreads - state.threadsDone;
    buttons.addButton(
      CardService.newTextButton()
        .setText(`Sync Next Batch (${remaining} left)`)
        .setTextButtonStyle(CardService.TextButtonStyle.FILLED)
        .setOnClickAction(
          CardService.newAction()
            .setFunctionName('onSyncNow')
            .setParameters({ continueBatch: 'true' })
            .setLoadIndicator(CardService.LoadIndicator.SPINNER)
        )
    );
  } else {
    buttons.addButton(
      CardService.newTextButton()
        .setText('Rescan')
        .setTextButtonStyle(CardService.TextButtonStyle.FILLED)
        .setOnClickAction(
          CardService.newAction()
            .setFunctionName('onSyncNow')
            .setParameters({ continueBatch: 'false' })
            .setLoadIndicator(CardService.LoadIndicator.SPINNER)
        )
    );
  }
  buttons.addButton(
    CardService.newTextButton()
      .setText('Back')
      .setOnClickAction(
        CardService.newAction().setFunctionName('onBackToHome')
      )
  );
  progressSection.addWidget(buttons);
  card.addSection(progressSection);

  if (state.recentFinds && state.recentFinds.length > 0) {
    const findsSection = CardService.newCardSection().setHeader(
      'Matched & Uploaded'
    );
    for (const item of state.recentFinds) {
      findsSection.addWidget(
        CardService.newDecoratedText()
          .setTopLabel(item.extractors)
          .setText(item.subject)
          .setBottomLabel(item.outcome)
          .setOpenLink(
            CardService.newOpenLink().setUrl(settings.serverUrl + '/all')
          )
      );
    }
    card.addSection(findsSection);
  }

  if (state.failedIds.length > 0) {
    const failuresSection = CardService.newCardSection().setHeader(
      `Failed Uploads (${state.failedIds.length}, retried on rescan)`
    );
    for (const item of state.failures) {
      failuresSection.addWidget(
        CardService.newDecoratedText()
          .setTopLabel(item.extractors)
          .setText(item.subject)
          .setBottomLabel(item.error)
          .setWrapText(true)
      );
    }
    card.addSection(failuresSection);
  }

  return card.build();
}

/**
 * Navigate back to the Homepage card.
 *
 * @return {GoogleAppsScript.Card_Service.ActionResponse}
 */
function onBackToHome() {
  return CardService.newActionResponseBuilder()
    .setNavigation(CardService.newNavigation().updateCard(onHomepage()))
    .build();
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
 * Background time-driven trigger entrypoint.
 *
 * Works through mail received since the cursor saved by the previous run
 * (at most `SYNC_LOOKBACK_SECONDS` back), oldest first, and moves the
 * cursor up to the last message handled, so a run that hits the time
 * budget is continued by the next one. Labelled threads are searched
 * too, so replies to them aren't missed.
 *
 * Messages the server rejects are kept in a retry list and tried again
 * by later runs until they fall out of the lookback window. The run
 * throws after finishing if anything failed, so failures show up in the
 * trigger's execution log and failure emails.
 *
 * @return {Object} `{ scanned, matched, uploaded, timedOut }`
 */
function syncMatchingEmails() {
  const maxDurationMs = 240000;
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
  let retries = [];
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
  let scanned = 0;
  let matchedCount = 0;
  let uploadedCount = 0;
  let timedOut = false;

  try {
    for (const { message, date } of entries) {
      if (Date.now() - startMs > maxDurationMs) {
        timedOut = true;
        break;
      }
      const msgId = message.getId();
      if (!checkedIds.has(msgId)) {
        scanned++;
        const { matching, result, error } = syncMessage(extractors, message);
        if (matching.length > 0) {
          matchedCount++;
        }
        if (error) {
          failed.push({ id: msgId, error: error.message });
        } else {
          checkedIds.add(msgId);
          if (result) {
            uploadedCount++;
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
    }
    if (!timedOut) {
      newCursor = Math.max(newCursor, startEpoch);
    }
  } finally {
    saveCheckedMessageIds(checkedIds);
    props.setProperty(PROP_SYNC_CURSOR, String(newCursor));
    // Retries this run didn't get to stay on the list.
    retries = retries.filter((r) => !handledIds.has(r.id));
    props.setProperty(PROP_SYNC_RETRIES, JSON.stringify(retries.concat(failed)));
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
  if (problems.length > 0) {
    throw new Error(problems.join('\n'));
  }

  return {
    scanned,
    matched: matchedCount,
    uploaded: uploadedCount,
    timedOut,
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

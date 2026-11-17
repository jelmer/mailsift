/**
 * Google Workspace Gmail Add-on for Mailsift.
 *
 * Shows, next to an open message, what Mailsift extracted from it. The
 * message is looked up by its Message-ID with `GET <serverUrl>/api/messages`;
 * getting mail to Mailsift is left to the milter or `imap-scan --watch`.
 */

const PROP_SERVER_URL = 'MAILSIFT_SERVER_URL';
const PROP_AUTH_HEADER = 'MAILSIFT_AUTH_HEADER';

/**
 * Load user settings from `PropertiesService`.
 *
 * @return {{serverUrl: string, authHeader: string}}
 */
function getSettings() {
  const props = PropertiesService.getUserProperties();
  return {
    serverUrl: (props.getProperty(PROP_SERVER_URL) || '').replace(/\/+$/, ''),
    authHeader: (props.getProperty(PROP_AUTH_HEADER) || '').trim(),
  };
}

/**
 * Request headers for `UrlFetchApp`, with the `Authorization` header for
 * an authenticating reverse proxy when one is configured.
 *
 * @param {Object} settings
 * @return {Object}
 */
function buildHeaders(settings) {
  const headers = {};
  if (settings.authHeader) {
    headers['Authorization'] = settings.authHeader.includes(' ')
      ? settings.authHeader
      : 'Bearer ' + settings.authHeader;
  }
  return headers;
}

/**
 * Ask Mailsift what it extracted from the message with `messageId`.
 *
 * @param {Object} settings
 * @param {string} messageId the `Message-ID` header
 * @return {Object|null} the `GET /api/messages` response, or `null` if no
 *   extractor ran on the message
 */
function lookupMessage(settings, messageId) {
  const url =
    `${settings.serverUrl}/api/messages?message_id=` +
    encodeURIComponent(messageId);
  const resp = UrlFetchApp.fetch(url, {
    headers: buildHeaders(settings),
    muteHttpExceptions: true,
  });
  const code = resp.getResponseCode();
  if (code === 404) {
    return null;
  }
  if (code !== 200) {
    throw new Error(`Mailsift returned HTTP ${code}`);
  }
  return JSON.parse(resp.getContentText());
}

/**
 * Homepage card, shown when no message is open.
 *
 * @return {GoogleAppsScript.Card_Service.Card}
 */
function onHomepage() {
  const settings = getSettings();
  if (!settings.serverUrl) {
    return onSettings();
  }
  const section = CardService.newCardSection()
    .addWidget(
      CardService.newTextParagraph().setText(
        'Open a message to see what Mailsift extracted from it.'
      )
    )
    .addWidget(buildButtons(settings));
  return CardService.newCardBuilder()
    .setHeader(
      CardService.newCardHeader()
        .setTitle('Mailsift')
        .setSubtitle(settings.serverUrl)
    )
    .addSection(section)
    .build();
}

/**
 * Contextual trigger handler for an opened message.
 *
 * @param {Object} e Gmail contextual event object
 * @return {GoogleAppsScript.Card_Service.Card}
 */
function onGmailMessageOpen(e) {
  const settings = getSettings();
  if (!settings.serverUrl) {
    return onSettings();
  }
  GmailApp.setCurrentMessageAccessToken(e.gmail.accessToken);
  const message = GmailApp.getMessageById(e.gmail.messageId);

  return CardService.newCardBuilder()
    .setHeader(
      CardService.newCardHeader()
        .setTitle('Mailsift')
        .setSubtitle(message.getSubject() || '(no subject)')
    )
    .addSection(buildMessageSection(settings, message.getHeader('Message-ID')))
    .build();
}

/**
 * Section listing what Mailsift extracted from the message with
 * `messageId`, with links to the records in Mailsift.
 *
 * @param {Object} settings
 * @param {string} messageId the `Message-ID` header, empty if missing
 * @return {GoogleAppsScript.Card_Service.CardSection}
 */
function buildMessageSection(settings, messageId) {
  const section = CardService.newCardSection();
  const paragraph = (text) =>
    section.addWidget(CardService.newTextParagraph().setText(text));

  let entry = null;
  if (!messageId) {
    paragraph('This message has no Message-ID to look it up by.');
  } else {
    try {
      entry = lookupMessage(settings, messageId);
      if (!entry) {
        paragraph('Mailsift has not extracted anything from this message.');
      }
    } catch (err) {
      paragraph(`Could not reach Mailsift: ${err.message}`);
    }
  }

  let widgets = 0;
  for (const run of entry ? entry.extractors : []) {
    if (run.error) {
      section.addWidget(
        CardService.newDecoratedText()
          .setTopLabel(`${run.extractor} failed`)
          .setText(run.error)
          .setWrapText(true)
      );
      widgets++;
      continue;
    }
    for (const record of run.records) {
      section.addWidget(buildRecordWidget(settings, summarizeProduced(record)));
      widgets++;
    }
  }
  if (entry && widgets === 0) {
    const names = entry.extractors.map((r) => r.extractor).join(', ');
    paragraph(`${names} found nothing to extract from this message.`);
  }
  return section.addWidget(buildButtons(settings));
}

/**
 * @param {Object} settings
 * @param {Object} summary see `summarizeProduced`
 * @return {GoogleAppsScript.Card_Service.DecoratedText}
 */
function buildRecordWidget(settings, summary) {
  const widget = CardService.newDecoratedText()
    .setTopLabel(summary.label)
    .setText(summary.title)
    .setWrapText(true)
    .setOpenLink(
      CardService.newOpenLink().setUrl(settings.serverUrl + summary.path)
    );
  if (summary.subtitle) {
    widget.setBottomLabel(summary.subtitle);
  }
  // Cosmetic: leave the icon out if this Icon name isn't available.
  if (CardService.Icon[summary.icon]) {
    widget.setStartIcon(
      CardService.newIconImage().setIcon(CardService.Icon[summary.icon])
    );
  }
  return widget;
}

/**
 * @param {Object} settings
 * @return {GoogleAppsScript.Card_Service.ButtonSet}
 */
function buildButtons(settings) {
  return CardService.newButtonSet()
    .addButton(
      CardService.newTextButton()
        .setText('Open Mailsift')
        .setOpenLink(CardService.newOpenLink().setUrl(settings.serverUrl))
    )
    .addButton(
      CardService.newTextButton()
        .setText('Settings')
        .setOnClickAction(
          CardService.newAction().setFunctionName('onSettings')
        )
    );
}

/**
 * Universal action handler for "Settings".
 *
 * @return {GoogleAppsScript.Card_Service.UniversalActionResponse}
 */
function onSettingsUniversal() {
  return CardService.newUniversalActionResponseBuilder()
    .displayAddOnCards([onSettings()])
    .build();
}

/**
 * Settings card.
 *
 * @return {GoogleAppsScript.Card_Service.Card}
 */
function onSettings() {
  const settings = getSettings();
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
 * Save user settings and return to the homepage.
 *
 * @param {Object} e
 * @return {GoogleAppsScript.Card_Service.ActionResponse}
 */
function onSaveSettings(e) {
  const inputs = e.formInput || {};
  PropertiesService.getUserProperties().setProperties({
    [PROP_SERVER_URL]: (inputs.serverUrl || '').trim().replace(/\/+$/, ''),
    [PROP_AUTH_HEADER]: (inputs.authHeader || '').trim(),
  });
  return CardService.newActionResponseBuilder()
    .setNavigation(CardService.newNavigation().updateCard(onHomepage()))
    .setNotification(
      CardService.newNotification().setText('Mailsift settings saved.')
    )
    .build();
}

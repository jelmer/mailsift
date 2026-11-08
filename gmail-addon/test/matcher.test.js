const test = require('node:test');
const assert = require('node:assert/strict');
const {
  extractFromDomain,
  matchesFromDomainPattern,
  compileSubjectRegex,
  matchesFilenamePattern,
  satisfiesBodyRequirement,
  extractPassingDkimDomains,
  satisfiesDkim,
  matchMessage,
  buildGmailSearchQuery,
} = require('../Matcher.js');

test('extractFromDomain extracts lowercased domain from bare and bracketed addresses', () => {
  assert.equal(
    extractFromDomain('easyJet <donotreply@easyjet.com>'),
    'easyjet.com'
  );
  assert.equal(extractFromDomain('billing@ACME.EXAMPLE'), 'acme.example');
  assert.equal(extractFromDomain('invalid-address'), null);
});

test('matchesFromDomainPattern supports exact and wildcard domains', () => {
  assert.equal(matchesFromDomainPattern('easyjet.com', 'easyjet.com'), true);
  assert.equal(
    matchesFromDomainPattern('easyjet.com', 'mail.easyjet.com'),
    false
  );
  assert.equal(matchesFromDomainPattern('*.easyjet.com', 'easyjet.com'), true);
  assert.equal(
    matchesFromDomainPattern('*.easyjet.com', 'mail.easyjet.com'),
    true
  );
  assert.equal(
    matchesFromDomainPattern('*.easyjet.com', 'noteasyjet.com'),
    false
  );
});

test('compileSubjectRegex converts inline (?i) flag to RegExp flags', () => {
  const re = compileSubjectRegex('(?i)easyJet booking reference');
  assert.equal(re.test('Your EASYJET BOOKING REFERENCE ABC123'), true);
  assert.equal(re.test('Unrelated subject'), false);
});

test('satisfiesBodyRequirement checks html, text, mime type, and filename globs', () => {
  const bodyParts = {
    hasHtml: true,
    hasText: false,
    mimeTypes: ['text/html', 'application/pdf', 'text/calendar'],
    attachmentFilenames: ['invite.ics', 'boarding-pass-1.pdf'],
  };
  assert.equal(satisfiesBodyRequirement('html', bodyParts), true);
  assert.equal(satisfiesBodyRequirement('text', bodyParts), false);
  assert.equal(
    satisfiesBodyRequirement('attachment:text/calendar', bodyParts),
    true
  );
  assert.equal(
    satisfiesBodyRequirement('attachment:filename:*.ics', bodyParts),
    true
  );
  assert.equal(
    satisfiesBodyRequirement('attachment:filename:boarding-*', bodyParts),
    true
  );
  assert.equal(
    satisfiesBodyRequirement('attachment:filename:*.pkpass', bodyParts),
    false
  );
});

test('extractPassingDkimDomains and satisfiesDkim evaluate Authentication-Results', () => {
  const raw =
    'Authentication-Results: mx.google.com;\r\n' +
    '       dkim=pass header.i=@easyjet.com header.s=s1;\r\n' +
    '       dkim=fail header.i=@spoof.example\r\n' +
    'From: <booking@easyjet.com>\r\n\r\nBody';
  const passing = extractPassingDkimDomains(raw);
  assert.deepEqual(passing, ['easyjet.com']);
  assert.equal(satisfiesDkim(['easyjet.com'], passing, 'easyjet.com'), true);
  assert.equal(satisfiesDkim(['@from'], passing, 'mail.easyjet.com'), true);
  assert.equal(
    satisfiesDkim(['.myshopify.com'], ['shop1.myshopify.com'], 'shop1.com'),
    true
  );
  assert.equal(
    satisfiesDkim(['.myshopify.com'], ['myshopify.com'], 'myshopify.com'),
    false
  );
});

test('matchMessage filters manifests by from_domains, subject_regex, requires, and require_dkim', () => {
  const manifests = [
    {
      name: 'easyjet',
      order: 50,
      from_domains: ['easyjet.com', '*.easyjet.com'],
      subject_regex: '(?i)booking reference',
      requires: ['html'],
      require_dkim: ['easyjet.com'],
    },
    {
      name: 'dhl',
      order: 100,
      from_domains: ['dhl.com'],
    },
  ];

  const matched = matchMessage(manifests, {
    from: 'easyJet <flight@mail.easyjet.com>',
    subject: 'Your easyJet Booking Reference EZY123',
    bodyParts: {
      hasHtml: true,
      hasText: true,
      mimeTypes: ['text/html'],
      attachmentFilenames: [],
    },
    rawRfc822:
      'Authentication-Results: mx.google.com; dkim=pass header.d=easyjet.com\r\n\r\n<html></html>',
  });

  assert.equal(matched.length, 1);
  assert.equal(matched[0].name, 'easyjet');
});

test('buildGmailSearchQuery narrows from: when all extractors constrain from_domains', () => {
  const manifests = [
    { name: 'easyjet', from_domains: ['easyjet.com', '*.easyjet.com'] },
    { name: 'dhl', from_domains: ['dhl.com'] },
  ];
  assert.equal(
    buildGmailSearchQuery(manifests, {
      newerThanDays: 7,
      excludeLabel: 'Mailsift',
    }),
    'newer_than:7d -label:Mailsift from:(dhl.com OR easyjet.com)'
  );

  // Adding a catch-all extractor omits the `from:` filter.
  manifests.push({ name: 'schema-ld', from_domains: [] });
  assert.equal(
    buildGmailSearchQuery(manifests, {
      newerThanDays: 7,
      excludeLabel: 'Mailsift',
    }),
    'newer_than:7d -label:Mailsift'
  );
});

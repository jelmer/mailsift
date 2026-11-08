/**
 * Email matcher for the Mailsift Gmail Add-on.
 *
 * Evaluates extractor manifests returned by `GET /api/extractors`
 * (`from_domains`, `subject_regex`, `requires`, `require_dkim`,
 * `unsigned_before`) against Gmail messages before uploading raw RFC822
 * messages to `POST /api/messages`.
 */

/**
 * Pull the lowercased domain out of a `From:` header value.
 * Mirrors `parse_from_domain` in `src/pipeline/mod.rs`.
 *
 * @param {string|null|undefined} fromHeader
 * @return {string|null}
 */
function extractFromDomain(fromHeader) {
  if (!fromHeader || typeof fromHeader !== 'string') {
    return null;
  }
  const lt = fromHeader.lastIndexOf('<');
  const gt = fromHeader.lastIndexOf('>');
  const addr =
    lt >= 0 && gt > lt ? fromHeader.slice(lt + 1, gt) : fromHeader;
  const at = addr.lastIndexOf('@');
  if (at < 0) {
    return null;
  }
  const domain = addr.slice(at + 1).trim().toLowerCase();
  return domain.length > 0 ? domain : null;
}

/**
 * Check if `domain` (lowercased) matches a `from_domains` pattern.
 * `*.example.com` matches both `example.com` and `sub.example.com`.
 *
 * @param {string} pattern
 * @param {string} domain
 * @return {boolean}
 */
function matchesFromDomainPattern(pattern, domain) {
  const pat = pattern.trim().toLowerCase();
  const dom = domain.trim().toLowerCase();
  if (pat.startsWith('*.')) {
    const root = pat.slice(2);
    return dom === root || dom.endsWith('.' + root);
  }
  return dom === pat;
}

/**
 * Compile a Mailsift `subject_regex` (which may start with Rust inline
 * flags like `(?i)`) into a JavaScript `RegExp`.
 *
 * @param {string} rawRegex
 * @return {RegExp}
 */
function compileSubjectRegex(rawRegex) {
  let pattern = rawRegex;
  const flagSet = new Set();
  const inlineMatch = pattern.match(/^\(\?([ims]+)\)/);
  if (inlineMatch) {
    for (const ch of inlineMatch[1]) {
      flagSet.add(ch);
    }
    pattern = pattern.slice(inlineMatch[0].length);
  }
  return new RegExp(pattern, Array.from(flagSet).join(''));
}

/**
 * Match a filename against a Mailsift `attachment:filename:<pattern>`
 * glob (supports exact match, leading `*suffix`, or trailing `prefix*`).
 *
 * @param {string} pattern
 * @param {string} filename
 * @return {boolean}
 */
function matchesFilenamePattern(pattern, filename) {
  const pat = pattern.toLowerCase();
  const name = filename.toLowerCase();
  if (pat.startsWith('*')) {
    return name.endsWith(pat.slice(1));
  }
  if (pat.endsWith('*')) {
    return name.startsWith(pat.slice(0, -1));
  }
  return name === pat;
}

/**
 * Check a single `requires:` entry against a message's body summary.
 *
 * @param {string} req
 * @param {Object} bodyParts `{ hasHtml, hasText, mimeTypes, attachmentFilenames }`
 * @return {boolean}
 */
function satisfiesBodyRequirement(req, bodyParts) {
  if (req === 'html') {
    return Boolean(bodyParts.hasHtml);
  }
  if (req === 'text') {
    return Boolean(bodyParts.hasText);
  }
  if (req.startsWith('attachment:filename:')) {
    const pat = req.slice('attachment:filename:'.length);
    return (bodyParts.attachmentFilenames || []).some((fn) =>
      matchesFilenamePattern(pat, fn)
    );
  }
  if (req.startsWith('attachment:')) {
    const wanted = req.slice('attachment:'.length).toLowerCase();
    return (bodyParts.mimeTypes || []).some(
      (mt) => mt.toLowerCase() === wanted
    );
  }
  return true;
}

/**
 * Parse passing DKIM signing domains (`header.d=...`) from the topmost
 * `Authentication-Results` header in a raw RFC822 message.
 *
 * @param {string|null|undefined} rawRfc822
 * @return {string[]}
 */
function extractPassingDkimDomains(rawRfc822) {
  if (!rawRfc822 || typeof rawRfc822 !== 'string') {
    return [];
  }
  const headerEnd = rawRfc822.search(/\r?\n\r?\n/);
  const headerBlock =
    headerEnd >= 0 ? rawRfc822.slice(0, headerEnd) : rawRfc822;
  const unfolded = headerBlock.replace(/\r?\n[ \t]+/g, ' ');
  const lines = unfolded.split(/\r?\n/);
  const authHeader = lines.find((l) =>
    /^authentication-results\s*:/i.test(l)
  );
  if (!authHeader) {
    return [];
  }
  const domains = [];
  const clauses = authHeader.split(';');
  for (const clause of clauses) {
    if (!/\bdkim\s*=\s*pass\b/i.test(clause)) {
      continue;
    }
    const dMatch = clause.match(/\bheader\.(?:d|i)\s*=\s*@?([^\s;]+)/i);
    if (dMatch) {
      domains.push(dMatch[1].replace(/^["']|["']$/g, '').toLowerCase());
    }
  }
  return domains;
}

/**
 * Check whether passing DKIM domains satisfy `require_dkim`.
 * Mirrors `dkim::satisfies` in `src/dkim.rs`.
 *
 * @param {string[]} requireDkim
 * @param {string[]} passingDomains
 * @param {string|null} fromDomain
 * @return {boolean}
 */
function satisfiesDkim(requireDkim, passingDomains, fromDomain) {
  if (!requireDkim || requireDkim.length === 0) {
    return true;
  }
  if (!passingDomains || passingDomains.length === 0) {
    return false;
  }
  return requireDkim.some((reqRaw) => {
    const req = reqRaw.toLowerCase();
    if (req === '@from') {
      if (!fromDomain) {
        return false;
      }
      return passingDomains.some(
        (d) =>
          d === fromDomain ||
          d.endsWith('.' + fromDomain) ||
          fromDomain.endsWith('.' + d)
      );
    }
    if (req.startsWith('.')) {
      return passingDomains.some((d) => d.endsWith(req));
    }
    return passingDomains.some((d) => d === req);
  });
}

/**
 * Fast header-only prefilter: evaluate `from_domains` and `subject_regex`
 * without inspecting MIME bodies, attachments, or raw RFC822 content.
 *
 * @param {Array<Object>} manifests
 * @param {Object} headers `{ from: string, subject: string }`
 * @return {Array<Object>} candidate manifests whose header rules match
 */
function matchHeaders(manifests, headers) {
  const fromDomain = extractFromDomain(headers.from);
  const subject = typeof headers.subject === 'string' ? headers.subject : '';

  return manifests.filter((manifest) => {
    const fromDomains = manifest.from_domains || [];
    if (fromDomains.length > 0) {
      if (!fromDomain) {
        return false;
      }
      const domainMatched = fromDomains.some((pat) =>
        matchesFromDomainPattern(pat, fromDomain)
      );
      if (!domainMatched) {
        return false;
      }
    }

    if (manifest.subject_regex) {
      try {
        const re = compileSubjectRegex(manifest.subject_regex);
        if (!re.test(subject)) {
          return false;
        }
      } catch (_) {
        return false;
      }
    }

    return true;
  });
}

/**
 * Evaluate all discovered extractor manifests against a message summary
 * and return the matching manifests.
 *
 * @param {Array<Object>} manifests
 * @param {Object} msg
 *   - `from`: `From:` header string
 *   - `subject`: `Subject:` header string
 *   - `bodyParts`: optional `{ hasHtml, hasText, mimeTypes, attachmentFilenames }`
 *   - `rawRfc822`: optional raw RFC822 string (for DKIM inspection)
 *   - `receivedAtEpoch`: optional unix epoch seconds
 * @return {Array<Object>} matching manifests
 */
function matchMessage(manifests, msg) {
  const fromDomain = extractFromDomain(msg.from);
  const passingDkim = msg.rawRfc822
    ? extractPassingDkimDomains(msg.rawRfc822)
    : null;

  return matchHeaders(manifests, msg).filter((manifest) => {
    const requires = manifest.requires || [];
    if (requires.length > 0 && msg.bodyParts) {
      const allHold = requires.every((r) =>
        satisfiesBodyRequirement(r, msg.bodyParts)
      );
      if (!allHold) {
        return false;
      }
    }

    const requireDkim = manifest.require_dkim || [];
    if (requireDkim.length > 0 && passingDkim !== null) {
      let unsignedOk = false;
      if (manifest.unsigned_before && typeof msg.receivedAtEpoch === 'number') {
        const cutoffMs = Date.parse(manifest.unsigned_before + 'T00:00:00Z');
        if (!Number.isNaN(cutoffMs) && msg.receivedAtEpoch * 1000 < cutoffMs) {
          unsignedOk = true;
        }
      }
      if (
        !unsignedOk &&
        !satisfiesDkim(requireDkim, passingDkim, fromDomain)
      ) {
        return false;
      }
    }

    return true;
  });
}

/**
 * Build a Gmail search query to locate candidate messages for the given
 * extractor manifests.
 *
 * When every manifest declares `from_domains`, the query narrows `from:`
 * to those base domains (just like `mailsift imap-scan` narrows `UID SEARCH`).
 * When a manifest has no `from_domains` but requires an attachment (like
 * `ics.yaml`), `has:attachment` is combined via `OR` rather than scanning
 * every message in the mailbox.
 *
 * @param {Array<Object>} manifests
 * @param {Object} opts `{ newerThanDays: number, excludeLabel: string }`
 * @return {string}
 */
function buildGmailSearchQuery(manifests, opts) {
  const options = opts || {};
  const newerThanDays = options.newerThanDays || 7;
  const excludeLabel = options.excludeLabel || 'Mailsift';

  const parts = [`newer_than:${newerThanDays}d`];
  if (excludeLabel) {
    parts.push(`-label:${excludeLabel}`);
  }

  if (manifests.length > 0) {
    const roots = new Set();
    let hasUnconstrained = false;
    let needsAttachment = false;

    for (const m of manifests) {
      if (m.from_domains && m.from_domains.length > 0) {
        for (const pat of m.from_domains) {
          const root = pat.trim().toLowerCase().replace(/^\*\./, '');
          if (root) {
            roots.add(root);
          }
        }
      } else {
        const reqs = m.requires || [];
        if (reqs.some((r) => r.startsWith('attachment:'))) {
          needsAttachment = true;
        } else {
          hasUnconstrained = true;
        }
      }
    }

    const domainList = Array.from(roots).sort();
    if (!hasUnconstrained && domainList.length > 0 && domainList.length <= 40) {
      const fromClause = `from:(${domainList.join(' OR ')})`;
      if (needsAttachment) {
        parts.push(`(${fromClause} OR has:attachment)`);
      } else {
        parts.push(fromClause);
      }
    }
  }

  return parts.join(' ');
}

if (typeof module !== 'undefined' && module.exports) {
  module.exports = {
    extractFromDomain,
    matchesFromDomainPattern,
    compileSubjectRegex,
    matchesFilenamePattern,
    satisfiesBodyRequirement,
    extractPassingDkimDomains,
    satisfiesDkim,
    matchHeaders,
    matchMessage,
    buildGmailSearchQuery,
  };
}

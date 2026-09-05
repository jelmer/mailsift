//! Unwrap forwarded mail before extractor dispatch.
//!
//! When a friend forwards a vendor confirmation to me, I'd like
//! mailsift to act on the original: the vendor's booking, not the
//! forwarder's wrapper. This module detects two forwarding shapes and
//! returns the inner RFC822 bytes when the outer sender is on the
//! `trusted_forwarders` allow-list:
//!
//! 1. Attachment forwards: a `message/rfc822` subpart. The inner body
//!    is the vendor's original mail bit-for-bit.
//!
//! 2. Inline forwards: the mail-client style
//!    ("---------- Forwarded message ---------" attribution line
//!    followed by pseudo-headers, then the quoted body). We
//!    reconstruct a synthetic RFC822 from the parsed pseudo-headers
//!    and, when available, the HTML fragment the client quoted. DKIM
//!    cannot be verified on inline-forwarded mail; no signature
//!    survives. The trust here comes entirely from the outer sender
//!    being on `trusted_forwarders`, which is why the caller is told
//!    to bypass `require_dkim` when we return
//!    `from_trusted_forwarder`.
//!
//! Random forwarded mail from senders not on the list keeps flowing
//! through the normal pipeline against the outer envelope.

use mailparse::{ParsedMail, parse_mail};
use tracing::debug;

/// Result of a successful unwrap.
pub struct Unwrapped {
    /// Synthesized or extracted RFC822 bytes of the inner message.
    pub inner: Vec<u8>,
    /// True when the unwrap was authorized purely by the outer sender
    /// being on `trusted_forwarders`. Callers should treat this as
    /// permission to bypass extractors' `require_dkim` checks: the
    /// forwarder is standing in for the vendor's signature.
    pub from_trusted_forwarder: bool,
}

/// Try to detect a forwarded mail and return the inner RFC822 bytes if
/// the outer sender is on the allow-list. Returns `None` when the mail
/// isn't a forward or when no inner part is recognised; the caller
/// then processes the original.
pub fn try_unwrap_forwarded(raw: &[u8], trusted_senders: &[String]) -> Option<Unwrapped> {
    if trusted_senders.is_empty() {
        return None;
    }
    let parsed = parse_mail(raw).ok()?;
    let outer_from = parsed
        .headers
        .iter()
        .find(|h| h.get_key_ref().eq_ignore_ascii_case("from"))
        .map(|h| h.get_value())?;
    if !is_trusted(&outer_from, trusted_senders) {
        debug!(
            outer_from = %outer_from,
            "forward not unwrapped: outer sender not on trusted_forwarders list"
        );
        return None;
    }

    if let Some(bytes) = unwrap_attachment_form(&parsed) {
        debug!(
            outer_from = %outer_from,
            bytes = bytes.len(),
            "unwrapped attachment forward; re-running pipeline on inner message"
        );
        return Some(Unwrapped {
            inner: bytes,
            from_trusted_forwarder: true,
        });
    }

    if let Some(bytes) = unwrap_inline_form(&parsed) {
        debug!(
            outer_from = %outer_from,
            bytes = bytes.len(),
            "unwrapped inline forward; re-running pipeline on inner message"
        );
        return Some(Unwrapped {
            inner: bytes,
            from_trusted_forwarder: true,
        });
    }

    None
}

/// Match the outer `From:` header against the trusted-sender list.
/// Comparison is on the bare email address (case-insensitive); the
/// list may also contain bare addresses.
fn is_trusted(from_header: &str, trusted: &[String]) -> bool {
    let addr = extract_email_address(from_header);
    trusted
        .iter()
        .any(|t| extract_email_address(t).eq_ignore_ascii_case(&addr))
}

/// Pull the email address out of a header value like
/// `"Joe Example <joe@example.com>"` or just `joe@example.com`.
fn extract_email_address(header: &str) -> String {
    if let Some(start) = header.rfind('<')
        && let Some(end) = header.rfind('>')
        && end > start
    {
        return header[start + 1..end].trim().to_string();
    }
    header.trim().to_string()
}

fn unwrap_attachment_form(parsed: &ParsedMail<'_>) -> Option<Vec<u8>> {
    let inner = find_rfc822_part(parsed)?;
    let bytes = inner.get_body_raw().ok()?;
    (!bytes.is_empty()).then_some(bytes)
}

/// Depth-first search for a `message/rfc822` subpart.
fn find_rfc822_part<'a, 'b>(parsed: &'b ParsedMail<'a>) -> Option<&'b ParsedMail<'a>> {
    if parsed.ctype.mimetype.eq_ignore_ascii_case("message/rfc822") {
        return Some(parsed);
    }
    for sub in &parsed.subparts {
        if let Some(found) = find_rfc822_part(sub) {
            return Some(found);
        }
    }
    None
}

/// Detect a client-inserted forward attribution block and synthesize an
/// inner RFC822 message from it. Returns `None` when no recognised
/// attribution line is present or when the pseudo-header block yields
/// no `From:`.
fn unwrap_inline_form(parsed: &ParsedMail<'_>) -> Option<Vec<u8>> {
    let plain = find_body(parsed, "text/plain")?;
    let html = find_body(parsed, "text/html");

    let attribution = detect_attribution(&plain)?;
    let inner_headers = parse_pseudo_headers(&plain[attribution.header_start..])?;
    inner_headers.from.as_ref()?;

    let inner_plain = extract_inner_plain(&plain, attribution.header_start);
    let inner_html = html.as_deref().and_then(extract_inner_html);

    Some(build_synthetic_rfc822(
        &inner_headers,
        &inner_plain,
        inner_html.as_deref(),
    ))
}

/// Depth-first search for the first part whose MIME type matches.
fn find_body(parsed: &ParsedMail<'_>, mimetype: &str) -> Option<String> {
    if parsed.ctype.mimetype.eq_ignore_ascii_case(mimetype) {
        return parsed.get_body().ok();
    }
    for sub in &parsed.subparts {
        if let Some(body) = find_body(sub, mimetype) {
            return Some(body);
        }
    }
    None
}

struct Attribution {
    /// Byte offset in `plain` at which pseudo-headers begin.
    header_start: usize,
}

/// Attribution-line markers emitted by common mail clients, matched
/// case-insensitively as substrings.
const ATTRIBUTION_MARKERS: &[&str] = &[
    // English
    "forwarded message",
    "begin forwarded message",
    // Dutch
    "doorgestuurd bericht",
    "oorspronkelijk bericht",
    // German
    "weitergeleitete nachricht",
    "urspr\u{00fc}ngliche nachricht",
    // French
    "message transf\u{00e9}r\u{00e9}",
    "message d'origine",
    // Spanish
    "mensaje reenviado",
    "mensaje original",
];

fn detect_attribution(plain: &str) -> Option<Attribution> {
    let lower = plain.to_lowercase();
    let idx = ATTRIBUTION_MARKERS
        .iter()
        .filter_map(|m| lower.find(m))
        .min()?;
    // Advance to the end of the attribution line so `header_start`
    // points at the first pseudo-header line.
    let after = plain[idx..].find('\n').map(|n| idx + n + 1)?;
    Some(Attribution {
        header_start: after,
    })
}

#[derive(Default)]
struct InnerHeaders {
    from: Option<String>,
    to: Option<String>,
    date: Option<String>,
    subject: Option<String>,
    reply_to: Option<String>,
    cc: Option<String>,
}

/// Which known field a pseudo-header label maps to.
#[derive(Clone, Copy)]
enum Field {
    From,
    To,
    Cc,
    Date,
    Subject,
    ReplyTo,
}

fn field_for(label: &str) -> Option<Field> {
    match label.to_lowercase().as_str() {
        "from" | "van" | "de" | "von" => Some(Field::From),
        "to" | "aan" | "\u{00e0}" | "an" | "para" => Some(Field::To),
        "date" | "datum" | "envoy\u{00e9}" | "fecha" => Some(Field::Date),
        "subject" | "onderwerp" | "objet" | "betreff" | "asunto" => Some(Field::Subject),
        "reply-to" | "beantwoorden aan" | "r\u{00e9}pondre \u{00e0}" => Some(Field::ReplyTo),
        "cc" | "kopie" => Some(Field::Cc),
        _ => None,
    }
}

impl InnerHeaders {
    fn slot(&mut self, field: Field) -> &mut Option<String> {
        match field {
            Field::From => &mut self.from,
            Field::To => &mut self.to,
            Field::Cc => &mut self.cc,
            Field::Date => &mut self.date,
            Field::Subject => &mut self.subject,
            Field::ReplyTo => &mut self.reply_to,
        }
    }
}

/// Parse the pseudo-header block that follows the attribution line.
///
/// Expects a short run of `<Label>: <value>` lines, one per canonical
/// header, terminated by a blank line or a non-header-shaped line.
/// Folded continuation lines (leading whitespace) are appended to the
/// preceding field's value, matching RFC 5322.
fn parse_pseudo_headers(block: &str) -> Option<InnerHeaders> {
    let mut headers = InnerHeaders::default();
    let mut last_field: Option<Field> = None;
    let mut consumed_any = false;

    for raw_line in block.lines() {
        if raw_line.trim().is_empty() {
            if consumed_any {
                break;
            }
            continue;
        }
        if raw_line.starts_with(' ') || raw_line.starts_with('\t') {
            if let Some(field) = last_field
                && let Some(existing) = headers.slot(field).as_mut()
            {
                existing.push(' ');
                existing.push_str(raw_line.trim());
            }
            continue;
        }
        let Some((label, value)) = raw_line.split_once(':') else {
            break;
        };
        let Some(field) = field_for(label.trim()) else {
            if consumed_any {
                break;
            }
            continue;
        };
        *headers.slot(field) = Some(value.trim().to_string());
        last_field = Some(field);
        consumed_any = true;
    }

    consumed_any.then_some(headers)
}

/// Body of the inner mail as the client rendered it: everything past
/// the pseudo-header block, verbatim.
fn extract_inner_plain(plain: &str, header_start: usize) -> String {
    let after_headers = &plain[header_start..];
    let body_start = after_headers
        .find("\n\n")
        .map(|i| i + 2)
        .or_else(|| after_headers.find("\r\n\r\n").map(|i| i + 4))
        .unwrap_or(0);
    after_headers[body_start..].to_string()
}

/// Extract Gmail's `<div class="gmail_quote ...">` (or blockquote)
/// wrapper contents. Returns `None` when no wrapper is found or when
/// the input exceeds [`crate::pipeline::MAX_MESSAGE_BYTES`]; the
/// synthetic message is then text/plain only.
fn extract_inner_html(html: &str) -> Option<String> {
    if html.len() > crate::pipeline::MAX_MESSAGE_BYTES {
        return None;
    }
    for tag in &["div", "blockquote"] {
        let marker = format!("<{tag} class=\"gmail_quote");
        let lower = html.to_lowercase();
        let Some(start) = lower.find(&marker) else {
            continue;
        };
        let open_kind = format!("<{tag}");
        let close_kind = format!("</{tag}>");
        let region_lower = &lower[start..];
        let mut depth: usize = 0;
        let mut cursor: usize = 0;
        loop {
            let next_open = region_lower[cursor..].find(&open_kind).map(|i| cursor + i);
            let next_close = region_lower[cursor..].find(&close_kind).map(|i| cursor + i);
            match (next_open, next_close) {
                (Some(o), Some(c)) if o < c => {
                    depth += 1;
                    cursor = o + open_kind.len();
                }
                (_, Some(c)) => {
                    depth = depth.saturating_sub(1);
                    cursor = c + close_kind.len();
                    if depth == 0 {
                        return Some(html[start..start + cursor].to_string());
                    }
                }
                _ => return Some(html[start..].to_string()),
            }
        }
    }
    None
}

/// Build a minimal RFC822 message from parsed pseudo-headers plus one
/// or both bodies. The result carries enough shape for the downstream
/// pipeline (mailparse, body-shape prefilter, extractor stdin) to
/// treat it like any other incoming mail.
fn build_synthetic_rfc822(headers: &InnerHeaders, plain: &str, html: Option<&str>) -> Vec<u8> {
    let mut out = String::new();
    if let Some(v) = &headers.from {
        push_header(&mut out, "From", v);
    }
    if let Some(v) = &headers.to {
        push_header(&mut out, "To", v);
    }
    if let Some(v) = &headers.cc {
        push_header(&mut out, "Cc", v);
    }
    if let Some(v) = &headers.reply_to {
        push_header(&mut out, "Reply-To", v);
    }
    if let Some(v) = &headers.date {
        push_header(&mut out, "Date", v);
    }
    if let Some(v) = &headers.subject {
        push_header(&mut out, "Subject", v);
    }
    out.push_str("MIME-Version: 1.0\r\n");

    match html {
        None => {
            out.push_str("Content-Type: text/plain; charset=utf-8\r\n\r\n");
            out.push_str(plain);
        }
        Some(html_body) => {
            let boundary = "mailsift_inline_forward_boundary";
            out.push_str(&format!(
                "Content-Type: multipart/alternative; boundary=\"{boundary}\"\r\n\r\n"
            ));
            out.push_str(&format!("--{boundary}\r\n"));
            out.push_str("Content-Type: text/plain; charset=utf-8\r\n\r\n");
            out.push_str(plain);
            out.push_str(&format!("\r\n--{boundary}\r\n"));
            out.push_str("Content-Type: text/html; charset=utf-8\r\n\r\n");
            out.push_str(html_body);
            out.push_str(&format!("\r\n--{boundary}--\r\n"));
        }
    }
    out.into_bytes()
}

/// Fold onto a single line and strip CR/LF so a mischievous
/// pseudo-header value can't inject additional headers.
fn push_header(out: &mut String, name: &str, value: &str) {
    let cleaned: String = value.chars().filter(|c| *c != '\r' && *c != '\n').collect();
    out.push_str(name);
    out.push_str(": ");
    out.push_str(cleaned.trim());
    out.push_str("\r\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_attachment_forward(outer_from: &str, inner: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"From: ");
        out.extend_from_slice(outer_from.as_bytes());
        out.extend_from_slice(
            b"\r\nTo: jelmer@example.org\r\n\
              Subject: Fwd: a thing\r\n\
              MIME-Version: 1.0\r\n\
              Content-Type: multipart/mixed; boundary=\"BOUNDARY\"\r\n\
              \r\n--BOUNDARY\r\n\
              Content-Type: text/plain; charset=utf-8\r\n\r\n\
              See the attached message.\r\n\
              --BOUNDARY\r\n\
              Content-Type: message/rfc822\r\n\
              Content-Disposition: attachment; filename=\"original.eml\"\r\n\r\n",
        );
        out.extend_from_slice(inner);
        out.extend_from_slice(b"\r\n--BOUNDARY--\r\n");
        out
    }

    const INNER_MAIL: &[u8] =
        b"From: vendor@example.com\r\nSubject: Order #123\r\n\r\nyour order is confirmed\r\n";

    #[test]
    fn unwraps_attachment_form_when_outer_sender_is_trusted() {
        let raw = make_attachment_forward("Joe <joe@example.com>", INNER_MAIL);
        let trusted = vec!["joe@example.com".to_string()];
        let unwrapped = try_unwrap_forwarded(&raw, &trusted).expect("should unwrap");
        assert!(unwrapped.inner.starts_with(b"From: vendor@example.com"));
        assert!(unwrapped.from_trusted_forwarder);
    }

    #[test]
    fn declines_when_outer_sender_is_not_trusted() {
        let raw = make_attachment_forward("attacker@spam.example", INNER_MAIL);
        let trusted = vec!["joe@example.com".to_string()];
        assert!(try_unwrap_forwarded(&raw, &trusted).is_none());
    }

    #[test]
    fn declines_when_no_forward_recognised() {
        let raw = b"From: joe@example.com\r\nSubject: Hi\r\n\r\nplain mail, no forward\r\n";
        let trusted = vec!["joe@example.com".to_string()];
        assert!(try_unwrap_forwarded(raw, &trusted).is_none());
    }

    #[test]
    fn declines_when_trusted_list_is_empty() {
        let raw = make_attachment_forward("Joe <joe@example.com>", INNER_MAIL);
        assert!(try_unwrap_forwarded(&raw, &[]).is_none());
    }

    #[test]
    fn extracts_email_address_from_angle_form() {
        assert_eq!(
            extract_email_address("Joe Example <joe@example.com>"),
            "joe@example.com"
        );
        assert_eq!(extract_email_address("joe@example.com"), "joe@example.com");
    }

    fn make_inline_forward(outer_from: &str, plain_body: &str, html_body: Option<&str>) -> Vec<u8> {
        let mut out = String::new();
        out.push_str(&format!("From: {outer_from}\r\n"));
        out.push_str("To: jelmer@example.org\r\n");
        out.push_str("Subject: Fwd: a thing\r\n");
        out.push_str("MIME-Version: 1.0\r\n");
        match html_body {
            None => {
                out.push_str("Content-Type: text/plain; charset=utf-8\r\n\r\n");
                out.push_str(plain_body);
            }
            Some(html) => {
                out.push_str("Content-Type: multipart/alternative; boundary=\"OUTER\"\r\n\r\n");
                out.push_str("--OUTER\r\n");
                out.push_str("Content-Type: text/plain; charset=utf-8\r\n\r\n");
                out.push_str(plain_body);
                out.push_str("\r\n--OUTER\r\n");
                out.push_str("Content-Type: text/html; charset=utf-8\r\n\r\n");
                out.push_str(html);
                out.push_str("\r\n--OUTER--\r\n");
            }
        }
        out.into_bytes()
    }

    const GMAIL_PLAIN: &str = "\
Some intro text I typed.

---------- Forwarded message ---------
From: Vendor <vendor@example.com>
Date: Sat, 23 Aug 2025 at 20:57
Subject: Your order
To: Friend <friend@example.com>

your order is confirmed
line two of the body
";

    #[test]
    fn unwraps_gmail_inline_forward() {
        let raw = make_inline_forward("Joe <joe@example.com>", GMAIL_PLAIN, None);
        let trusted = vec!["joe@example.com".to_string()];
        let unwrapped = try_unwrap_forwarded(&raw, &trusted).expect("should unwrap");
        assert!(unwrapped.from_trusted_forwarder);
        let text = String::from_utf8(unwrapped.inner).unwrap();
        assert!(text.contains("From: Vendor <vendor@example.com>"));
        assert!(text.contains("Subject: Your order"));
        assert!(text.contains("your order is confirmed"));
    }

    #[test]
    fn unwraps_apple_mail_style_attribution() {
        let plain = "\
FYI.

Begin forwarded message:
From: Vendor <vendor@example.com>
Subject: Your order
Date: 23 Aug 2025 20:57:00 +0200
To: Friend <friend@example.com>

body
";
        let raw = make_inline_forward("Joe <joe@example.com>", plain, None);
        let trusted = vec!["joe@example.com".to_string()];
        let unwrapped = try_unwrap_forwarded(&raw, &trusted).expect("should unwrap");
        let text = String::from_utf8(unwrapped.inner).unwrap();
        assert!(text.contains("From: Vendor <vendor@example.com>"));
        assert!(text.contains("Subject: Your order"));
    }

    #[test]
    fn unwraps_dutch_localised_attribution() {
        let plain = "\
Zie hieronder.

---------- Doorgestuurd bericht ---------
Van: Vendor <vendor@example.com>
Datum: za 23 aug 2025 om 20:57
Onderwerp: Je bestelling
Aan: Friend <friend@example.com>

de bestelling is bevestigd
";
        let raw = make_inline_forward("Joe <joe@example.com>", plain, None);
        let trusted = vec!["joe@example.com".to_string()];
        let unwrapped = try_unwrap_forwarded(&raw, &trusted).expect("should unwrap");
        let text = String::from_utf8(unwrapped.inner).unwrap();
        assert!(text.contains("From: Vendor <vendor@example.com>"));
        assert!(text.contains("Subject: Je bestelling"));
    }

    #[test]
    fn unwraps_german_localised_attribution() {
        let plain = "\
Weitergeleitete Nachricht
Von: Vendor <vendor@example.com>
Betreff: Deine Bestellung
Datum: 23. August 2025
An: Friend <friend@example.com>

body
";
        let raw = make_inline_forward("Joe <joe@example.com>", plain, None);
        let trusted = vec!["joe@example.com".to_string()];
        let unwrapped = try_unwrap_forwarded(&raw, &trusted).expect("should unwrap");
        let text = String::from_utf8(unwrapped.inner).unwrap();
        assert!(text.contains("From: Vendor <vendor@example.com>"));
        assert!(text.contains("Subject: Deine Bestellung"));
    }

    #[test]
    fn inline_forward_carries_html_when_present() {
        let html = "\
<div>my note</div>\
<div class=\"gmail_quote gmail_quote_container\">\
<div class=\"gmail_attr\">---------- Forwarded message ---------</div>\
<div>inner html body</div>\
</div>\
";
        let raw = make_inline_forward("Joe <joe@example.com>", GMAIL_PLAIN, Some(html));
        let trusted = vec!["joe@example.com".to_string()];
        let unwrapped = try_unwrap_forwarded(&raw, &trusted).expect("should unwrap");
        let text = String::from_utf8(unwrapped.inner).unwrap();
        assert!(text.contains("Content-Type: multipart/alternative"));
        assert!(text.contains("inner html body"));
    }

    #[test]
    fn declines_inline_when_attribution_has_no_from_line() {
        let plain = "\
---------- Forwarded message ---------
(all headers stripped)

body
";
        let raw = make_inline_forward("Joe <joe@example.com>", plain, None);
        let trusted = vec!["joe@example.com".to_string()];
        assert!(try_unwrap_forwarded(&raw, &trusted).is_none());
    }

    #[test]
    fn pseudo_header_injection_is_neutralised() {
        // A malicious value with an embedded CRLF must not smuggle
        // additional headers into the synthesized message.
        let plain = "\
---------- Forwarded message ---------
From: attacker@example.com\r\nX-Injected: yes
Subject: Your order
Date: Sat, 23 Aug 2025
To: friend@example.com

body
";
        let raw = make_inline_forward("Joe <joe@example.com>", plain, None);
        let trusted = vec!["joe@example.com".to_string()];
        let unwrapped = try_unwrap_forwarded(&raw, &trusted).expect("should unwrap");
        let text = String::from_utf8(unwrapped.inner).unwrap();
        assert!(!text.contains("X-Injected"));
    }

    #[test]
    fn extract_inner_html_returns_none_when_input_exceeds_size_cap() {
        let mut html = String::with_capacity(crate::pipeline::MAX_MESSAGE_BYTES + 128);
        html.push_str("<div class=\"gmail_quote\">start");
        while html.len() <= crate::pipeline::MAX_MESSAGE_BYTES {
            html.push('x');
        }
        html.push_str("</div>");
        assert!(html.len() > crate::pipeline::MAX_MESSAGE_BYTES);
        assert!(extract_inner_html(&html).is_none());
    }
}

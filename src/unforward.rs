//! Unwrap forwarded mail before extractor dispatch.
//!
//! When a friend forwards a vendor confirmation to me, I'd like
//! mailsift to act on the original; the vendor's DKIM-signed
//! receipt, not my friend's forwarding wrapper. This module detects a
//! forwarded message and returns the inner RFC822 bytes when the outer
//! sender is on a configured allow-list.
//!
//! Only the canonical `message/rfc822`-attachment form is handled here;
//! inline ("---------- Forwarded message ---------") forwards are not
//! reassembled; they'd require parsing structured headers out of a
//! free-form body, which is fragile and would silently mis-attribute
//! DKIM signatures.
//!
//! The trust check is intentionally narrow: even though we recheck the
//! inner message's DKIM against its own `Authentication-Results`
//! header, we only do that for outer senders the user has put on the
//! `trusted_forwarders` list. Random forwarded mail keeps flowing
//! through the normal extractor pipeline against the outer envelope.

use mailparse::parse_mail;
use tracing::debug;

/// Try to detect a forwarded mail and return the inner RFC822 bytes if
/// the outer sender is on the allow-list. Returns `None` when the mail
/// isn't a forward or when no inner part is recognised; the caller
/// then processes the original.
pub fn try_unwrap_forwarded(raw: &[u8], trusted_senders: &[String]) -> Option<Vec<u8>> {
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

    // Look for a `message/rfc822` subpart anywhere in the tree.
    let inner = find_rfc822_part(&parsed)?;
    let bytes = inner.get_body_raw().ok()?;
    if bytes.is_empty() {
        return None;
    }
    debug!(
        outer_from = %outer_from,
        bytes = bytes.len(),
        "unwrapped forwarded mail; re-running pipeline on inner message"
    );
    Some(bytes)
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

/// Depth-first search for a `message/rfc822` subpart.
fn find_rfc822_part<'a, 'b>(
    parsed: &'b mailparse::ParsedMail<'a>,
) -> Option<&'b mailparse::ParsedMail<'a>> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn make_forward(outer_from: &str, inner: &[u8]) -> Vec<u8> {
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
    fn unwraps_when_outer_sender_is_trusted() {
        let raw = make_forward("Joe <joe@example.com>", INNER_MAIL);
        let trusted = vec!["joe@example.com".to_string()];
        let unwrapped = try_unwrap_forwarded(&raw, &trusted).expect("should unwrap");
        assert!(unwrapped.starts_with(b"From: vendor@example.com"));
        assert!(unwrapped.contains_str_basic("your order is confirmed"));
    }

    #[test]
    fn declines_when_outer_sender_is_not_trusted() {
        let raw = make_forward("attacker@spam.example", INNER_MAIL);
        let trusted = vec!["joe@example.com".to_string()];
        assert!(try_unwrap_forwarded(&raw, &trusted).is_none());
    }

    #[test]
    fn declines_when_no_inner_rfc822_part() {
        let raw = b"From: joe@example.com\r\nSubject: Hi\r\n\r\nplain mail, no forward\r\n";
        let trusted = vec!["joe@example.com".to_string()];
        assert!(try_unwrap_forwarded(raw, &trusted).is_none());
    }

    #[test]
    fn declines_when_trusted_list_is_empty() {
        let raw = make_forward("Joe <joe@example.com>", INNER_MAIL);
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

    // Tiny `contains` helper to avoid pulling in extra deps for tests.
    trait ContainsBytes {
        fn contains_str_basic(&self, needle: &str) -> bool;
    }
    impl ContainsBytes for Vec<u8> {
        fn contains_str_basic(&self, needle: &str) -> bool {
            self.windows(needle.len()).any(|w| w == needle.as_bytes())
        }
    }
}

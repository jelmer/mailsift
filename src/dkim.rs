//! Trust the topmost `Authentication-Results` header to tell us which
//! DKIM signatures passed.
//!
//! Why "topmost"? The header is added by each MTA the message passes
//! through; earlier hops' headers are attacker-controlled if the
//! attacker controls a relay along the path. Only the first
//! header, added by our own MTA, is trustworthy. RFC 8601 §5 says
//! the same thing.
//!
//! We parse the header's `dkim=pass` items and collect their `header.d`
//! (the signing domain). A manifest's `require_dkim` list is satisfied
//! when at least one of the listed domains matches one of those.

use std::collections::HashSet;

use mailparse::MailHeader;

/// Collect signing domains that pass DKIM, according to the topmost
/// `Authentication-Results` header.
///
/// Takes already-parsed headers so the caller can share one parse
/// across the dkim check, the from-domain prefilter, and the
/// message-date fallback.
pub fn passing_dkim_domains(headers: &[MailHeader<'_>]) -> HashSet<String> {
    let mut out = HashSet::new();
    let Some(header) = first_auth_results(headers) else {
        return out;
    };
    // The header body looks like:
    //   rhonwyn.jelmer.uk;
    //     dkim=pass (1024-bit key; unprotected) header.d=ns.nl header.i=@ns.nl ...;
    //     dkim=pass (1024-bit key) header.d=ns.nl header.i=@ns.nl ...;
    //     dkim-atps=neutral
    //
    // Comments may themselves contain `;`, so strip comments before
    // splitting on `;`. Then for each item that starts with `dkim=pass`,
    // pull out its `header.d=` value.
    let header = strip_comments(&header);
    for item in header.split(';') {
        let item = item.trim();
        if !item.starts_with("dkim=pass") {
            continue;
        }
        for token in item.split_whitespace() {
            if let Some(domain) = token.strip_prefix("header.d=") {
                let d = domain.trim_end_matches(',').trim_matches('"');
                if !d.is_empty() {
                    out.insert(d.to_ascii_lowercase());
                }
            }
        }
    }
    out
}

/// Remove RFC 5322-style `(comment)` runs, including nested ones.
fn strip_comments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut depth = 0;
    for c in input.chars() {
        match c {
            '(' => depth += 1,
            ')' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    out.push(' ');
                }
            }
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

/// Return the value of the first `Authentication-Results` header
/// (top-of-message, i.e. closest to our own MTA), with any RFC 5322
/// folded continuation lines unfolded into a single string.
fn first_auth_results(headers: &[MailHeader<'_>]) -> Option<String> {
    headers
        .iter()
        .find(|h| {
            h.get_key_ref()
                .eq_ignore_ascii_case("authentication-results")
        })
        .map(|h| h.get_value())
}

/// Check whether at least one of `wanted` is in `passing`.
///
/// A `wanted` entry starting with `.` is a suffix match: e.g.
/// `.myshopify.com` matches `xyz.myshopify.com` but not the bare
/// `myshopify.com`. This lets a manifest say "any DKIM-signed mail
/// from infrastructure under this parent zone is good enough"; used
/// by the generic Shopify-template extractor, which doesn't want to
/// hard-code a list of shop domains.
///
/// Both sides are expected to be lowercased: extractor manifests
/// normalise `require_dkim` at load time, and `passing_dkim_domains`
/// lowercases on insert.
pub fn satisfies(wanted: &[String], passing: &HashSet<String>) -> bool {
    wanted.iter().any(|w| {
        if let Some(suffix) = w.strip_prefix('.') {
            passing.iter().any(|p| {
                p.len() > suffix.len() + 1
                    && p.ends_with(suffix)
                    && p.as_bytes()[p.len() - suffix.len() - 1] == b'.'
            })
        } else {
            passing.contains(w.as_str())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mailparse::parse_headers;

    /// Parse `raw` and run the helper on the resulting headers. Keeps
    /// the tests terse without bringing back the old `&[u8]` wrapper.
    fn domains(raw: &[u8]) -> HashSet<String> {
        let (headers, _) = parse_headers(raw).expect("test inputs are well-formed");
        passing_dkim_domains(&headers)
    }

    #[test]
    fn no_header() {
        let raw = b"From: a\r\n\r\nbody";
        assert!(domains(raw).is_empty());
    }

    fn expect_domains<const N: usize>(raw: &[u8], expected: [&str; N]) {
        let got = domains(raw);
        let want: HashSet<String> = expected.iter().map(|s| (*s).to_string()).collect();
        assert_eq!(got, want);
    }

    #[test]
    fn simple_pass() {
        let raw = b"\
Authentication-Results: my.mta;\r
\tdkim=pass header.d=vendor.com header.i=@vendor.com\r
From: x\r
\r
body";
        expect_domains(raw, ["vendor.com"]);
    }

    #[test]
    fn multiple_pass_items() {
        let raw = b"\
Authentication-Results: my.mta;\r
\tdkim=pass (1024-bit key; unprotected) header.d=ns.nl header.i=@ns.nl;\r
\tdkim=pass (1024-bit key) header.d=ns.nl header.i=@ns.nl;\r
\tdkim-atps=neutral\r
From: x\r
\r
body";
        expect_domains(raw, ["ns.nl"]);
    }

    #[test]
    fn pass_with_quoted_d() {
        let raw = b"\
Authentication-Results: my.mta; dkim=pass header.d=\"easyjet.com\"\r
From: x\r
\r
body";
        expect_domains(raw, ["easyjet.com"]);
    }

    #[test]
    fn ignore_fail() {
        let raw = b"\
Authentication-Results: my.mta;\r
\tdkim=fail header.d=evil.example;\r
\tdkim=pass header.d=good.example\r
From: x\r
\r
body";
        expect_domains(raw, ["good.example"]);
    }

    #[test]
    fn only_topmost_header_is_trusted() {
        // Two Authentication-Results headers; the bottom one (attacker
        // controlled) claims a passing signature for vendor.com, but the
        // top one (our MTA) doesn't. We should only see what the top
        // header says.
        let raw = b"\
Authentication-Results: trusted.mta; dkim=pass header.d=real.example\r
Received: from somewhere\r
Authentication-Results: untrusted.relay; dkim=pass header.d=spoofed.example\r
From: x\r
\r
body";
        expect_domains(raw, ["real.example"]);
    }

    #[test]
    fn satisfies_helper() {
        let mut p = HashSet::new();
        p.insert("vendor.com".to_string());
        assert!(satisfies(&["vendor.com".to_string()], &p));
        assert!(satisfies(
            &["other.example".to_string(), "vendor.com".to_string()],
            &p
        ));
        assert!(!satisfies(&["other.example".to_string()], &p));
    }

    #[test]
    fn satisfies_suffix_form() {
        let mut p = HashSet::new();
        p.insert("xyz.myshopify.com".to_string());
        // Suffix matches a subdomain.
        assert!(satisfies(&[".myshopify.com".to_string()], &p));
        // Bare parent zone does NOT match the suffix form; the leading
        // dot specifically requires a subdomain.
        let mut bare = HashSet::new();
        bare.insert("myshopify.com".to_string());
        assert!(!satisfies(&[".myshopify.com".to_string()], &bare));
        // Spoofed lookalike (`evil-myshopify.com`) doesn't match;
        // the suffix check insists on a dot before it.
        let mut evil = HashSet::new();
        evil.insert("evil-myshopify.com".to_string());
        assert!(!satisfies(&[".myshopify.com".to_string()], &evil));
        // A passing domain that's literally the wanted entry (leading
        // dot included) must NOT match. The length guard insists on at
        // least one byte of subdomain before the suffix; without that
        // strictness, `.myshopify.com` itself would spuriously match
        // any `.myshopify.com` requirement.
        let mut dotted = HashSet::new();
        dotted.insert(".myshopify.com".to_string());
        assert!(!satisfies(&[".myshopify.com".to_string()], &dotted));
    }

    #[test]
    fn strip_comments_preserves_uncommented_text() {
        assert_eq!(strip_comments("hello world"), "hello world");
    }

    #[test]
    fn strip_comments_replaces_top_level_comment_with_space() {
        assert_eq!(strip_comments("a (b) c"), "a   c");
    }

    #[test]
    fn strip_comments_drops_nested_content_without_extra_spaces() {
        // Inner `)` returns from depth 2 to depth 1: nothing emitted.
        // Outer `)` returns from depth 1 to depth 0: one space emitted.
        assert_eq!(strip_comments("a (b (c) d) e"), "a   e");
    }

    #[test]
    fn strip_comments_handles_stray_close_paren() {
        // A `)` at depth 0 must NOT decrement depth. If the guard were
        // wrong (mutation: `depth > 0` -> `true`), a stray `)` followed
        // by `(content)` would consume the `(`, leaking `content)` into
        // the output. With the guard in place, the inner `(content)` is
        // a proper comment and gets replaced with a single space.
        let got = strip_comments("a) (secret) b");
        assert!(!got.contains("secret"));
    }

    /// Regression: a `(comment with ; semicolon)` inside the value can't
    /// be allowed to split a `dkim=pass` item in two.
    #[test]
    fn comments_with_semicolons_dont_split_items() {
        let raw = b"\
Authentication-Results: rhonwyn.jelmer.uk;\r
\tdkim=pass (1024-bit key; unprotected) header.d=booking.com header.i=noreply@booking.com header.a=rsa-sha256 header.s=bk header.b=y+8TvmRb;\r
\tdkim-atps=neutral\r
From: x\r
\r
body";
        expect_domains(raw, ["booking.com"]);
    }
}

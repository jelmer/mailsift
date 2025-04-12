//! CalDAV target for `event` artifacts.
//!
//! On first use the sink does a small PROPFIND dance to discover, for
//! the authenticated user:
//!
//! - the current user principal (`DAV:current-user-principal` on the
//!   base URL),
//! - the schedule inbox URL on the principal
//!   (`CALDAV:schedule-inbox-URL`, RFC 6638),
//! - the default calendar to file plain events into
//!   (`CALDAV:schedule-default-calendar-URL` on the schedule inbox).
//!
//! Events are then PUT to `<collection>/<UID>.ics`. iMIP scheduling
//! requests (an enclosing `METHOD:REQUEST`) go to the schedule inbox;
//! everything else goes to the default calendar; that way the user's
//! calendar app sees plain events directly while invitations still flow
//! through the scheduling pipeline.
//!
//! Authentication is delegated to [`super::http_auth`]: we send our
//! preferred scheme preemptively and fall back on a 401 if the server's
//! `WWW-Authenticate` invites another scheme we can offer.
//!
//! Internally the HTTP client is async (`reqwest::Client`). The public
//! `file()` entry point is sync; it blocks on the supplied tokio runtime
//! handle. Use a runtime that the caller already owns (the milter
//! reuses its main runtime; replay / imap-scan build a dedicated one in
//! `main.rs`).

use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use quick_xml::events::Event as XmlEvent;
use quick_xml::name::{Namespace, ResolveResult};
use quick_xml::reader::NsReader;
use reqwest::{Client, Method, StatusCode, Url};
use tokio::runtime::Handle;
use tracing::{debug, info};

use super::http_auth::{self, Auth};
use super::http_client::{build_client, truncate};
use super::{FileOutcome, SingleEvent};

/// Everything except RFC 3986 "unreserved" characters
/// (`ALPHA / DIGIT / "-" / "." / "_" / "~"`) gets percent-encoded.
const UID_PATH_SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

/// Resolved CalDAV collection URLs for a given user. Filled in lazily on
/// the first `file()` call so startup doesn't pay the discovery cost if
/// the sink is never used.
#[derive(Debug, Clone)]
struct Collections {
    /// `CALDAV:schedule-default-calendar-URL`. Plain VEVENTs land here.
    default_calendar: String,
    /// `CALDAV:schedule-inbox-URL`. iMIP scheduling messages land here.
    schedule_inbox: String,
}

pub struct CaldavSink {
    client: Client,
    runtime: Handle,
    base_url: String,
    auth: Auth,
    collections: OnceLock<Collections>,
}

impl CaldavSink {
    /// Build a sink for the given CalDAV server URL.
    ///
    /// `base_url` is the server root (e.g. `https://cal.example.org/`)
    /// or any URL the server will accept a PROPFIND for
    /// `DAV:current-user-principal` on; most servers honour the
    /// well-known `/.well-known/caldav` redirect from the root, so the
    /// shortest URL that authenticates the user is enough.
    ///
    /// `user` and `password` are optional. With the `gssapi` feature
    /// enabled, omitting them switches to Kerberos-only auth (the
    /// caller is expected to have a ticket in their credential cache).
    /// Without the feature, credentials are required.
    pub fn new(
        base_url: String,
        user: Option<String>,
        password: Option<String>,
        runtime: Handle,
    ) -> Result<Self> {
        if base_url.is_empty() {
            anyhow::bail!("CalDAV base URL must not be empty");
        }
        let auth = http_auth::build_auth(&base_url, user, password, "CalDAV")?;
        let client = build_client("CalDAV")?;
        Ok(Self {
            client,
            runtime,
            base_url,
            auth,
            collections: OnceLock::new(),
        })
    }

    pub fn file(&self, event: &SingleEvent) -> Result<FileOutcome> {
        // The trait boundary is sync; everything below is async. Block
        // on the caller-supplied runtime handle. Safe to call from any
        // thread that isn't itself running an async task on that
        // runtime; the milter pipeline runs on `spawn_blocking`, and
        // replay / imap-scan don't have an ambient runtime, so they
        // pass a dedicated `Runtime::handle()`.
        self.runtime.block_on(self.file_async(event))
    }

    async fn file_async(&self, event: &SingleEvent) -> Result<FileOutcome> {
        let collections = self.ensure_collections().await?;
        let collection = if is_imip_request(event.method.as_deref()) {
            &collections.schedule_inbox
        } else {
            &collections.default_calendar
        };
        let url = event_url(collection, &event.uid);
        let body = event.body.clone();
        let response = http_auth::send_with_auth_retry(&self.client, &self.auth, |client| {
            client
                .put(&url)
                .header(
                    reqwest::header::CONTENT_TYPE,
                    "text/calendar; charset=utf-8",
                )
                .body(body.clone())
        })
        .await
        .with_context(|| format!("PUT {url}"))?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "CalDAV PUT to {url} returned {status}: {}",
                truncate(&body, 200)
            ));
        }

        match status {
            StatusCode::CREATED => {
                info!(target = %url, "event created");
                Ok(FileOutcome::Created(url))
            }
            _ => {
                // 200, 204 etc.: existing resource updated.
                info!(target = %url, %status, "event updated");
                Ok(FileOutcome::Updated(url))
            }
        }
    }

    async fn ensure_collections(&self) -> Result<&Collections> {
        if let Some(c) = self.collections.get() {
            return Ok(c);
        }
        let resolved = self
            .discover_collections()
            .await
            .context("discovering CalDAV collections")?;
        // Race-tolerant: if another caller got here first, prefer the
        // value that's already there (semantically equivalent; both
        // would have done the same PROPFIND walk).
        let _ = self.collections.set(resolved);
        Ok(self.collections.get().expect("just set above"))
    }

    async fn discover_collections(&self) -> Result<Collections> {
        let principal = self.discover_principal().await?;
        debug!(principal, "discovered current-user-principal");
        let schedule_inbox = self.discover_schedule_inbox(&principal).await?;
        debug!(schedule_inbox, "discovered schedule inbox");
        let default_calendar = self.discover_default_calendar(&schedule_inbox).await?;
        debug!(default_calendar, "discovered default calendar");
        Ok(Collections {
            default_calendar,
            schedule_inbox,
        })
    }

    async fn discover_principal(&self) -> Result<String> {
        const BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<propfind xmlns="DAV:">
  <prop><current-user-principal/></prop>
</propfind>"#;
        let xml = self.propfind(&self.base_url, 0, BODY).await?;
        let href = extract_first_property_href(&xml, &[("DAV:", "current-user-principal")])
            .ok_or_else(|| missing_property("current-user-principal", &self.base_url, &xml))?;
        self.resolve(&self.base_url, &href)
    }

    async fn discover_schedule_inbox(&self, principal_url: &str) -> Result<String> {
        const BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<propfind xmlns="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <prop><C:schedule-inbox-URL/></prop>
</propfind>"#;
        let xml = self.propfind(principal_url, 0, BODY).await?;
        let inbox = extract_first_property_href(
            &xml,
            &[("urn:ietf:params:xml:ns:caldav", "schedule-inbox-URL")],
        )
        .ok_or_else(|| missing_property("schedule-inbox-URL", principal_url, &xml))?;
        self.resolve(principal_url, &inbox)
    }

    async fn discover_default_calendar(&self, inbox_url: &str) -> Result<String> {
        const BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<propfind xmlns="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <prop><C:schedule-default-calendar-URL/></prop>
</propfind>"#;
        let xml = self.propfind(inbox_url, 0, BODY).await?;
        let href = extract_first_property_href(
            &xml,
            &[(
                "urn:ietf:params:xml:ns:caldav",
                "schedule-default-calendar-URL",
            )],
        )
        .ok_or_else(|| missing_property("schedule-default-calendar-URL", inbox_url, &xml))?;
        self.resolve(inbox_url, &href)
    }

    /// Send a PROPFIND and return the response body. Only 207 Multi-Status
    /// (RFC 4918) counts as success.
    async fn propfind(&self, url: &str, depth: u8, body: &str) -> Result<String> {
        let body = body.to_string();
        debug!(url, depth, "PROPFIND");
        let response = http_auth::send_with_auth_retry(&self.client, &self.auth, |client| {
            client
                .request(
                    Method::from_bytes(b"PROPFIND").expect("PROPFIND is valid"),
                    url,
                )
                .header(
                    reqwest::header::CONTENT_TYPE,
                    "application/xml; charset=utf-8",
                )
                .header("Depth", depth.to_string())
                .body(body.clone())
        })
        .await
        .with_context(|| format!("PROPFIND {url}"))?;

        let status = response.status();
        debug!(url, %status, "PROPFIND response");
        if status != StatusCode::MULTI_STATUS {
            let text = response.text().await.unwrap_or_default();
            anyhow::bail!(
                "CalDAV PROPFIND {url} returned {status}: {}",
                truncate(&text, 200)
            );
        }
        response
            .text()
            .await
            .with_context(|| format!("reading PROPFIND {url} body"))
    }

    /// Resolve a (possibly relative) href against a base URL, preserving
    /// the resulting URL as an absolute string with a trailing slash if
    /// the server didn't include one; CalDAV collections are
    /// path-like so consistent trailing slashes simplify the later PUT
    /// composition.
    fn resolve(&self, base: &str, href: &str) -> Result<String> {
        let base_url = Url::parse(base).with_context(|| format!("parsing CalDAV base {base}"))?;
        let joined = base_url
            .join(href)
            .with_context(|| format!("resolving href {href:?} against {base}"))?;
        Ok(joined.to_string())
    }
}

fn event_url(collection: &str, uid: &str) -> String {
    let sep = if collection.ends_with('/') { "" } else { "/" };
    let encoded = utf8_percent_encode(uid, UID_PATH_SEGMENT);
    format!("{collection}{sep}{encoded}.ics")
}

/// `true` when the enclosing calendar carries `METHOD:REQUEST`, the iMIP
/// scheduling method that belongs in the schedule inbox.
fn is_imip_request(method: Option<&str>) -> bool {
    matches!(method, Some(m) if m.eq_ignore_ascii_case("REQUEST"))
}

/// Build the "no `<prop>` href found" error with the truncated PROPFIND
/// response body attached, so the WARN log line tells the user why
/// discovery failed without needing a debug toggle. The most common
/// causes are a server that returns the property as empty (no value)
/// or a path that doesn't expose that property at all; both are
/// readable directly from the body snippet.
fn missing_property(local_name: &str, url: &str, body: &str) -> anyhow::Error {
    anyhow!(
        "CalDAV PROPFIND on {url} returned no {local_name} href; body: {}",
        truncate(body.trim(), 400)
    )
}

/// Walk a PROPFIND multistatus response and return the first `<href>`
/// found inside any of the requested properties.
///
/// `wanted` is a list of `(namespace, local_name)` pairs; the function
/// returns as soon as it finds an `<href>` inside any of them. We walk
/// the document with `quick_xml` so we tolerate the various capitalisations
/// and prefix choices CalDAV servers use (e.g. `D:href`, `href`,
/// `CAL:schedule-inbox-URL` vs `C:schedule-inbox-URL`).
fn extract_first_property_href(xml: &str, wanted: &[(&str, &str)]) -> Option<String> {
    let mut reader = NsReader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut prop_depth: u32 = 0;
    let mut in_wanted: bool = false;
    let mut current_href: Option<String> = None;
    let mut in_href: bool = false;

    loop {
        match reader.read_resolved_event_into(&mut buf) {
            Err(_) => return None,
            Ok((_, XmlEvent::Eof)) => return None,
            Ok((ns, XmlEvent::Start(e))) => {
                let local = e.local_name();
                if !in_wanted
                    && wanted.iter().any(|(wns, wname)| {
                        ns_matches(&ns, wns)
                            && local.as_ref().eq_ignore_ascii_case(wname.as_bytes())
                    })
                {
                    in_wanted = true;
                    prop_depth = 1;
                } else if in_wanted {
                    prop_depth += 1;
                    if ns_matches(&ns, "DAV:") && local.as_ref().eq_ignore_ascii_case(b"href") {
                        in_href = true;
                        current_href = Some(String::new());
                    }
                }
            }
            Ok((_, XmlEvent::Empty(_))) => {
                // No-op for our purposes; empty elements have no text.
            }
            Ok((_, XmlEvent::Text(t))) => {
                if in_href
                    && let Some(buf) = current_href.as_mut()
                    && let Ok(s) = t.unescape()
                {
                    buf.push_str(s.as_ref());
                }
            }
            Ok((_, XmlEvent::End(_))) => {
                if in_href {
                    in_href = false;
                    if let Some(href) = current_href.take()
                        && !href.trim().is_empty()
                    {
                        return Some(href.trim().to_string());
                    }
                }
                if in_wanted {
                    prop_depth -= 1;
                    if prop_depth == 0 {
                        in_wanted = false;
                    }
                }
            }
            _ => {}
        }
        buf.clear();
    }
}

/// `true` when a quick-xml resolved namespace matches `wanted` (URI form
/// like `"DAV:"` or `"urn:ietf:params:xml:ns:caldav"`). Treats
/// [`ResolveResult::Unbound`] as a match for `"DAV:"` only; CalDAV
/// responses commonly omit a default-namespace declaration on `<href>`
/// despite the root carrying `xmlns="DAV:"`, but the local name is
/// still meaningful in the DAV vocabulary.
fn ns_matches(resolved: &ResolveResult<'_>, wanted: &str) -> bool {
    match resolved {
        ResolveResult::Bound(Namespace(ns)) => ns == &wanted.as_bytes(),
        ResolveResult::Unbound => wanted == "DAV:",
        ResolveResult::Unknown(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::runtime::Runtime;

    fn test_handle() -> Handle {
        // A 1-thread runtime is enough for the tests; the only call
        // that ever uses it is `event_url`, which doesn't need it.
        // Stash the runtime in a static so it doesn't drop while the
        // handle is in use.
        use std::sync::OnceLock;
        static RT: OnceLock<Runtime> = OnceLock::new();
        RT.get_or_init(|| Runtime::new().unwrap()).handle().clone()
    }

    #[test]
    fn event_url_with_trailing_slash() {
        assert_eq!(
            event_url(
                "https://cal.example.org/dav/jelmer/calendar/",
                "flight-fr1234@mailsift",
            ),
            "https://cal.example.org/dav/jelmer/calendar/flight-fr1234%40mailsift.ics"
        );
    }

    #[test]
    fn event_url_without_trailing_slash() {
        assert_eq!(
            event_url(
                "https://cal.example.org/dav/jelmer/calendar",
                "flight-fr1234",
            ),
            "https://cal.example.org/dav/jelmer/calendar/flight-fr1234.ics"
        );
    }

    #[test]
    fn username_without_password_is_rejected() {
        let result = CaldavSink::new(
            "https://cal.example.org/".into(),
            Some("u".into()),
            None,
            test_handle(),
        );
        let err = match result {
            Ok(_) => panic!("expected error, got Ok"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("without a password"), "{err}");
    }

    #[test]
    #[cfg(not(feature = "gssapi"))]
    fn missing_credentials_without_gssapi_is_rejected() {
        let result = CaldavSink::new("https://cal.example.org/".into(), None, None, test_handle());
        let err = match result {
            Ok(_) => panic!("expected error, got Ok"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("requires a username"), "{err}");
    }

    #[test]
    fn imip_request_detection() {
        assert!(is_imip_request(Some("REQUEST")));
        assert!(is_imip_request(Some("request")));
        assert!(!is_imip_request(Some("PUBLISH")));
        assert!(!is_imip_request(Some("REPLY")));
        assert!(!is_imip_request(Some("CANCEL")));
        assert!(!is_imip_request(None));
    }

    #[test]
    fn extracts_current_user_principal() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:">
  <D:response>
    <D:href>/</D:href>
    <D:propstat>
      <D:prop>
        <D:current-user-principal>
          <D:href>/principals/users/jelmer/</D:href>
        </D:current-user-principal>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
</D:multistatus>"#;
        let got = extract_first_property_href(xml, &[("DAV:", "current-user-principal")]);
        assert_eq!(got.as_deref(), Some("/principals/users/jelmer/"));
    }

    #[test]
    fn extracts_calendar_home_set_with_default_namespace() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<multistatus xmlns="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <response>
    <href>/principals/users/jelmer/</href>
    <propstat>
      <prop>
        <C:calendar-home-set>
          <href>/calendars/jelmer/</href>
        </C:calendar-home-set>
      </prop>
      <status>HTTP/1.1 200 OK</status>
    </propstat>
  </response>
</multistatus>"#;
        let got = extract_first_property_href(
            xml,
            &[("urn:ietf:params:xml:ns:caldav", "calendar-home-set")],
        );
        assert_eq!(got.as_deref(), Some("/calendars/jelmer/"));
    }

    #[test]
    fn extracts_schedule_inbox_url() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:response>
    <D:href>/principals/users/jelmer/</D:href>
    <D:propstat>
      <D:prop>
        <C:schedule-inbox-URL>
          <D:href>/calendars/jelmer/inbox/</D:href>
        </C:schedule-inbox-URL>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
</D:multistatus>"#;
        let got = extract_first_property_href(
            xml,
            &[("urn:ietf:params:xml:ns:caldav", "schedule-inbox-URL")],
        );
        assert_eq!(got.as_deref(), Some("/calendars/jelmer/inbox/"));
    }

    #[test]
    fn extracts_schedule_default_calendar_url() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:response>
    <D:href>/calendars/jelmer/inbox/</D:href>
    <D:propstat>
      <D:prop>
        <C:schedule-default-calendar-URL>
          <D:href>/calendars/jelmer/personal/</D:href>
        </C:schedule-default-calendar-URL>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
</D:multistatus>"#;
        let got = extract_first_property_href(
            xml,
            &[(
                "urn:ietf:params:xml:ns:caldav",
                "schedule-default-calendar-URL",
            )],
        );
        assert_eq!(got.as_deref(), Some("/calendars/jelmer/personal/"));
    }

    #[test]
    fn extracts_current_user_principal_with_generated_prefix() {
        // Real response from a calypso-style server: the namespace is
        // declared with the auto-generated prefix `ns0` rather than the
        // conventional `D`. Earlier prefix-guessing logic missed this.
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<ns0:multistatus xmlns:ns0="DAV:"><ns0:response><ns0:href>/dav/jelmer/inbox/</ns0:href><ns0:propstat><ns0:status>HTTP/1.1 200 OK</ns0:status><ns0:prop><ns0:current-user-principal><ns0:href>/dav/jelmer/</ns0:href></ns0:current-user-principal></ns0:prop></ns0:propstat></ns0:response></ns0:multistatus>"#;
        let got = extract_first_property_href(xml, &[("DAV:", "current-user-principal")]);
        assert_eq!(got.as_deref(), Some("/dav/jelmer/"));
    }

    #[test]
    fn missing_property_returns_none() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:">
  <D:response>
    <D:href>/</D:href>
    <D:propstat>
      <D:prop><D:displayname>Home</D:displayname></D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
</D:multistatus>"#;
        let got = extract_first_property_href(xml, &[("DAV:", "current-user-principal")]);
        assert_eq!(got, None);
    }
}

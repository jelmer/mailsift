//! Generic WebDAV PUT target.
//!
//! Used by the tickets and receipts targets to upload artifacts to a
//! WebDAV collection. Shares the [`super::http_auth`] machinery with
//! the CalDAV target; both honour Basic/Negotiate challenge-driven
//! auth.
//!
//! Layout: each PUT lands at `<base_url>/<sub_path>`, where `sub_path`
//! is set by the caller (e.g. `<year>/<slug>.<ext>`). The first time a
//! PUT to a sub-collection fails with 409 we MKCOL the parent(s) and
//! retry. [`WebdavSink::update`] reads the resource, lets the caller
//! decide what should replace it, and makes the PUT conditional on the
//! resource not having changed in between (`If-Match` on its ETag), so
//! that decision can't be overtaken by another writer.
//!
//! Like CalDAV, the public entry point is sync and blocks on the
//! supplied tokio runtime handle. Each request runs through the shared
//! auth retry loop.

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::{Client, Method, StatusCode};
use tokio::runtime::Handle;
use tracing::{debug, info};

use super::http_auth::{self, Auth, Fetched};
use super::http_client::{build_client_with_timeout, truncate};
use super::json_target;
use super::sink::{FileOutcome, Merge, log_kept};

/// Everything except RFC 3986 "unreserved" characters
/// (`ALPHA / DIGIT / "-" / "." / "_" / "~"`) gets percent-encoded when
/// composing path segments. We never decode `/` so callers can pass
/// hierarchical sub-paths.
const PATH_SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~')
    .remove(b'/');

pub struct WebdavSink {
    client: Client,
    runtime: Handle,
    base_url: String,
    auth: Auth,
}

impl WebdavSink {
    /// Build a sink for the given collection URL.
    ///
    /// `user` and `password` follow the same rules as
    /// [`super::caldav::CaldavSink`]: with the `gssapi` feature both
    /// can be omitted (Kerberos from the credential cache); without the
    /// feature both are required.
    pub fn new(
        base_url: String,
        user: Option<String>,
        password: Option<String>,
        runtime: Handle,
    ) -> Result<Self> {
        if base_url.is_empty() {
            anyhow::bail!("WebDAV base URL must not be empty");
        }
        let auth = http_auth::build_auth(&base_url, user, password, "WebDAV")?;
        let client = build_client_with_timeout("WebDAV", Duration::from_secs(60))?;
        Ok(Self {
            client,
            runtime,
            base_url,
            auth,
        })
    }

    /// Replace `<base_url>/<sub_path>` with whatever `merge` makes of
    /// what is there. The PUT is conditional on the resource not having
    /// changed since it was read; if it has, start over.
    pub fn update(
        &self,
        sub_path: &str,
        content_type: &str,
        kind: &str,
        merge: &Merge<'_>,
    ) -> Result<FileOutcome> {
        let url = self.target_url(sub_path);
        for _ in 0..http_auth::MAX_UPDATE_ATTEMPTS {
            let found =
                self.runtime
                    .block_on(http_auth::get_if_exists(&self.client, &self.auth, &url))?;
            let Some(body) = merge(found.as_ref().map(|found| found.body.as_slice()))? else {
                return Ok(log_kept(url, kind));
            };
            let put = self.put_async(sub_path, content_type, &body, found.as_ref());
            if let Some(outcome) = self.runtime.block_on(put)? {
                return Ok(outcome);
            }
            debug!(url, "changed since it was read; starting over");
        }
        Err(anyhow!("{url} kept changing while being updated"))
    }

    /// PUT `body`, on condition that the resource is still as `found`.
    /// `None` when it is not.
    async fn put_async(
        &self,
        sub_path: &str,
        content_type: &str,
        body: &[u8],
        found: Option<&Fetched>,
    ) -> Result<Option<FileOutcome>> {
        let url = self.target_url(sub_path);
        let mut response = self.send_put(&url, content_type, body, found).await?;

        // 409 Conflict from a PUT typically means a parent collection
        // doesn't exist. Walk the path, MKCOL each missing parent, and
        // retry the PUT once.
        if response.status() == StatusCode::CONFLICT {
            debug!(url, "PUT 409, creating parent collections via MKCOL");
            self.ensure_parent_collections(sub_path).await?;
            response = self.send_put(&url, content_type, body, found).await?;
        }
        if response.status() == StatusCode::PRECONDITION_FAILED {
            return Ok(None);
        }
        self.classify(&url, response).await.map(Some)
    }

    /// Whether the JSON record at `<base_url>/<sub_path>`, if any, was
    /// filed from a newer message than one dated `incoming`.
    pub fn filed_from_newer(
        &self,
        sub_path: &str,
        incoming: Option<DateTime<Utc>>,
    ) -> Result<bool> {
        // Nothing to order by; spare the round trip.
        if incoming.is_none() {
            return Ok(false);
        }
        let url = self.target_url(sub_path);
        Ok(self
            .runtime
            .block_on(http_auth::get_if_exists(&self.client, &self.auth, &url))?
            .is_some_and(|found| json_target::is_from_newer(&found.body, &url, incoming)))
    }

    async fn classify(&self, url: &str, response: reqwest::Response) -> Result<FileOutcome> {
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "WebDAV PUT to {url} returned {status}: {}",
                truncate(&body, 200)
            ));
        }
        match status {
            StatusCode::CREATED => {
                info!(target = %url, "uploaded");
                Ok(FileOutcome::Created(url.to_string()))
            }
            _ => {
                info!(target = %url, %status, "replaced");
                Ok(FileOutcome::Updated(url.to_string()))
            }
        }
    }

    async fn send_put(
        &self,
        url: &str,
        content_type: &str,
        body: &[u8],
        found: Option<&Fetched>,
    ) -> Result<reqwest::Response> {
        let content_type = content_type.to_string();
        let body = body.to_vec();
        http_auth::send_with_auth_retry(&self.client, &self.auth, |client| {
            let request = client
                .put(url)
                .header(reqwest::header::CONTENT_TYPE, content_type.clone())
                .body(body.clone());
            http_auth::unless_changed(request, found)
        })
        .await
        .with_context(|| format!("PUT {url}"))
    }

    /// MKCOL each ancestor collection in `sub_path` that doesn't exist
    /// yet. Idempotent: a `405 Method Not Allowed` response (which most
    /// servers return for an MKCOL on an existing collection) is
    /// treated as success.
    async fn ensure_parent_collections(&self, sub_path: &str) -> Result<()> {
        let segments: Vec<&str> = sub_path.split('/').collect();
        if segments.len() <= 1 {
            // No parent segments to create.
            return Ok(());
        }
        // Build up the path piece by piece, MKCOLing each level.
        let mut accumulated = String::new();
        for segment in &segments[..segments.len() - 1] {
            if segment.is_empty() {
                continue;
            }
            if !accumulated.is_empty() {
                accumulated.push('/');
            }
            accumulated.push_str(segment);
            let url = self.target_url(&accumulated);
            let response = http_auth::send_with_auth_retry(&self.client, &self.auth, |client| {
                client.request(Method::from_bytes(b"MKCOL").expect("MKCOL is valid"), &url)
            })
            .await
            .with_context(|| format!("MKCOL {url}"))?;
            let status = response.status();
            // Treat "already exists" / "method not allowed on a
            // collection that exists" as a successful no-op.
            if status.is_success() || status == StatusCode::METHOD_NOT_ALLOWED {
                debug!(url, %status, "mkcol ok");
                continue;
            }
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "MKCOL {url} returned {status}: {}",
                truncate(&body, 200)
            ));
        }
        Ok(())
    }

    /// Compose `<base_url>/<sub_path>`, percent-encoding the sub-path
    /// while preserving `/` separators.
    pub(super) fn target_url(&self, sub_path: &str) -> String {
        let sep = if self.base_url.ends_with('/') {
            ""
        } else {
            "/"
        };
        let trimmed = sub_path.trim_start_matches('/');
        let encoded = utf8_percent_encode(trimmed, PATH_SEGMENT);
        format!("{}{}{encoded}", self.base_url, sep)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::runtime::Runtime;

    fn test_handle() -> Handle {
        use std::sync::OnceLock;
        static RT: OnceLock<Runtime> = OnceLock::new();
        RT.get_or_init(|| Runtime::new().unwrap()).handle().clone()
    }

    fn sink(base: &str) -> WebdavSink {
        WebdavSink::new(
            base.into(),
            Some("u".into()),
            Some("p".into()),
            test_handle(),
        )
        .unwrap()
    }

    use crate::targets::fake_dav::FakeDav;

    fn at(rfc3339: &str) -> Option<DateTime<Utc>> {
        Some(rfc3339.parse().unwrap())
    }

    #[test]
    fn filed_from_newer_compares_against_the_stored_record() {
        let server = FakeDav::start();
        let s = server.webdav_sink();
        server.put(
            "/2026/acme-1.json",
            br#"{"receivedAt":"2026-01-28T10:00:00Z"}"#,
        );

        assert!(
            s.filed_from_newer("2026/acme-1.json", at("2025-10-28T10:00:00Z"))
                .unwrap()
        );
        assert!(
            !s.filed_from_newer("2026/acme-1.json", at("2026-01-28T10:00:00Z"))
                .unwrap()
        );
        assert!(
            !s.filed_from_newer("2026/acme-1.json", at("2026-02-01T10:00:00Z"))
                .unwrap()
        );
    }

    fn record(received_at: &str) -> Vec<u8> {
        format!(r#"{{"receivedAt":"{received_at}"}}"#).into_bytes()
    }

    /// File `body` at `2026/acme-1.json` unless what is there is newer.
    fn file(s: &WebdavSink, body: &[u8]) -> FileOutcome {
        let incoming = json_target::received_at_in(body).unwrap();
        s.update(
            "2026/acme-1.json",
            "application/json",
            "receipt",
            &json_target::replace_unless_newer("acme-1", body, incoming),
        )
        .unwrap()
    }

    #[test]
    fn update_creates_then_replaces() {
        let server = FakeDav::start();
        let s = server.webdav_sink();
        let first = file(&s, &record("2026-01-20T10:00:00Z"));
        assert!(matches!(first, FileOutcome::Created(_)));
        let second = file(&s, &record("2026-01-28T10:00:00Z"));
        assert!(matches!(second, FileOutcome::Updated(_)));
        assert_eq!(
            server.resource("/2026/acme-1.json").unwrap(),
            record("2026-01-28T10:00:00Z")
        );
    }

    /// Another writer files a newer record between our read and our
    /// write. Our PUT must not land on top of it.
    #[test]
    fn update_does_not_overwrite_a_newer_record_that_overtook_it() {
        let server = FakeDav::start();
        let s = server.webdav_sink();
        server.put("/2026/acme-1.json", &record("2026-01-20T10:00:00Z"));
        server.overtake_after_next_get("/2026/acme-1.json", &record("2026-01-28T10:00:00Z"));

        let outcome = file(&s, &record("2026-01-25T10:00:00Z"));
        assert!(matches!(outcome, FileOutcome::Kept(_)));
        assert_eq!(
            server.resource("/2026/acme-1.json").unwrap(),
            record("2026-01-28T10:00:00Z")
        );
        assert_eq!(
            server.requests(),
            vec![
                "GET /2026/acme-1.json",
                "PUT /2026/acme-1.json",
                "GET /2026/acme-1.json",
            ]
        );
    }

    /// The same, where nothing was there when we looked.
    #[test]
    fn update_does_not_overwrite_a_newer_record_created_under_it() {
        let server = FakeDav::start();
        let s = server.webdav_sink();
        server.overtake_after_next_get("/2026/acme-1.json", &record("2026-01-28T10:00:00Z"));

        let outcome = file(&s, &record("2026-01-25T10:00:00Z"));
        assert!(matches!(outcome, FileOutcome::Kept(_)));
        assert_eq!(
            server.resource("/2026/acme-1.json").unwrap(),
            record("2026-01-28T10:00:00Z")
        );
    }

    /// An older record overtaking us only costs a second attempt.
    #[test]
    fn update_replaces_an_older_record_that_overtook_it() {
        let server = FakeDav::start();
        let s = server.webdav_sink();
        server.put("/2026/acme-1.json", &record("2026-01-20T10:00:00Z"));
        server.overtake_after_next_get("/2026/acme-1.json", &record("2026-01-22T10:00:00Z"));

        let outcome = file(&s, &record("2026-01-25T10:00:00Z"));
        assert!(matches!(outcome, FileOutcome::Updated(_)));
        assert_eq!(
            server.resource("/2026/acme-1.json").unwrap(),
            record("2026-01-25T10:00:00Z")
        );
    }

    #[test]
    fn filed_from_newer_is_false_when_nothing_is_stored() {
        let server = FakeDav::start();
        let s = server.webdav_sink();
        assert!(
            !s.filed_from_newer("2026/acme-1.json", at("2025-10-28T10:00:00Z"))
                .unwrap()
        );
        assert_eq!(server.requests(), vec!["GET /2026/acme-1.json"]);
    }

    #[test]
    fn filed_from_newer_skips_the_request_for_an_undated_message() {
        let server = FakeDav::start();
        let s = server.webdav_sink();
        assert!(!s.filed_from_newer("2026/acme-1.json", None).unwrap());
        assert_eq!(server.requests(), Vec::<String>::new());
    }

    #[test]
    fn target_url_with_trailing_slash() {
        let s = sink("https://dav.example.org/files/");
        assert_eq!(
            s.target_url("2024/foo.pdf"),
            "https://dav.example.org/files/2024/foo.pdf"
        );
    }

    #[test]
    fn target_url_without_trailing_slash() {
        let s = sink("https://dav.example.org/files");
        assert_eq!(
            s.target_url("2024/foo.pdf"),
            "https://dav.example.org/files/2024/foo.pdf"
        );
    }

    #[test]
    fn target_url_strips_leading_slash_on_sub_path() {
        let s = sink("https://dav.example.org/files/");
        assert_eq!(
            s.target_url("/2024/foo.pdf"),
            "https://dav.example.org/files/2024/foo.pdf"
        );
    }

    #[test]
    fn target_url_percent_encodes_special_chars() {
        let s = sink("https://dav.example.org/files/");
        // `@` and ` ` get encoded; `/` and `.` stay as-is.
        assert_eq!(
            s.target_url("2024/order@123.json"),
            "https://dav.example.org/files/2024/order%40123.json"
        );
    }

    #[test]
    fn empty_base_url_is_rejected() {
        let result = WebdavSink::new("".into(), Some("u".into()), Some("p".into()), test_handle());
        let err = match result {
            Ok(_) => panic!("expected error"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("must not be empty"), "{err}");
    }

    #[test]
    fn username_without_password_is_rejected() {
        let result = WebdavSink::new(
            "https://dav.example.org/files/".into(),
            Some("u".into()),
            None,
            test_handle(),
        );
        let err = match result {
            Ok(_) => panic!("expected error"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("without a password"), "{err}");
    }
}

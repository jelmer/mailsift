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
//! retry. PUT semantics are idempotent: an existing resource at the
//! same name is replaced.
//!
//! Like CalDAV, the public entry point is sync and blocks on the
//! supplied tokio runtime handle. Each request runs through the shared
//! auth retry loop.

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::{Client, Method, StatusCode};
use tokio::runtime::Handle;
use tracing::{debug, info};

use super::http_auth::{self, Auth};
use super::http_client::{build_client_with_timeout, truncate};

/// What we did with a PUT.
#[derive(Debug)]
pub enum PutOutcome {
    /// The server created the resource (2xx including `201 Created`).
    Created(String),
    /// The server replaced an existing resource (`200 OK`/`204 No Content`).
    Updated(String),
}

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

    /// PUT a blob to `<base_url>/<sub_path>` with the given Content-Type.
    /// `body` is moved in; callers that want to reuse it should clone
    /// before calling.
    pub fn put(&self, sub_path: &str, content_type: &str, body: Vec<u8>) -> Result<PutOutcome> {
        self.runtime
            .block_on(self.put_async(sub_path, content_type, body))
    }

    async fn put_async(
        &self,
        sub_path: &str,
        content_type: &str,
        body: Vec<u8>,
    ) -> Result<PutOutcome> {
        let url = self.target_url(sub_path);
        let response = self.send_put(&url, content_type, &body).await?;
        let status = response.status();

        // 409 Conflict from a PUT typically means a parent collection
        // doesn't exist. Walk the path, MKCOL each missing parent, and
        // retry the PUT once.
        if status == StatusCode::CONFLICT {
            debug!(url, "PUT 409, creating parent collections via MKCOL");
            self.ensure_parent_collections(sub_path).await?;
            let response = self.send_put(&url, content_type, &body).await?;
            return self.classify(&url, response).await;
        }

        self.classify(&url, response).await
    }

    async fn classify(&self, url: &str, response: reqwest::Response) -> Result<PutOutcome> {
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
                Ok(PutOutcome::Created(url.to_string()))
            }
            _ => {
                info!(target = %url, %status, "replaced");
                Ok(PutOutcome::Updated(url.to_string()))
            }
        }
    }

    async fn send_put(
        &self,
        url: &str,
        content_type: &str,
        body: &[u8],
    ) -> Result<reqwest::Response> {
        let content_type = content_type.to_string();
        let body = body.to_vec();
        http_auth::send_with_auth_retry(&self.client, &self.auth, |client| {
            client
                .put(url)
                .header(reqwest::header::CONTENT_TYPE, content_type.clone())
                .body(body.clone())
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
    fn target_url(&self, sub_path: &str) -> String {
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

//! Error type shared by the handlers, plus the path-validation helpers
//! that produce one.

use std::io;
use std::path::{Component, Path, PathBuf};

use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};

use super::AppState;
use super::render::{esc, page};

/// Wraps `anyhow::Error` with an HTTP status. Bad requests and
/// missing resources get their own status; everything else falls
/// through as `500`.
pub(super) struct AppError {
    pub(super) status: StatusCode,
    pub(super) err: anyhow::Error,
}

impl AppError {
    fn bad_request(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            err: anyhow::anyhow!(msg.into()),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        // Log the full detail server-side; render a generic reason to
        // the client so we don't leak filesystem paths or config keys.
        if self.status.is_server_error() {
            tracing::warn!(error = format!("{:#}", self.err), "web request failed");
        }
        let reason = self.status.canonical_reason().unwrap_or("Error");
        let state = AppState::default();
        (
            self.status,
            Html(page(&state, reason, &format!("<p>{}</p>", esc(reason)))),
        )
            .into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(err: E) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            err: err.into(),
        }
    }
}

/// Map a filesystem error to a status code: NotFound -> 404, else 500.
pub(super) fn read_status(path: &Path, err: io::Error) -> AppError {
    let status = if err.kind() == io::ErrorKind::NotFound {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    AppError {
        status,
        err: anyhow::Error::from(err).context(format!("reading {}", path.display())),
    }
}

pub(super) fn require_dir<'a>(dir: Option<&'a Path>, label: &str) -> Result<&'a Path, AppError> {
    dir.ok_or_else(|| AppError {
        status: StatusCode::NOT_FOUND,
        err: anyhow::anyhow!("no {label} directory configured; set {label}_dir in your config"),
    })
}

/// Reject `.`, `..`, absolute paths, and anything that would traverse
/// outside the artifact dir. Applied to every path segment coming off
/// the URL before we join it onto a filesystem path.
pub(super) fn safe_segment(seg: &str) -> Result<&str, AppError> {
    if seg.is_empty()
        || seg == "."
        || seg == ".."
        || seg.contains('/')
        || seg.contains('\\')
        || seg.contains('\0')
    {
        return Err(AppError::bad_request("invalid path segment"));
    }
    // Belt-and-braces: even though the above catches slashes, run the
    // segment through PathBuf::components() to make sure nothing weird
    // survives (e.g. platform-specific traversal).
    let pb = PathBuf::from(seg);
    if pb.components().count() != 1 || !matches!(pb.components().next(), Some(Component::Normal(_)))
    {
        return Err(AppError::bad_request("invalid path segment"));
    }
    Ok(seg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_segment_rejects_traversal() {
        assert!(safe_segment("..").is_err());
        assert!(safe_segment("a/b").is_err());
        assert!(safe_segment("").is_err());
        assert!(safe_segment("ok.json").is_ok());
    }
}

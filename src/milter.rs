//! Postfix milter front-end.
//!
//! Listens on a Unix or TCP socket, accumulates each message in memory,
//! and hands it to the extraction pipeline at end-of-message. Always
//! returns `Continue` (accept); extraction is best-effort and must not
//! block mail delivery.

use std::ffi::CString;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use bytes::Bytes;
use indymilter::{Callbacks, EomContext, Status};
use tokio::net::{TcpListener, UnixListener};
use tokio::signal;
use tracing::{info, warn};

use crate::pipeline::{self, OwnedTargets};

#[derive(Clone)]
pub struct MilterConfig {
    pub extractors: Arc<Vec<crate::extractor::Extractor>>,
    pub targets: OwnedTargets,
    pub deadline: Duration,
}

/// Per-connection state. One message is processed at a time per
/// connection, so we just accumulate into a single buffer that gets
/// drained at EOM.
#[derive(Default)]
pub struct MessageState {
    /// Reconstructed RFC822 bytes: headers, then a blank line, then body.
    buf: Vec<u8>,
    /// Have we emitted the header/body separator yet?
    headers_done: bool,
}

impl MessageState {
    fn push_header(&mut self, name: &CString, value: &CString) {
        self.buf.extend_from_slice(name.as_bytes());
        self.buf.extend_from_slice(b": ");
        self.buf.extend_from_slice(value.as_bytes());
        self.buf.extend_from_slice(b"\r\n");
    }

    fn finish_headers(&mut self) {
        if !self.headers_done {
            self.buf.extend_from_slice(b"\r\n");
            self.headers_done = true;
        }
    }

    fn push_body(&mut self, chunk: &[u8]) {
        self.finish_headers();
        self.buf.extend_from_slice(chunk);
    }

    fn take(&mut self) -> Vec<u8> {
        self.finish_headers();
        std::mem::take(&mut self.buf)
    }

    fn reset(&mut self) {
        self.buf.clear();
        self.headers_done = false;
    }
}

pub async fn run(socket: &str, config: MilterConfig) -> Result<()> {
    let config = Arc::new(config);

    let callbacks = build_callbacks(Arc::clone(&config));
    let milter_config = Default::default();

    info!(socket = %socket, "milter listening");

    if let Some(path) = socket.strip_prefix("unix:") {
        // Best-effort cleanup of stale socket from a previous run.
        let _ = std::fs::remove_file(path);
        let listener =
            UnixListener::bind(path).with_context(|| format!("binding unix socket {path}"))?;
        indymilter::run(listener, callbacks, milter_config, signal::ctrl_c())
            .await
            .context("milter loop")?;
    } else if let Some(addr) = socket.strip_prefix("tcp:") {
        let listener = TcpListener::bind(addr)
            .await
            .with_context(|| format!("binding tcp {addr}"))?;
        indymilter::run(listener, callbacks, milter_config, signal::ctrl_c())
            .await
            .context("milter loop")?;
    } else {
        bail!("socket must be 'unix:<path>' or 'tcp:<host>:<port>', got {socket:?}");
    }

    Ok(())
}

pub fn build_callbacks(config: Arc<MilterConfig>) -> Callbacks<MessageState> {
    // Each stage we want to receive must be registered, otherwise
    // negotiation strips it. We need headers and body; we register stub
    // handlers for HELO/RCPT/DATA so the MTA can go through them
    // without confusion.
    Callbacks::<MessageState>::new()
        .on_connect(|_cx, _hostname, _socket_info| Box::pin(async move { Status::Continue }))
        .on_helo(|_cx, _hostname| Box::pin(async move { Status::Continue }))
        .on_rcpt(|_cx, _args| Box::pin(async move { Status::Continue }))
        .on_data(|_cx| Box::pin(async move { Status::Continue }))
        .on_mail(|cx, _args| {
            Box::pin(async move {
                if let Some(state) = cx.data.as_mut() {
                    state.reset();
                }
                Status::Continue
            })
        })
        .on_header(|cx, name, value| {
            Box::pin(async move {
                let state = cx.data.get_or_insert_with(MessageState::default);
                state.push_header(&name, &value);
                Status::Continue
            })
        })
        .on_eoh(|cx| {
            Box::pin(async move {
                let state = cx.data.get_or_insert_with(MessageState::default);
                state.finish_headers();
                Status::Continue
            })
        })
        .on_body(|cx, chunk: Bytes| {
            Box::pin(async move {
                let state = cx.data.get_or_insert_with(MessageState::default);
                state.push_body(&chunk);
                Status::Continue
            })
        })
        .on_eom({
            let config = Arc::clone(&config);
            move |cx| {
                let config = Arc::clone(&config);
                Box::pin(async move { handle_eom(cx, config).await })
            }
        })
        .on_abort(|cx| {
            Box::pin(async move {
                if let Some(state) = cx.data.as_mut() {
                    state.reset();
                }
                Status::Continue
            })
        })
}

async fn handle_eom(cx: &mut EomContext<MessageState>, config: Arc<MilterConfig>) -> Status {
    let raw = match cx.data.as_mut() {
        Some(state) => state.take(),
        None => {
            warn!("EOM with no accumulated message state");
            return Status::Continue;
        }
    };

    let deadline = config.deadline;
    let extractors = Arc::clone(&config.extractors);
    let targets = config.targets.clone();

    // Off-load to a blocking thread: the pipeline spawns subprocesses and
    // does sync I/O. Cap the whole thing with a hard wall-clock deadline.
    // Milter front-end sees mail before our MTA's DKIM check has run,
    // so we can't enforce `require_dkim` here. Skip the check; extractors
    // that need DKIM only fire in replay / imap-scan / maildir-watch.
    let work = tokio::task::spawn_blocking(move || {
        pipeline::run(
            &raw,
            "milter",
            &extractors,
            targets.borrowed(),
            pipeline::DkimPolicy::Skip,
            false,
            None,
        )
    });

    match tokio::time::timeout(deadline, work).await {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(e))) => warn!(error = %e, "pipeline failed"),
        Ok(Err(e)) => warn!(error = %e, "pipeline task panicked"),
        Err(_) => warn!(
            deadline_secs = deadline.as_secs(),
            "pipeline deadline exceeded; mail accepted without extraction"
        ),
    }

    Status::Continue
}

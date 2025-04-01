//! Local-directory target for `event` artifacts.
//!
//! Each event is filed as `<UID>.ics` under the configured directory.
//! Existing files are overwritten; same semantics as a CalDAV PUT by
//! UID.

use std::path::Path;

use anyhow::Result;
use tracing::info;

use super::sink::{sanitize_uid, write_atomic};
use super::{FileOutcome, SingleEvent};

pub fn file_single(event: &SingleEvent, dir: &Path) -> Result<FileOutcome> {
    let target = dir.join(sanitize_uid(&event.uid)).with_extension("ics");
    let existed = target.exists();
    write_atomic(&target, event.body.as_bytes())?;
    let label = target.display().to_string();
    if existed {
        info!(target = %label, "event updated");
        Ok(FileOutcome::Updated(label))
    } else {
        info!(target = %label, "event created");
        Ok(FileOutcome::Created(label))
    }
}

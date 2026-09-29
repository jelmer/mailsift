//! Local-directory target for `event` artifacts.
//!
//! Each event is filed as `<UID>.ics` under the configured directory.
//! Existing files are overwritten; same semantics as a CalDAV PUT by
//! UID.

use std::path::Path;

use anyhow::Result;

use super::sink::{log_file_outcome, sanitize_uid, write_atomic};
use super::{FileOutcome, SingleEvent};

pub fn file_single(event: &SingleEvent, dir: &Path) -> Result<FileOutcome> {
    let target = dir.join(sanitize_uid(&event.uid)).with_extension("ics");
    let existed = target.exists();
    write_atomic(&target, event.body.as_bytes())?;
    Ok(log_file_outcome(&target, existed, "event"))
}

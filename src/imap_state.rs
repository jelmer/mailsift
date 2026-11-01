//! Where `imap-scan --watch` got to in each mailbox, so a restart
//! carries on from there instead of rescanning the whole mailbox.
//!
//! One small JSON file per account and mailbox under
//! `$XDG_STATE_HOME/mailsift/imap/`, replaced atomically on every
//! update. A cursor only means something for the UIDVALIDITY it was
//! recorded under; when the server reports a different one the mailbox
//! was renumbered and the cursor is ignored.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    /// The account and mailbox this is for, for whoever reads the file.
    pub mailbox: String,
    pub uid_validity: u32,
    /// Every message up to and including this UID has been processed.
    pub last_uid: u32,
}

impl Cursor {
    /// The UID to resume after, if this cursor applies to a mailbox the
    /// server currently reports with `uid_validity`.
    pub fn resume_after(&self, uid_validity: Option<u32>) -> Option<u32> {
        (Some(self.uid_validity) == uid_validity).then_some(self.last_uid)
    }
}

pub struct CursorFile {
    path: PathBuf,
    mailbox: String,
}

impl CursorFile {
    /// The cursor file for `mailbox` (an account-qualified name such as
    /// `user@host:993/INBOX`) under the default state directory.
    pub fn for_mailbox(mailbox: &str) -> Option<Self> {
        let base = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
            .or_else(|| {
                std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("state"))
            })?;
        let digest = Sha256::digest(mailbox.as_bytes());
        let name: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
        Some(Self::at(
            base.join("mailsift")
                .join("imap")
                .join(format!("{name}.json")),
            mailbox,
        ))
    }

    pub fn at(path: PathBuf, mailbox: &str) -> Self {
        Self {
            path,
            mailbox: mailbox.to_string(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The saved cursor, or `None` when nothing has been saved yet. A
    /// file that exists but can't be read is an error: guessing would
    /// mean either skipping mail or rescanning everything.
    pub fn load(&self) -> Result<Option<Cursor>> {
        let body = match fs::read(&self.path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(e).with_context(|| format!("reading {}", self.path.display()));
            }
        };
        let cursor = serde_json::from_slice(&body).with_context(|| {
            format!(
                "parsing {}; remove it, or pass --rescan, to start over",
                self.path.display()
            )
        })?;
        Ok(Some(cursor))
    }

    pub fn save(&self, uid_validity: u32, last_uid: u32) -> Result<()> {
        let cursor = Cursor {
            mailbox: self.mailbox.clone(),
            uid_validity,
            last_uid,
        };
        let dir = self
            .path
            .parent()
            .with_context(|| format!("{} has no parent directory", self.path.display()))?;
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let mut tmp = tempfile::NamedTempFile::new_in(dir)
            .with_context(|| format!("creating a temporary file in {}", dir.display()))?;
        serde_json::to_writer_pretty(&mut tmp, &cursor)?;
        tmp.write_all(b"\n")?;
        tmp.persist(&self.path)
            .with_context(|| format!("replacing {}", self.path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cursor(last_uid: u32) -> Cursor {
        Cursor {
            mailbox: "me@imap.example.org:993/INBOX".to_string(),
            uid_validity: 7,
            last_uid,
        }
    }

    #[test]
    fn round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let file = CursorFile::at(
            dir.path().join("imap").join("x.json"),
            "me@imap.example.org:993/INBOX",
        );
        assert_eq!(file.load().unwrap(), None);
        file.save(7, 41).unwrap();
        file.save(7, 42).unwrap();
        assert_eq!(file.load().unwrap(), Some(cursor(42)));
    }

    #[test]
    fn a_corrupt_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.json");
        fs::write(&path, "{not json").unwrap();
        let err = CursorFile::at(path, "x").load().unwrap_err();
        assert!(format!("{err:#}").contains("--rescan"), "{err:#}");
    }

    #[test]
    fn resumes_only_under_the_same_uidvalidity() {
        assert_eq!(cursor(42).resume_after(Some(7)), Some(42));
        assert_eq!(cursor(42).resume_after(Some(8)), None);
        assert_eq!(cursor(42).resume_after(None), None);
    }

    #[test]
    fn mailboxes_get_separate_files() {
        let a = CursorFile::for_mailbox("me@host:993/INBOX").unwrap();
        let b = CursorFile::for_mailbox("me@host:993/Archive").unwrap();
        assert_ne!(a.path(), b.path());
    }
}

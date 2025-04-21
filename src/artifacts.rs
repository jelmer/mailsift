use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Event,
    Reservation,
    Bill,
    Parcel,
    Receipt,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Event => "event",
            Kind::Reservation => "reservation",
            Kind::Bill => "bill",
            Kind::Parcel => "parcel",
            Kind::Receipt => "receipt",
        }
    }
}

#[derive(Debug)]
#[allow(dead_code)] // slug/ext used by additional kinds in later milestones
pub struct Artifact {
    pub kind: Kind,
    pub path: PathBuf,
    /// Base name before the kind suffix (e.g. "flight-fr1234" for
    /// "flight-fr1234.event.ics").
    pub slug: String,
    /// Extension after the kind marker. For event this is fixed.
    pub ext: String,
}

#[derive(Debug, Default, Deserialize)]
#[allow(dead_code)] // annotations passed through to logs in later milestones
pub struct Manifest {
    #[serde(default)]
    pub notes: Vec<String>,
    #[serde(default)]
    pub annotations: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug)]
pub struct ScanResult {
    pub artifacts: Vec<Artifact>,
    pub manifest: Option<Manifest>,
}

/// Scan a directory for artifacts written by an extractor.
///
/// Recognises files of the form `<slug>.<kind>.<ext>` for the known kinds.
/// `_manifest.json` is parsed if present. Files starting with `.` or `_`
/// (other than `_manifest.json`) are ignored silently; files that don't
/// match a known suffix produce a warning via the returned `unknown` list.
pub fn scan(dir: &Path) -> Result<(ScanResult, Vec<PathBuf>)> {
    let mut artifacts = Vec::new();
    let mut manifest = None;
    let mut unknown = Vec::new();

    let entries = fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?;
    for entry in entries {
        let entry = entry?;
        let ft = entry.file_type()?;
        if !ft.is_file() {
            continue;
        }
        let path = entry.path();
        let name = match entry.file_name().into_string() {
            Ok(n) => n,
            Err(_) => continue,
        };

        if name == "_manifest.json" {
            let body = fs::read_to_string(&path)
                .with_context(|| format!("reading manifest {}", path.display()))?;
            manifest = Some(
                serde_json::from_str(&body)
                    .with_context(|| format!("parsing manifest {}", path.display()))?,
            );
            continue;
        }
        if name.starts_with('.') || name.starts_with('_') {
            continue;
        }

        match classify(&name) {
            Some((kind, slug, ext)) => artifacts.push(Artifact {
                kind,
                path,
                slug,
                ext,
            }),
            None => unknown.push(path),
        }
    }

    Ok((
        ScanResult {
            artifacts,
            manifest,
        },
        unknown,
    ))
}

/// Parse a filename into (kind, slug, ext).
fn classify(name: &str) -> Option<(Kind, String, String)> {
    for (suffix, kind, ext) in [
        (".event.ics", Kind::Event, "ics"),
        (".reservation.json", Kind::Reservation, "json"),
        (".bill.json", Kind::Bill, "json"),
        (".parcel.json", Kind::Parcel, "json"),
        (".receipt.json", Kind::Receipt, "json"),
    ] {
        if let Some(stem) = name.strip_suffix(suffix) {
            if stem.is_empty() {
                return None;
            }
            return Some((kind, stem.to_string(), ext.to_string()));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_event() {
        let (kind, slug, ext) = classify("flight-fr1234.event.ics").unwrap();
        assert_eq!(kind, Kind::Event);
        assert_eq!(slug, "flight-fr1234");
        assert_eq!(ext, "ics");
    }

    #[test]
    fn classify_unknown() {
        assert!(classify("random.txt").is_none());
        assert!(classify(".event.ics").is_none()); // empty slug
    }
}

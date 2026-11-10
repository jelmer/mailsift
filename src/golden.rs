//! Golden-file tests for extractors (`mailsift test-extractor`).
//!
//! Each fixture message `<stem>.eml` has a `<stem>.expected.json` next
//! to it naming the extractor to run and the artifacts it should write.
//! Artifact values are the parsed JSON for `.json` files, the text for
//! other UTF-8 files and `{"sha256": ...}` for binary blobs.

use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::extractor::{self, Extractor};
use crate::{dkim, pipeline};

/// Contents of a `<stem>.expected.json` file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expected {
    pub extractor: String,
    pub artifacts: BTreeMap<String, Value>,
}

impl Expected {
    fn to_bytes(&self) -> Result<Vec<u8>> {
        let mut out = serde_json::to_vec_pretty(self).context("serialising expected file")?;
        out.push(b'\n');
        Ok(out)
    }
}

/// The expected file for `fixture`: `<stem>.expected.json` beside it.
pub fn expected_path(fixture: &Path) -> Result<PathBuf> {
    let stem = fixture
        .file_stem()
        .ok_or_else(|| anyhow!("{} has no file name", fixture.display()))?;
    let mut name = stem.to_owned();
    name.push(".expected.json");
    Ok(fixture.with_file_name(name))
}

/// Why the manifest's dispatch rules would keep `ex` from ever seeing
/// `raw`, or `None` if it would run. Mirrors the checks in
/// [`pipeline::run`]; `require_dkim` is only checked when `check_dkim`
/// is set, since saved fixtures rarely carry `Authentication-Results:`.
pub fn dispatch_refusal(ex: &Extractor, raw: &[u8], check_dkim: bool) -> Option<String> {
    if raw.len() > pipeline::MAX_MESSAGE_BYTES {
        return Some(format!(
            "message is larger than the {} byte limit",
            pipeline::MAX_MESSAGE_BYTES
        ));
    }
    let (from_domain, subject) = pipeline::match_headers_from_raw(raw);
    if let Some(why) = ex.header_mismatch(from_domain.as_deref(), subject.as_deref()) {
        return Some(why);
    }
    // Like the pipeline, a message whose MIME tree doesn't parse skips
    // the body-shape check and is left to the extractor.
    if let Ok(mail) = mailparse::parse_mail(raw) {
        let unmet = ex.unmet_body_requirements(&pipeline::body_parts_from_mail(mail));
        if !unmet.is_empty() {
            return Some(format!("message lacks requires: {}", unmet.join(", ")));
        }
    }
    if check_dkim && !ex.require_dkim.is_empty() {
        let headers = mailparse::parse_headers(raw)
            .map(|(headers, _)| headers)
            .unwrap_or_default();
        let passing = dkim::passing_dkim_domains(&headers);
        if !dkim::satisfies(&ex.require_dkim, &passing, from_domain.as_deref()) {
            let mut passing: Vec<&str> = passing.iter().map(String::as_str).collect();
            passing.sort_unstable();
            return Some(format!(
                "no passing DKIM signature matches require_dkim [{}] (passing: [{}])",
                ex.require_dkim.join(", "),
                passing.join(", ")
            ));
        }
    }
    None
}

/// Render an artifact file as it is stored in an expected file.
pub fn render_artifact(name: &str, bytes: &[u8]) -> Result<Value> {
    if name.ends_with(".json") {
        return serde_json::from_slice(bytes).with_context(|| format!("{name} is not valid JSON"));
    }
    Ok(match std::str::from_utf8(bytes) {
        Ok(text) => Value::String(text.to_string()),
        Err(_) => {
            let digest: String = Sha256::digest(bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            json!({ "sha256": digest })
        }
    })
}

/// Run `ex` over `raw` and render every artifact it writes. Files
/// mailsift would not recognise are an error, since they would be
/// dropped in production.
pub fn produce(ex: &Extractor, raw: &[u8], timeout: Duration) -> Result<BTreeMap<String, Value>> {
    let run = extractor::run_one(ex, raw, timeout)?;
    if !run.unknown_files.is_empty() {
        let names: Vec<String> = run
            .unknown_files
            .iter()
            .map(|p| {
                p.file_name()
                    .unwrap_or(p.as_os_str())
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        bail!(
            "wrote unrecognised files mailsift would ignore: {}",
            names.join(", ")
        );
    }
    let mut out = BTreeMap::new();
    for artifact in &run.result.artifacts {
        let name = artifact
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| anyhow!("artifact {} has no UTF-8 name", artifact.path.display()))?;
        let bytes = fs::read(&artifact.path).with_context(|| format!("reading artifact {name}"))?;
        out.insert(name.to_string(), render_artifact(name, &bytes)?);
    }
    Ok(out)
}

/// One way the produced artifacts differ from the expected ones.
#[derive(Debug, PartialEq, Eq)]
pub enum ArtifactDiff {
    Missing(String),
    Unexpected(String),
    /// `details` lists the differing JSON values, or a unified diff of
    /// the text.
    Changed {
        name: String,
        details: Vec<String>,
    },
}

pub fn compare(
    expected: &BTreeMap<String, Value>,
    actual: &BTreeMap<String, Value>,
) -> Vec<ArtifactDiff> {
    let mut names: Vec<&String> = expected.keys().chain(actual.keys()).collect();
    names.sort_unstable();
    names.dedup();
    names
        .into_iter()
        .filter_map(|name| match (expected.get(name), actual.get(name)) {
            (Some(_), None) => Some(ArtifactDiff::Missing(name.clone())),
            (None, Some(_)) => Some(ArtifactDiff::Unexpected(name.clone())),
            (Some(e), Some(a)) if e != a => Some(ArtifactDiff::Changed {
                name: name.clone(),
                details: describe_change(e, a),
            }),
            _ => None,
        })
        .collect()
}

fn describe_change(expected: &Value, actual: &Value) -> Vec<String> {
    if let (Value::String(e), Value::String(a)) = (expected, actual) {
        return text_diff(e, a);
    }
    let mut out = Vec::new();
    json_diff("$", expected, actual, &mut out);
    out
}

fn json_diff(path: &str, expected: &Value, actual: &Value, out: &mut Vec<String>) {
    match (expected, actual) {
        (Value::Object(e), Value::Object(a)) => {
            for (key, ev) in e {
                let p = format!("{path}.{key}");
                match a.get(key) {
                    Some(av) => json_diff(&p, ev, av, out),
                    None => out.push(format!("{p}: missing, expected {ev}")),
                }
            }
            for (key, av) in a {
                if !e.contains_key(key) {
                    out.push(format!("{path}.{key}: unexpected {av}"));
                }
            }
        }
        (Value::Array(e), Value::Array(a)) => {
            for i in 0..e.len().max(a.len()) {
                let p = format!("{path}[{i}]");
                match (e.get(i), a.get(i)) {
                    (Some(ev), Some(av)) => json_diff(&p, ev, av, out),
                    (Some(ev), None) => out.push(format!("{p}: missing, expected {ev}")),
                    (None, Some(av)) => out.push(format!("{p}: unexpected {av}")),
                    (None, None) => unreachable!("index below the longer array's length"),
                }
            }
        }
        _ if expected != actual => out.push(format!("{path}: expected {expected}, got {actual}")),
        _ => {}
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Line<'a> {
    Same(&'a str),
    Removed(&'a str),
    Added(&'a str),
}

/// Above this many LCS table cells (after trimming the common prefix
/// and suffix) the differing middle is shown as removed then added.
const MAX_LCS_CELLS: usize = 4_000_000;

const DIFF_CONTEXT: usize = 3;

fn diff_lines<'a>(old: &[&'a str], new: &[&'a str]) -> Vec<Line<'a>> {
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let a = &old[prefix..old.len() - suffix];
    let b = &new[prefix..new.len() - suffix];

    let mut out: Vec<Line> = old[..prefix].iter().map(|l| Line::Same(l)).collect();
    if a.len().saturating_mul(b.len()) > MAX_LCS_CELLS {
        out.extend(a.iter().map(|l| Line::Removed(l)));
        out.extend(b.iter().map(|l| Line::Added(l)));
    } else {
        // lcs[i * w + j] is the LCS length of a[i..] and b[j..].
        let w = b.len() + 1;
        let mut lcs = vec![0u32; (a.len() + 1) * w];
        for i in (0..a.len()).rev() {
            for j in (0..b.len()).rev() {
                lcs[i * w + j] = if a[i] == b[j] {
                    lcs[(i + 1) * w + j + 1] + 1
                } else {
                    lcs[(i + 1) * w + j].max(lcs[i * w + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < a.len() && j < b.len() {
            if a[i] == b[j] {
                out.push(Line::Same(a[i]));
                i += 1;
                j += 1;
            } else if lcs[(i + 1) * w + j] >= lcs[i * w + j + 1] {
                out.push(Line::Removed(a[i]));
                i += 1;
            } else {
                out.push(Line::Added(b[j]));
                j += 1;
            }
        }
        out.extend(a[i..].iter().map(|l| Line::Removed(l)));
        out.extend(b[j..].iter().map(|l| Line::Added(l)));
    }
    out.extend(old[old.len() - suffix..].iter().map(|l| Line::Same(l)));
    out
}

/// A unified-style line diff from `expected` to `actual`.
fn text_diff(expected: &str, actual: &str) -> Vec<String> {
    let old: Vec<&str> = expected.lines().collect();
    let new: Vec<&str> = actual.lines().collect();
    let lines = diff_lines(&old, &new);
    let changed: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| !matches!(l, Line::Same(_)))
        .map(|(i, _)| i)
        .collect();
    if changed.is_empty() {
        return vec!["texts differ only in line endings or a trailing newline".to_string()];
    }

    let mut out = Vec::new();
    let mut k = 0;
    while k < changed.len() {
        let start = changed[k].saturating_sub(DIFF_CONTEXT);
        let mut last = changed[k];
        while k + 1 < changed.len() && changed[k + 1] - last <= 2 * DIFF_CONTEXT + 1 {
            k += 1;
            last = changed[k];
        }
        k += 1;
        let end = (last + DIFF_CONTEXT + 1).min(lines.len());

        let count_old = |ls: &[Line]| ls.iter().filter(|l| !matches!(l, Line::Added(_))).count();
        let count_new = |ls: &[Line]| ls.iter().filter(|l| !matches!(l, Line::Removed(_))).count();
        out.push(format!(
            "@@ -{},{} +{},{} @@",
            count_old(&lines[..start]) + 1,
            count_old(&lines[start..end]),
            count_new(&lines[..start]) + 1,
            count_new(&lines[start..end]),
        ));
        for line in &lines[start..end] {
            out.push(match line {
                Line::Same(l) => format!(" {l}"),
                Line::Removed(l) => format!("-{l}"),
                Line::Added(l) => format!("+{l}"),
            });
        }
    }
    out
}

pub struct Options<'a> {
    /// `--extractor`: which extractor to run. Required to create an
    /// expected file; otherwise it must agree with the file.
    pub extractor: Option<&'a str>,
    pub update: bool,
    pub check_dkim: bool,
    pub timeout: Duration,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Pass,
    Mismatch(Vec<ArtifactDiff>),
    /// There is no expected file and `update` wasn't set.
    NoExpected,
    /// `update` wrote the expected file; `created` if it didn't exist.
    Written {
        created: bool,
    },
    /// `update` found the expected file already up to date.
    Unchanged,
}

pub struct Report {
    pub expected_path: PathBuf,
    /// The extractor that ran, when one did.
    pub extractor: Option<String>,
    pub outcome: Outcome,
}

/// Run one fixture. An `Err` means the fixture failed for a reason
/// other than differing output: a dispatch rule refused it, the
/// extractor failed, or the expected file is unusable.
pub fn test_fixture(fixture: &Path, extractors: &[Extractor], opts: &Options) -> Result<Report> {
    if fixture.to_string_lossy().ends_with(".expected.json") {
        bail!("this is an expected file, not a fixture message");
    }
    let expected_path = expected_path(fixture)?;
    let existing_bytes = match fs::read(&expected_path) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == ErrorKind::NotFound => None,
        Err(e) => {
            return Err(e).with_context(|| format!("reading {}", expected_path.display()));
        }
    };
    let existing: Option<Expected> = existing_bytes
        .as_deref()
        .map(serde_json::from_slice)
        .transpose()
        .with_context(|| format!("parsing {}", expected_path.display()))?;

    if existing.is_none() && !opts.update {
        return Ok(Report {
            expected_path,
            extractor: None,
            outcome: Outcome::NoExpected,
        });
    }

    let name = match (
        opts.extractor,
        existing.as_ref().map(|e| e.extractor.as_str()),
    ) {
        (Some(given), Some(named)) if given != named => bail!(
            "--extractor {given} does not match extractor {named} named in {}",
            expected_path.display()
        ),
        (Some(name), _) | (None, Some(name)) => name.to_string(),
        (None, None) => bail!(
            "no expected file {}; pass --extractor NAME with --update to create one",
            expected_path.display()
        ),
    };
    let ex = extractors
        .iter()
        .find(|ex| ex.name == name)
        .ok_or_else(|| anyhow!("unknown extractor {name}"))?;

    let raw = fs::read(fixture).with_context(|| format!("reading {}", fixture.display()))?;
    if let Some(why) = dispatch_refusal(ex, &raw, opts.check_dkim) {
        bail!("{name} would not be dispatched for this message: {why}");
    }
    let actual = produce(ex, &raw, opts.timeout)?;

    let outcome = match existing {
        Some(expected) if !opts.update => {
            let diffs = compare(&expected.artifacts, &actual);
            if diffs.is_empty() {
                Outcome::Pass
            } else {
                Outcome::Mismatch(diffs)
            }
        }
        _ => {
            let bytes = Expected {
                extractor: name.clone(),
                artifacts: actual,
            }
            .to_bytes()?;
            if existing_bytes.as_deref() == Some(bytes.as_slice()) {
                Outcome::Unchanged
            } else {
                fs::write(&expected_path, &bytes)
                    .with_context(|| format!("writing {}", expected_path.display()))?;
                Outcome::Written {
                    created: existing_bytes.is_none(),
                }
            }
        }
    };
    Ok(Report {
        expected_path,
        extractor: Some(name),
        outcome,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artifacts(entries: &[(&str, Value)]) -> BTreeMap<String, Value> {
        entries
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn expected_path_replaces_extension() {
        assert_eq!(
            expected_path(Path::new("tests/corpus/acme-receipt.eml")).unwrap(),
            PathBuf::from("tests/corpus/acme-receipt.expected.json")
        );
        assert_eq!(
            expected_path(Path::new("bare")).unwrap(),
            PathBuf::from("bare.expected.json")
        );
    }

    #[test]
    fn test_fixture_rejects_expected_files() {
        let opts = Options {
            extractor: None,
            update: false,
            check_dkim: false,
            timeout: Duration::from_secs(1),
        };
        let err = test_fixture(Path::new("corpus/a.expected.json"), &[], &opts)
            .err()
            .expect("expected an error");
        assert_eq!(
            err.to_string(),
            "this is an expected file, not a fixture message"
        );
    }

    #[test]
    fn render_artifact_by_kind() {
        assert_eq!(
            render_artifact("a.receipt.json", br#"{"orderNumber": "X1"}"#).unwrap(),
            json!({"orderNumber": "X1"})
        );
        assert_eq!(
            render_artifact("a.event.ics", b"BEGIN:VCALENDAR\r\n").unwrap(),
            json!("BEGIN:VCALENDAR\r\n")
        );
        assert_eq!(
            render_artifact("a.ticket.pdf", b"\xff\xfe").unwrap(),
            json!({"sha256": "b3d510ef04275ca8e698e5b3cbb0ece3949ef9252f0cdc839e9ee347409a2209"})
        );
    }

    #[test]
    fn render_artifact_rejects_invalid_json() {
        let err = render_artifact("a.parcel.json", b"{").unwrap_err();
        assert_eq!(err.to_string(), "a.parcel.json is not valid JSON");
    }

    #[test]
    fn compare_reports_missing_unexpected_and_changed() {
        let expected = artifacts(&[
            ("gone.parcel.json", json!({})),
            ("same.parcel.json", json!({"trackingNumber": "T1"})),
            (
                "x.receipt.json",
                json!({"orderNumber": "A", "items": [1, 2], "old": true}),
            ),
        ]);
        let actual = artifacts(&[
            ("new.parcel.json", json!({})),
            ("same.parcel.json", json!({"trackingNumber": "T1"})),
            (
                "x.receipt.json",
                json!({"orderNumber": "B", "items": [1], "extra": null}),
            ),
        ]);
        assert_eq!(
            compare(&expected, &actual),
            vec![
                ArtifactDiff::Missing("gone.parcel.json".into()),
                ArtifactDiff::Unexpected("new.parcel.json".into()),
                ArtifactDiff::Changed {
                    name: "x.receipt.json".into(),
                    details: vec![
                        "$.items[1]: missing, expected 2".into(),
                        "$.old: missing, expected true".into(),
                        "$.orderNumber: expected \"A\", got \"B\"".into(),
                        "$.extra: unexpected null".into(),
                    ],
                },
            ]
        );
    }

    #[test]
    fn compare_text_and_binary() {
        let expected = artifacts(&[
            ("a.event.ics", json!("one\ntwo\nthree\n")),
            ("b.ticket.pdf", json!({"sha256": "aa"})),
        ]);
        let actual = artifacts(&[
            ("a.event.ics", json!("one\n2\nthree\n")),
            ("b.ticket.pdf", json!({"sha256": "bb"})),
        ]);
        assert_eq!(
            compare(&expected, &actual),
            vec![
                ArtifactDiff::Changed {
                    name: "a.event.ics".into(),
                    details: vec![
                        "@@ -1,3 +1,3 @@".into(),
                        " one".into(),
                        "-two".into(),
                        "+2".into(),
                        " three".into(),
                    ],
                },
                ArtifactDiff::Changed {
                    name: "b.ticket.pdf".into(),
                    details: vec!["$.sha256: expected \"aa\", got \"bb\"".into()],
                },
            ]
        );
    }

    #[test]
    fn text_diff_splits_distant_changes_into_hunks() {
        let old: String = (1..=20).map(|i| format!("{i}\n")).collect();
        let new: String = old
            .lines()
            .map(|l| match l {
                "2" => "two\n".to_string(),
                "19" => "nineteen\n".to_string(),
                _ => format!("{l}\n"),
            })
            .collect();
        assert_eq!(
            text_diff(&old, &new),
            vec![
                "@@ -1,5 +1,5 @@",
                " 1",
                "-2",
                "+two",
                " 3",
                " 4",
                " 5",
                "@@ -16,5 +16,5 @@",
                " 16",
                " 17",
                " 18",
                "-19",
                "+nineteen",
                " 20",
            ]
        );
    }

    #[test]
    fn text_diff_handles_insertions() {
        assert_eq!(
            text_diff("a\nc\n", "a\nb\nc\n"),
            vec!["@@ -1,2 +1,3 @@", " a", "+b", " c"]
        );
    }

    #[test]
    fn text_diff_line_endings_only() {
        assert_eq!(
            text_diff("a\r\nb\r\n", "a\nb\n"),
            vec!["texts differ only in line endings or a trailing newline"]
        );
    }
}

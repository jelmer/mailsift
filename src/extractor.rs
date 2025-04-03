use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use tempfile::TempDir;
use tracing::{debug, info, warn};
use wait_timeout::ChildExt;

use crate::artifacts::{ScanResult, scan};

/// A manifest as parsed from `<name>.yaml`.
///
/// `from_domains`, `subject_regex` and `requires` act as cheap
/// prefilters: the first two against parsed message headers, and
/// `requires` against a body-shape summary built from the parsed MIME
/// tree.
#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub name: String,
    /// Lower numbers run earlier. Defaults to 100.
    #[serde(default = "default_order")]
    pub order: i32,
    /// Path to the executable to run, relative to the manifest's
    /// directory. Defaults to `<manifest-stem>.py` next to the manifest.
    pub script: Option<String>,
    /// Dispatch hints: when set, the extractor only runs against a
    /// message whose `From` domain matches one of these patterns.
    /// Patterns are case-insensitive; `*.example.com` matches any
    /// subdomain (including the bare domain).
    #[serde(default)]
    pub from_domains: Vec<String>,
    /// Dispatch hint: when set, the extractor only runs against a
    /// message whose `Subject` matches this regex.
    #[serde(default)]
    pub subject_regex: Option<String>,
    #[serde(default)]
    pub requires: Vec<String>,
}

fn default_order() -> i32 {
    100
}

pub struct Extractor {
    pub name: String,
    pub script: PathBuf,
    pub order: i32,
    /// Compiled `from_domains` patterns (lowercased). Empty means
    /// "match any sender".
    from_domains: Vec<FromDomainPattern>,
    /// Compiled `subject_regex`, if any. `None` means "match any
    /// subject".
    subject_regex: Option<regex::Regex>,
    /// Compiled body-shape requirements from `requires:`. Empty means
    /// "no constraint on the message body".
    body_requirements: Vec<BodyRequirement>,
}

/// A single `requires:` entry, parsed from the manifest. Used to
/// decide, from a body-shape summary alone, whether the extractor
/// could possibly match the message.
pub enum BodyRequirement {
    /// `requires: html`: needs a `text/html` part anywhere in the tree.
    Html,
    /// `requires: text`: needs a `text/plain` part anywhere in the
    /// tree.
    Text,
    /// `requires: attachment:<type>/<subtype>`: needs a part of the
    /// given MIME type. Stored lowercase (RFC 2045 §5.1).
    AttachmentMime { ty: String, subtype: String },
    /// `requires: attachment:filename:<pattern>`: needs a part whose
    /// `filename` (Content-Disposition) or legacy `name`
    /// (Content-Type) parameter matches the pattern. Comparison is
    /// case-insensitive.
    AttachmentFilename(FilenamePattern),
}

/// Filename-pattern matcher for `requires: attachment:filename:...`.
/// Deliberately minimal; only the wildcard forms actually used in
/// extractor manifests are supported.
pub enum FilenamePattern {
    /// Exact (case-insensitive) match.
    Exact(String),
    /// `*foo`: the lowercased filename must end with this suffix.
    Suffix(String),
    /// `foo*`: the lowercased filename must start with this prefix.
    Prefix(String),
}

impl FilenamePattern {
    fn matches(&self, name: &str) -> bool {
        let lower = name.to_ascii_lowercase();
        match self {
            FilenamePattern::Exact(s) => &lower == s,
            FilenamePattern::Suffix(s) => lower.ends_with(s),
            FilenamePattern::Prefix(s) => lower.starts_with(s),
        }
    }
}

fn parse_filename_pattern(raw: &str) -> Result<FilenamePattern> {
    let lower = raw.to_ascii_lowercase();
    let star_count = lower.bytes().filter(|&b| b == b'*').count();
    if star_count == 0 {
        return Ok(FilenamePattern::Exact(lower));
    }
    if star_count > 1 {
        bail!(
            "filename pattern {raw:?} has more than one '*'; only leading or trailing wildcards are supported"
        );
    }
    if let Some(rest) = lower.strip_prefix('*') {
        Ok(FilenamePattern::Suffix(rest.to_string()))
    } else if let Some(rest) = lower.strip_suffix('*') {
        Ok(FilenamePattern::Prefix(rest.to_string()))
    } else {
        bail!(
            "filename pattern {raw:?} has '*' in the middle; only leading or trailing wildcards are supported"
        );
    }
}

fn parse_body_requirement(raw: &str) -> Result<BodyRequirement> {
    if raw == "html" {
        return Ok(BodyRequirement::Html);
    }
    if raw == "text" {
        return Ok(BodyRequirement::Text);
    }
    let rest = raw
        .strip_prefix("attachment:")
        .ok_or_else(|| anyhow!("unknown requires entry {raw:?}"))?;
    if let Some(glob) = rest.strip_prefix("filename:") {
        return Ok(BodyRequirement::AttachmentFilename(parse_filename_pattern(
            glob,
        )?));
    }
    let (ty, subtype) = rest
        .split_once('/')
        .ok_or_else(|| anyhow!("requires {raw:?}: expected attachment:<type>/<subtype>"))?;
    if ty.is_empty() || subtype.is_empty() {
        bail!("requires {raw:?}: empty MIME type or subtype");
    }
    Ok(BodyRequirement::AttachmentMime {
        ty: ty.to_ascii_lowercase(),
        subtype: subtype.to_ascii_lowercase(),
    })
}

/// Compiled form of a single `from_domains` entry.
enum FromDomainPattern {
    /// Exact match against the (lowercased) sender domain.
    Exact(String),
    /// `*.example.com`-style wildcard: matches `example.com` and any
    /// subdomain.
    Wildcard(String),
}

impl FromDomainPattern {
    fn matches(&self, domain: &str) -> bool {
        match self {
            FromDomainPattern::Exact(p) => domain == p,
            FromDomainPattern::Wildcard(p) => {
                domain == p
                    || domain
                        .strip_suffix(p)
                        .is_some_and(|prefix| prefix.ends_with('.'))
            }
        }
    }
}

fn parse_from_domain(raw: &str) -> FromDomainPattern {
    let lower = raw.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("*.") {
        FromDomainPattern::Wildcard(rest.to_string())
    } else {
        FromDomainPattern::Exact(lower)
    }
}

impl Extractor {
    /// Return `true` when the manifest's `from_domains` /
    /// `subject_regex` hints don't rule this extractor out for the
    /// given message.
    ///
    /// An empty hint list means "no constraint". When hints are
    /// present, at least one of each declared category must match.
    ///
    /// `from_domain` must already be lowercased.
    pub fn matches_headers(&self, from_domain: Option<&str>, subject: Option<&str>) -> bool {
        if !self.from_domains.is_empty() {
            let Some(domain) = from_domain else {
                return false;
            };
            debug_assert!(
                domain.bytes().all(|b| !b.is_ascii_uppercase()),
                "from_domain must be lowercased before matches_headers"
            );
            if !self.from_domains.iter().any(|p| p.matches(domain)) {
                return false;
            }
        }
        if let Some(re) = &self.subject_regex {
            let Some(subj) = subject else {
                return false;
            };
            if !re.is_match(subj) {
                return false;
            }
        }
        true
    }

    /// Return `true` when the manifest's `requires:` body constraints
    /// are satisfied by the given parts summary. Every requirement
    /// must hold; an empty requirement list always matches.
    pub fn body_could_match(&self, parts: &BodyParts) -> bool {
        self.body_requirements.iter().all(|r| match r {
            BodyRequirement::Html => parts.has_html,
            BodyRequirement::Text => parts.has_text,
            BodyRequirement::AttachmentMime { ty, subtype } => parts
                .mime_types
                .iter()
                .any(|(t, s)| t == ty && s == subtype),
            BodyRequirement::AttachmentFilename(pat) => {
                parts.attachment_filenames.iter().any(|f| pat.matches(f))
            }
        })
    }
}

/// A flattened summary of the MIME-part shape of a single message.
#[derive(Default)]
pub struct BodyParts {
    pub has_html: bool,
    pub has_text: bool,
    /// Every leaf part's `(type, subtype)`, lowercased.
    pub mime_types: Vec<(String, String)>,
    /// Filenames from `Content-Disposition: ...; filename=` and
    /// `Content-Type: ...; name=`, in original case. Matching is
    /// case-insensitive.
    pub attachment_filenames: Vec<String>,
}

impl BodyParts {
    /// Record one leaf MIME part: bump `has_html` / `has_text` when it
    /// looks textual, push the lowercased `(type, subtype)` tuple, and
    /// append any candidate filename. `ty` and `subtype` must already
    /// be lowercased.
    pub fn push_leaf(&mut self, ty: &str, subtype: &str, filename: Option<&str>) {
        if ty == "text" && subtype == "html" {
            self.has_html = true;
        }
        if ty == "text" && subtype == "plain" {
            self.has_text = true;
        }
        if let Some(name) = filename {
            self.attachment_filenames.push(name.to_string());
        }
        self.mime_types.push((ty.to_string(), subtype.to_string()));
    }
}

#[allow(dead_code)] // tempdir kept alive for artifact reads; unknown_files surfaced via logs
pub struct ExtractorRun {
    pub extractor: String,
    pub tempdir: TempDir,
    pub result: ScanResult,
    pub unknown_files: Vec<PathBuf>,
}

/// Discover extractors under `dir` by scanning for `*.yaml` manifests.
///
/// Each manifest names a script (defaults to a sibling `<stem>.py`) and
/// declares an `order`; results are returned sorted by `order` then by
/// manifest filename for stability.
pub fn discover(dir: &Path) -> Result<Vec<Extractor>> {
    let mut out: Vec<Extractor> = Vec::new();

    let entries =
        fs::read_dir(dir).with_context(|| format!("listing extractors dir {}", dir.display()))?;
    for entry in entries {
        let entry = entry?;
        let ft = entry.file_type()?;
        if !ft.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || name.starts_with('_') {
            continue;
        }
        if !name.ends_with(".yaml") {
            continue;
        }

        let manifest_path = entry.path();
        let body = fs::read_to_string(&manifest_path)
            .with_context(|| format!("reading manifest {}", manifest_path.display()))?;
        let manifest: Manifest = serde_norway::from_str(&body)
            .with_context(|| format!("parsing manifest {}", manifest_path.display()))?;

        let stem = manifest_path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| anyhow!("bad manifest filename {}", manifest_path.display()))?;
        let script_rel = manifest
            .script
            .clone()
            .unwrap_or_else(|| format!("{stem}.py"));
        let script_path = manifest_path
            .parent()
            .ok_or_else(|| anyhow!("manifest {} has no parent dir", manifest_path.display()))?
            .join(&script_rel)
            .canonicalize()
            .with_context(|| {
                format!(
                    "canonicalizing script path {script_rel} for {}",
                    manifest.name
                )
            })?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&script_path)
                .with_context(|| format!("stat {}", script_path.display()))?
                .permissions()
                .mode();
            if mode & 0o111 == 0 {
                warn!(
                    extractor = %manifest.name,
                    script = %script_path.display(),
                    "skipping extractor whose script is not executable"
                );
                continue;
            }
        }

        let from_domains = manifest
            .from_domains
            .iter()
            .map(|d| parse_from_domain(d))
            .collect();
        let subject_regex = manifest
            .subject_regex
            .as_deref()
            .map(|s| {
                regex::Regex::new(s).with_context(|| {
                    format!(
                        "compiling subject_regex {s:?} for extractor {}",
                        manifest.name
                    )
                })
            })
            .transpose()?;
        let body_requirements: Vec<BodyRequirement> = manifest
            .requires
            .iter()
            .map(|r| {
                parse_body_requirement(r)
                    .with_context(|| format!("parsing requires {r:?} for {}", manifest.name))
            })
            .collect::<Result<_>>()?;

        out.push(Extractor {
            name: manifest.name,
            script: script_path,
            order: manifest.order,
            from_domains,
            subject_regex,
            body_requirements,
        });
    }

    out.sort_by(|a, b| a.order.cmp(&b.order).then_with(|| a.name.cmp(&b.name)));
    Ok(out)
}

/// Run one extractor with the raw RFC822 on stdin and a fresh tempdir as
/// cwd. Returns the discovered artifacts; the tempdir is kept alive in the
/// returned struct so the caller can read the artifact files before drop.
pub fn run_one(extractor: &Extractor, raw: &[u8], timeout: Duration) -> Result<ExtractorRun> {
    let tempdir = TempDir::new().context("creating extractor tempdir")?;

    let mut child = Command::new(&extractor.script)
        .current_dir(tempdir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning extractor {}", extractor.name))?;

    {
        let mut stdin = child.stdin.take().expect("piped stdin");
        let _ = stdin.write_all(raw);
        drop(stdin);
    }

    let status = match child
        .wait_timeout(timeout)
        .context("waiting for extractor")?
    {
        Some(status) => status,
        None => {
            warn!(extractor = %extractor.name, "extractor timed out, killing");
            let _ = child.kill();
            let _ = child.wait();
            bail!("extractor {} timed out", extractor.name);
        }
    };

    let mut stderr_buf = Vec::new();
    let mut stdout_buf = Vec::new();
    if let Some(mut s) = child.stderr.take() {
        use std::io::Read;
        let _ = s.read_to_end(&mut stderr_buf);
    }
    if let Some(mut s) = child.stdout.take() {
        use std::io::Read;
        let _ = s.read_to_end(&mut stdout_buf);
    }

    let stderr = String::from_utf8_lossy(&stderr_buf);
    if !stderr.trim().is_empty() {
        for line in stderr.lines() {
            info!(extractor = %extractor.name, "stderr: {line}");
        }
    }
    if !status.success() {
        bail!("extractor {} exited with status {}", extractor.name, status);
    }

    debug!(extractor = %extractor.name, "scanning tempdir for artifacts");
    let (result, unknown) = scan(tempdir.path())?;
    for path in &unknown {
        warn!(
            extractor = %extractor.name,
            file = %path.display(),
            "ignoring file with unrecognised suffix"
        );
    }
    Ok(ExtractorRun {
        extractor: extractor.name.clone(),
        tempdir,
        result,
        unknown_files: unknown,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ex(from_domains: Vec<&str>, subject_regex: Option<&str>) -> Extractor {
        ex_full(from_domains, subject_regex, Vec::new())
    }

    fn ex_full(
        from_domains: Vec<&str>,
        subject_regex: Option<&str>,
        body_requirements: Vec<BodyRequirement>,
    ) -> Extractor {
        Extractor {
            name: "t".into(),
            script: PathBuf::from("/bin/true"),
            order: 100,
            from_domains: from_domains.into_iter().map(parse_from_domain).collect(),
            subject_regex: subject_regex.map(|s| regex::Regex::new(s).unwrap()),
            body_requirements,
        }
    }

    #[test]
    fn from_domain_exact() {
        let p = parse_from_domain("example.com");
        assert!(p.matches("example.com"));
        assert!(!p.matches("foo.example.com"));
        assert!(!p.matches("notexample.com"));
    }

    #[test]
    fn from_domain_wildcard_matches_bare_and_sub() {
        let p = parse_from_domain("*.example.com");
        assert!(p.matches("example.com"));
        assert!(p.matches("mail.example.com"));
        assert!(p.matches("a.b.example.com"));
    }

    #[test]
    fn from_domain_wildcard_does_not_match_unrelated() {
        let p = parse_from_domain("*.example.com");
        // Suffix-only match without dot boundary must be rejected,
        // otherwise `notexample.com` would falsely match `example.com`.
        assert!(!p.matches("notexample.com"));
        assert!(!p.matches("badexample.com"));
    }

    #[test]
    fn matches_headers_no_constraints_runs_on_anything() {
        let e = ex(vec![], None);
        assert!(e.matches_headers(None, None));
        assert!(e.matches_headers(Some("anyone.example"), Some("hello")));
    }

    #[test]
    fn matches_headers_from_domain_required_but_missing() {
        let e = ex(vec!["example.com"], None);
        assert!(!e.matches_headers(None, Some("hi")));
    }

    #[test]
    fn matches_headers_subject_regex() {
        let e = ex(vec![], Some(r"^Receipt for"));
        assert!(e.matches_headers(None, Some("Receipt for order 1234")));
        assert!(!e.matches_headers(None, Some("Unrelated")));
        assert!(!e.matches_headers(None, None));
    }

    fn parts(
        has_html: bool,
        has_text: bool,
        mime_types: &[(&str, &str)],
        filenames: &[&str],
    ) -> BodyParts {
        BodyParts {
            has_html,
            has_text,
            mime_types: mime_types
                .iter()
                .map(|(t, s)| ((*t).into(), (*s).into()))
                .collect(),
            attachment_filenames: filenames.iter().map(|s| (*s).into()).collect(),
        }
    }

    #[test]
    fn parse_body_requirement_html_text() {
        assert!(matches!(
            parse_body_requirement("html").unwrap(),
            BodyRequirement::Html
        ));
        assert!(matches!(
            parse_body_requirement("text").unwrap(),
            BodyRequirement::Text
        ));
    }

    #[test]
    fn parse_body_requirement_attachment_mime() {
        let r = parse_body_requirement("attachment:Text/Calendar").unwrap();
        let BodyRequirement::AttachmentMime { ty, subtype } = r else {
            panic!("expected AttachmentMime");
        };
        assert_eq!(ty, "text");
        assert_eq!(subtype, "calendar");
    }

    #[test]
    fn parse_body_requirement_attachment_filename() {
        let r = parse_body_requirement("attachment:filename:*.ICS").unwrap();
        let BodyRequirement::AttachmentFilename(p) = r else {
            panic!("expected AttachmentFilename");
        };
        assert!(p.matches("invite.ics"));
        assert!(p.matches("INVITE.ICS"));
        assert!(!p.matches("invite.txt"));
    }

    #[test]
    fn body_could_match_html_requirement() {
        let e = ex_full(vec![], None, vec![BodyRequirement::Html]);
        assert!(e.body_could_match(&parts(true, false, &[], &[])));
        assert!(!e.body_could_match(&parts(false, true, &[], &[])));
    }

    #[test]
    fn body_could_match_attachment_filename() {
        let pat = parse_filename_pattern("*.ics").unwrap();
        let e = ex_full(vec![], None, vec![BodyRequirement::AttachmentFilename(pat)]);
        assert!(e.body_could_match(&parts(false, false, &[], &["invite.ics"])));
        assert!(!e.body_could_match(&parts(false, false, &[], &["x.pdf"])));
    }
}

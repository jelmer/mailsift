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

/// A manifest as parsed from `extractors/<name>.yaml`.
///
/// `from_domains`, `subject_regex` and `requires` act as cheap
/// prefilters: the first two against parsed message headers, and
/// `requires` against an IMAP `BODYSTRUCTURE` summary in
/// [`crate::imap_scan`].
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
    /// Only run this extractor when the message has a passing DKIM
    /// signature from one of these domains, per the topmost
    /// `Authentication-Results` header. When the message lacks the
    /// header entirely the extractor is skipped; only the milter
    /// front-end (which sees mail before our MTA authenticates it)
    /// can't enforce this, and explicitly opts out at run time.
    ///
    /// An entry starting with `.` is a suffix match against the
    /// signing domain: `.myshopify.com` matches `xyz.myshopify.com`
    /// but not the bare `myshopify.com` itself. Useful for SaaS
    /// platforms (Shopify, Mailgun, ...) where every tenant's
    /// notifications are DKIM-signed under a shared parent zone.
    #[serde(default)]
    pub require_dkim: Vec<String>,
}

fn default_order() -> i32 {
    100
}

pub struct Extractor {
    pub name: String,
    pub script: PathBuf,
    pub order: i32,
    pub require_dkim: Vec<String>,
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

/// A single `requires:` entry, parsed from the manifest. Used by the
/// IMAP prefilter to decide, from a `BODYSTRUCTURE` response alone
/// (without fetching the body), whether the extractor could possibly
/// match the message.
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
/// extractor manifests are supported, so we don't drag in a
/// general-purpose glob crate.
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
    /// An empty hint list means "no constraint"; the extractor will
    /// run on every message. When hints are present, at least one of
    /// each declared category must match.
    ///
    /// `from_domain` must already be lowercased; the pipeline's
    /// `match_headers_from_raw` and the IMAP prefilter both guarantee
    /// this, which lets the hot path here avoid a per-call allocation.
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
/// Built two ways: from an IMAP `BODYSTRUCTURE` response (imap-scan
/// prefilter) and from a parsed MIME tree (pipeline body-shape check
/// before forking each extractor). Both walkers funnel each leaf
/// through [`push_leaf`] so the bookkeeping stays in lock-step.
#[derive(Default)]
pub struct BodyParts {
    pub has_html: bool,
    pub has_text: bool,
    /// Every leaf part's `(type, subtype)`, lowercased.
    pub mime_types: Vec<(String, String)>,
    /// Filenames from `Content-Disposition: ...; filename=` and
    /// `Content-Type: ...; name=`, in original case. Matching against
    /// `FilenamePattern` is case-insensitive.
    pub attachment_filenames: Vec<String>,
}

impl BodyParts {
    /// Record one leaf MIME part: bump `has_html` / `has_text` when it
    /// looks textual, push the lowercased `(type, subtype)` tuple, and
    /// append any candidate filename (from Content-Disposition's
    /// `filename=` or Content-Type's legacy `name=`). `ty` and `subtype`
    /// must already be lowercased; both call sites have them in that
    /// form for free.
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

/// Discover extractors under each directory in `dirs` by scanning for
/// `*.yaml` manifests.
///
/// Each manifest names a script (defaults to a sibling `<stem>.py`) and
/// declares an `order`; results are returned sorted by `order` then by
/// manifest filename for stability.
///
/// When multiple directories list a manifest with the same `name:`
/// field, the first directory wins, letting a personal directory of
/// overrides layer on top of an upstream-shipped set. Duplicates from
/// later directories are skipped with a debug log.
pub fn discover(dirs: &[PathBuf]) -> Result<Vec<Extractor>> {
    let mut out: Vec<Extractor> = Vec::new();
    for dir in dirs {
        discover_into(dir, &mut out)?;
    }
    out.sort_by(|a, b| a.order.cmp(&b.order).then_with(|| a.name.cmp(&b.name)));
    Ok(out)
}

/// One validation problem found while linting a manifest. Returned by
/// [`lint`] so the CLI can print every issue (instead of bailing on
/// the first like [`discover`] does).
pub struct LintIssue {
    /// Path to the manifest file (the `.yaml`), or to a script /
    /// directory when the problem is associated with one of those.
    pub source: PathBuf,
    /// One-line problem description.
    pub message: String,
}

/// Validate every discoverable extractor manifest under each `dirs`
/// directory and return one [`LintIssue`] per problem.
///
/// Where [`discover`] is the production loader (it bails on the first
/// error so a broken manifest can't slip into the pipeline), `lint` is
/// the developer loop: it collects every issue across every manifest
/// and reports them all in one pass.
///
/// Checks (mirroring `discover_into`):
///   1. The directory itself is readable.
///   2. Each `*.yaml` parses as a [`Manifest`].
///   3. `script:` (or the default sibling `<stem>.py`) exists and is
///      executable.
///   4. `subject_regex` compiles.
///   5. Each `requires:` entry is one of the known shapes.
///   6. No two manifests across all directories share the same `name:`
///      (a real collision, distinct from the legitimate override case
///      where two directories list a same-named manifest deliberately;
///      we surface both so the user can confirm the override is wanted).
pub fn lint(dirs: &[PathBuf]) -> Vec<LintIssue> {
    let mut issues: Vec<LintIssue> = Vec::new();
    // Tracks `name -> first-seen manifest path` across all dirs, so a
    // later directory's same-named manifest can be reported alongside
    // the one it would shadow.
    let mut seen_names: std::collections::HashMap<String, PathBuf> =
        std::collections::HashMap::new();

    for dir in dirs {
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) => {
                issues.push(LintIssue {
                    source: dir.clone(),
                    message: format!("cannot list directory: {e}"),
                });
                continue;
            }
        };
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            if !ft.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || name.starts_with('_') || !name.ends_with(".yaml") {
                continue;
            }
            lint_one(&entry.path(), &mut issues, &mut seen_names);
        }
    }

    issues
}

/// Validate a single manifest file and append any problems to `issues`.
/// Mirrors `discover_into`'s checks but accumulates instead of bailing.
fn lint_one(
    manifest_path: &Path,
    issues: &mut Vec<LintIssue>,
    seen_names: &mut std::collections::HashMap<String, PathBuf>,
) {
    let body = match fs::read_to_string(manifest_path) {
        Ok(b) => b,
        Err(e) => {
            issues.push(LintIssue {
                source: manifest_path.to_path_buf(),
                message: format!("cannot read manifest: {e}"),
            });
            return;
        }
    };
    let manifest: Manifest = match serde_norway::from_str(&body) {
        Ok(m) => m,
        Err(e) => {
            issues.push(LintIssue {
                source: manifest_path.to_path_buf(),
                message: format!("YAML parse error: {e}"),
            });
            return;
        }
    };

    // Duplicate-name check across all linted dirs. Record only on
    // first sight; subsequent matches turn into issues so an
    // upstream/personal-override collision is visible.
    if let Some(prev) = seen_names.get(&manifest.name) {
        if prev != manifest_path {
            issues.push(LintIssue {
                source: manifest_path.to_path_buf(),
                message: format!(
                    "duplicate name {:?}: also defined in {}",
                    manifest.name,
                    prev.display()
                ),
            });
        }
    } else {
        seen_names.insert(manifest.name.clone(), manifest_path.to_path_buf());
    }

    // Resolve and check the script path.
    let stem = manifest_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("<unknown>");
    let script_rel = manifest
        .script
        .clone()
        .unwrap_or_else(|| format!("{stem}.py"));
    let script_path = manifest_path
        .parent()
        .map(|p| p.join(&script_rel))
        .unwrap_or_else(|| PathBuf::from(&script_rel));
    if !script_path.exists() {
        issues.push(LintIssue {
            source: script_path.clone(),
            message: format!(
                "script {:?} (from manifest {}) does not exist",
                script_rel,
                manifest_path.display()
            ),
        });
    } else {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            match fs::metadata(&script_path) {
                Ok(meta) if meta.permissions().mode() & 0o111 == 0 => {
                    issues.push(LintIssue {
                        source: script_path.clone(),
                        message: "script is not executable (chmod +x)".to_string(),
                    });
                }
                Err(e) => {
                    issues.push(LintIssue {
                        source: script_path.clone(),
                        message: format!("cannot stat script: {e}"),
                    });
                }
                _ => {}
            }
        }
    }

    if let Some(re) = manifest.subject_regex.as_deref()
        && let Err(e) = regex::Regex::new(re)
    {
        issues.push(LintIssue {
            source: manifest_path.to_path_buf(),
            message: format!("subject_regex {re:?} fails to compile: {e}"),
        });
    }

    for entry in &manifest.requires {
        if let Err(e) = parse_body_requirement(entry) {
            issues.push(LintIssue {
                source: manifest_path.to_path_buf(),
                message: format!("requires entry {entry:?}: {e}"),
            });
        }
    }

    // `from_domains` patterns can't really fail to parse (we only
    // distinguish `*.foo` from `foo`), but a leading-only wildcard
    // with an empty suffix is almost certainly a typo.
    for d in &manifest.from_domains {
        if d.trim().is_empty() || d == "*." || d == "*" {
            issues.push(LintIssue {
                source: manifest_path.to_path_buf(),
                message: format!("from_domains entry {d:?} is empty or a bare wildcard"),
            });
        }
    }
}

/// Scan one directory and append its (non-duplicate) extractors to
/// `out`. Factored out so [`discover`] can iterate directories without
/// repeating the body.
fn discover_into(dir: &Path, out: &mut Vec<Extractor>) -> Result<()> {
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

        // Lowercase `require_dkim` once at load time so the hot-path
        // `dkim::satisfies` comparison doesn't have to.
        let require_dkim = manifest
            .require_dkim
            .into_iter()
            .map(|d| d.to_ascii_lowercase())
            .collect();

        let body_requirements: Vec<BodyRequirement> = manifest
            .requires
            .iter()
            .map(|r| {
                parse_body_requirement(r)
                    .with_context(|| format!("parsing requires {r:?} for {}", manifest.name))
            })
            .collect::<Result<_>>()?;

        if let Some(existing) = out.iter().find(|e| e.name == manifest.name) {
            debug!(
                extractor = %manifest.name,
                shadowed_by = %existing.script.display(),
                ignored = %script_path.display(),
                "skipping duplicate extractor: earlier directory wins"
            );
            continue;
        }

        out.push(Extractor {
            name: manifest.name,
            script: script_path,
            order: manifest.order,
            require_dkim,
            from_domains,
            subject_regex,
            body_requirements,
        });
    }
    Ok(())
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
        // Best-effort: if the extractor exits before reading all of stdin
        // we don't want to crash the parent on the resulting EPIPE.
        let _ = stdin.write_all(raw);
        drop(stdin);
    }

    // `wait_timeout` is a real blocking wait with a deadline, so the
    // happy path returns the moment the extractor finishes; no more
    // 50ms-per-extractor floor.
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

    // Drain stdout / stderr now that the child has exited. The pipes
    // are bounded; if an extractor wrote enough that the OS-level
    // buffer filled, it would have blocked before exit, so by the
    // time we reach here both streams have either been drained or
    // are small enough to read in full.
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
    use std::fs;

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
            require_dkim: Vec::new(),
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
        assert!(!p.matches(""));
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
        assert!(!p.matches("evil.com"));
    }

    #[test]
    fn from_domain_lowercases_at_parse() {
        let p = parse_from_domain("Example.COM");
        // `matches` requires lowercased input.
        assert!(p.matches("example.com"));
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
    fn matches_headers_from_domain_matches() {
        let e = ex(vec!["example.com"], None);
        assert!(e.matches_headers(Some("example.com"), None));
        assert!(!e.matches_headers(Some("evil.example"), None));
    }

    #[test]
    fn matches_headers_subject_regex() {
        let e = ex(vec![], Some(r"^Receipt for"));
        assert!(e.matches_headers(None, Some("Receipt for order 1234")));
        assert!(!e.matches_headers(None, Some("Unrelated")));
        assert!(!e.matches_headers(None, None));
    }

    #[test]
    fn matches_headers_both_must_match() {
        let e = ex(vec!["example.com"], Some(r"^Receipt"));
        assert!(e.matches_headers(Some("example.com"), Some("Receipt #1")));
        assert!(!e.matches_headers(Some("example.com"), Some("Other")));
        assert!(!e.matches_headers(Some("evil.example"), Some("Receipt #1")));
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
    fn parse_body_requirement_rejects_unknown() {
        assert!(parse_body_requirement("nope").is_err());
        assert!(parse_body_requirement("attachment:").is_err());
        assert!(parse_body_requirement("attachment:foo").is_err());
        assert!(parse_body_requirement("attachment:filename:foo*bar*").is_err());
    }

    #[test]
    fn filename_pattern_exact_and_wildcards() {
        let exact = parse_filename_pattern("invite.ics").unwrap();
        assert!(exact.matches("invite.ics"));
        assert!(!exact.matches("other.ics"));

        let suffix = parse_filename_pattern("*.ics").unwrap();
        assert!(suffix.matches("anything.ics"));
        assert!(suffix.matches("a.b.ics"));
        assert!(!suffix.matches("ics"));

        let prefix = parse_filename_pattern("foo*").unwrap();
        assert!(prefix.matches("foo"));
        assert!(prefix.matches("foobar"));
        assert!(!prefix.matches("barfoo"));
    }

    #[test]
    fn body_could_match_empty_requirements_matches_anything() {
        let e = ex_full(vec![], None, vec![]);
        assert!(e.body_could_match(&parts(false, false, &[], &[])));
    }

    #[test]
    fn body_could_match_html_requirement() {
        let e = ex_full(vec![], None, vec![BodyRequirement::Html]);
        assert!(e.body_could_match(&parts(true, false, &[], &[])));
        assert!(!e.body_could_match(&parts(false, true, &[], &[])));
    }

    #[test]
    fn body_could_match_attachment_mime_requirement() {
        let e = ex_full(
            vec![],
            None,
            vec![BodyRequirement::AttachmentMime {
                ty: "text".into(),
                subtype: "calendar".into(),
            }],
        );
        assert!(e.body_could_match(&parts(false, false, &[("text", "calendar")], &[])));
        assert!(!e.body_could_match(&parts(true, true, &[("text", "html")], &[])));
    }

    #[test]
    fn body_could_match_attachment_filename_requirement() {
        let e = ex_full(
            vec![],
            None,
            vec![BodyRequirement::AttachmentFilename(
                parse_filename_pattern("*.ics").unwrap(),
            )],
        );
        assert!(e.body_could_match(&parts(false, false, &[], &["invite.ics"])));
        assert!(!e.body_could_match(&parts(false, false, &[], &["invoice.pdf"])));
    }

    #[test]
    fn body_could_match_requires_all() {
        let e = ex_full(
            vec![],
            None,
            vec![
                BodyRequirement::Html,
                BodyRequirement::AttachmentMime {
                    ty: "text".into(),
                    subtype: "calendar".into(),
                },
            ],
        );
        assert!(e.body_could_match(&parts(true, false, &[("text", "calendar")], &[])));
        // html present but no calendar attachment; must fail
        assert!(!e.body_could_match(&parts(true, false, &[("text", "html")], &[])));
        // calendar attachment present but no html part; must fail
        assert!(!e.body_could_match(&parts(false, false, &[("text", "calendar")], &[])));
    }

    /// Build a temporary extractors directory with one manifest +
    /// executable script. Returns the directory (kept alive) so the
    /// caller can run `discover` on it.
    #[cfg(unix)]
    fn fixture(name: &str, body: &str, executable: bool) -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        fs::write(dir.path().join(format!("{name}.yaml")), body).unwrap();
        let script = dir.path().join(format!("{name}.py"));
        fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        let mode = if executable { 0o755 } else { 0o644 };
        fs::set_permissions(&script, fs::Permissions::from_mode(mode)).unwrap();
        dir
    }

    #[cfg(unix)]
    #[test]
    fn discover_default_order_is_100() {
        let dir = fixture("a", "name: a\n", true);
        let got = discover(&[dir.path().to_path_buf()]).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].order, 100);
    }

    #[cfg(unix)]
    #[test]
    fn discover_skips_non_executable_script() {
        let dir = fixture("a", "name: a\n", false);
        let got = discover(&[dir.path().to_path_buf()]).unwrap();
        assert_eq!(got.len(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn discover_skips_dot_prefixed_manifest() {
        let dir = fixture(".hidden", "name: hidden\n", true);
        let got = discover(&[dir.path().to_path_buf()]).unwrap();
        assert_eq!(got.len(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn discover_skips_underscore_prefixed_manifest() {
        let dir = fixture("_disabled", "name: disabled\n", true);
        let got = discover(&[dir.path().to_path_buf()]).unwrap();
        assert_eq!(got.len(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn discover_earlier_dir_wins_on_name_collision() {
        // Both directories ship a manifest with name `royal-mail`; the
        // earlier one in the list should be the one we keep, so a
        // personal override directory can layer on top of an upstream
        // bundle.
        let first = fixture("royal-mail", "name: royal-mail\norder: 5\n", true);
        let second = fixture("royal-mail", "name: royal-mail\norder: 50\n", true);
        let got = discover(&[first.path().to_path_buf(), second.path().to_path_buf()]).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].order, 5);
        // Script should resolve under the first dir, not the second.
        assert!(got[0].script.starts_with(first.path()));
    }

    #[cfg(unix)]
    #[test]
    fn discover_merges_distinct_names_across_dirs() {
        let a = fixture("a", "name: a\n", true);
        let b = fixture("b", "name: b\n", true);
        let got = discover(&[a.path().to_path_buf(), b.path().to_path_buf()]).unwrap();
        assert_eq!(got.len(), 2);
        let names: Vec<&str> = got.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[cfg(unix)]
    #[test]
    fn lint_clean_manifest_has_no_issues() {
        let dir = fixture("clean", "name: clean\norder: 10\n", true);
        let issues = lint(&[dir.path().to_path_buf()]);
        assert!(
            issues.is_empty(),
            "expected no issues, got: {:?}",
            issues.iter().map(|i| &i.message).collect::<Vec<_>>()
        );
    }

    #[cfg(unix)]
    #[test]
    fn lint_reports_yaml_parse_error() {
        let dir = tempfile::TempDir::new().unwrap();
        fs::write(dir.path().join("broken.yaml"), "name: [unterminated\n").unwrap();
        let issues = lint(&[dir.path().to_path_buf()]);
        assert_eq!(issues.len(), 1);
        assert!(
            issues[0].message.contains("YAML parse error"),
            "got: {:?}",
            issues[0].message
        );
    }

    #[cfg(unix)]
    #[test]
    fn lint_reports_bad_regex() {
        let dir = fixture(
            "bad-re",
            "name: bad-re\nsubject_regex: \"[unterminated\"\n",
            true,
        );
        let issues = lint(&[dir.path().to_path_buf()]);
        assert_eq!(issues.len(), 1);
        assert!(
            issues[0].message.contains("subject_regex"),
            "got: {:?}",
            issues[0].message
        );
    }

    #[cfg(unix)]
    #[test]
    fn lint_reports_bad_requires() {
        // `nope` isn't one of the known shapes (html / text /
        // attachment:...). Quoted so YAML sees it as a string and
        // we exercise the requires-parser, not the YAML loader.
        let dir = fixture("bad-req", "name: bad-req\nrequires:\n  - \"nope\"\n", true);
        let issues = lint(&[dir.path().to_path_buf()]);
        assert_eq!(issues.len(), 1);
        assert!(
            issues[0].message.contains("requires entry"),
            "got: {:?}",
            issues[0].message
        );
    }

    #[cfg(unix)]
    #[test]
    fn lint_reports_missing_script() {
        let dir = tempfile::TempDir::new().unwrap();
        // Manifest exists, but no sibling script alongside.
        fs::write(dir.path().join("orphan.yaml"), "name: orphan\n").unwrap();
        let issues = lint(&[dir.path().to_path_buf()]);
        assert_eq!(issues.len(), 1);
        assert!(
            issues[0].message.contains("does not exist"),
            "got: {:?}",
            issues[0].message
        );
    }

    #[cfg(unix)]
    #[test]
    fn lint_reports_non_executable_script() {
        let dir = fixture("noexec", "name: noexec\n", false);
        let issues = lint(&[dir.path().to_path_buf()]);
        assert_eq!(issues.len(), 1);
        assert!(
            issues[0].message.contains("not executable"),
            "got: {:?}",
            issues[0].message
        );
    }

    #[cfg(unix)]
    #[test]
    fn lint_reports_duplicate_name_across_dirs() {
        let first = fixture("dup", "name: dup\n", true);
        let second = fixture("dup", "name: dup\n", true);
        let issues = lint(&[first.path().to_path_buf(), second.path().to_path_buf()]);
        assert_eq!(issues.len(), 1);
        assert!(
            issues[0].message.contains("duplicate name"),
            "got: {:?}",
            issues[0].message
        );
    }

    #[cfg(unix)]
    #[test]
    fn lint_accumulates_issues_across_files() {
        // Two manifests in one dir, each broken in a different way.
        let dir = tempfile::TempDir::new().unwrap();
        fs::write(dir.path().join("a.yaml"), ": not yaml\n").unwrap();
        fs::write(
            dir.path().join("b.yaml"),
            "name: b\nsubject_regex: \"[bad\"\n",
        )
        .unwrap();
        // b.py is needed so the script-existence check doesn't add a
        // second issue for b.
        let script = dir.path().join("b.py");
        fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let issues = lint(&[dir.path().to_path_buf()]);
        assert_eq!(
            issues.len(),
            2,
            "expected two issues, got: {:?}",
            issues.iter().map(|i| &i.message).collect::<Vec<_>>()
        );
    }

    #[cfg(unix)]
    #[test]
    fn lint_rejects_bare_wildcard_from_domain() {
        let dir = fixture("wild", "name: wild\nfrom_domains:\n  - \"*\"\n", true);
        let issues = lint(&[dir.path().to_path_buf()]);
        assert_eq!(issues.len(), 1);
        assert!(
            issues[0].message.contains("from_domains"),
            "got: {:?}",
            issues[0].message
        );
    }
}

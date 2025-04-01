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
#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub name: String,
    /// Lower numbers run earlier. Defaults to 100.
    #[serde(default = "default_order")]
    pub order: i32,
    /// Path to the executable to run, relative to the manifest's
    /// directory. Defaults to `<manifest-stem>.py` next to the manifest.
    pub script: Option<String>,
}

fn default_order() -> i32 {
    100
}

pub struct Extractor {
    pub name: String,
    pub script: PathBuf,
    pub order: i32,
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

        out.push(Extractor {
            name: manifest.name,
            script: script_path,
            order: manifest.order,
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
        // Best-effort: if the extractor exits before reading all of stdin
        // we don't want to crash the parent on the resulting EPIPE.
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

    #[test]
    fn discover_missing_dir_is_error() {
        let err = match discover(Path::new("/nonexistent/path/to/extractors")) {
            Ok(_) => panic!("expected error for missing dir"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("listing extractors dir"));
    }

    #[test]
    fn discover_empty_dir_yields_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let extractors = discover(tmp.path()).unwrap();
        assert!(extractors.is_empty());
    }
}

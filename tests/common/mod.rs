//! Helpers shared between integration tests.

use std::path::{Path, PathBuf};

/// The `mailsift` binary, cut off from the config and state of whoever
/// is running the tests. Otherwise a developer's own `config.toml`
/// supplies targets a test didn't ask for, so the test passes for them
/// and fails elsewhere, and its stats land in their real log.
pub fn mailsift_std() -> std::process::Command {
    let scratch = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let mut command = std::process::Command::new(assert_cmd::cargo::cargo_bin("mailsift"));
    command
        .env("XDG_CONFIG_HOME", scratch.join("no-config"))
        .env("XDG_STATE_HOME", scratch.join("state"));
    command
}

/// [`mailsift_std`], with `assert_cmd`'s assertions.
pub fn mailsift() -> assert_cmd::Command {
    assert_cmd::Command::from_std(mailsift_std())
}

/// Copy the fixture message `name` with its `Date:` header replaced,
/// as if the same sender had sent it at another time. The copy lives
/// as long as the returned directory.
// Not every test file that needs the binary also needs this.
#[allow(dead_code)]
pub fn redated_fixture(name: &str, date: &str) -> (tempfile::TempDir, PathBuf) {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/eml")
        .join(name);
    let original = std::fs::read_to_string(&fixture)
        .unwrap_or_else(|e| panic!("read {}: {e}", fixture.display()));
    let redated: Vec<String> = original
        .lines()
        .map(|line| match line.starts_with("Date: ") {
            true => format!("Date: {date}"),
            false => line.to_string(),
        })
        .collect();
    assert_ne!(redated.join("\n"), original.trim_end(), "no Date: header");

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(name);
    std::fs::write(&path, redated.join("\n") + "\n").expect("write redated message");
    (dir, path)
}

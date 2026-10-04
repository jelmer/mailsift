//! Helpers shared between integration tests.

use std::path::{Path, PathBuf};

/// Copy the fixture message `name` with its `Date:` header replaced,
/// as if the same sender had sent it at another time. The copy lives
/// as long as the returned directory.
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

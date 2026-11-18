use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

mod common;

const MANIFEST: &str = "\
name: acme
script: acme.sh
from_domains: [acme.example]
requires: [text]
require_dkim: [acme.example]
";

const SCRIPT: &str = r#"#!/bin/sh
cat > /dev/null
printf '{"orderNumber": "ACME-1", "merchant": "Acme Widgets"}' > acme-1.receipt.json
printf 'BEGIN:VCALENDAR\r\nUID:acme-1\r\nEND:VCALENDAR\r\n' > acme-1.event.ics
printf '\377\376' > acme-1.ticket.pdf
"#;

const MESSAGE: &str = "\
From: Acme Widgets <orders@acme.example>
To: Robin Example <robin@example.org>
Subject: Your order ACME-1
Date: Mon, 02 Mar 2026 10:00:00 +0000
Content-Type: text/plain

Thanks for your order.
";

const EXPECTED: &str = r#"{
  "extractor": "acme",
  "artifacts": {
    "acme-1.event.ics": "BEGIN:VCALENDAR\r\nUID:acme-1\r\nEND:VCALENDAR\r\n",
    "acme-1.receipt.json": {
      "merchant": "Acme Widgets",
      "orderNumber": "ACME-1"
    },
    "acme-1.ticket.pdf": {
      "sha256": "b3d510ef04275ca8e698e5b3cbb0ece3949ef9252f0cdc839e9ee347409a2209"
    }
  }
}
"#;

struct Setup {
    dir: tempfile::TempDir,
}

impl Setup {
    fn new(script: &str) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let extractors = dir.path().join("extractors");
        fs::create_dir(&extractors).unwrap();
        fs::write(extractors.join("acme.yaml"), MANIFEST).unwrap();
        let script_path = extractors.join("acme.sh");
        fs::write(&script_path, script).unwrap();
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755)).unwrap();
        fs::create_dir(dir.path().join("corpus")).unwrap();
        Setup { dir }
    }

    fn fixture(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.dir.path().join("corpus").join(name);
        fs::write(&path, contents).unwrap();
        path
    }

    /// Run `test-extractor` from the tempdir, so fixture paths print
    /// relative to it.
    fn run(&self, args: &[&str]) -> (Option<i32>, String) {
        let output = common::mailsift()
            .current_dir(self.dir.path())
            .env("RUST_LOG", "error")
            .arg("test-extractor")
            .arg("--extractors")
            .arg("extractors")
            .args(args)
            .output()
            .expect("run mailsift");
        (
            output.status.code(),
            String::from_utf8(output.stdout).expect("utf8 stdout"),
        )
    }

    fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.dir.path().join(rel)).unwrap()
    }
}

fn exists(path: &Path) -> bool {
    path.try_exists().unwrap()
}

#[test]
fn update_creates_expected_file_then_passes() {
    let setup = Setup::new(SCRIPT);
    setup.fixture("acme-order.eml", MESSAGE);

    let (code, stdout) = setup.run(&["corpus/acme-order.eml"]);
    assert_eq!(
        stdout,
        "FAIL  corpus/acme-order.eml: no expected file corpus/acme-order.expected.json; \
         run with --update to create it\n0 passed, 1 failed\n"
    );
    assert_eq!(code, Some(1));

    let (code, stdout) = setup.run(&["--update", "--extractor", "acme", "corpus/acme-order.eml"]);
    assert_eq!(
        stdout,
        "new   corpus/acme-order.eml (acme): wrote corpus/acme-order.expected.json\n\
         0 passed, 1 created, 0 updated, 0 failed\n"
    );
    assert_eq!(code, Some(0));
    assert_eq!(setup.read("corpus/acme-order.expected.json"), EXPECTED);

    let (code, stdout) = setup.run(&["corpus/acme-order.eml"]);
    assert_eq!(
        stdout,
        "ok    corpus/acme-order.eml (acme)\n1 passed, 0 failed\n"
    );
    assert_eq!(code, Some(0));

    let (code, stdout) = setup.run(&["--update", "corpus/acme-order.eml"]);
    assert_eq!(
        stdout,
        "ok    corpus/acme-order.eml (acme)\n1 passed, 0 created, 0 updated, 0 failed\n"
    );
    assert_eq!(code, Some(0));
}

#[test]
fn mismatch_is_reported_per_artifact_and_update_fixes_it() {
    let setup = Setup::new(SCRIPT);
    setup.fixture("acme-order.eml", MESSAGE);
    let stale = EXPECTED
        .replace("\"ACME-1\"", "\"ACME-2\"")
        .replace("UID:acme-1", "UID:acme-2")
        .replace("    \"acme-1.ticket.pdf\": {\n      \"sha256\": \"b3d510ef04275ca8e698e5b3cbb0ece3949ef9252f0cdc839e9ee347409a2209\"\n    }\n", "    \"acme-1.bill.json\": {}\n");
    assert_ne!(stale, EXPECTED);
    setup.fixture("acme-order.expected.json", &stale);

    let (code, stdout) = setup.run(&["corpus/acme-order.eml"]);
    assert_eq!(
        stdout,
        "\
FAIL  corpus/acme-order.eml (acme)
      missing: acme-1.bill.json
      changed: acme-1.event.ics
        @@ -1,3 +1,3 @@
         BEGIN:VCALENDAR
        -UID:acme-2
        +UID:acme-1
         END:VCALENDAR
      changed: acme-1.receipt.json
        $.orderNumber: expected \"ACME-2\", got \"ACME-1\"
      unexpected: acme-1.ticket.pdf
0 passed, 1 failed
"
    );
    assert_eq!(code, Some(1));

    let (code, stdout) = setup.run(&["--update", "corpus/acme-order.eml"]);
    assert_eq!(
        stdout,
        "upd   corpus/acme-order.eml (acme): updated corpus/acme-order.expected.json\n\
         0 passed, 0 created, 1 updated, 0 failed\n"
    );
    assert_eq!(code, Some(0));
    assert_eq!(setup.read("corpus/acme-order.expected.json"), EXPECTED);
}

#[test]
fn fixture_refused_by_dispatch_rules_fails() {
    let setup = Setup::new(SCRIPT);
    setup.fixture(
        "acme-other-sender.eml",
        &MESSAGE.replace("orders@acme.example", "orders@elsewhere.example"),
    );
    setup.fixture(
        "acme-html-only.eml",
        &MESSAGE.replace("text/plain", "text/html"),
    );

    let (code, stdout) = setup.run(&[
        "--update",
        "--extractor",
        "acme",
        "corpus/acme-other-sender.eml",
        "corpus/acme-html-only.eml",
    ]);
    assert_eq!(
        stdout,
        "FAIL  corpus/acme-other-sender.eml: acme would not be dispatched for this message: \
         From: domain \"elsewhere.example\" matches none of from_domains [acme.example]\n\
         FAIL  corpus/acme-html-only.eml: acme would not be dispatched for this message: \
         message lacks requires: text\n\
         0 passed, 0 created, 0 updated, 2 failed\n"
    );
    assert_eq!(code, Some(1));
    assert!(!exists(
        &setup
            .dir
            .path()
            .join("corpus/acme-other-sender.expected.json")
    ));
}

#[test]
fn check_dkim_enforces_require_dkim() {
    let setup = Setup::new(SCRIPT);
    setup.fixture("acme-order.eml", MESSAGE);
    setup.fixture("acme-order.expected.json", EXPECTED);

    let (code, stdout) = setup.run(&["--check-dkim", "corpus/acme-order.eml"]);
    assert_eq!(
        stdout,
        "FAIL  corpus/acme-order.eml: acme would not be dispatched for this message: \
         no passing DKIM signature matches require_dkim [acme.example] (passing: [])\n\
         0 passed, 1 failed\n"
    );
    assert_eq!(code, Some(1));

    setup.fixture(
        "acme-order.eml",
        &format!(
            "Authentication-Results: mx.example.org; dkim=pass header.d=acme.example\n{MESSAGE}"
        ),
    );
    let (code, stdout) = setup.run(&["--check-dkim", "corpus/acme-order.eml"]);
    assert_eq!(
        stdout,
        "ok    corpus/acme-order.eml (acme)\n1 passed, 0 failed\n"
    );
    assert_eq!(code, Some(0));
}

#[test]
fn extractor_flag_must_match_expected_file() {
    let setup = Setup::new(SCRIPT);
    setup.fixture("acme-order.eml", MESSAGE);
    setup.fixture(
        "acme-order.expected.json",
        &EXPECTED.replace("\"extractor\": \"acme\"", "\"extractor\": \"globex\""),
    );

    let (code, stdout) = setup.run(&["--extractor", "acme", "corpus/acme-order.eml"]);
    assert_eq!(
        stdout,
        "FAIL  corpus/acme-order.eml: --extractor acme does not match extractor globex \
         named in corpus/acme-order.expected.json\n0 passed, 1 failed\n"
    );
    assert_eq!(code, Some(1));
}

#[test]
fn unrecognised_output_and_failures_are_reported() {
    let setup = Setup::new("#!/bin/sh\nif grep -q broken; then exit 3; fi\ntouch notes.txt\n");
    setup.fixture("acme-order.eml", MESSAGE);
    setup.fixture(
        "acme-broken.eml",
        &MESSAGE.replace("Thanks for your order.", "broken"),
    );

    let (code, stdout) = setup.run(&[
        "--update",
        "--extractor",
        "acme",
        "corpus/acme-order.eml",
        "corpus/acme-broken.eml",
    ]);
    assert_eq!(code, Some(1));
    assert_eq!(
        stdout,
        "FAIL  corpus/acme-order.eml: wrote unrecognised files mailsift would ignore: notes.txt\n\
         FAIL  corpus/acme-broken.eml: extractor acme failed (exit status: 3)\n\
         0 passed, 0 created, 0 updated, 2 failed\n"
    );
}

#[test]
fn empty_output_is_a_valid_expectation() {
    let setup = Setup::new("#!/bin/sh\ncat > /dev/null\n");
    setup.fixture("acme-newsletter.eml", MESSAGE);

    let (code, _) = setup.run(&[
        "--update",
        "--extractor",
        "acme",
        "corpus/acme-newsletter.eml",
    ]);
    assert_eq!(code, Some(0));
    assert_eq!(
        setup.read("corpus/acme-newsletter.expected.json"),
        "{\n  \"extractor\": \"acme\",\n  \"artifacts\": {}\n}\n"
    );
    let (code, stdout) = setup.run(&["corpus/acme-newsletter.eml"]);
    assert_eq!(
        stdout,
        "ok    corpus/acme-newsletter.eml (acme)\n1 passed, 0 failed\n"
    );
    assert_eq!(code, Some(0));
}

#[test]
fn unknown_extractor_flag_is_a_usage_error() {
    let setup = Setup::new(SCRIPT);
    setup.fixture("acme-order.eml", MESSAGE);
    let (code, stdout) = setup.run(&["--extractor", "globex", "corpus/acme-order.eml"]);
    assert_eq!(code, Some(2));
    assert_eq!(stdout, "");
}

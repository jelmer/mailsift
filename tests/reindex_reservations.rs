use std::fs;

mod common;

#[test]
fn mapping_lists_moves_merges_and_uids_on_a_dry_run_too() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path().join("reservations");
    fs::create_dir_all(dir.join("2046")).unwrap();
    let leg = |received: &str| {
        serde_json::json!({
            "@type": "FlightReservation",
            "reservationNumber": "ABC123",
            "receivedAt": received,
            "reservationFor": {
                "flightNumber": "0491",
                "airline": {"iataCode": "BA", "name": "British Airways"},
                "departureTime": "2046-03-01T08:00:00Z"
            }
        })
    };
    for (name, received) in [
        ("british-airways-abc123-ba0491.json", "2046-01-01T00:00:00Z"),
        ("british-airways-abc123.json", "2046-02-01T00:00:00Z"),
    ] {
        fs::write(
            dir.join("2046").join(name),
            serde_json::to_vec(&leg(received)).unwrap(),
        )
        .unwrap();
    }
    let expected = concat!(
        r#"{"from":"2046/british-airways-abc123-ba0491.json","kind":"moved","to":"2046/ba-abc123-ba491.json"}"#,
        "\n",
        r#"{"from":"2046/british-airways-abc123.json","kind":"merged","to":"2046/ba-abc123-ba491.json"}"#,
        "\n",
        r#"{"from":"flight-ABC123-BA0491@mailsift","kind":"uid","to":"flight-ABC123-BA491@mailsift"}"#,
        "\n",
    );

    for dry_run in [true, false] {
        let mapping = tmp.path().join(format!("mapping-{dry_run}.jsonl"));
        let mut cmd = common::mailsift();
        cmd.arg("reindex-reservations")
            .arg("--reservations-dir")
            .arg(&dir)
            .arg("--mapping-out")
            .arg(&mapping);
        if dry_run {
            cmd.arg("--dry-run");
        }
        cmd.assert().success();
        assert_eq!(fs::read_to_string(&mapping).unwrap(), expected);
    }
    let mut names: Vec<_> = fs::read_dir(dir.join("2046"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, vec!["ba-abc123-ba491.json".to_string()]);
}

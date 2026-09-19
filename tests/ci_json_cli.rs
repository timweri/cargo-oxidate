use chrono::{Duration, Utc};
use serde_json::json;
use std::fs;
use std::process::{Command, Output};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_cargo-oxidate");

fn fixture() -> (TempDir, std::path::PathBuf) {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("Cargo.lock"),
        r#"version = 4

[[package]]
name = "widget"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "0000000000000000000000000000000000000000000000000000000000000000"
"#,
    )
    .unwrap();

    let cache = project.path().join("responses.json");
    fs::write(
        &cache,
        serde_json::to_vec(&json!({
            "version": 1,
            "publish_dates": {
                "widget/1.0.0": (Utc::now() - Duration::days(1)).to_rfc3339(),
            },
            "all_versions": {},
            "index_records": {},
        }))
        .unwrap(),
    )
    .unwrap();
    (project, cache)
}

fn run(project: &TempDir, cache: &std::path::Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .current_dir(project.path())
        .args(args)
        .args([
            "--format",
            "json",
            "--min-age-days",
            "30",
            "--cache-path",
            cache.to_str().unwrap(),
        ])
        .output()
        .unwrap()
}

#[test]
fn json_is_one_document_with_the_same_violation_in_direct_and_cargo_forms() {
    let (project, cache) = fixture();
    for args in [&[][..], &["oxidate"][..]] {
        let output = run(&project, &cache, args);
        assert_eq!(output.status.code(), Some(1));
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.ends_with('\n'));
        let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
        assert_eq!(report["schema_version"], 1);
        assert_eq!(report["status"], "violations");
        assert_eq!(report["summary"]["total_packages"], 1);
        assert_eq!(report["summary"]["violations"], 1);
        assert_eq!(report["violations"][0]["kind"], "too_new");
        assert!(report["warnings"].is_array());
        assert!(report["errors"].is_array());
        assert!(report["suggestions"].is_null());
    }
}

#[test]
fn json_stdout_stays_parseable_at_every_verbosity() {
    let (project, cache) = fixture();
    for args in [&["--quiet"][..], &["--verbose"][..]] {
        let output = run(&project, &cache, args);
        assert_eq!(output.status.code(), Some(1));
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap();
    }
}

#[test]
fn post_parse_input_error_is_a_json_document_with_null_package_counts() {
    let project = tempfile::tempdir().unwrap();
    let output = Command::new(BIN)
        .current_dir(project.path())
        .args(["--format", "json"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["status"], "error");
    assert!(report["summary"]["total_packages"].is_null());
    assert_eq!(report["errors"][0]["category"], "input");
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .starts_with("error:")
    );

    let quiet = Command::new(BIN)
        .current_dir(project.path())
        .args(["--format", "json", "--quiet"])
        .output()
        .unwrap();
    assert_eq!(quiet.status.code(), Some(2));
    serde_json::from_slice::<serde_json::Value>(&quiet.stdout).unwrap();
    assert!(
        String::from_utf8(quiet.stderr)
            .unwrap()
            .starts_with("error:")
    );
}

#[test]
fn invalid_format_is_left_to_clap() {
    let project = tempfile::tempdir().unwrap();
    let output = Command::new(BIN)
        .current_dir(project.path())
        .args(["--format", "yaml", "--min-age-days", "30"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("invalid value")
    );
}

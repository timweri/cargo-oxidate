use chrono::{Duration, Utc};
use serde_json::json;
use std::fs;
use std::process::Command;
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

fn run(project: &TempDir, cache: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(BIN)
        .current_dir(project.path())
        .args(args)
        .args([
            "--min-age-days",
            "30",
            "--cache-path",
            cache.to_str().unwrap(),
        ])
        .output()
        .unwrap()
}

#[test]
fn default_logs_are_concise_and_keep_results_on_stdout() {
    let (project, cache) = fixture();
    let output = run(&project, &cache, &["--exempt", "other"]);

    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    // Read the date back from the fixture so a run across midnight still agrees.
    let cached: serde_json::Value = serde_json::from_slice(&fs::read(&cache).unwrap()).unwrap();
    let published = &cached["publish_dates"]["widget/1.0.0"].as_str().unwrap()[..10];
    let expected_lines = [
        "Dependency age policy".to_string(),
        "  Lockfile: Cargo.lock".to_string(),
        "  Minimum age: 30 days".to_string(),
        "  Maximum age: not set".to_string(),
        "  Exempt packages: other".to_string(),
        "  Missing publish dates: reported as violations".to_string(),
        "Dependency age violations".to_string(),
        format!("  widget@1.0.0: Below minimum age 30 days (published {published}, 1 days old)"),
    ];
    for line in expected_lines {
        assert!(
            stdout.lines().any(|l| l == line),
            "{line:?} not in:\n{stdout}"
        );
    }
    assert!(stderr.contains("Checking dependency ages in"));
    assert!(!stderr.contains("Checking widget@1.0.0"));
}

#[test]
fn quiet_hides_start_and_verbose_shows_each_checked_package() {
    let (project, cache) = fixture();
    let quiet = run(&project, &cache, &["--quiet"]);
    assert_eq!(quiet.status.code(), Some(1));
    let quiet_stderr = String::from_utf8(quiet.stderr).unwrap();
    assert!(!quiet_stderr.contains("Checking dependency ages in"));
    assert!(!quiet_stderr.contains("Checking widget@1.0.0"));

    let verbose = run(&project, &cache, &["--verbose"]);
    assert_eq!(verbose.status.code(), Some(1));
    let verbose_stderr = String::from_utf8(verbose.stderr).unwrap();
    assert!(verbose_stderr.contains("Checking dependency ages in"));
    assert!(verbose_stderr.contains("Checking widget@1.0.0"));
}

#[test]
fn required_errors_stay_on_stderr_in_default_and_quiet_modes() {
    let project = tempfile::tempdir().unwrap();
    fs::write(project.path().join("Cargo.lock"), "this is not a lockfile").unwrap();

    for args in [&[][..], &["--quiet"][..]] {
        let output = Command::new(BIN)
            .current_dir(project.path())
            .args(args)
            .args(["--min-age-days", "30"])
            .output()
            .unwrap();

        assert_eq!(output.status.code(), Some(2), "args: {args:?}");
        let stdout = String::from_utf8(output.stdout).unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stdout.contains("Dependency age check incomplete"));
        assert!(stdout.contains("Coverage unavailable."));
        assert!(!stdout.contains("error:"), "stdout was:\n{stdout}");
        assert_eq!(
            stderr
                .lines()
                .filter(|line| line.starts_with("error: "))
                .count(),
            1,
            "stderr was:\n{stderr}"
        );
        assert!(stderr.contains("Cargo.lock"));
        assert!(stderr.contains("parse error"));
        assert!(!stderr.contains("Checking dependency ages in"));
    }
}

#[test]
fn optional_warnings_stay_on_stderr_in_default_and_quiet_modes() {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("Cargo.lock"),
        r#"version = 4

[[package]]
name = "local-widget"
version = "1.0.0"
"#,
    )
    .unwrap();
    let cache = project.path().join("responses.json");
    for args in [&[][..], &["--quiet"][..]] {
        fs::write(&cache, "not JSON").unwrap();
        let output = Command::new(BIN)
            .current_dir(project.path())
            .args(args)
            .args([
                "--min-age-days",
                "30",
                "--cache-path",
                cache.to_str().unwrap(),
            ])
            .output()
            .unwrap();

        assert_eq!(
            output.status.code(),
            Some(0),
            "args: {args:?}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stdout.contains("No eligible dependencies checked."));
        assert!(stdout.contains("packages: total=1, checked=0"));
        assert!(!stdout.contains("warning:"), "stdout was:\n{stdout}");
        assert_eq!(
            stderr.matches("warning:").count(),
            1,
            "stderr was:\n{stderr}"
        );
        assert!(stderr.contains("Could not read cache"));
        assert_eq!(
            stderr.contains("Checking dependency ages in"),
            args.is_empty(),
            "stderr was:\n{stderr}"
        );
    }
}

fn run_without_lockfile(args: &[&str]) -> (Option<i32>, String) {
    let project = tempfile::tempdir().unwrap();
    let output = Command::new(BIN)
        .current_dir(project.path())
        .args(args)
        .output()
        .unwrap();
    (
        output.status.code(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

#[test]
fn quiet_and_verbose_cannot_be_combined() {
    let (code, stderr) = run_without_lockfile(&["--quiet", "--verbose"]);

    assert_eq!(code, Some(2), "stderr was:\n{stderr}");
    assert!(
        stderr.contains("cannot be used with"),
        "stderr was:\n{stderr}"
    );
}

#[test]
fn numeric_arguments_above_the_cap_are_rejected_by_the_parser() {
    for args in [
        &["--min-age-days", "100001"][..],
        &["--max-age-days", "100001"][..],
        &["--min-age-days", "9223372036854775808"][..],
        &["--min-age-days", "30", "--cache-max-age-hours", "10000001"][..],
        &["--min-age-days", "30", "--timeout", "86401"][..],
        &["--min-age-days", "30", "--timeout", "0"][..],
    ] {
        let (code, stderr) = run_without_lockfile(args);
        assert_eq!(code, Some(2), "args: {args:?}\nstderr:\n{stderr}");
        assert!(stderr.contains("is not in "), "stderr was:\n{stderr}");
    }
}

#[test]
fn numeric_arguments_at_the_cap_pass_the_parser() {
    let (code, stderr) = run_without_lockfile(&[
        "--min-age-days",
        "100000",
        "--max-age-days",
        "100000",
        "--cache-max-age-hours",
        "10000000",
        "--timeout",
        "86400",
    ]);

    assert_eq!(code, Some(2), "stderr was:\n{stderr}");
    assert!(stderr.contains("Cargo.lock"), "stderr was:\n{stderr}");
    assert!(!stderr.contains("is not in "), "stderr was:\n{stderr}");
}

#[test]
fn min_age_above_max_age_is_a_usage_error() {
    let (code, stderr) = run_without_lockfile(&["--min-age-days", "400", "--max-age-days", "30"]);

    assert_eq!(code, Some(2), "stderr was:\n{stderr}");
    assert!(
        stderr.contains("--min-age-days (400) must not exceed --max-age-days (30)"),
        "stderr was:\n{stderr}"
    );
}

#[cfg(unix)]
#[test]
fn non_utf8_lockfile_path_is_a_clean_error() {
    use std::os::unix::ffi::OsStrExt;

    let project = tempfile::tempdir().unwrap();
    let output = Command::new(BIN)
        .current_dir(project.path())
        .arg("--min-age-days")
        .arg("7")
        .arg(std::ffi::OsStr::from_bytes(b"caf\xe9/Cargo.lock"))
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "stderr was:\n{stderr}");
    assert!(
        stderr.contains("Cargo.lock path does not exist"),
        "stderr was:\n{stderr}"
    );
}

#[test]
fn json_is_one_document_with_the_same_violation_in_direct_and_cargo_forms() {
    let (project, cache) = fixture();
    for args in [
        &["--format", "json"][..],
        &["oxidate", "--format", "json"][..],
    ] {
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
    for flag in ["--quiet", "--verbose"] {
        let output = run(&project, &cache, &["--format", "json", flag]);
        assert_eq!(output.status.code(), Some(1));
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap();
    }
}

#[test]
fn post_parse_input_error_is_a_json_document_with_null_package_counts() {
    let project = tempfile::tempdir().unwrap();
    for args in [
        &["--format", "json"][..],
        &["--format", "json", "--quiet"][..],
    ] {
        let output = Command::new(BIN)
            .current_dir(project.path())
            .args(args)
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
    }
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

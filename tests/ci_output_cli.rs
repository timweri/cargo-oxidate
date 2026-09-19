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
    let output = run(&project, &cache, &[]);

    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stdout.contains("Dependency age violations"));
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
fn cargo_subcommand_form_honors_verbose_progress() {
    let (project, cache) = fixture();
    let output = run(&project, &cache, &["oxidate", "--verbose"]);

    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("Checking widget@1.0.0")
    );
}

#[test]
fn quiet_and_verbose_cannot_be_combined() {
    let (project, cache) = fixture();
    let output = run(&project, &cache, &["--quiet", "--verbose"]);

    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("cannot be used with")
    );
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

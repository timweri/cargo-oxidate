use super::*;
use crate::api::test_support::{FakeTransport, ScriptedResponse};
use std::num::NonZeroU32;
use std::time::Duration;

fn package(name: &str, version: &str, is_registry: bool) -> lockfile::Package {
    lockfile::Package {
        name: name.to_string(),
        version: version.to_string(),
        is_registry,
        source: None,
        dependencies: vec![],
    }
}

fn client(transport: FakeTransport) -> api::CratesIoClient<FakeTransport> {
    api::CratesIoClient::with_transport(
        transport,
        None,
        24,
        api::RetryPolicy {
            retry_count: NonZeroU32::new(1).unwrap(),
            retry_delay: Duration::ZERO,
            pacing_delay: Duration::ZERO,
        },
    )
}

fn version_url(name: &str, version: &str) -> String {
    format!("https://crates.io/api/v1/crates/{name}/{version}")
}

fn version_body(created_at: chrono::DateTime<chrono::Utc>) -> String {
    format!(r#"{{"version":{{"created_at":"{created_at}"}}}}"#)
}

#[test]
fn suggest_fix_without_min_age_days_fails_to_parse() {
    let result = Cli::try_parse_from(["cargo-oxidate", "--suggest-fix", "--max-age-days", "30"]);
    assert!(result.is_err());
}

#[test]
fn suggest_fix_with_min_age_days_parses() {
    let result = Cli::try_parse_from(["cargo-oxidate", "--suggest-fix", "--min-age-days", "7"]);
    assert!(result.is_ok());
}

#[test]
fn include_prerelease_without_suggest_fix_fails_to_parse() {
    let result = Cli::try_parse_from([
        "cargo-oxidate",
        "--include-prerelease",
        "--min-age-days",
        "7",
    ]);
    assert!(result.is_err());
}

#[test]
fn include_prerelease_with_suggest_fix_parses() {
    let result = Cli::try_parse_from([
        "cargo-oxidate",
        "--suggest-fix",
        "--min-age-days",
        "7",
        "--include-prerelease",
    ]);
    assert!(result.is_ok());
}

#[test]
fn required_lookup_errors_take_precedence_and_keep_other_findings() {
    let now = chrono::Utc::now();
    let transport = FakeTransport::new();
    transport.push(
        &version_url("young", "1.0.0"),
        ScriptedResponse::Http(200, version_body(now)),
    );
    transport.push(
        &version_url("missing", "1.0.0"),
        ScriptedResponse::Http(404, String::new()),
    );
    transport.push(
        &version_url("broken", "1.0.0"),
        ScriptedResponse::Http(400, String::new()),
    );
    let policy = policy::FreshnessPolicy::new(Some(30), None, false, vec![]).unwrap();
    let report = check_packages(
        &mut client(transport),
        &policy,
        &[
            package("young", "1.0.0", true),
            package("missing", "1.0.0", true),
            package("broken", "1.0.0", true),
            package("local", "0.1.0", false),
        ],
        PathBuf::from("Cargo.lock"),
        now,
        None,
    );

    let summary = report.summary.as_ref().unwrap();
    assert_eq!(report.exit_code(), 2);
    assert_eq!(summary.total_packages, 4);
    assert_eq!(summary.checked_packages, 2);
    assert_eq!(summary.failed_packages, 1);
    assert_eq!(summary.unsupported_packages, 1);
    assert_eq!(report.violations.len(), 2);
    assert_eq!(report.required_errors.len(), 1);
    assert_eq!(report.required_errors[0].package.as_deref(), Some("broken"));
}

#[test]
fn excluded_missing_dates_are_not_required_errors() {
    let now = chrono::Utc::now();
    let transport = FakeTransport::new();
    transport.push(
        &version_url("missing", "1.0.0"),
        ScriptedResponse::Http(404, String::new()),
    );
    let policy = policy::FreshnessPolicy::new(Some(30), None, true, vec![]).unwrap();
    let report = check_packages(
        &mut client(transport),
        &policy,
        &[package("missing", "1.0.0", true)],
        PathBuf::from("Cargo.lock"),
        now,
        None,
    );

    assert_eq!(report.exit_code(), 0);
    assert_eq!(report.summary.unwrap().excluded_missing_packages, 1);
    assert!(report.violations.is_empty());
    assert!(report.required_errors.is_empty());
}

#[test]
fn unsupported_entries_are_counted_before_exemptions() {
    let policy = policy::FreshnessPolicy::new(Some(30), None, false, vec!["same".into()]).unwrap();
    let report = check_packages(
        &mut client(FakeTransport::new()),
        &policy,
        &[
            package("same", "1.0.0", false),
            package("same", "1.0.0", true),
        ],
        PathBuf::from("Cargo.lock"),
        chrono::Utc::now(),
        None,
    );

    assert_eq!(report.exit_code(), 0);
    let summary = report.summary.as_ref().unwrap();
    assert_eq!(summary.unsupported_packages, 1);
    assert_eq!(summary.exempt_packages, 1);
    assert_eq!(summary.checked_packages, 0);
}

use super::*;
use crate::api::test_support::{
    FakeTransport, ScriptedResponse, instant_policy, version_body, version_url,
};

fn package(name: &str, version: &str, is_registry: bool) -> lockfile::Package {
    lockfile::Package {
        name: name.to_string(),
        version: version.to_string(),
        is_registry,
        source: None,
        dependencies: vec![],
    }
}

#[test]
fn suggest_fix_and_include_prerelease_require_their_prerequisite_flags() {
    let cases: [(&[&str], bool); 4] = [
        (&["--suggest-fix", "--max-age-days", "30"], false),
        (&["--suggest-fix", "--min-age-days", "7"], true),
        (&["--include-prerelease", "--min-age-days", "7"], false),
        (
            &[
                "--suggest-fix",
                "--min-age-days",
                "7",
                "--include-prerelease",
            ],
            true,
        ),
    ];
    for (args, parses) in cases {
        let result = Cli::try_parse_from(["cargo-oxidate"].into_iter().chain(args.iter().copied()));
        assert_eq!(result.is_ok(), parses, "args: {args:?}");
    }
}

#[test]
fn required_lookup_errors_take_precedence_and_keep_other_findings() {
    let now = chrono::Utc::now();
    let transport = FakeTransport::default();
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
    transport.push(
        &version_url("flaky", "2.0.0"),
        ScriptedResponse::Http(503, String::new()),
    );
    let policy =
        policy::FreshnessPolicy::new(&report::Policy::new(Some(30), None, false, vec![])).unwrap();
    let report = check_packages(
        &mut api::CratesIoClient::with_transport(transport, None, 24, instant_policy(1)),
        &policy,
        &[
            package("young", "1.0.0", true),
            package("missing", "1.0.0", true),
            package("broken", "1.0.0", true),
            package("flaky", "2.0.0", true),
            package("local", "0.1.0", false),
        ],
        PathBuf::from("Cargo.lock"),
        now,
        None,
    );

    let summary = report.summary.as_ref().unwrap();
    assert_eq!(report.exit_code(), 2);
    assert_eq!(summary.total_packages, 5);
    assert_eq!(summary.checked_packages, 2);
    assert_eq!(summary.failed_packages, 2);
    assert_eq!(summary.unsupported_packages, 1);
    assert_eq!(report.violations.len(), 2);
    assert_eq!(report.required_errors.len(), 2);
    assert_eq!(report.required_errors[0].package.as_deref(), Some("broken"));
    assert_eq!(report.required_errors[0].version.as_deref(), Some("1.0.0"));
    assert!(!report.required_errors[0].retryable);
    assert_eq!(report.required_errors[1].package.as_deref(), Some("flaky"));
    assert_eq!(report.required_errors[1].version.as_deref(), Some("2.0.0"));
    assert!(report.required_errors[1].retryable);
}

#[test]
fn excluded_missing_dates_are_not_required_errors() {
    let now = chrono::Utc::now();
    let transport = FakeTransport::default();
    transport.push(
        &version_url("missing", "1.0.0"),
        ScriptedResponse::Http(404, String::new()),
    );
    let policy =
        policy::FreshnessPolicy::new(&report::Policy::new(Some(30), None, true, vec![])).unwrap();
    let report = check_packages(
        &mut api::CratesIoClient::with_transport(transport, None, 24, instant_policy(1)),
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
    let policy = policy::FreshnessPolicy::new(&report::Policy::new(
        Some(30),
        None,
        false,
        vec!["same".into()],
    ))
    .unwrap();
    let report = check_packages(
        &mut api::CratesIoClient::with_transport(
            FakeTransport::default(),
            None,
            24,
            instant_policy(1),
        ),
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

#[test]
fn unavailable_warning_carries_the_package_spec() {
    let spec = "registry+https://example.test/index#anyhow";
    let warning = unavailable_warning(&suggest::Outcome::Unavailable {
        package: "anyhow".into(),
        package_spec: spec.into(),
        locked_version: "1.0.102".into(),
        reason: "network error".into(),
    })
    .unwrap();

    assert_eq!(warning.category, "suggestion");
    assert_eq!(warning.package.as_deref(), Some("anyhow"));
    assert_eq!(warning.version.as_deref(), Some("1.0.102"));
    assert_eq!(warning.package_spec.as_deref(), Some(spec));
    assert_eq!(
        warning.message,
        format!("Could not determine a downgrade for {spec}@1.0.102: network error")
    );
}

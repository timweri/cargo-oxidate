/// Drives the whole pipeline — lockfile intake, manifest reading, and
/// `generate_suggestions` — over the committed fixture at
/// `tests/fixtures/suggest_fix_e2e/`, a two-member workspace-ish layout
/// (a real workspace with one member) plus a hand-written lockfile.
/// Covers all four outcome kinds at once: a suggestion, a package
/// blocked by a manifest requirement, one blocked by a transitive
/// dependent, and one with nothing old enough in range.
use super::*;
use crate::api::RetryPolicy;
use crate::api::test_support::{FakeTransport, ScriptedResponse, index_url, versions_url};
use crate::manifest::load_direct_requirements;
use crate::report::Aged;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::time::Duration;

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/suggest_fix_e2e")
}

fn versions_body(entries: &[(&str, i64, bool)], now: DateTime<Utc>) -> String {
    let versions: Vec<String> = entries
        .iter()
        .map(|(num, days_ago, yanked)| {
            let created_at = now - chrono::Duration::days(*days_ago);
            format!(
                r#"{{"num":"{num}","created_at":"{}","yanked":{yanked}}}"#,
                created_at.to_rfc3339()
            )
        })
        .collect();
    format!(r#"{{"versions":[{}]}}"#, versions.join(","))
}

fn too_new(package: &str, locked_version: &str) -> Violation {
    Violation {
        package: package.to_string(),
        version: locked_version.to_string(),
        kind: ViolationKind::TooNew(Aged {
            published: now() - chrono::Duration::days(5),
            age_days: 5,
        }),
    }
}

#[test]
fn covers_a_suggestion_two_blocked_kinds_and_no_compliant_version() {
    let dir = fixture_dir();
    let packages = crate::lockfile::load(Path::new("Cargo.lock"), &dir).unwrap();
    let (direct_requirements, warnings) = load_direct_requirements(&dir);
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");

    let transport = FakeTransport::default();
    transport.push(
        &versions_url("alpha"),
        ScriptedResponse::Http(
            200,
            versions_body(&[("1.5.0", 5, false), ("1.4.0", 50, false)], now()),
        ),
    );
    transport.push(
        &versions_url("beta"),
        ScriptedResponse::Http(
            200,
            versions_body(&[("1.5.0", 5, false), ("1.4.0", 50, false)], now()),
        ),
    );
    transport.push(
        &versions_url("gamma"),
        ScriptedResponse::Http(
            200,
            versions_body(&[("1.5.0", 5, false), ("1.4.0", 50, false)], now()),
        ),
    );
    transport.push(
        &versions_url("delta"),
        ScriptedResponse::Http(200, versions_body(&[("1.5.0", 5, false)], now())),
    );
    transport.push(
        &index_url("consumer"),
        ScriptedResponse::Http(
            200,
            r#"{"vers":"2.0.0","deps":[{"name":"gamma","req":"^1.5"}]}"#.to_string(),
        ),
    );

    let mut client = CratesIoClient::with_transport(
        transport,
        None,
        24,
        RetryPolicy {
            retry_count: NonZeroU32::new(1).unwrap(),
            retry_delay: Duration::from_millis(0),
            pacing_delay: Duration::from_millis(0),
        },
    );

    let violations = vec![
        too_new("alpha", "1.5.0"),
        too_new("beta", "1.5.0"),
        too_new("gamma", "1.5.0"),
        too_new("delta", "1.5.0"),
    ];

    let outcomes = suggestions(
        &mut client,
        &violations,
        &packages,
        &direct_requirements,
        &dir,
    );

    assert_eq!(outcomes.len(), 4);

    match &outcomes[0] {
        Outcome::Suggest {
            package,
            suggested_version,
            suggested_age_days,
            unverified_dependents,
            ..
        } => {
            assert_eq!(package, "alpha");
            assert_eq!(suggested_version, "1.4.0");
            assert_eq!(*suggested_age_days, 50);
            assert!(unverified_dependents.is_empty());
        }
        _ => panic!("expected alpha to be Suggest"),
    }

    match &outcomes[1] {
        Outcome::Blocked {
            package,
            newest_compliant,
            blocker,
            ..
        } => {
            assert_eq!(package, "beta");
            assert_eq!(newest_compliant, "1.4.0");
            assert_eq!(blocker.name, "app/Cargo.toml");
            assert_eq!(blocker.version, None);
            assert_eq!(blocker.req, "^1.5");
        }
        _ => panic!("expected beta to be Blocked"),
    }

    match &outcomes[2] {
        Outcome::Blocked {
            package,
            newest_compliant,
            blocker,
            ..
        } => {
            assert_eq!(package, "gamma");
            assert_eq!(newest_compliant, "1.4.0");
            assert_eq!(blocker.name, "consumer");
            assert_eq!(blocker.version, Some("2.0.0".to_string()));
            assert_eq!(blocker.req, "^1.5");
        }
        _ => panic!("expected gamma to be Blocked"),
    }

    assert!(matches!(
        &outcomes[3],
        Outcome::NoCompliantVersion { package, .. } if package == "delta"
    ));
}

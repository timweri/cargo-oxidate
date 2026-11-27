use super::*;
use crate::api::test_support::{
    FakeTransport, ScriptedResponse, index_url, instant_policy, versions_url,
};
use crate::lockfile::PackageRef;
use crate::report::Aged;
use chrono::TimeZone;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap()
}

const MIN_AGE_30: AgeBounds = AgeBounds {
    min_days: 30,
    max_days: None,
};

trait FakeTransportExt {
    /// Scripts a crate's version list: `(num, days_ago)`, none yanked.
    fn versions(&self, name: &str, entries: &[(&str, i64)]);
    fn versions_error(&self, name: &str);
    fn index_ok(&self, name: &str, records_ndjson: &str);
    fn index_error(&self, name: &str);
}

impl FakeTransportExt for FakeTransport {
    fn versions(&self, name: &str, entries: &[(&str, i64)]) {
        let versions: Vec<String> = entries
            .iter()
            .map(|(num, days_ago)| {
                let created_at = now() - chrono::Duration::days(*days_ago);
                format!(
                    r#"{{"num":"{num}","created_at":"{}","yanked":false}}"#,
                    created_at.to_rfc3339()
                )
            })
            .collect();
        let body = format!(r#"{{"versions":[{}]}}"#, versions.join(","));
        self.push(&versions_url(name), ScriptedResponse::Http(200, body));
    }

    fn versions_error(&self, name: &str) {
        self.push(&versions_url(name), ScriptedResponse::Error);
    }

    fn index_ok(&self, name: &str, records_ndjson: &str) {
        self.push(
            &index_url(name),
            ScriptedResponse::Http(200, records_ndjson.to_string()),
        );
    }

    fn index_error(&self, name: &str) {
        self.push(&index_url(name), ScriptedResponse::Error);
    }
}

/// A client with retry/pacing delays zeroed out, so the suite doesn't sleep.
fn fast_client(transport: FakeTransport, cache: Option<&Path>) -> CratesIoClient<FakeTransport> {
    CratesIoClient::with_transport(transport, cache, 24, instant_policy(1))
}

fn too_new(package: &str, locked_version: &str) -> Violation {
    Violation {
        package: package.to_string(),
        version: locked_version.to_string(),
        kind: ViolationKind::TooNew(Aged {
            published: now(),
            age_days: 1,
        }),
    }
}

/// Calls production `generate_suggestions` with the fixed 30-day
/// minimum age, no prerelease admission, and `now()` that almost every
/// call site in this module shares.
fn suggestions(
    transport: FakeTransport,
    violations: &[Violation],
    packages: &[Package],
    direct_requirements: &[DirectRequirement],
    working_dir: &Path,
) -> Vec<Outcome> {
    let mut no_progress = |_| {};
    generate_suggestions(
        &mut fast_client(transport, None),
        violations,
        packages,
        direct_requirements,
        working_dir,
        MIN_AGE_30,
        false,
        now(),
        &mut no_progress,
    )
}

/// One-line rendering of an outcome for whole-value assertions. The
/// package spec is shown only when it differs from the package name.
fn summary(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Suggest {
            package,
            package_spec,
            suggested_version,
            suggested_age_days,
            unverified_dependents,
            ..
        } => {
            let mut line = format!(
                "{package}: suggest {suggested_version} ({suggested_age_days}d){}",
                spec_suffix(package, package_spec)
            );
            if !unverified_dependents.is_empty() {
                line += &format!(" unverified: {}", unverified_dependents.join(","));
            }
            line
        }
        Outcome::Blocked {
            package,
            package_spec,
            newest_compliant,
            blocker,
            ..
        } => {
            let version = blocker
                .version
                .as_deref()
                .map_or_else(String::new, |v| format!("@{v}"));
            let also = if blocker.also_suggested {
                " (also suggested)"
            } else {
                ""
            };
            let blocker_spec = blocker
                .package_spec
                .as_deref()
                .map_or_else(String::new, |spec| format!(" blocker_spec={spec}"));
            format!(
                "{package}: blocked at {newest_compliant} by {}{version} {}{also}{blocker_spec}{}",
                blocker.name,
                blocker.req,
                spec_suffix(package, package_spec)
            )
        }
        Outcome::NoCompliantVersion {
            package,
            package_spec,
            ..
        } => format!(
            "{package}: no compliant version{}",
            spec_suffix(package, package_spec)
        ),
        Outcome::Unavailable {
            package,
            package_spec,
            reason,
            ..
        } => format!(
            "{package}: unavailable ({reason}){}",
            spec_suffix(package, package_spec)
        ),
    }
}

/// ` spec=...` when the spec differs from the package name, else empty.
fn spec_suffix(package: &str, package_spec: &str) -> String {
    if package_spec == package {
        String::new()
    } else {
        format!(" spec={package_spec}")
    }
}

fn summaries(outcomes: &[Outcome]) -> Vec<String> {
    outcomes.iter().map(summary).collect()
}

fn v(s: &str) -> Version {
    Version::parse(s).unwrap()
}

fn make_version(version: &str, days_ago: i64, yanked: bool) -> CrateVersionInfo {
    let created_at = now() - chrono::Duration::days(days_ago);
    CrateVersionInfo {
        num: version.to_string(),
        created_at,
        yanked,
    }
}

fn constraint(req: &str) -> Constraint {
    Constraint {
        blocker_name: "dep".to_string(),
        blocker_version: Some("1.0.0".to_string()),
        blocker_source: None,
        req: VersionReq::parse(req).unwrap(),
    }
}

mod filter_candidates_tests;
mod generate_suggestions_tests;
mod walk_tests;

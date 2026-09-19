use super::*;
use crate::lockfile::PackageRef;
use chrono::TimeZone;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap()
}

/// Calls production `generate_suggestions` with the fixed 30-day
/// minimum age, no prerelease admission, and `now()` that almost every
/// call site in this module shares, unwrapping the `Some` result.
fn suggestions<T: Transport>(
    client: &mut CratesIoClient<T>,
    violations: &[Violation],
    packages: &[Package],
    direct_requirements: &[DirectRequirement],
    working_dir: &Path,
) -> Vec<Outcome> {
    generate_suggestions(
        client,
        violations,
        packages,
        direct_requirements,
        working_dir,
        30,
        false,
        now(),
    )
    .unwrap()
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
        req: VersionReq::parse(req).unwrap(),
    }
}

mod end_to_end_tests;
mod filter_candidates_tests;
mod generate_suggestions_tests;
mod manifest_dependent_identity_tests;
mod manifest_registry_identity_tests;
mod source_collision_cargo_tests;
mod walk_tests;

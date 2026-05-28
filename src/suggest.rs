use crate::api::CrateVersionInfo;
use chrono::Utc;

pub struct Suggestion {
    pub package: String,
    pub suggested_version: String,
    pub suggested_age_days: i64,
}

pub struct UpgradeSuggestion {
    pub package: String,
    pub current_version: String,
    pub suggested_version: String,
    pub suggested_age_days: i64,
    pub is_direct: bool,
}

pub fn find_compliant_version(
    versions: &[CrateVersionInfo],
    min_age_days: u64,
) -> Option<(String, i64)> {
    let now = Utc::now();
    let min_age_threshold = now - chrono::Duration::days(min_age_days as i64);

    versions
        .iter()
        .filter(|v| !v.yanked && v.created_at <= min_age_threshold)
        .max_by_key(|v| v.created_at)
        .map(|v| {
            let age_days = (now - v.created_at).num_days();
            (v.num.clone(), age_days)
        })
}

/// Finds the newest age-compliant, non-yanked version strictly greater than
/// `current_version_str`, optionally constrained by a `VersionReq`.
///
/// Pre-release versions (anything with a `-` tag: `-alpha`, `-alpha.1`,
/// `-beta.2`, `-rc.1`, `-pre`, `-0.x`, etc.) are always excluded - there is no
/// opt-in. Build metadata (`+build...`) does not affect ordering and is fine.
pub fn find_upgrade_version(
    versions: &[CrateVersionInfo],
    min_age_days: u64,
    current_version_str: &str,
    version_req: Option<&semver::VersionReq>,
) -> Option<(String, i64)> {
    let now = Utc::now();
    let min_age_threshold = now - chrono::Duration::days(min_age_days as i64);
    let current = semver::Version::parse(current_version_str).ok()?;

    versions
        .iter()
        .filter(|v| !v.yanked && v.created_at <= min_age_threshold)
        .filter(|v| {
            semver::Version::parse(&v.num)
                .map(|sv| {
                    sv.pre.is_empty()
                        && sv > current
                        && version_req.map_or(true, |req| req.matches(&sv))
                })
                .unwrap_or(false)
        })
        .max_by_key(|v| v.created_at)
        .map(|v| {
            let age_days = (now - v.created_at).num_days();
            (v.num.clone(), age_days)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn make_version(version: &str, days_ago: i64, yanked: bool) -> CrateVersionInfo {
        let created_at = Utc::now() - chrono::Duration::days(days_ago);
        CrateVersionInfo {
            num: version.to_string(),
            created_at,
            yanked,
        }
    }

    #[test]
    fn test_find_compliant_version_basic() {
        let versions = vec![
            make_version("1.0.0", 100, false),
            make_version("1.1.0", 50, false),
            make_version("1.2.0", 20, false),
            make_version("1.3.0", 5, false),
        ];

        let result = find_compliant_version(&versions, 30);
        assert!(result.is_some());
        let (version, age_days) = result.unwrap();
        assert_eq!(version, "1.1.0"); // Newest version older than 30 days
        assert!(age_days >= 50 && age_days <= 51); // Allow for timing variance
    }

    #[test]
    fn test_find_compliant_version_filters_yanked() {
        let versions = vec![
            make_version("1.0.0", 100, false),
            make_version("1.1.0", 50, true), // yanked
            make_version("1.2.0", 20, false),
        ];

        let result = find_compliant_version(&versions, 30);
        assert!(result.is_some());
        let (version, _) = result.unwrap();
        assert_eq!(version, "1.0.0"); // Skips yanked 1.1.0
    }

    #[test]
    fn test_find_compliant_version_no_compliant() {
        let versions = vec![
            make_version("1.0.0", 10, false),
            make_version("1.1.0", 5, false),
            make_version("1.2.0", 2, false),
        ];

        let result = find_compliant_version(&versions, 30);
        assert!(result.is_none());
    }

    #[test]
    fn test_find_compliant_version_all_yanked() {
        let versions = vec![
            make_version("1.0.0", 100, true),
            make_version("1.1.0", 50, true),
        ];

        let result = find_compliant_version(&versions, 30);
        assert!(result.is_none());
    }

    #[test]
    fn test_find_compliant_version_empty() {
        let versions: Vec<CrateVersionInfo> = vec![];
        let result = find_compliant_version(&versions, 30);
        assert!(result.is_none());
    }

    #[test]
    fn test_find_compliant_version_exact_threshold() {
        let versions = vec![
            make_version("1.0.0", 30, false),
            make_version("1.1.0", 29, false),
        ];

        let result = find_compliant_version(&versions, 30);
        assert!(result.is_some());
        let (version, _) = result.unwrap();
        assert_eq!(version, "1.0.0"); // Exactly 30 days should be compliant
    }

    #[test]
    fn test_find_compliant_version_picks_newest_compliant() {
        let versions = vec![
            make_version("1.0.0", 100, false),
            make_version("1.1.0", 90, false),
            make_version("1.2.0", 80, false),
            make_version("1.3.0", 70, false),
            make_version("1.4.0", 10, false), // Too new
        ];

        let result = find_compliant_version(&versions, 50);
        assert!(result.is_some());
        let (version, _) = result.unwrap();
        assert_eq!(version, "1.3.0"); // Newest among compliant versions
    }

    #[test]
    fn test_find_upgrade_version_upgrade_available() {
        let versions = vec![
            make_version("1.0.0", 100, false),
            make_version("1.1.0", 50, false),
            make_version("1.2.0", 20, false),
        ];
        let result = find_upgrade_version(&versions, 30, "1.0.0", None);
        assert!(result.is_some());
        let (ver, _) = result.unwrap();
        assert_eq!(ver, "1.1.0");
    }

    #[test]
    fn test_find_upgrade_version_already_newest() {
        let versions = vec![
            make_version("1.0.0", 100, false),
            make_version("1.1.0", 50, false),
        ];
        let result = find_upgrade_version(&versions, 30, "1.1.0", None);
        assert!(result.is_none()); // Already at newest compliant
    }

    #[test]
    fn test_find_upgrade_version_current_is_newer() {
        let versions = vec![
            make_version("1.0.0", 100, false),
            make_version("1.1.0", 50, false),
            make_version("1.2.0", 5, false), // Too new
        ];
        // Current version is 1.2.0 (which is too new), newest compliant is 1.1.0
        // 1.1.0 < 1.2.0, so no upgrade suggestion
        let result = find_upgrade_version(&versions, 30, "1.2.0", None);
        assert!(result.is_none());
    }

    #[test]
    fn test_find_upgrade_version_invalid_current_version() {
        let versions = vec![make_version("1.0.0", 100, false)];
        let result = find_upgrade_version(&versions, 30, "not-a-version", None);
        assert!(result.is_none());
    }

    #[test]
    fn test_find_upgrade_version_with_version_req() {
        let versions = vec![
            make_version("1.0.0", 100, false),
            make_version("1.5.0", 50, false),
            make_version("2.0.0", 40, false),
        ];
        let req = semver::VersionReq::parse("^1.0").unwrap();
        let result = find_upgrade_version(&versions, 30, "1.0.0", Some(&req));
        assert!(result.is_some());
        let (ver, _) = result.unwrap();
        assert_eq!(ver, "1.5.0"); // 2.0.0 is filtered out by version req
    }

    #[test]
    fn test_find_upgrade_version_excludes_prerelease() {
        let versions = vec![
            make_version("1.0.0", 100, false),
            make_version("1.1.0-alpha.1", 50, false),
            make_version("1.1.0", 40, false),
        ];
        let result = find_upgrade_version(&versions, 30, "1.0.0", None);
        assert!(result.is_some());
        let (ver, _) = result.unwrap();
        assert_eq!(ver, "1.1.0"); // Skips 1.1.0-alpha.1
    }
}

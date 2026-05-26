use crate::api::CrateVersionInfo;
use chrono::Utc;

pub struct Suggestion {
    pub package: String,
    pub current_version: String,
    pub suggested_version: String,
    pub suggested_age_days: i64,
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
}

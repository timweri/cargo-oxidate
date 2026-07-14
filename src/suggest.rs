use crate::api::CrateVersionInfo;
use chrono::{DateTime, Utc};
use semver::Version;

pub fn find_compliant_versions(
    versions: &[CrateVersionInfo],
    current_version: &str,
    min_age_days: u64,
    max_age_days: Option<u64>,
) -> Vec<(String, i64)> {
    find_compliant_versions_at(
        versions,
        current_version,
        min_age_days,
        max_age_days,
        Utc::now(),
    )
}

fn find_compliant_versions_at(
    versions: &[CrateVersionInfo],
    current_version: &str,
    min_age_days: u64,
    max_age_days: Option<u64>,
    now: DateTime<Utc>,
) -> Vec<(String, i64)> {
    let Ok(current_version) = Version::parse(current_version) else {
        return Vec::new();
    };
    let min_age_threshold = now - chrono::Duration::days(min_age_days as i64);
    let max_age_threshold = max_age_days.map(|days| now - chrono::Duration::days(days as i64));

    let mut candidates: Vec<_> = versions
        .iter()
        .filter_map(|v| {
            let version = Version::parse(&v.num).ok()?;
            if v.yanked
                || version >= current_version
                || v.created_at > min_age_threshold
                || max_age_threshold.is_some_and(|threshold| v.created_at < threshold)
            {
                return None;
            }
            let age_days = (now - v.created_at).num_days();
            Some((version, v.num.clone(), age_days))
        })
        .collect();

    candidates.sort_by(|a, b| b.0.cmp(&a.0));
    candidates
        .into_iter()
        .map(|(_, version, age_days)| (version, age_days))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn make_version(version: &str, days_ago: i64, yanked: bool) -> CrateVersionInfo {
        let created_at = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
            - chrono::Duration::days(days_ago);
        CrateVersionInfo {
            num: version.to_string(),
            created_at,
            yanked,
        }
    }

    #[test]
    fn test_find_compliant_versions_choose_closest_downgrade() {
        let versions = vec![
            make_version("1.0.0", 100, false),
            make_version("1.1.0", 50, false),
            make_version("1.2.0", 40, false),
            make_version("1.3.0", 5, false),
        ];

        let result = find_compliant_versions_at(
            &versions,
            "1.3.0",
            30,
            None,
            Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
        );
        assert_eq!(result[0].0, "1.2.0");
    }

    #[test]
    fn test_find_compliant_versions_filter_yanked() {
        let versions = vec![
            make_version("1.0.0", 100, false),
            make_version("1.1.0", 50, true), // yanked
            make_version("1.2.0", 20, false),
        ];

        let result = find_compliant_versions_at(
            &versions,
            "1.2.0",
            30,
            None,
            Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
        );
        assert_eq!(result[0].0, "1.0.0");
    }

    #[test]
    fn test_find_compliant_versions_no_compliant() {
        let versions = vec![
            make_version("1.0.0", 10, false),
            make_version("1.1.0", 5, false),
            make_version("1.2.0", 2, false),
        ];

        let result = find_compliant_versions_at(
            &versions,
            "1.3.0",
            30,
            None,
            Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
        );
        assert!(result.is_empty());
    }

    #[test]
    fn test_find_compliant_versions_all_yanked() {
        let versions = vec![
            make_version("1.0.0", 100, true),
            make_version("1.1.0", 50, true),
        ];

        let result = find_compliant_versions_at(
            &versions,
            "1.2.0",
            30,
            None,
            Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
        );
        assert!(result.is_empty());
    }

    #[test]
    fn test_find_compliant_versions_empty() {
        let versions: Vec<CrateVersionInfo> = vec![];
        let result = find_compliant_versions_at(
            &versions,
            "1.0.0",
            30,
            None,
            Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
        );
        assert!(result.is_empty());
    }

    #[test]
    fn test_find_compliant_versions_exact_threshold() {
        let versions = vec![
            make_version("1.0.0", 30, false),
            make_version("1.1.0", 29, false),
        ];

        let result = find_compliant_versions_at(
            &versions,
            "1.2.0",
            30,
            None,
            Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
        );
        assert_eq!(result[0].0, "1.0.0");
    }

    #[test]
    fn test_find_compliant_versions_respect_maximum_age() {
        let versions = vec![
            make_version("1.0.0", 100, false),
            make_version("1.1.0", 90, false),
            make_version("1.2.0", 80, false),
            make_version("1.3.0", 90, false),
            make_version("1.4.0", 10, false), // Too new
        ];

        let result = find_compliant_versions_at(
            &versions,
            "1.4.0",
            50,
            Some(85),
            Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
        );
        assert_eq!(result[0].0, "1.2.0");
    }
}

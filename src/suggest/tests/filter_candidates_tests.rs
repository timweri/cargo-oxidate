use super::*;

/// The versions `filter_candidates` keeps for `locked`, in order.
fn kept(
    versions: &[CrateVersionInfo],
    locked: &str,
    bounds: AgeBounds,
    allow_prerelease: bool,
) -> Vec<String> {
    filter_candidates(versions, &v(locked), bounds, now(), allow_prerelease)
        .into_iter()
        .map(|(version, _)| version.to_string())
        .collect()
}

/// Versions published `days_ago` days before `now()`, none yanked.
fn published(entries: &[(&str, i64)]) -> Vec<CrateVersionInfo> {
    entries
        .iter()
        .map(|&(num, days_ago)| make_version(num, days_ago, false))
        .collect()
}

#[test]
fn excludes_too_new_yanked_and_out_of_range() {
    let versions = vec![
        make_version("1.0.0", 100, false),
        make_version("1.1.0", 50, true), // yanked
        make_version("1.2.0", 40, false),
        make_version("0.9.0", 200, false), // different major: out of range
        make_version("1.3.0", 5, false),   // too new (min age 30)
    ];

    assert_eq!(
        kept(&versions, "1.5.0", MIN_AGE_30, false),
        ["1.2.0", "1.0.0"]
    );
}

#[test]
fn higher_semver_precedence_wins_over_more_recent_publish_date() {
    // 1.4.0 outranks 1.3.9 by semver even though 1.3.9 was published
    // more recently, and the oldest-published 1.2.0 sorts last.
    let versions = published(&[("1.2.0", 200), ("1.4.0", 100), ("1.3.9", 40)]);

    assert_eq!(
        kept(&versions, "1.5.0", MIN_AGE_30, false),
        ["1.4.0", "1.3.9", "1.2.0"]
    );
}

#[test]
fn publish_date_breaks_ties_in_equal_semver_precedence() {
    // Build metadata doesn't affect precedence, so the more recently
    // published build.2 comes first.
    let versions = published(&[("1.3.0+build.1", 100), ("1.3.0+build.2", 50)]);

    assert_eq!(
        kept(&versions, "1.5.0", MIN_AGE_30, false),
        ["1.3.0+build.2", "1.3.0+build.1"]
    );
}

#[test]
fn excludes_versions_that_do_not_precede_locked() {
    // (candidates, locked): none may be offered as a downgrade.
    let cases = [
        // Higher semver, though published before locked.
        (&[("1.4.0", 100), ("1.3.0", 50)][..], "1.3.0"),
        // Equal precedence: build metadata is ignored, on either side.
        (&[("1.3.0", 100), ("1.3.0+build.1", 100)], "1.3.0"),
        (&[("1.3.0", 100), ("1.3.0+build.1", 100)], "1.3.0+build.2"),
        // A stable release or later prerelease outranks the locked prerelease.
        (&[("1.0.0", 100)], "1.0.0-beta.1"),
        (&[("1.0.0-beta.2", 100)], "1.0.0-beta.1"),
    ];
    for (candidates, locked) in cases {
        let versions = published(candidates);
        assert_eq!(
            kept(&versions, locked, MIN_AGE_30, false),
            Vec::<String>::new(),
            "locked {locked}"
        );
    }
}

#[test]
fn prereleases_are_kept_only_when_allowed_or_locked_is_a_prerelease() {
    // (locked, allow_prerelease, candidate kept?)
    let cases = [
        ("1.2.0", false, false),
        ("1.0.0", true, true),
        ("1.0.0-beta.2", false, true),
    ];
    for (locked, allow_prerelease, expected) in cases {
        let versions = published(&[("1.0.0-beta.1", 100)]);
        let candidates = kept(&versions, locked, MIN_AGE_30, allow_prerelease);
        assert_eq!(
            !candidates.is_empty(),
            expected,
            "locked {locked}, allow_prerelease {allow_prerelease}"
        );
    }
}

#[test]
fn max_age_is_inclusive() {
    let versions = published(&[("1.0.0", 365), ("1.1.0", 366), ("1.2.0", 1100)]);
    let bounds = AgeBounds {
        min_days: 30,
        max_days: Some(365),
    };

    assert_eq!(kept(&versions, "1.5.0", bounds, false), ["1.0.0"]);
}

#[test]
fn keeps_only_the_locked_versions_caret_compatible_zone() {
    // (locked, candidates, expected kept)
    let cases: [(_, &[_], &[&str]); 3] = [
        // 0.x: the minor is the zone.
        (
            "0.3.5",
            &[("0.3.1", 100), ("0.2.9", 100), ("0.0.5", 100)],
            &["0.3.1"],
        ),
        // 0.0.x: the patch is the zone, so nothing lower is compatible.
        ("0.0.5", &[("0.0.4", 100), ("0.0.1", 100)], &[]),
        // 1.x: the major is the zone, even against a 0.x sharing its minor.
        ("1.3.0", &[("1.0.0", 100), ("0.3.0", 100)], &["1.0.0"]),
    ];
    for (locked, candidates, expected) in cases {
        let versions = published(candidates);
        assert_eq!(
            kept(&versions, locked, MIN_AGE_30, false),
            expected,
            "locked {locked}"
        );
    }
}

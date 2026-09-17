use super::*;

#[test]
fn excludes_too_new_yanked_and_out_of_range() {
    let versions = vec![
        make_version("1.0.0", 100, false),
        make_version("1.1.0", 50, true),   // yanked
        make_version("1.2.0", 40, false),  // compliant, same major as locked
        make_version("0.9.0", 200, false), // different major: out of range
        make_version("1.3.0", 5, false),   // too new (min age 30)
    ];

    let result = filter_candidates(&versions, &v("1.5.0"), 30, now(), false);
    let nums: Vec<String> = result.iter().map(|(ver, _)| ver.to_string()).collect();
    // Newest-first by publish date among the two survivors.
    assert_eq!(nums, vec!["1.2.0".to_string(), "1.0.0".to_string()]);
}

#[test]
fn sorted_newest_first_by_publish_date() {
    let versions = vec![
        make_version("1.0.0", 100, false),
        make_version("1.1.0", 200, false),
        make_version("1.2.0", 50, false),
    ];

    let result = filter_candidates(&versions, &v("1.5.0"), 30, now(), false);
    let nums: Vec<String> = result.iter().map(|(ver, _)| ver.to_string()).collect();
    assert_eq!(nums, vec!["1.2.0", "1.0.0", "1.1.0"]);
}

#[test]
fn prerelease_excluded_by_default() {
    let versions = vec![make_version("1.1.0-beta.1", 100, false)];
    let result = filter_candidates(&versions, &v("1.0.0"), 30, now(), false);
    assert!(result.is_empty());
}

#[test]
fn prerelease_included_with_flag_when_range_matches() {
    // Same compatible zone (1.0.0), prerelease allowed by the flag.
    let versions = vec![make_version("1.0.0-beta.1", 100, false)];
    let result = filter_candidates(&versions, &v("1.0.0-beta.2"), 30, now(), true);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].0.to_string(), "1.0.0-beta.1");
}

#[test]
fn prerelease_allowed_when_locked_is_itself_a_prerelease() {
    let versions = vec![make_version("1.0.0-beta.1", 100, false)];
    let result = filter_candidates(&versions, &v("1.0.0-beta.2"), 30, now(), false);
    assert_eq!(result.len(), 1);
}

#[test]
fn excludes_a_higher_version_published_earlier_than_locked() {
    // "1.4.0" was published before "1.3.0" but is a higher semantic
    // version, so it must never be offered as a downgrade even
    // though it's older on the publish timeline.
    let versions = vec![
        make_version("1.4.0", 100, false),
        make_version("1.3.0", 50, false),
    ];
    let result = filter_candidates(&versions, &v("1.3.0"), 30, now(), false);
    let nums: Vec<String> = result.iter().map(|(ver, _)| ver.to_string()).collect();
    assert!(nums.is_empty(), "expected no candidates, got {nums:?}");
}

#[test]
fn excludes_a_version_equal_in_precedence_including_build_metadata_only_differences() {
    let versions = vec![
        make_version("1.3.0", 100, false),
        make_version("1.3.0+build.1", 100, false),
    ];
    let result = filter_candidates(&versions, &v("1.3.0"), 30, now(), false);
    assert!(result.is_empty());
}

#[test]
fn excludes_versions_equal_in_precedence_to_a_locked_version_with_build_metadata() {
    // Locked itself carries build metadata this time: candidates
    // differing only in build metadata (or lacking it) still have
    // equal semantic precedence and must not be offered.
    let versions = vec![
        make_version("1.3.0", 100, false),
        make_version("1.3.0+build.1", 100, false),
    ];
    let result = filter_candidates(&versions, &v("1.3.0+build.2"), 30, now(), false);
    assert!(result.is_empty());
}

#[test]
fn excludes_stable_release_above_a_locked_prerelease() {
    // A stable release outranks any prerelease of the same
    // major.minor.patch, so it must not be offered as a "downgrade"
    // from a locked prerelease.
    let versions = vec![make_version("1.0.0", 100, false)];
    let result = filter_candidates(&versions, &v("1.0.0-beta.1"), 30, now(), false);
    assert!(result.is_empty());
}

#[test]
fn excludes_a_later_prerelease_above_a_locked_prerelease() {
    let versions = vec![make_version("1.0.0-beta.2", 100, false)];
    let result = filter_candidates(&versions, &v("1.0.0-beta.1"), 30, now(), false);
    assert!(result.is_empty());
}

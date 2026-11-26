use super::*;

/// Walks `(version, age_days)` candidates against `reqs` and renders the result.
fn walked(candidates: &[(&str, i64)], reqs: &[&str]) -> String {
    let candidates = candidates.iter().map(|&(ver, age)| (v(ver), age)).collect();
    let constraints = reqs.iter().map(|req| constraint(req)).collect();
    match walk(candidates, constraints) {
        WalkResult::Suggest(version, age) => format!("suggest {version} ({age}d)"),
        WalkResult::Blocked {
            newest_compliant,
            blocker,
        } => format!("blocked at {newest_compliant} by {}", blocker.req),
        WalkResult::NoCompliantVersion => "no compliant version".to_string(),
    }
}

#[test]
fn newest_accepted_when_every_constraint_matches() {
    assert_eq!(
        walked(&[("1.3.0", 5), ("1.2.0", 20)], &["^1.2"]),
        "suggest 1.3.0 (5d)"
    );
}

#[test]
fn walk_continues_to_older_version_when_newest_is_rejected() {
    // `~1.1` narrows to the 1.1.x line, so only the oldest candidate
    // satisfies it — the walk must skip past the two newer ones.
    assert_eq!(
        walked(&[("1.3.0", 5), ("1.2.0", 20), ("1.1.0", 40)], &["~1.1"]),
        "suggest 1.1.0 (40d)"
    );
}

#[test]
fn blocked_when_no_candidate_satisfies_every_constraint() {
    assert_eq!(
        walked(&[("1.3.0", 5), ("1.2.0", 20)], &["^2.0"]),
        "blocked at 1.3.0 by ^2.0"
    );
}

#[test]
fn blocker_is_the_constraint_that_rejects_every_candidate() {
    // `<=1.3` comes first and rejects the newest candidate, but 1.2.0
    // satisfies it — only `>=1.4.5` makes every downgrade impossible.
    assert_eq!(
        walked(&[("1.4.0", 5), ("1.2.0", 20)], &["<=1.3", ">=1.4.5"]),
        "blocked at 1.4.0 by >=1.4.5"
    );
}

#[test]
fn blocker_falls_back_when_no_single_constraint_blocks_all_candidates() {
    // The block is a genuine combination, so the first constraint that
    // rejects the newest candidate is reported.
    assert_eq!(
        walked(&[("1.4.0", 5), ("1.2.0", 20)], &["<=1.3", ">=1.4"]),
        "blocked at 1.4.0 by <=1.3"
    );
}

#[test]
fn no_compliant_version_when_candidates_empty() {
    assert_eq!(walked(&[], &["^1.0"]), "no compliant version");
}

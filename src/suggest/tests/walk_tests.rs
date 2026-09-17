use super::*;

#[test]
fn newest_accepted_when_every_constraint_matches() {
    let candidates = vec![(v("1.3.0"), 5), (v("1.2.0"), 20)];
    let constraints = vec![constraint("^1.2")];

    match walk(candidates, constraints) {
        WalkResult::Suggest(version, age) => {
            assert_eq!(version.to_string(), "1.3.0");
            assert_eq!(age, 5);
        }
        _ => panic!("expected Suggest"),
    }
}

#[test]
fn walk_continues_to_older_version_when_newest_is_rejected() {
    let candidates = vec![(v("1.3.0"), 5), (v("1.2.0"), 20), (v("1.1.0"), 40)];
    // `~1.1` narrows to the 1.1.x line, so only the oldest candidate
    // satisfies it — the walk must skip past the two newer ones.
    let constraints = vec![constraint("~1.1")];
    match walk(candidates, constraints) {
        WalkResult::Suggest(version, _) => assert_eq!(version.to_string(), "1.1.0"),
        _ => panic!("expected Suggest"),
    }
}

#[test]
fn blocked_when_no_candidate_satisfies_every_constraint() {
    let candidates = vec![(v("1.3.0"), 5), (v("1.2.0"), 20)];
    let constraints = vec![constraint("^2.0")];

    match walk(candidates, constraints) {
        WalkResult::Blocked {
            newest_compliant,
            blocker,
        } => {
            assert_eq!(newest_compliant.to_string(), "1.3.0");
            assert_eq!(blocker.blocker_name, "dep");
            assert_eq!(blocker.req.to_string(), "^2.0");
        }
        _ => panic!("expected Blocked"),
    }
}

#[test]
// Pins that the blocker is the constraint rejecting every candidate.
// `<=1.3` comes first and rejects the newest candidate, but 1.2.0
// satisfies it — only `>=1.4.5` makes every downgrade impossible.
fn blocker_is_the_constraint_that_rejects_every_candidate() {
    let candidates = vec![(v("1.4.0"), 5), (v("1.2.0"), 20)];
    let constraints = vec![constraint("<=1.3"), constraint(">=1.4.5")];

    match walk(candidates, constraints) {
        WalkResult::Blocked { blocker, .. } => {
            assert_eq!(blocker.req.to_string(), ">=1.4.5");
        }
        _ => panic!("expected Blocked"),
    }
}

#[test]
// Pins the fallback: when no single constraint rejects every
// candidate, the block is a genuine combination, so we fall back to
// the first constraint that rejects the newest candidate.
fn blocker_falls_back_when_no_single_constraint_blocks_all_candidates() {
    let candidates = vec![(v("1.4.0"), 5), (v("1.2.0"), 20)];
    let constraints = vec![constraint("<=1.3"), constraint(">=1.4")];

    match walk(candidates, constraints) {
        WalkResult::Blocked { blocker, .. } => {
            assert_eq!(blocker.req.to_string(), "<=1.3");
        }
        _ => panic!("expected Blocked"),
    }
}

#[test]
fn no_compliant_version_when_candidates_empty() {
    match walk(vec![], vec![constraint("^1.0")]) {
        WalkResult::NoCompliantVersion => {}
        _ => panic!("expected NoCompliantVersion"),
    }
}

#[test]
fn no_constraints_picks_newest_by_age() {
    let candidates = vec![(v("1.3.0"), 5), (v("1.2.0"), 20)];
    match walk(candidates, vec![]) {
        WalkResult::Suggest(version, age) => {
            assert_eq!(version.to_string(), "1.3.0");
            assert_eq!(age, 5);
        }
        _ => panic!("expected Suggest"),
    }
}

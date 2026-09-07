use crate::api::{CrateVersionInfo, CratesIoClient, Transport};
use crate::lockfile::Package;
use crate::manifest::DirectRequirement;
use crate::report::{Violation, ViolationKind};
use chrono::{DateTime, Utc};
use semver::{Version, VersionReq};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// A version requirement currently placed on a package, and who placed it —
/// either a dependent recorded in the lockfile (`blocker_version: Some`) or
/// one of the user's own manifests (`blocker_version: None`).
pub struct Constraint {
    pub blocker_name: String,
    pub blocker_version: Option<String>,
    pub req: VersionReq,
}

/// The package and requirement standing in the way of a downgrade.
pub struct Blocker {
    pub name: String,
    pub version: Option<String>,
    pub req: String,
    /// Set when `name` is itself a package this run suggests downgrading —
    /// applying that suggestion first may unblock this one.
    pub also_suggested: bool,
}

/// The single outcome of checking one "too new" violation: a working
/// suggestion, a package nothing could unblock, or one with no candidate
/// old enough in range at all.
pub enum Outcome {
    Suggest {
        package: String,
        locked_version: String,
        suggested_version: String,
        suggested_age_days: i64,
        unverified_dependents: Vec<String>,
    },
    Blocked {
        package: String,
        locked_version: String,
        newest_compliant: String,
        blocker: Blocker,
    },
    NoCompliantVersion {
        package: String,
        locked_version: String,
    },
}

/// Whether `a` and `b` fall in the same caret-compatible zone: the same
/// leading nonzero component (major, or minor when major is 0, or patch
/// when both are 0) — cargo's own notion of "compatible" versions. Unlike
/// parsing `^{a}` as a requirement and matching `b` against it, this is
/// symmetric, which is what bounding a *downgrade* search needs: a caret
/// requirement built from the locked version only ever accepts versions at
/// or above it.
fn same_compatible_zone(a: &Version, b: &Version) -> bool {
    if a.major != 0 || b.major != 0 {
        a.major == b.major
    } else if a.minor != 0 || b.minor != 0 {
        a.minor == b.minor
    } else {
        a.patch == b.patch
    }
}

/// Filters `versions` to non-yanked, at least `min_age_days` old as of
/// `now`, within the caret-compatible zone of `locked`, sorted newest first
/// by publish date. Prereleases are excluded unless `allow_prerelease` is
/// set or `locked` is itself a prerelease.
fn filter_candidates(
    versions: &[CrateVersionInfo],
    locked: &Version,
    min_age_days: u64,
    now: DateTime<Utc>,
    allow_prerelease: bool,
) -> Vec<(Version, i64)> {
    let min_age_threshold = now - chrono::Duration::days(min_age_days as i64);
    let allow_pre = allow_prerelease || !locked.pre.is_empty();

    let mut candidates: Vec<(Version, DateTime<Utc>, i64)> = versions
        .iter()
        .filter(|v| !v.yanked && v.created_at <= min_age_threshold)
        .filter_map(|v| {
            let parsed = Version::parse(&v.num).ok()?;
            if !allow_pre && !parsed.pre.is_empty() {
                return None;
            }
            if !same_compatible_zone(locked, &parsed) {
                return None;
            }
            let age_days = (now - v.created_at).num_days();
            Some((parsed, v.created_at, age_days))
        })
        .collect();

    candidates.sort_by_key(|(_, created_at, _)| std::cmp::Reverse(*created_at));
    candidates.into_iter().map(|(v, _, age)| (v, age)).collect()
}

enum WalkResult {
    Suggest(Version, i64),
    Blocked {
        newest_compliant: Version,
        blocker: Constraint,
    },
    NoCompliantVersion,
}

/// Walks `candidates` (already filtered and sorted newest first) looking for
/// the first one every constraint accepts. When none does, reports the
/// newest candidate and the first constraint it fails.
fn walk(candidates: Vec<(Version, i64)>, constraints: Vec<Constraint>) -> WalkResult {
    let Some((newest, _)) = candidates.first() else {
        return WalkResult::NoCompliantVersion;
    };
    let newest = newest.clone();

    for (version, age_days) in &candidates {
        if constraints.iter().all(|c| c.req.matches(version)) {
            return WalkResult::Suggest(version.clone(), *age_days);
        }
    }

    let blocker = constraints
        .into_iter()
        .find(|c| !c.req.matches(&newest))
        .expect("newest candidate was rejected, so some constraint must reject it");
    WalkResult::Blocked {
        newest_compliant: newest,
        blocker,
    }
}

/// Every registry-crate dependent (from the lockfile) whose recorded
/// requirement on `name` could not be read, kept as a display name rather
/// than aborting the constraint gathering.
struct GatheredConstraints {
    constraints: Vec<Constraint>,
    unverified_dependents: Vec<String>,
}

/// Maps `(name, version)` to every lockfile package that depends on it,
/// built once per run so `gather_constraints` doesn't rescan every package
/// for every "too new" violation.
type DependentsIndex<'a> = HashMap<(&'a str, &'a str), Vec<&'a Package>>;

fn build_dependents_index(all_packages: &[Package]) -> DependentsIndex<'_> {
    let mut index: DependentsIndex = HashMap::new();
    for pkg in all_packages {
        for dep in &pkg.dependencies {
            index
                .entry((dep.name.as_str(), dep.version.as_str()))
                .or_default()
                .push(pkg);
        }
    }
    index
}

/// Gathers every version requirement currently placed on `name` at
/// `locked_version`: from lockfile-recorded dependents (via the crates.io
/// sparse index for registry dependents) and from the user's own manifests
/// (via `direct_requirements`, included whenever a non-registry dependent
/// records the edge).
fn gather_constraints<T: Transport>(
    client: &mut CratesIoClient<T>,
    dependents_index: &DependentsIndex,
    direct_requirements: &[DirectRequirement],
    working_dir: &Path,
    name: &str,
    locked_version: &str,
) -> GatheredConstraints {
    let mut constraints = Vec::new();
    let mut unverified_dependents = Vec::new();

    let dependents = dependents_index
        .get(&(name, locked_version))
        .into_iter()
        .flatten()
        .copied();

    for dependent in dependents {
        if dependent.is_registry {
            match client.fetch_index_record(&dependent.name) {
                Ok(records) => match records.iter().find(|r| r.vers == dependent.version) {
                    Some(record) => {
                        for dep in &record.deps {
                            if dep.kind.as_deref() == Some("dev") {
                                continue;
                            }
                            let real_name = dep.package.as_deref().unwrap_or(&dep.name);
                            if real_name != name {
                                continue;
                            }
                            match VersionReq::parse(&dep.req) {
                                Ok(req) => constraints.push(Constraint {
                                    blocker_name: dependent.name.clone(),
                                    blocker_version: Some(dependent.version.clone()),
                                    req,
                                }),
                                Err(_) => unverified_dependents.push(dependent.name.clone()),
                            }
                        }
                    }
                    None => unverified_dependents.push(dependent.name.clone()),
                },
                Err(_) => unverified_dependents.push(dependent.name.clone()),
            }
        } else {
            // Scoped to this dependent's own manifest, not every manifest in
            // the workspace that happens to mention the same crate name —
            // two members can lock the same crate name at different major
            // versions, each with its own unrelated requirement.
            for req in direct_requirements
                .iter()
                .filter(|r| r.crate_name == name && r.declaring_package == dependent.name)
            {
                constraints.push(Constraint {
                    blocker_name: manifest_label(&req.manifest, working_dir),
                    blocker_version: None,
                    req: req.req.clone(),
                });
            }
        }
    }

    GatheredConstraints {
        constraints,
        unverified_dependents,
    }
}

fn manifest_label(path: &Path, working_dir: &Path) -> String {
    path.strip_prefix(working_dir)
        .unwrap_or(path)
        .display()
        .to_string()
}

/// Generates one outcome for every "too new" violation. Returns `None` when
/// there are no "too new" violations, so the caller prints nothing; returns
/// `Some` (possibly empty) once the flow has run.
///
/// A package whose own version list fails to fetch is simply absent from
/// the result, matching the tool's established tolerance for per-package
/// fetch failures.
#[allow(clippy::too_many_arguments)]
pub fn generate_suggestions<T: Transport>(
    client: &mut CratesIoClient<T>,
    violations: &[Violation],
    all_packages: &[Package],
    direct_requirements: &[DirectRequirement],
    working_dir: &Path,
    min_age_days: u64,
    allow_prerelease: bool,
    now: DateTime<Utc>,
) -> Option<Vec<Outcome>> {
    let too_new: Vec<&Violation> = violations
        .iter()
        .filter(|v| matches!(v.kind, ViolationKind::TooNew(_)))
        .collect();

    if too_new.is_empty() {
        return None;
    }

    let dependents_index = build_dependents_index(all_packages);

    let mut outcomes = Vec::new();
    eprintln!("\nFetching version suggestions...");
    for (i, violation) in too_new.iter().enumerate() {
        eprintln!("  [{}/{}] {}", i + 1, too_new.len(), violation.package);

        let Ok(locked) = Version::parse(&violation.version) else {
            continue;
        };

        let versions = match client.fetch_all_versions(&violation.package) {
            Ok(versions) => versions,
            Err(e) => {
                eprintln!(
                    "\n  Warning: failed to fetch versions for {}: {e}",
                    violation.package
                );
                continue;
            }
        };

        let gathered = gather_constraints(
            client,
            &dependents_index,
            direct_requirements,
            working_dir,
            &violation.package,
            &violation.version,
        );
        let candidates = filter_candidates(&versions, &locked, min_age_days, now, allow_prerelease);

        let outcome = match walk(candidates, gathered.constraints) {
            WalkResult::Suggest(version, age_days) => Outcome::Suggest {
                package: violation.package.clone(),
                locked_version: violation.version.clone(),
                suggested_version: version.to_string(),
                suggested_age_days: age_days,
                unverified_dependents: gathered.unverified_dependents,
            },
            WalkResult::Blocked {
                newest_compliant,
                blocker,
            } => Outcome::Blocked {
                package: violation.package.clone(),
                locked_version: violation.version.clone(),
                newest_compliant: newest_compliant.to_string(),
                blocker: Blocker {
                    // Whether `blocker_name` actually resolved to a
                    // suggestion is only known once every violation has been
                    // walked, so this starts false and is patched below.
                    also_suggested: false,
                    name: blocker.blocker_name,
                    version: blocker.blocker_version,
                    req: blocker.req.to_string(),
                },
            },
            WalkResult::NoCompliantVersion => Outcome::NoCompliantVersion {
                package: violation.package.clone(),
                locked_version: violation.version.clone(),
            },
        };
        outcomes.push(outcome);
    }

    let suggested_names: HashSet<String> = outcomes
        .iter()
        .filter_map(|o| match o {
            Outcome::Suggest { package, .. } => Some(package.clone()),
            _ => None,
        })
        .collect();
    for outcome in &mut outcomes {
        if let Outcome::Blocked { blocker, .. } = outcome
            && blocker.version.is_some()
        {
            blocker.also_suggested = suggested_names.contains(blocker.name.as_str());
        }
    }

    Some(outcomes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap()
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

    mod filter_candidates_tests {
        use super::*;

        #[test]
        fn excludes_too_new_yanked_and_out_of_range() {
            let versions = vec![
                make_version("1.0.0", 100, false),
                make_version("1.1.0", 50, true),   // yanked
                make_version("1.2.0", 40, false),  // compliant, same major as locked
                make_version("2.0.0", 200, false), // different major: out of range
                make_version("1.3.0", 5, false),   // too new (min age 30)
            ];

            let result = filter_candidates(&versions, &v("1.0.0"), 30, now(), false);
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

            let result = filter_candidates(&versions, &v("1.0.0"), 30, now(), false);
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
            let versions = vec![make_version("1.0.0-beta.2", 100, false)];
            let result = filter_candidates(&versions, &v("1.0.0-beta.1"), 30, now(), true);
            assert_eq!(result.len(), 1);
            assert_eq!(result[0].0.to_string(), "1.0.0-beta.2");
        }

        #[test]
        fn prerelease_allowed_when_locked_is_itself_a_prerelease() {
            let versions = vec![make_version("1.0.0-beta.2", 100, false)];
            let result = filter_candidates(&versions, &v("1.0.0-beta.1"), 30, now(), false);
            assert_eq!(result.len(), 1);
        }
    }

    mod walk_tests {
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
    }

    mod generate_suggestions_tests {
        use super::*;
        use crate::api::RetryPolicy;
        use crate::api::test_support::{FakeTransport, ScriptedResponse, index_url, versions_url};
        use crate::lockfile::PackageRef;
        use crate::report::Aged;
        use std::num::NonZeroU32;
        use std::path::PathBuf;
        use std::time::Duration;

        trait FakeTransportExt {
            fn ok(&self, name: &str, versions_json: &str);
            fn error(&self, name: &str);
            fn index_ok(&self, name: &str, records_ndjson: &str);
            fn index_error(&self, name: &str);
        }

        impl FakeTransportExt for FakeTransport {
            fn ok(&self, name: &str, versions_json: &str) {
                self.push(
                    &versions_url(name),
                    ScriptedResponse::Http(200, versions_json.to_string()),
                );
            }

            fn error(&self, name: &str) {
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

        /// Builds a client with retry/pacing delays zeroed out, so the test
        /// suite doesn't sleep.
        fn fast_client(transport: FakeTransport) -> CratesIoClient<FakeTransport> {
            CratesIoClient::with_transport(
                transport,
                None,
                24,
                RetryPolicy {
                    retry_count: NonZeroU32::new(1).unwrap(),
                    retry_delay: Duration::from_millis(0),
                    pacing_delay: Duration::from_millis(0),
                },
            )
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

        fn too_old(package: &str) -> Violation {
            Violation {
                package: package.to_string(),
                version: "1.0.0".to_string(),
                kind: ViolationKind::TooOld(Aged {
                    published: now(),
                    age_days: 1000,
                }),
            }
        }

        fn pkg(name: &str, version: &str, deps: &[(&str, &str)]) -> Package {
            Package {
                name: name.to_string(),
                version: version.to_string(),
                is_registry: true,
                dependencies: deps
                    .iter()
                    .map(|(n, v)| PackageRef {
                        name: n.to_string(),
                        version: v.to_string(),
                    })
                    .collect(),
            }
        }

        fn non_registry_pkg(name: &str, version: &str, deps: &[(&str, &str)]) -> Package {
            Package {
                is_registry: false,
                ..pkg(name, version, deps)
            }
        }

        #[test]
        fn only_too_new_violations_are_fetched() {
            let transport = FakeTransport::default();
            transport.ok("serde", &versions_body(&[("1.0.0", 50, false)], now()));
            // "syn" has no scripted response: if it were fetched, the
            // transport would panic.
            let mut client = fast_client(transport);

            let violations = vec![too_new("serde", "1.0.0"), too_old("syn")];
            let packages = vec![pkg("serde", "1.0.0", &[])];
            let outcomes = generate_suggestions(
                &mut client,
                &violations,
                &packages,
                &[],
                Path::new("/work"),
                30,
                false,
                now(),
            )
            .unwrap();

            assert_eq!(outcomes.len(), 1);
        }

        #[test]
        fn a_failed_fetch_does_not_abort_the_others() {
            let transport = FakeTransport::default();
            transport.error("serde");
            transport.ok("syn", &versions_body(&[("1.0.0", 50, false)], now()));
            let mut client = fast_client(transport);

            let violations = vec![too_new("serde", "1.0.0"), too_new("syn", "1.0.0")];
            let packages = vec![pkg("serde", "1.0.0", &[]), pkg("syn", "1.0.0", &[])];
            let outcomes = generate_suggestions(
                &mut client,
                &violations,
                &packages,
                &[],
                Path::new("/work"),
                30,
                false,
                now(),
            )
            .unwrap();

            assert_eq!(outcomes.len(), 1);
            assert!(matches!(&outcomes[0], Outcome::Suggest { package, .. } if package == "syn"));
        }

        #[test]
        fn no_compliant_version_reports_the_no_candidate_outcome() {
            let transport = FakeTransport::default();
            transport.ok("serde", &versions_body(&[("1.0.0", 5, false)], now()));
            let mut client = fast_client(transport);

            let violations = vec![too_new("serde", "1.0.0")];
            let packages = vec![pkg("serde", "1.0.0", &[])];
            let outcomes = generate_suggestions(
                &mut client,
                &violations,
                &packages,
                &[],
                Path::new("/work"),
                30,
                false,
                now(),
            )
            .unwrap();

            assert!(matches!(outcomes[0], Outcome::NoCompliantVersion { .. }));
        }

        #[test]
        fn no_too_new_violations_yields_none() {
            let transport = FakeTransport::default();
            let mut client = fast_client(transport);

            let violations = vec![too_old("syn")];
            let outcomes = generate_suggestions(
                &mut client,
                &violations,
                &[],
                &[],
                Path::new("/work"),
                30,
                false,
                now(),
            );

            assert!(outcomes.is_none());
        }

        #[test]
        fn transitive_dependent_requirement_blocks_the_newest_candidate() {
            // "app" depends on serde 1.5.0; serde's index says app requires ^1.5.
            let transport = FakeTransport::default();
            transport.ok(
                "serde",
                &versions_body(&[("1.5.0", 5, false), ("1.4.0", 50, false)], now()),
            );
            transport.index_ok(
                "app",
                r#"{"vers":"1.0.0","deps":[{"name":"serde","req":"^1.5"}]}"#,
            );
            let mut client = fast_client(transport);

            let violations = vec![too_new("serde", "1.5.0")];
            let packages = vec![
                pkg("serde", "1.5.0", &[]),
                pkg("app", "1.0.0", &[("serde", "1.5.0")]),
            ];
            let outcomes = generate_suggestions(
                &mut client,
                &violations,
                &packages,
                &[],
                Path::new("/work"),
                30,
                false,
                now(),
            )
            .unwrap();

            match &outcomes[0] {
                Outcome::Blocked {
                    newest_compliant,
                    blocker,
                    ..
                } => {
                    assert_eq!(newest_compliant, "1.4.0");
                    assert_eq!(blocker.name, "app");
                    assert_eq!(blocker.req, "^1.5");
                }
                _ => panic!("expected Blocked"),
            }
        }

        #[test]
        fn dev_kind_edge_from_a_transitive_dependent_is_ignored() {
            let transport = FakeTransport::default();
            transport.ok("serde", &versions_body(&[("1.4.0", 50, false)], now()));
            transport.index_ok(
                "app",
                r#"{"vers":"1.0.0","deps":[{"name":"serde","req":"^1.5","kind":"dev"}]}"#,
            );
            let mut client = fast_client(transport);

            let violations = vec![too_new("serde", "1.4.0")];
            let packages = vec![
                pkg("serde", "1.4.0", &[]),
                pkg("app", "1.0.0", &[("serde", "1.4.0")]),
            ];
            let outcomes = generate_suggestions(
                &mut client,
                &violations,
                &packages,
                &[],
                Path::new("/work"),
                30,
                false,
                now(),
            )
            .unwrap();

            assert!(matches!(outcomes[0], Outcome::Suggest { .. }));
        }

        #[test]
        fn renamed_dependency_is_matched_by_its_real_name() {
            let transport = FakeTransport::default();
            transport.ok("serde", &versions_body(&[("1.4.0", 50, false)], now()));
            transport.index_ok(
                "app",
                r#"{"vers":"1.0.0","deps":[{"name":"my_serde","package":"serde","req":"^1.5"}]}"#,
            );
            let mut client = fast_client(transport);

            let violations = vec![too_new("serde", "1.5.0")];
            let packages = vec![
                pkg("serde", "1.5.0", &[]),
                pkg("app", "1.0.0", &[("serde", "1.5.0")]),
            ];
            let outcomes = generate_suggestions(
                &mut client,
                &violations,
                &packages,
                &[],
                Path::new("/work"),
                30,
                false,
                now(),
            )
            .unwrap();

            assert!(matches!(outcomes[0], Outcome::Blocked { .. }));
        }

        #[test]
        fn failed_index_fetch_yields_suggestion_with_unverified_annotation() {
            let transport = FakeTransport::default();
            transport.ok("serde", &versions_body(&[("1.4.0", 50, false)], now()));
            transport.index_error("app");
            let mut client = fast_client(transport);

            let violations = vec![too_new("serde", "1.4.0")];
            let packages = vec![
                pkg("serde", "1.4.0", &[]),
                pkg("app", "1.0.0", &[("serde", "1.4.0")]),
            ];
            let outcomes = generate_suggestions(
                &mut client,
                &violations,
                &packages,
                &[],
                Path::new("/work"),
                30,
                false,
                now(),
            )
            .unwrap();

            match &outcomes[0] {
                Outcome::Suggest {
                    unverified_dependents,
                    ..
                } => assert_eq!(unverified_dependents, &["app".to_string()]),
                _ => panic!("expected Suggest"),
            }
        }

        #[test]
        fn a_package_locked_at_two_versions_produces_two_outcomes() {
            let transport = FakeTransport::default();
            transport.ok(
                "serde",
                &versions_body(&[("1.0.0", 50, false), ("2.0.0", 50, false)], now()),
            );
            let mut client = fast_client(transport);

            let violations = vec![too_new("serde", "1.0.0"), too_new("serde", "2.0.0")];
            let packages = vec![pkg("serde", "1.0.0", &[]), pkg("serde", "2.0.0", &[])];
            let outcomes = generate_suggestions(
                &mut client,
                &violations,
                &packages,
                &[],
                Path::new("/work"),
                30,
                false,
                now(),
            )
            .unwrap();

            assert_eq!(outcomes.len(), 2);
        }

        #[test]
        fn also_suggested_is_false_when_the_blocker_itself_has_no_suggestion() {
            // "y" blocks "target", and "y" is itself in the too-new set —
            // but y's own walk resolves to NoCompliantVersion, not Suggest,
            // so the blocked message must not claim a fix for y exists.
            let transport = FakeTransport::default();
            transport.ok(
                "target",
                &versions_body(&[("1.5.0", 5, false), ("1.4.0", 50, false)], now()),
            );
            transport.ok("y", &versions_body(&[("1.5.0", 5, false)], now()));
            transport.index_ok(
                "y",
                r#"{"vers":"1.5.0","deps":[{"name":"target","req":"^1.5"}]}"#,
            );
            let mut client = fast_client(transport);

            let violations = vec![too_new("target", "1.5.0"), too_new("y", "1.5.0")];
            let packages = vec![
                pkg("target", "1.5.0", &[]),
                pkg("y", "1.5.0", &[("target", "1.5.0")]),
            ];
            let outcomes = generate_suggestions(
                &mut client,
                &violations,
                &packages,
                &[],
                Path::new("/work"),
                30,
                false,
                now(),
            )
            .unwrap();

            match &outcomes[0] {
                Outcome::Blocked { blocker, .. } => {
                    assert_eq!(blocker.name, "y");
                    assert!(
                        !blocker.also_suggested,
                        "y has no Suggest outcome of its own"
                    );
                }
                _ => panic!("expected target to be Blocked"),
            }
            assert!(matches!(
                &outcomes[1],
                Outcome::NoCompliantVersion { package, .. } if package == "y"
            ));
        }

        #[test]
        fn manifest_constraint_is_scoped_to_the_declaring_dependent() {
            // member_a locks clap@2.5.0 and requires ^2; member_b locks a
            // different clap version and requires ^3. member_b's unrelated
            // requirement must not leak into member_a's constraint set.
            let transport = FakeTransport::default();
            transport.ok(
                "clap",
                &versions_body(&[("2.5.0", 5, false), ("2.0.0", 50, false)], now()),
            );
            let mut client = fast_client(transport);

            let direct_requirements = vec![
                crate::manifest::DirectRequirement {
                    manifest: PathBuf::from("/work/member_a/Cargo.toml"),
                    declaring_package: "member_a".to_string(),
                    crate_name: "clap".to_string(),
                    req: VersionReq::parse("^2").unwrap(),
                },
                crate::manifest::DirectRequirement {
                    manifest: PathBuf::from("/work/member_b/Cargo.toml"),
                    declaring_package: "member_b".to_string(),
                    crate_name: "clap".to_string(),
                    req: VersionReq::parse("^3").unwrap(),
                },
            ];
            let packages = vec![
                pkg("clap", "2.5.0", &[]),
                non_registry_pkg("member_a", "0.1.0", &[("clap", "2.5.0")]),
                non_registry_pkg("member_b", "0.1.0", &[("clap", "3.1.0")]),
            ];

            let violations = vec![too_new("clap", "2.5.0")];
            let outcomes = generate_suggestions(
                &mut client,
                &violations,
                &packages,
                &direct_requirements,
                Path::new("/work"),
                30,
                false,
                now(),
            )
            .unwrap();

            match &outcomes[0] {
                Outcome::Suggest {
                    suggested_version, ..
                } => assert_eq!(suggested_version, "2.0.0"),
                _ => panic!("expected clap to be Suggest: member_b's ^3 must not apply"),
            }
        }
    }

    /// Drives the whole pipeline — lockfile intake, manifest reading, and
    /// `generate_suggestions` — over the committed fixture at
    /// `tests/fixtures/suggest_fix_e2e/`, a two-member workspace-ish layout
    /// (a real workspace with one member) plus a hand-written lockfile.
    /// Covers all four outcome kinds at once: a suggestion, a package
    /// blocked by a manifest requirement, one blocked by a transitive
    /// dependent, and one with nothing old enough in range.
    mod end_to_end_tests {
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

            let outcomes = generate_suggestions(
                &mut client,
                &violations,
                &packages,
                &direct_requirements,
                &dir,
                30,
                false,
                now(),
            )
            .unwrap();

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
    }
}

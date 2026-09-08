use crate::api::{CrateVersionInfo, CratesIoClient, Transport};
use crate::lockfile::{Package, PackageRef};
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
    /// Set when this blocker's own `name` and `version` is itself a package
    /// this run suggests downgrading. That suggestion may unblock this one,
    /// though nothing here checks whether the older version relaxes its
    /// requirement.
    pub also_suggested: bool,
}

/// The single outcome of checking one "too new" violation: a working
/// suggestion, a package nothing could unblock, or one with no candidate
/// old enough in range at all.
pub enum Outcome {
    Suggest {
        package: String,
        /// The pkgid to print in the update command: the bare package name,
        /// or `{source}#{package}` when another package in the lockfile
        /// shares this name and locked version, so the abbreviated spec
        /// would be ambiguous to Cargo.
        package_spec: String,
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
/// `now`, strictly older in semantic precedence than `locked`, within the
/// caret-compatible zone of `locked`, sorted newest first by publish date.
/// Prereleases are excluded unless `allow_prerelease` is set or `locked` is
/// itself a prerelease.
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
            // `Version`'s `Ord` compares precedence per the semver spec:
            // build metadata never affects it, so this also rejects a
            // version differing from `locked` only in build metadata, and
            // prereleases order below the release they precede.
            if parsed >= *locked {
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

/// Maps `(name, version, source)` to every lockfile package that depends on
/// it, built once per run so `gather_constraints` doesn't rescan every
/// package for every "too new" violation. `source` distinguishes same-name,
/// same-version packages from different origins (crates.io, an alternate
/// registry, git) so a dependent of one doesn't leak into another's
/// constraint set.
type DependentsIndex<'a> = HashMap<(&'a str, &'a str, Option<&'a str>), Vec<&'a Package>>;

fn build_dependents_index(all_packages: &[Package]) -> DependentsIndex<'_> {
    let mut index: DependentsIndex = HashMap::new();
    for pkg in all_packages {
        for dep in &pkg.dependencies {
            for source in resolve_dependency_sources(dep, all_packages) {
                index
                    .entry((dep.name.as_str(), dep.version.as_str(), source))
                    .or_default()
                    .push(pkg);
            }
        }
    }
    index
}

/// Maps `(name, version)` to every lockfile package sharing them, built once
/// per run so the target-source lookup and the ambiguity check in
/// `generate_suggestions` don't each rescan every package for every "too
/// new" violation.
type NameVersionIndex<'a> = HashMap<(&'a str, &'a str), Vec<&'a Package>>;

fn build_name_version_index(all_packages: &[Package]) -> NameVersionIndex<'_> {
    let mut index: NameVersionIndex = HashMap::new();
    for pkg in all_packages {
        index
            .entry((pkg.name.as_str(), pkg.version.as_str()))
            .or_default()
            .push(pkg);
    }
    index
}

/// The source(s) a dependency edge could refer to. An edge that already
/// carries a source names it exactly. An edge without one refers either to
/// a path package sharing the name and version — cargo only omits the
/// source when the resolved target genuinely has none, so a path package is
/// the definite target even when a same-name/same-version registry package
/// also exists — or, absent any such path package, to whichever single
/// package the edge names, resolved here against the lockfile's own package
/// list. If more than one still shares the name and version (and the edge
/// still has no source to disambiguate with), the edge is kept under every
/// one of them rather than guessed at, so an edge that's genuinely ambiguous
/// still counts as a dependent everywhere it might apply.
fn resolve_dependency_sources<'a>(
    dep: &'a PackageRef,
    all_packages: &'a [Package],
) -> Vec<Option<&'a str>> {
    if let Some(source) = dep.source.as_deref() {
        return vec![Some(source)];
    }
    let has_path_match = all_packages
        .iter()
        .any(|p| p.name == dep.name && p.version == dep.version && p.source.is_none());
    if has_path_match {
        return vec![None];
    }
    let matches: Vec<Option<&str>> = all_packages
        .iter()
        .filter(|p| p.name == dep.name && p.version == dep.version)
        .map(|p| p.source.as_deref())
        .collect();
    if matches.is_empty() {
        vec![None]
    } else {
        matches
    }
}

/// Gathers every version requirement currently placed on `name` at
/// `locked_version` from `source`: from lockfile-recorded dependents (via
/// the crates.io sparse index for registry dependents) and from the user's
/// own manifests (via `direct_requirements`, included whenever a
/// non-registry dependent records the edge).
fn gather_constraints<T: Transport>(
    client: &mut CratesIoClient<T>,
    dependents_index: &DependentsIndex,
    direct_requirements: &[DirectRequirement],
    working_dir: &Path,
    name: &str,
    locked_version: &str,
    source: Option<&str>,
) -> GatheredConstraints {
    let mut constraints = Vec::new();
    let mut unverified_dependents = Vec::new();

    let dependents = dependents_index
        .get(&(name, locked_version, source))
        .into_iter()
        .flatten()
        .copied();

    for dependent in dependents {
        // A dependent counts as verified only if every requirement it
        // records on `name` parsed and at least one matches the locked
        // version. A fetch failure, a missing index record, or an
        // unparseable requirement is annotated instead of silently dropped.
        let matched;
        let unreadable;
        let mut has_unverified_leftover = false;
        if dependent.is_registry {
            let result = registry_dependent_constraints(client, dependent, name, locked_version);
            matched = result.matched;
            unreadable = result.unreadable;
            has_unverified_leftover = result.has_unverified_leftover;
            constraints.extend(result.constraints);
        } else {
            let mut manifest_matched = false;
            // Scoped to this dependent's own manifest, not every manifest in
            // the workspace that happens to mention the same crate name —
            // two members can lock the same crate name at different major
            // versions, each with its own unrelated requirement.
            for req in direct_requirements
                .iter()
                .filter(|r| r.crate_name == name && r.declaring_package == dependent.name)
                .filter(|r| {
                    Version::parse(locked_version).is_ok_and(|version| r.req.matches(&version))
                })
            {
                manifest_matched = true;
                constraints.push(Constraint {
                    blocker_name: manifest_label(&req.manifest, working_dir),
                    blocker_version: None,
                    req: req.req.clone(),
                });
            }
            matched = manifest_matched;
            unreadable = false;
        }
        if !matched || unreadable || has_unverified_leftover {
            unverified_dependents.push(dependent.name.clone());
        }
    }

    GatheredConstraints {
        constraints,
        unverified_dependents,
    }
}

/// Requirements a registry dependent's crates.io index record places on
/// `name` at `locked_version`. `unreadable` is set on a fetch failure, a
/// missing index record, or an unparseable requirement. `has_unverified_leftover`
/// is set when a matching declaration exists whose applicability couldn't be
/// settled even though other, mandatory declarations were enforced.
struct RegistryConstraints {
    constraints: Vec<Constraint>,
    matched: bool,
    unreadable: bool,
    has_unverified_leftover: bool,
}

impl RegistryConstraints {
    /// The index record for the dependent (or the matching version within
    /// it) couldn't be read at all, so nothing can be enforced.
    fn unreadable() -> Self {
        RegistryConstraints {
            constraints: Vec::new(),
            matched: false,
            unreadable: true,
            has_unverified_leftover: false,
        }
    }
}

fn registry_dependent_constraints<T: Transport>(
    client: &mut CratesIoClient<T>,
    dependent: &Package,
    name: &str,
    locked_version: &str,
) -> RegistryConstraints {
    let mut matched = false;

    let Ok(records) = client.fetch_index_record(&dependent.name) else {
        return RegistryConstraints::unreadable();
    };
    let Some(record) = records.iter().find(|r| r.vers == dependent.version) else {
        return RegistryConstraints::unreadable();
    };

    let mut unreadable = false;
    // Every matching declaration, alongside whether it alone guarantees the
    // requirement is active: unconditional (no `target`) and non-optional.
    let mut matching_reqs: Vec<(VersionReq, bool)> = Vec::new();
    for dep in &record.deps {
        if dep.kind.as_deref() == Some("dev") {
            continue;
        }
        let real_name = dep.package.as_deref().unwrap_or(&dep.name);
        if real_name != name {
            continue;
        }
        match VersionReq::parse(&dep.req) {
            Ok(req) if Version::parse(locked_version).is_ok_and(|v| req.matches(&v)) => {
                let mandatory = dep.target.is_none() && dep.optional != Some(true);
                matching_reqs.push((req, mandatory));
            }
            Ok(_) => {}
            Err(_) => unreadable = true,
        }
    }

    // Duplicate declarations of the *same* requirement aren't ambiguous —
    // the index lists one entry per target, so a requirement repeated
    // across, say, `cfg(unix)` and `cfg(windows)` tables is still a single
    // requirement, not competing candidates. (Not necessarily adjacent, so a
    // plain `dedup()` wouldn't catch every repeat.) A requirement is
    // mandatory if any declaration producing it is unconditional and
    // non-optional — such a declaration guarantees the requirement is
    // active no matter what else matches.
    let mut distinct: Vec<(VersionReq, bool)> = Vec::new();
    for (req, mandatory) in matching_reqs {
        match distinct.iter_mut().find(|(r, _)| *r == req) {
            Some((_, m)) => *m = *m || mandatory,
            None => distinct.push((req, mandatory)),
        }
    }
    let (mandatory_reqs, uncertain_reqs): (Vec<_>, Vec<_>) =
        distinct.into_iter().partition(|(_, mandatory)| *mandatory);

    let mut constraints = Vec::new();
    let mut has_unverified_leftover = false;

    if !mandatory_reqs.is_empty() {
        // Every known-mandatory requirement is always active, so all of
        // them are enforced regardless of how many other, uncertain
        // declarations also match.
        matched = true;
        for (req, _) in mandatory_reqs {
            constraints.push(Constraint {
                blocker_name: dependent.name.clone(),
                blocker_version: Some(dependent.version.clone()),
                req,
            });
        }
        // A leftover uncertain declaration can't be resolved either way, so
        // the dependent is still worth flagging even though the mandatory
        // requirements above are enforced as definite blockers.
        has_unverified_leftover = !uncertain_reqs.is_empty();
    } else if let [(req, _)] = uncertain_reqs.as_slice() {
        // With no mandatory declaration to settle it, a single uncertain
        // declaration is the unique explanation for this lockfile edge and
        // is enforced as a definite blocker.
        matched = true;
        constraints.push(Constraint {
            blocker_name: dependent.name.clone(),
            blocker_version: Some(dependent.version.clone()),
            req: req.clone(),
        });
    }
    // Otherwise: no matching declaration, or several distinct uncertain
    // ones with no mandatory declaration to settle it — neither can be
    // enforced, so the dependent is reported unverified instead.

    RegistryConstraints {
        constraints,
        matched,
        unreadable,
        has_unverified_leftover,
    }
}

fn manifest_label(path: &Path, working_dir: &Path) -> String {
    path.strip_prefix(working_dir)
        .unwrap_or(path)
        .display()
        .to_string()
}

/// The pkgid to print in an update command for `name` at `version`:
/// abbreviated to the bare name, unless another package shares the name and
/// version — a path or git package, say — in which case Cargo would reject
/// the abbreviated spec as ambiguous. Qualifying with `target_source` (the
/// source of the package the suggestion is actually for) disambiguates it,
/// per Cargo's package ID specification grammar: `[<kind>+]<url>#<name>@<version>`.
fn build_package_spec(name: &str, target_source: Option<&str>, is_ambiguous: bool) -> String {
    match (is_ambiguous, target_source) {
        (true, Some(source)) => format!("{source}#{name}"),
        _ => name.to_string(),
    }
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
    let name_version_index = build_name_version_index(all_packages);

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

        let same_name_version = name_version_index
            .get(&(violation.package.as_str(), violation.version.as_str()))
            .map(Vec::as_slice)
            .unwrap_or_default();

        // Violations are only ever raised for registry packages (see the
        // `is_registry` filter that builds `violations`), so the crates.io
        // entry matching this name and version is the one this violation
        // refers to — not any git or alternate-registry package that
        // happens to share the same name and version.
        let target_source = same_name_version
            .iter()
            .find(|p| p.is_registry)
            .and_then(|p| p.source.as_deref());

        let package_spec = build_package_spec(
            &violation.package,
            target_source,
            same_name_version.len() > 1,
        );

        let gathered = gather_constraints(
            client,
            &dependents_index,
            direct_requirements,
            working_dir,
            &violation.package,
            &violation.version,
            target_source,
        );
        let candidates = filter_candidates(&versions, &locked, min_age_days, now, allow_prerelease);

        let outcome = match walk(candidates, gathered.constraints) {
            WalkResult::Suggest(version, age_days) => Outcome::Suggest {
                package: violation.package.clone(),
                package_spec,
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
                    // Whether this blocker's own locked version resolved to a
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

    // Keyed by locked version as well as name. A suggestion for one locked
    // version of a package says nothing about another version of it, which
    // may have no compliant version at all.
    let suggested: HashSet<(String, String)> = outcomes
        .iter()
        .filter_map(|o| match o {
            Outcome::Suggest {
                package,
                locked_version,
                ..
            } => Some((package.clone(), locked_version.clone())),
            _ => None,
        })
        .collect();
    for outcome in &mut outcomes {
        if let Outcome::Blocked { blocker, .. } = outcome
            && let Some(version) = &blocker.version
        {
            blocker.also_suggested = suggested.contains(&(blocker.name.clone(), version.clone()));
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
                source: Some("registry+https://github.com/rust-lang/crates.io-index".to_string()),
                dependencies: deps
                    .iter()
                    .map(|(n, v)| PackageRef {
                        name: n.to_string(),
                        version: v.to_string(),
                        source: None,
                    })
                    .collect(),
            }
        }

        fn non_registry_pkg(name: &str, version: &str, deps: &[(&str, &str)]) -> Package {
            Package {
                is_registry: false,
                source: None,
                ..pkg(name, version, deps)
            }
        }

        #[test]
        fn aliased_registry_requirements_follow_the_locked_version() {
            let transport = FakeTransport::default();
            transport.index_ok("app", r#"{"vers":"1.0.0","deps":[{"name":"foo_old","package":"foo","req":"^1"},{"name":"foo_new","package":"foo","req":"^2"}]}"#);
            let mut client = fast_client(transport);
            let packages = vec![pkg("app", "1.0.0", &[("foo", "1.5.0"), ("foo", "2.5.0")])];
            let index = build_dependents_index(&packages);
            for (locked, candidate) in [("1.5.0", "1.4.0"), ("2.5.0", "2.4.0")] {
                let gathered = gather_constraints(
                    &mut client,
                    &index,
                    &[],
                    Path::new("/work"),
                    "foo",
                    locked,
                    None,
                );
                assert_eq!(gathered.constraints.len(), 1);
                assert!(gathered.unverified_dependents.is_empty());
                assert!(matches!(
                    walk(
                        vec![(Version::parse(candidate).unwrap(), 50)],
                        gathered.constraints
                    ),
                    WalkResult::Suggest(_, _)
                ));
            }
        }

        #[test]
        fn unmatched_registry_requirements_are_unverified() {
            let transport = FakeTransport::default();
            transport.index_ok(
                "app",
                r#"{"vers":"1.0.0","deps":[{"name":"foo","req":"^1.5"}]}"#,
            );
            let mut client = fast_client(transport);
            let packages = vec![pkg("app", "1.0.0", &[("foo", "1.4.0")])];
            let index = build_dependents_index(&packages);
            let gathered = gather_constraints(
                &mut client,
                &index,
                &[],
                Path::new("/work"),
                "foo",
                "1.4.0",
                None,
            );
            assert!(gathered.constraints.is_empty());
            assert_eq!(gathered.unverified_dependents, ["app"]);
        }

        #[test]
        fn missing_non_registry_requirements_are_unverified() {
            let mut client = fast_client(FakeTransport::default());
            let packages = vec![non_registry_pkg("git-app", "1.0.0", &[("foo", "1.5.0")])];
            let index = build_dependents_index(&packages);
            let gathered = gather_constraints(
                &mut client,
                &index,
                &[],
                Path::new("/work"),
                "foo",
                "1.5.0",
                None,
            );
            assert!(gathered.constraints.is_empty());
            assert_eq!(gathered.unverified_dependents, ["git-app"]);
        }

        #[test]
        fn aliased_manifest_requirements_follow_the_locked_version() {
            let mut client = fast_client(FakeTransport::default());
            let packages = vec![non_registry_pkg(
                "app",
                "1.0.0",
                &[("foo", "1.5.0"), ("foo", "2.5.0")],
            )];
            let requirements: Vec<_> = ["^1", "^2"]
                .into_iter()
                .map(|req| DirectRequirement {
                    manifest: "/work/Cargo.toml".into(),
                    declaring_package: "app".to_string(),
                    crate_name: "foo".to_string(),
                    req: VersionReq::parse(req).unwrap(),
                })
                .collect();
            let index = build_dependents_index(&packages);
            for (locked, candidate) in [("1.5.0", "1.4.0"), ("2.5.0", "2.4.0")] {
                let gathered = gather_constraints(
                    &mut client,
                    &index,
                    &requirements,
                    Path::new("/work"),
                    "foo",
                    locked,
                    None,
                );
                assert_eq!(gathered.constraints.len(), 1);
                assert!(gathered.unverified_dependents.is_empty());
                assert!(matches!(
                    walk(
                        vec![(Version::parse(candidate).unwrap(), 50)],
                        gathered.constraints
                    ),
                    WalkResult::Suggest(_, _)
                ));
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
            transport.ok(
                "syn",
                &versions_body(&[("1.0.0", 50, false), ("1.1.0", 5, false)], now()),
            );
            let mut client = fast_client(transport);

            let violations = vec![too_new("serde", "1.0.0"), too_new("syn", "1.1.0")];
            let packages = vec![pkg("serde", "1.0.0", &[]), pkg("syn", "1.1.0", &[])];
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
        fn same_name_version_collision_across_sources_does_not_leak_dependents() {
            // Two packages both named "serde" locked at 1.5.0: one from
            // crates.io, one from git. "consumer" depends on the git one
            // specifically. The crates.io serde must not inherit consumer's
            // requirement just because the name and version happen to match.
            let transport = FakeTransport::default();
            transport.ok(
                "serde",
                &versions_body(&[("1.5.0", 5, false), ("1.4.0", 50, false)], now()),
            );
            // If "consumer" were (wrongly) treated as a dependent of the
            // crates.io serde, this scripted index response would be
            // fetched and its ^1.5 requirement would block the downgrade.
            transport.index_ok(
                "consumer",
                r#"{"vers":"1.0.0","deps":[{"name":"serde","req":"^1.5"}]}"#,
            );
            let mut client = fast_client(transport);

            let registry_source = "registry+https://github.com/rust-lang/crates.io-index";
            let git_source =
                "git+https://github.com/example/serde#0000000000000000000000000000000000000000";

            let packages = vec![
                Package {
                    name: "serde".to_string(),
                    version: "1.5.0".to_string(),
                    is_registry: true,
                    source: Some(registry_source.to_string()),
                    dependencies: vec![],
                },
                Package {
                    name: "serde".to_string(),
                    version: "1.5.0".to_string(),
                    is_registry: false,
                    source: Some(git_source.to_string()),
                    dependencies: vec![],
                },
                Package {
                    name: "consumer".to_string(),
                    version: "1.0.0".to_string(),
                    is_registry: true,
                    source: Some(registry_source.to_string()),
                    dependencies: vec![PackageRef {
                        name: "serde".to_string(),
                        version: "1.5.0".to_string(),
                        source: Some(git_source.to_string()),
                    }],
                },
            ];

            let violations = vec![too_new("serde", "1.5.0")];
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

            assert!(
                matches!(&outcomes[0], Outcome::Suggest { suggested_version, .. } if suggested_version == "1.4.0"),
                "the crates.io serde must not be blocked by consumer's requirement on the git serde: {:?}",
                match &outcomes[0] {
                    Outcome::Blocked { blocker, .. } => format!("Blocked by {}", blocker.name),
                    _ => "other".to_string(),
                }
            );
        }

        #[test]
        fn source_collision_yields_a_source_qualified_package_spec() {
            // Same fixture as above: a crates.io "serde" and a git "serde"
            // both locked at 1.5.0. Cargo would reject the abbreviated
            // `serde@1.5.0` spec as ambiguous, so the suggestion for the
            // registry package must qualify it with the registry source.
            let transport = FakeTransport::default();
            transport.ok(
                "serde",
                &versions_body(&[("1.5.0", 5, false), ("1.4.0", 50, false)], now()),
            );
            let mut client = fast_client(transport);

            let registry_source = "registry+https://github.com/rust-lang/crates.io-index";
            let git_source =
                "git+https://github.com/example/serde#0000000000000000000000000000000000000000";

            let packages = vec![
                Package {
                    name: "serde".to_string(),
                    version: "1.5.0".to_string(),
                    is_registry: true,
                    source: Some(registry_source.to_string()),
                    dependencies: vec![],
                },
                Package {
                    name: "serde".to_string(),
                    version: "1.5.0".to_string(),
                    is_registry: false,
                    source: Some(git_source.to_string()),
                    dependencies: vec![],
                },
            ];

            let violations = vec![too_new("serde", "1.5.0")];
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
                Outcome::Suggest { package_spec, .. } => {
                    assert_eq!(package_spec, &format!("{registry_source}#serde"));
                }
                _ => panic!("expected serde to be Suggest"),
            }
        }

        #[test]
        fn no_collision_keeps_the_abbreviated_package_spec() {
            let transport = FakeTransport::default();
            transport.ok(
                "serde",
                &versions_body(&[("1.5.0", 5, false), ("1.4.0", 50, false)], now()),
            );
            let mut client = fast_client(transport);

            let violations = vec![too_new("serde", "1.5.0")];
            let packages = vec![pkg("serde", "1.5.0", &[])];
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
                Outcome::Suggest { package_spec, .. } => assert_eq!(package_spec, "serde"),
                _ => panic!("expected serde to be Suggest"),
            }
        }

        #[test]
        fn path_package_sharing_a_name_and_version_does_not_leak_dependents_to_the_registry_package()
         {
            // A path package "local-crate" 0.1.0 and a crates.io package of
            // the same name and version both exist. "consumer" depends on
            // the path one via an edge that omits the source, as cargo does
            // for any edge whose true target has none. Since a path
            // package's own source is always omitted too, the crates.io
            // package must not inherit consumer's requirement just because
            // the name and version happen to collide.
            let transport = FakeTransport::default();
            transport.ok(
                "local-crate",
                &versions_body(&[("1.1.0", 5, false), ("1.0.0", 50, false)], now()),
            );
            // If "consumer" were (wrongly) treated as a dependent of the
            // crates.io local-crate, this scripted index response would be
            // fetched and its ^1.1 requirement would block the downgrade.
            transport.index_ok(
                "consumer",
                r#"{"vers":"1.0.0","deps":[{"name":"local-crate","req":"^1.1"}]}"#,
            );
            let mut client = fast_client(transport);

            let registry_source = "registry+https://github.com/rust-lang/crates.io-index";

            let packages = vec![
                Package {
                    name: "local-crate".to_string(),
                    version: "1.1.0".to_string(),
                    is_registry: true,
                    source: Some(registry_source.to_string()),
                    dependencies: vec![],
                },
                Package {
                    name: "local-crate".to_string(),
                    version: "1.1.0".to_string(),
                    is_registry: false,
                    source: None,
                    dependencies: vec![],
                },
                Package {
                    name: "consumer".to_string(),
                    version: "1.0.0".to_string(),
                    is_registry: true,
                    source: Some(registry_source.to_string()),
                    dependencies: vec![PackageRef {
                        name: "local-crate".to_string(),
                        version: "1.1.0".to_string(),
                        source: None,
                    }],
                },
            ];

            let violations = vec![too_new("local-crate", "1.1.0")];
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

            assert!(
                matches!(&outcomes[0], Outcome::Suggest { suggested_version, .. } if suggested_version == "1.0.0"),
                "the crates.io local-crate must not be blocked by consumer's requirement on the path local-crate: {:?}",
                match &outcomes[0] {
                    Outcome::Blocked { blocker, .. } => format!("Blocked by {}", blocker.name),
                    _ => "other".to_string(),
                }
            );
        }

        #[test]
        fn dev_kind_edge_from_a_transitive_dependent_is_ignored() {
            let transport = FakeTransport::default();
            transport.ok(
                "serde",
                &versions_body(&[("1.5.0", 5, false), ("1.4.0", 50, false)], now()),
            );
            transport.index_ok(
                "app",
                r#"{"vers":"1.0.0","deps":[{"name":"serde","req":"^1.5","kind":"dev"}]}"#,
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
        fn ambiguous_optional_declaration_does_not_block_the_downgrade() {
            // "app" declares both a normal `serde = "^1"` and a disabled,
            // renamed optional `serde_new = { package = "serde", version =
            // "^1.5", optional = true }`. Both match locked serde@1.5.0, but
            // since the optional one may not even be activated, neither can
            // be enforced as a definite blocker.
            let transport = FakeTransport::default();
            transport.ok(
                "serde",
                &versions_body(&[("1.5.0", 5, false), ("1.4.0", 50, false)], now()),
            );
            transport.index_ok(
                "app",
                r#"{"vers":"1.0.0","deps":[{"name":"serde","req":"^1"},{"name":"serde_new","package":"serde","req":"^1.5","optional":true}]}"#,
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
                Outcome::Suggest {
                    suggested_version,
                    unverified_dependents,
                    ..
                } => {
                    assert_eq!(suggested_version, "1.4.0");
                    assert_eq!(unverified_dependents, &["app".to_string()]);
                }
                _ => panic!("expected serde to be Suggest, with app marked unverified"),
            }
        }

        #[test]
        fn unique_optional_declaration_still_blocks() {
            // Only the optional, renamed declaration matches — no ambiguity,
            // so it's the unique explanation for the lockfile edge and must
            // still be enforced.
            let transport = FakeTransport::default();
            transport.ok(
                "serde",
                &versions_body(&[("1.5.0", 5, false), ("1.4.0", 50, false)], now()),
            );
            transport.index_ok(
                "app",
                r#"{"vers":"1.0.0","deps":[{"name":"serde_new","package":"serde","req":"^1.5","optional":true}]}"#,
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
                _ => panic!("expected serde to be Blocked by app's unique optional declaration"),
            }
        }

        #[test]
        fn mandatory_requirements_from_different_kinds_both_block() {
            // "app" declares an unconditional, nonoptional normal
            // requirement of `^1.5` and an unconditional, nonoptional build
            // requirement of `^1` on serde. Both are always active, so both
            // are enforced — the more restrictive one (`^1.5`) rejects the
            // downgrade to 1.4.0.
            let transport = FakeTransport::default();
            transport.ok(
                "serde",
                &versions_body(&[("1.5.0", 5, false), ("1.4.0", 50, false)], now()),
            );
            transport.index_ok(
                "app",
                r#"{"vers":"1.0.0","deps":[{"name":"serde","req":"^1.5"},{"name":"serde","req":"^1","kind":"build"}]}"#,
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
                _ => panic!("expected serde to be Blocked by app's mandatory requirement"),
            }
        }

        #[test]
        fn mandatory_requirement_still_blocks_alongside_uncertain_declaration() {
            // "app" declares an unconditional, nonoptional normal
            // requirement of `^1.5`, and a separate disabled, renamed
            // optional declaration matching a looser `^1`. The mandatory
            // requirement is enforced regardless of the uncertain one, even
            // though the uncertain one alone would have permitted the
            // downgrade.
            let transport = FakeTransport::default();
            transport.ok(
                "serde",
                &versions_body(&[("1.5.0", 5, false), ("1.4.0", 50, false)], now()),
            );
            transport.index_ok(
                "app",
                r#"{"vers":"1.0.0","deps":[{"name":"serde","req":"^1.5"},{"name":"serde_new","package":"serde","req":"^1","optional":true}]}"#,
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
                _ => panic!("expected serde to be Blocked by app's mandatory requirement"),
            }
        }

        #[test]
        fn identical_requirement_repeated_across_targets_still_blocks() {
            // "app" declares the same `serde = "^1.5"` requirement under two
            // target-specific tables (e.g. cfg(unix) and cfg(windows)), which
            // the index lists as two separate `deps` entries with identical
            // `req` strings. This is not the same situation as two distinct
            // declarations that might not both be active — the requirement
            // is identical either way, so it must still be enforced as a
            // definite blocker rather than merely "unverified".
            let transport = FakeTransport::default();
            transport.ok(
                "serde",
                &versions_body(&[("1.5.0", 5, false), ("1.4.0", 50, false)], now()),
            );
            transport.index_ok(
                "app",
                r#"{"vers":"1.0.0","deps":[{"name":"serde","req":"^1.5","target":"cfg(unix)"},{"name":"serde","req":"^1.5","target":"cfg(windows)"}]}"#,
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
                _ => panic!(
                    "expected serde to be Blocked by app's requirement, not merely unverified"
                ),
            }
        }

        #[test]
        fn failed_index_fetch_yields_suggestion_with_unverified_annotation() {
            let transport = FakeTransport::default();
            transport.ok(
                "serde",
                &versions_body(&[("1.5.0", 5, false), ("1.4.0", 50, false)], now()),
            );
            transport.index_error("app");
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
        fn also_suggested_is_false_when_only_another_version_of_the_blocker_is_suggested() {
            // "foo" is locked at both 1.5.0 and 2.5.0. Only 1.5.0 resolves to
            // a Suggest; the 2.5.0 that blocks "target" has no compliant
            // version, so the blocked message must not point at the unrelated
            // 1.5.0 downgrade.
            let transport = FakeTransport::default();
            transport.ok(
                "target",
                &versions_body(&[("1.5.0", 5, false), ("1.4.0", 50, false)], now()),
            );
            transport.ok(
                "foo",
                &versions_body(
                    &[
                        ("1.5.0", 5, false),
                        ("1.4.0", 50, false),
                        ("2.5.0", 5, false),
                    ],
                    now(),
                ),
            );
            transport.index_ok(
                "foo",
                r#"{"vers":"2.5.0","deps":[{"name":"target","req":"^1.5"}]}"#,
            );
            let mut client = fast_client(transport);

            let violations = vec![
                too_new("target", "1.5.0"),
                too_new("foo", "1.5.0"),
                too_new("foo", "2.5.0"),
            ];
            let packages = vec![
                pkg("target", "1.5.0", &[]),
                pkg("foo", "1.5.0", &[]),
                pkg("foo", "2.5.0", &[("target", "1.5.0")]),
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

            assert!(
                matches!(&outcomes[1], Outcome::Suggest { package, locked_version, .. }
                    if package == "foo" && locked_version == "1.5.0"),
                "foo 1.5.0 should be suggested, otherwise the test proves nothing"
            );
            match &outcomes[0] {
                Outcome::Blocked { blocker, .. } => {
                    assert_eq!(blocker.name, "foo");
                    assert_eq!(blocker.version.as_deref(), Some("2.5.0"));
                    assert!(
                        !blocker.also_suggested,
                        "the suggestion is for foo 1.5.0, which does not unblock foo 2.5.0"
                    );
                }
                _ => panic!("expected target to be Blocked"),
            }
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

    /// Builds a real Cargo project with a source collision — a crates.io
    /// package and a path package sharing a name and locked version — and
    /// runs a real `cargo` against the spec `build_package_spec` produces,
    /// to verify it's the source-qualified pkgid Cargo itself expects,
    /// rather than merely a string this crate assumes is valid.
    mod source_collision_cargo_tests {
        use super::*;
        use sha2::{Digest, Sha256};
        use std::process::Command;

        const CRATE_NAME: &str = "semver";
        const CRATE_VERSION: &str = "1.0.28";

        /// Writes a minimal crate (`Cargo.toml` + `src/lib.rs`) at `dir`.
        fn write_crate_source(dir: &Path, name: &str, version: &str) {
            std::fs::create_dir_all(dir.join("src")).unwrap();
            std::fs::write(
                dir.join("Cargo.toml"),
                format!(
                    "[package]\nname = \"{name}\"\nversion = \"{version}\"\nedition = \"2021\"\n"
                ),
            )
            .unwrap();
            std::fs::write(dir.join("src/lib.rs"), "").unwrap();
        }

        /// Packs `crate_dir` (already containing a `{name}-{version}`
        /// top-level directory) into a `.crate` tarball, Cargo's own
        /// publish format.
        fn pack_crate_tarball(crate_dir: &Path, name: &str, version: &str) -> Vec<u8> {
            let mut bytes = Vec::new();
            {
                let encoder =
                    flate2::write::GzEncoder::new(&mut bytes, flate2::Compression::default());
                let mut builder = tar::Builder::new(encoder);
                builder
                    .append_dir_all(format!("{name}-{version}"), crate_dir)
                    .unwrap();
                builder.finish().unwrap();
            }
            bytes
        }

        /// Assembles a local-registry source (see Cargo's source-replacement
        /// docs) at `registry_dir`, containing one crate. Local-registry
        /// index entries are sharded by name length: a 4+ character name
        /// shards under its first two, then next two, characters.
        fn write_local_registry(registry_dir: &Path, name: &str, version: &str) {
            let build_dir = registry_dir
                .join(".build")
                .join(format!("{name}-{version}"));
            write_crate_source(&build_dir, name, version);
            let tarball = pack_crate_tarball(&build_dir, name, version);

            std::fs::write(
                registry_dir.join(format!("{name}-{version}.crate")),
                &tarball,
            )
            .unwrap();

            let cksum = Sha256::digest(&tarball)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>();
            let shard = registry_dir
                .join("index")
                .join(&name[0..2])
                .join(&name[2..4]);
            std::fs::create_dir_all(&shard).unwrap();
            std::fs::write(
                shard.join(name),
                format!(
                    r#"{{"name":"{name}","vers":"{version}","deps":[],"cksum":"{cksum}","features":{{}},"yanked":false}}"#
                ),
            )
            .unwrap();
        }

        fn run_cargo(args: &[&str], cwd: &Path) -> std::process::Output {
            Command::new("cargo")
                .args(args)
                .current_dir(cwd)
                .output()
                .expect("failed to run cargo")
        }

        #[test]
        fn qualified_spec_resolves_where_the_abbreviated_spec_is_ambiguous() {
            let root = tempfile::tempdir().unwrap();
            let registry_dir = root.path().join("registry");
            let workspace_dir = root.path().join("workspace");

            write_local_registry(&registry_dir, CRATE_NAME, CRATE_VERSION);
            write_crate_source(
                &workspace_dir.join("vendor-semver"),
                CRATE_NAME,
                CRATE_VERSION,
            );

            std::fs::create_dir_all(workspace_dir.join(".cargo")).unwrap();
            std::fs::write(
                workspace_dir.join(".cargo/config.toml"),
                format!(
                    "[source.local-vendor]\nlocal-registry = \"{}\"\n\n[source.crates-io]\nreplace-with = \"local-vendor\"\n",
                    registry_dir.display()
                ),
            )
            .unwrap();
            std::fs::create_dir_all(workspace_dir.join("src")).unwrap();
            std::fs::write(workspace_dir.join("src/main.rs"), "fn main() {}\n").unwrap();
            std::fs::write(
                workspace_dir.join("Cargo.toml"),
                format!(
                    "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n{CRATE_NAME} = \"{CRATE_VERSION}\"\n{CRATE_NAME}-path = {{ package = \"{CRATE_NAME}\", path = \"vendor-semver\" }}\n"
                ),
            )
            .unwrap();

            let lock = run_cargo(&["generate-lockfile", "--offline"], &workspace_dir);
            assert!(
                lock.status.success(),
                "generate-lockfile failed: {}",
                String::from_utf8_lossy(&lock.stderr)
            );

            let packages = crate::lockfile::load(Path::new("Cargo.lock"), &workspace_dir).unwrap();
            let target_source = packages
                .iter()
                .find(|p| p.name == CRATE_NAME && p.is_registry)
                .and_then(|p| p.source.as_deref());
            let is_ambiguous = packages
                .iter()
                .filter(|p| p.name == CRATE_NAME && p.version == CRATE_VERSION)
                .count()
                > 1;
            let spec = build_package_spec(CRATE_NAME, target_source, is_ambiguous);
            assert!(
                spec.contains('#'),
                "expected a source-qualified spec for a name/version collision, got {spec}"
            );

            // The abbreviated spec really is ambiguous in this fixture —
            // otherwise the qualified spec above proves nothing.
            let abbreviated = run_cargo(
                &[
                    "update",
                    "--offline",
                    "-p",
                    &format!("{CRATE_NAME}@{CRATE_VERSION}"),
                    "--precise",
                    CRATE_VERSION,
                ],
                &workspace_dir,
            );
            assert!(
                !abbreviated.status.success()
                    && String::from_utf8_lossy(&abbreviated.stderr).contains("ambiguous"),
                "expected the abbreviated spec to be ambiguous in this fixture: {}",
                String::from_utf8_lossy(&abbreviated.stderr)
            );

            let qualified = run_cargo(
                &[
                    "update",
                    "--offline",
                    "-p",
                    &format!("{spec}@{CRATE_VERSION}"),
                    "--precise",
                    CRATE_VERSION,
                ],
                &workspace_dir,
            );
            assert!(
                qualified.status.success(),
                "expected the source-qualified spec to resolve without an ambiguous-specification error: {}",
                String::from_utf8_lossy(&qualified.stderr)
            );
        }
    }
}

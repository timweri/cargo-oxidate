use crate::api::{CrateVersionInfo, CratesIoClient, Transport};
use crate::lockfile::Package;
use crate::manifest::{DirectRequirement, RequirementSource};
use crate::report::{Violation, ViolationKind};
use chrono::{DateTime, Utc};
use semver::{Version, VersionReq};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// What places a requirement: a locked package or a manifest. The blocker's
/// own name is carried beside it, so the two kinds cannot be confused.
pub enum BlockerKind {
    /// A lockfile package, named by the blocker's `name`.
    Package {
        version: String,
        /// Cargo's pkgid: the bare name unless another package shares the
        /// name and version, then `{source}#{name}`.
        package_spec: String,
    },
    /// A `Cargo.toml`, named by the blocker's `name` as a path.
    Manifest,
}

/// A requirement placed by a lockfile dependent or the user's manifest.
pub struct Constraint {
    pub blocker_name: String,
    pub blocker_kind: BlockerKind,
    pub req: VersionReq,
}

/// The package or manifest and requirement blocking a downgrade.
pub struct Blocker {
    pub name: String,
    pub kind: BlockerKind,
    pub req: String,
    /// Whether this run also suggests downgrading this locked package.
    pub also_suggested: bool,
}

/// The result of checking one "too new" violation.
pub enum Outcome {
    Suggest {
        package: String,
        /// Bare name unless a same-name, same-version package makes Cargo's
        /// abbreviated pkgid ambiguous, then `{source}#{package}`.
        package_spec: String,
        locked_version: String,
        suggested_version: String,
        suggested_age_days: i64,
        /// Dependent labels: the bare name when its name and version are
        /// unique in the lockfile, else its source-qualified spec.
        unverified_dependents: Vec<String>,
    },
    Blocked {
        package: String,
        /// Same as [`Outcome::Suggest::package_spec`].
        package_spec: String,
        locked_version: String,
        newest_compliant: String,
        blocker: Blocker,
    },
    NoCompliantVersion {
        package: String,
        /// Same as [`Outcome::Suggest::package_spec`].
        package_spec: String,
        locked_version: String,
    },
    Unavailable {
        package: String,
        /// Same as [`Outcome::Suggest::package_spec`].
        package_spec: String,
        locked_version: String,
        reason: String,
    },
}

/// Sent to the progress callback before each downgrade check.
pub struct SuggestionProgress {
    pub current: usize,
    pub total: usize,
    pub package: String,
}

/// Whether `a` and `b` share Cargo's symmetric caret-compatible zone: major,
/// minor when major is zero, or patch when both are zero.
fn same_compatible_zone(a: &Version, b: &Version) -> bool {
    if a.major != 0 || b.major != 0 {
        a.major == b.major
    } else if a.minor != 0 || b.minor != 0 {
        a.minor == b.minor
    } else {
        a.patch == b.patch
    }
}

/// Age limits a suggested version must satisfy, mirroring the policy check.
#[derive(Debug, Clone, Copy)]
pub struct AgeBounds {
    pub min_days: u64,
    pub max_days: Option<u64>,
}

/// Keeps old-enough, non-yanked compatible versions older than `locked`
/// and no older than the maximum age, ordered by semver precedence
/// descending (publish date breaks ties). Excludes prereleases unless
/// allowed or `locked` is one.
fn filter_candidates(
    versions: &[CrateVersionInfo],
    locked: &Version,
    age_bounds: AgeBounds,
    now: DateTime<Utc>,
    allow_prerelease: bool,
) -> Vec<(Version, i64)> {
    let min_age_threshold = now - chrono::Duration::days(age_bounds.min_days as i64);
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
            // `Version`'s `Ord` breaks precedence ties on build metadata,
            // so a version differing from `locked` only in build metadata
            // would otherwise slip past a plain `>=` comparison despite
            // having equal semantic precedence. `cmp_precedence` follows
            // the semver spec instead: it ignores build metadata, and
            // orders prereleases below the release they precede.
            if parsed.cmp_precedence(locked) != std::cmp::Ordering::Less {
                return None;
            }
            let age_days = (now - v.created_at).num_days();
            if age_bounds.max_days.is_some_and(|max| age_days > max as i64) {
                return None;
            }
            Some((parsed, v.created_at, age_days))
        })
        .collect();

    candidates.sort_by(|(a_ver, a_created, _), (b_ver, b_created, _)| {
        b_ver
            .cmp_precedence(a_ver)
            .then_with(|| b_created.cmp(a_created))
    });
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
/// newest candidate and the constraint responsible for the block.
fn walk(candidates: Vec<(Version, i64)>, mut constraints: Vec<Constraint>) -> WalkResult {
    let Some((newest, _)) = candidates.first() else {
        return WalkResult::NoCompliantVersion;
    };
    let newest = newest.clone();

    for (version, age_days) in &candidates {
        if constraints.iter().all(|c| c.req.matches(version)) {
            return WalkResult::Suggest(version.clone(), *age_days);
        }
    }

    // Prefer a constraint that rejects every candidate: that one alone makes
    // the downgrade impossible. Falling back to the first constraint the
    // newest candidate fails only misattributes when no single constraint
    // blocks everything — there the block is a genuine combination, and this
    // still explains why the newest candidate was rejected.
    let blocker_idx = constraints
        .iter()
        .position(|c| candidates.iter().all(|(v, _)| !c.req.matches(v)))
        .or_else(|| constraints.iter().position(|c| !c.req.matches(&newest)))
        .expect("newest candidate was rejected, so some constraint must reject it");
    let blocker = constraints.swap_remove(blocker_idx);
    WalkResult::Blocked {
        newest_compliant: newest,
        blocker,
    }
}

/// Constraints plus dependent labels whose requirements remain unverified.
struct GatheredConstraints {
    constraints: Vec<Constraint>,
    unverified_dependents: Vec<String>,
}

/// Dependents keyed by full package identity, including source, so equal name
/// and version from different origins never share constraints.
type DependentsIndex<'a> = HashMap<(&'a str, &'a str, Option<&'a str>), Vec<&'a Package>>;

/// Packages keyed by name and version for source lookup and pkgid ambiguity.
type NameVersionIndex<'a> = HashMap<(&'a str, &'a str), Vec<&'a Package>>;

fn build_indexes(all_packages: &[Package]) -> (DependentsIndex<'_>, NameVersionIndex<'_>) {
    let mut dependents = DependentsIndex::new();
    let mut names_and_versions = NameVersionIndex::new();
    for pkg in all_packages {
        names_and_versions
            .entry((pkg.name.as_str(), pkg.version.as_str()))
            .or_default()
            .push(pkg);
        for dep in &pkg.dependencies {
            dependents
                .entry((
                    dep.name.as_str(),
                    dep.version.as_str(),
                    dep.source.as_deref(),
                ))
                .or_default()
                .push(pkg);
        }
    }
    (dependents, names_and_versions)
}

/// Count versions within the eligible source, ignoring duplicate edges.
fn eligible_locked_versions(dependent: &Package, name: &str, source: Option<&str>) -> usize {
    dependent
        .dependencies
        .iter()
        .filter(|d| d.name == name && d.source.as_deref() == source)
        .map(|d| d.version.as_str())
        .collect::<HashSet<_>>()
        .len()
}

/// A single declaration's parsed requirement plus the evidence the shared
/// attribution policy needs: whether it is definitely active (mandatory) or
/// merely possible (optional/aliased). Target-specific declarations are
/// always mandatory too: Cargo's resolver evaluates every target table
/// regardless of the host platform. Local declarations are always mandatory,
/// since their representation drops that distinction.
struct NormalizedDeclaration {
    req: VersionReq,
    mandatory: bool,
}

/// The shared attribution policy's decision: which parsed requirements are
/// verified enforceable, plus whether the parent must still be marked
/// unverified. Both can hold at once (a mandatory constraint alongside an
/// uncertain declaration).
struct AttributionResult {
    enforced: Vec<VersionReq>,
    unverified: bool,
}

/// Applies the requirement-attribution policy shared by local manifests and
/// registry index records: dedupe by parsed requirement (aliases with an
/// equal requirement count once), decide multi-version ambiguity, and decide
/// which requirements are verified enforceable.
fn attribute_requirements(
    declarations: &[NormalizedDeclaration],
    locked_versions: usize,
) -> AttributionResult {
    let mut deduped: Vec<NormalizedDeclaration> = Vec::new();
    for decl in declarations {
        match deduped.iter_mut().find(|existing| existing.req == decl.req) {
            Some(existing) => existing.mandatory |= decl.mandatory,
            None => deduped.push(NormalizedDeclaration {
                req: decl.req.clone(),
                mandatory: decl.mandatory,
            }),
        }
    }

    if declarations.is_empty() || locked_versions > 1 && deduped.len() > 1 {
        return AttributionResult {
            enforced: Vec::new(),
            unverified: true,
        };
    }

    let mut enforced = Vec::new();
    let mut unverified = false;
    if deduped.iter().any(|d| d.mandatory) {
        // A mandatory declaration makes every matching mandatory requirement
        // definite; matching uncertain declarations remain annotations.
        for d in deduped {
            if d.mandatory {
                enforced.push(d.req);
            } else {
                unverified = true;
            }
        }
    } else if let [d] = deduped.as_slice() {
        // One uncertain declaration is the unique explanation for the edge.
        enforced.push(d.req.clone());
    }

    if enforced.is_empty() {
        unverified = true;
    }

    AttributionResult {
        enforced,
        unverified,
    }
}

/// Gathers requirements from registry dependents and local manifests.
#[allow(clippy::too_many_arguments)]
fn gather_constraints<T: Transport>(
    client: &mut CratesIoClient<T>,
    dependents_index: &DependentsIndex,
    name_version_index: &NameVersionIndex,
    direct_requirements: &[DirectRequirement],
    working_dir: &Path,
    name: &str,
    locked_version: &str,
    source: Option<&str>,
) -> GatheredConstraints {
    let mut constraints = Vec::new();
    let mut unverified_dependents = Vec::new();
    let locked = Version::parse(locked_version).ok();

    let dependents = dependents_index
        .get(&(name, locked_version, source))
        .into_iter()
        .flatten()
        .copied();

    for dependent in dependents {
        let unverified = if dependent.is_registry {
            let result = registry_dependent_constraints(
                client,
                name_version_index,
                dependent,
                name,
                locked.as_ref(),
                source,
            );
            let unverified = result.unverified;
            constraints.extend(result.constraints);
            unverified
        } else if dependent.source.is_none() {
            // Only an unsourced non-registry dependent is local; other
            // sources cannot be verified through a workspace manifest.
            // Match the declaring package's name and version, and only its
            // crates.io declarations, to avoid unrelated local requirements.
            let matching: Vec<&DirectRequirement> = direct_requirements
                .iter()
                .filter(|r| r.crate_name == name && r.declaring_package == dependent.name)
                .filter(|r| r.declaring_version.as_deref() == Some(dependent.version.as_str()))
                .filter(|r| r.source == RequirementSource::CratesIo)
                .filter(|r| locked.as_ref().is_some_and(|v| r.req.matches(v)))
                .collect();

            let declarations: Vec<NormalizedDeclaration> = matching
                .iter()
                .map(|r| NormalizedDeclaration {
                    req: r.req.clone(),
                    mandatory: true,
                })
                .collect();
            let locked_versions = eligible_locked_versions(dependent, name, source);
            let result = attribute_requirements(&declarations, locked_versions);

            if result.unverified {
                true
            } else {
                // Every local declaration is mandatory, so on this
                // non-ambiguous path `result.enforced` always contains every
                // deduped requirement from `matching`; no filter is needed to
                // decide which of `matching` to keep.
                constraints.extend(matching.iter().map(|req| Constraint {
                    blocker_name: manifest_label(&req.manifest, working_dir),
                    blocker_kind: BlockerKind::Manifest,
                    req: req.req.clone(),
                }));
                false
            }
        } else {
            // Git and alternate-registry requirements cannot be read here.
            true
        };
        if unverified {
            // The bare name when it identifies the dependent, else its spec.
            let label = package_spec_of(dependent, name_version_index);
            if !unverified_dependents.contains(&label) {
                unverified_dependents.push(label);
            }
        }
    }

    GatheredConstraints {
        constraints,
        unverified_dependents,
    }
}

/// Requirements a registry dependent's crates.io index record places on
/// `name` at `locked`, plus whether an uncertain declaration remains.
struct RegistryConstraints {
    constraints: Vec<Constraint>,
    unverified: bool,
}

fn registry_dependent_constraints<T: Transport>(
    client: &mut CratesIoClient<T>,
    name_version_index: &NameVersionIndex,
    dependent: &Package,
    name: &str,
    locked: Option<&Version>,
    source: Option<&str>,
) -> RegistryConstraints {
    // Without a readable index record for the dependent, nothing can be enforced.
    let record = client
        .fetch_index_record(&dependent.name, &dependent.version)
        .ok()
        .and_then(|records| {
            records
                .into_iter()
                .find(|record| record.vers == dependent.version)
        });
    let Some(record) = record else {
        return RegistryConstraints {
            constraints: Vec::new(),
            unverified: true,
        };
    };

    let mut declarations = Vec::new();
    let mut unverified = false;
    for dep in &record.deps {
        let real_name = dep.package.as_deref().unwrap_or(&dep.name);
        if dep.kind.as_deref() == Some("dev") || dep.registry.is_some() || real_name != name {
            continue;
        }
        match VersionReq::parse(&dep.req) {
            Ok(req) if locked.is_some_and(|v| req.matches(v)) => {
                let mandatory = dep.optional != Some(true);
                declarations.push(NormalizedDeclaration { req, mandatory });
            }
            Ok(_) => {}
            Err(_) => unverified = true,
        }
    }

    let locked_versions = eligible_locked_versions(dependent, name, source);
    let result = attribute_requirements(&declarations, locked_versions);

    let constraints = result
        .enforced
        .into_iter()
        .map(|req| Constraint {
            blocker_name: dependent.name.clone(),
            blocker_kind: BlockerKind::Package {
                version: dependent.version.clone(),
                package_spec: package_spec_of(dependent, name_version_index),
            },
            req,
        })
        .collect();

    RegistryConstraints {
        unverified: unverified || result.unverified,
        constraints,
    }
}

fn manifest_label(path: &Path, working_dir: &Path) -> String {
    path.strip_prefix(working_dir)
        .unwrap_or(path)
        .display()
        .to_string()
}

/// Uses a source-qualified pkgid when a shared name and version is ambiguous.
/// A lockfile source can already end in a `#fragment` (a git source's precise
/// commit); it is dropped so the result stays a single `{source}#{name}`.
fn build_package_spec(name: &str, target_source: Option<&str>, is_ambiguous: bool) -> String {
    match (is_ambiguous, target_source) {
        (true, Some(source)) => {
            let source = source.split_once('#').map_or(source, |(base, _)| base);
            format!("{source}#{name}")
        }
        _ => name.to_string(),
    }
}

/// Cargo's pkgid for a lockfile package: its bare name unless another package
/// shares its name and version, then `{source}#{name}`.
fn package_spec_of(package: &Package, index: &NameVersionIndex) -> String {
    let is_ambiguous = index
        .get(&(package.name.as_str(), package.version.as_str()))
        .is_some_and(|packages| packages.len() > 1);
    build_package_spec(&package.name, package.source.as_deref(), is_ambiguous)
}

/// Generates outcomes for "too new" violations; empty when there are none.
#[allow(clippy::too_many_arguments)]
pub fn generate_suggestions<T: Transport>(
    client: &mut CratesIoClient<T>,
    violations: &[Violation],
    all_packages: &[Package],
    direct_requirements: &[DirectRequirement],
    working_dir: &Path,
    age_bounds: AgeBounds,
    allow_prerelease: bool,
    now: DateTime<Utc>,
    on_progress: &mut dyn FnMut(SuggestionProgress),
) -> Vec<Outcome> {
    let too_new: Vec<&Violation> = violations
        .iter()
        .filter(|v| matches!(v.kind, ViolationKind::TooNew(_)))
        .collect();

    let (dependents_index, name_version_index) = build_indexes(all_packages);

    let mut outcomes = Vec::new();
    for (i, violation) in too_new.iter().enumerate() {
        on_progress(SuggestionProgress {
            current: i + 1,
            total: too_new.len(),
            package: violation.package.clone(),
        });

        let same_name_version = name_version_index
            .get(&(violation.package.as_str(), violation.version.as_str()))
            .map(Vec::as_slice)
            .unwrap_or_default();

        // This registry package is the violation target, even if another
        // source shares its name and version.
        let target_source = same_name_version
            .iter()
            .find(|p| p.is_registry)
            .and_then(|p| p.source.as_deref());

        let package_spec = build_package_spec(
            &violation.package,
            target_source,
            same_name_version.len() > 1,
        );

        let Ok(locked) = Version::parse(&violation.version) else {
            outcomes.push(Outcome::Unavailable {
                package: violation.package.clone(),
                package_spec,
                locked_version: violation.version.clone(),
                reason: "Locked version is not valid SemVer".to_string(),
            });
            continue;
        };

        let versions = match client.fetch_all_versions(&violation.package) {
            Ok(versions) => versions,
            Err(e) => {
                outcomes.push(Outcome::Unavailable {
                    package: violation.package.clone(),
                    package_spec,
                    locked_version: violation.version.clone(),
                    reason: e.to_string(),
                });
                continue;
            }
        };

        let gathered = gather_constraints(
            client,
            &dependents_index,
            &name_version_index,
            direct_requirements,
            working_dir,
            &violation.package,
            &violation.version,
            target_source,
        );
        let candidates = filter_candidates(&versions, &locked, age_bounds, now, allow_prerelease);

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
                package_spec,
                locked_version: violation.version.clone(),
                newest_compliant: newest_compliant.to_string(),
                blocker: Blocker {
                    // Whether this blocker's own locked version resolved to a
                    // suggestion is only known once every violation has been
                    // walked, so this starts false and is patched below.
                    also_suggested: false,
                    name: blocker.blocker_name,
                    kind: blocker.blocker_kind,
                    req: blocker.req.to_string(),
                },
            },
            WalkResult::NoCompliantVersion => Outcome::NoCompliantVersion {
                package: violation.package.clone(),
                package_spec,
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
            && let BlockerKind::Package { version, .. } = &blocker.kind
        {
            blocker.also_suggested = suggested.contains(&(blocker.name.clone(), version.clone()));
        }
    }

    outcomes
}

#[cfg(test)]
mod tests;

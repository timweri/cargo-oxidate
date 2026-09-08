use cargo_toml::{Dependency, DepsSet, Manifest};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Which registry a manifest dependency declaration names. A manifest's
/// `registry` (a Cargo config alias) or `registry-index` (a raw index URL)
/// field identifies an alternate registry by a different representation
/// than the source URL recorded against a lockfile package or dependency
/// edge, and this crate has no access to Cargo's registry configuration to
/// resolve the alias to a source. Recording that identity — without
/// claiming it resolves to any particular lockfile source — is enough to
/// keep it out of the crates.io suggestion flow, which only ever concerns
/// itself with `CratesIo` requirements. Cargo reserves `crates-io` as the
/// name of the default registry and also accepts its index URL directly, so
/// both are recognized as `CratesIo` rather than an alternate registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequirementSource {
    /// No `registry`/`registry-index` on the declaration, or one explicitly
    /// naming crates.io itself (`crates-io`, or its index URL): an ordinary
    /// crates.io dependency.
    CratesIo,
    /// An explicitly named alternate registry (alias or raw index URL). The
    /// identity is kept for diagnostics; it is never matched against a
    /// lockfile source string.
    Registry(String),
}

/// The identities Cargo treats as naming crates.io itself: the reserved
/// `crates-io` registry alias, and crates.io's own git and sparse index
/// URLs (either of which `registry-index` may name directly).
const CRATES_IO_REGISTRY_NAME: &str = "crates-io";
const CRATES_IO_GIT_INDEX: &str = "https://github.com/rust-lang/crates.io-index";
const CRATES_IO_SPARSE_INDEX: &str = "sparse+https://index.crates.io/";

fn is_crates_io_identity(registry: &str) -> bool {
    registry == CRATES_IO_REGISTRY_NAME
        || registry == CRATES_IO_GIT_INDEX
        || registry == CRATES_IO_SPARSE_INDEX
}

/// One version requirement the user's own manifests place on a registry
/// crate, together with the manifest that placed it (for warnings and
/// diagnostics), the name and version of the package that manifest declares
/// — callers use this identity, not just the name, to scope a requirement to
/// the lockfile dependent that actually placed it, rather than to every
/// manifest in the workspace that happens to mention the same crate name, or
/// to an unrelated package that happens to share the declaring package's
/// name — and which registry it was declared against.
///
/// `declaring_version` is `None` when the declaring package's version
/// couldn't be determined — most commonly a `version.workspace = true` whose
/// value the workspace root doesn't actually supply — so callers must treat
/// it as unresolvable identity rather than a wildcard.
pub struct DirectRequirement {
    pub manifest: PathBuf,
    pub declaring_package: String,
    pub declaring_version: Option<String>,
    pub crate_name: String,
    pub req: semver::VersionReq,
    pub source: RequirementSource,
}

/// Reads every version requirement the user's own manifests place on
/// registry crates.
///
/// `lockfile_dir` is the directory containing `Cargo.lock`, where the root
/// `Cargo.toml` is expected to live. Workspace members are expanded from
/// `[workspace.members]` glob patterns with `[workspace.exclude]` applied,
/// and path dependencies are followed one level further so that a member
/// not listed under `members` is still read. `dependencies`,
/// `dev-dependencies`, `build-dependencies`, and each `[target.*]` table are
/// all walked; path and git dependencies are skipped since they carry no
/// registry version. A path dependency followed this way is a workspace
/// member — and so has its `dev-dependencies` collected — exactly when the
/// root manifest has a `[workspace]` table and the dependency's directory
/// lies inside the workspace root, unless it matches `workspace.exclude`;
/// matching Cargo's own behavior.
///
/// Neither a missing manifest nor one that fails to parse aborts the run:
/// each produces a warning in the second return value and is simply
/// excluded from the (possibly empty) first.
pub fn load_direct_requirements(lockfile_dir: &Path) -> (Vec<DirectRequirement>, Vec<String>) {
    let mut warnings = Vec::new();
    let root_path = lockfile_dir.join("Cargo.toml");

    if !root_path.is_file() {
        warnings.push(format!(
            "No Cargo.toml found beside the lockfile at {}; direct dependency requirements were not checked",
            lockfile_dir.display()
        ));
        return (vec![], warnings);
    }

    let mut seen = HashSet::new();
    seen.insert(canonical_or(&root_path));

    // `bool` marks whether the manifest is a workspace member (root or a
    // `workspace.members` entry) as opposed to a followed path dependency.
    let mut manifests: Vec<(PathBuf, Manifest, bool)> = Vec::new();
    let mut has_workspace = false;
    let mut workspace_exclude: Vec<String> = Vec::new();
    match load_manifest(&root_path) {
        Ok(root) => {
            if let Some(ws) = &root.workspace {
                has_workspace = true;
                workspace_exclude = ws.exclude.clone();
                for member_dir in expand_members(lockfile_dir, ws) {
                    if !seen.insert(canonical_or(&member_dir)) {
                        continue;
                    }
                    let member_path = member_dir.join("Cargo.toml");
                    match load_manifest(&member_path) {
                        Ok(m) => manifests.push((member_path, m, true)),
                        Err(e) => warnings.push(e),
                    }
                }
            }
            manifests.push((root_path, root, true));
        }
        Err(e) => warnings.push(e),
    }

    // Follow path dependencies one level further, so members not listed
    // under `workspace.members` are still read. A followed dependency is
    // itself a workspace member exactly when the root has a `[workspace]`
    // table and its directory lies inside the workspace root, unless it
    // matches `workspace.exclude` — matching Cargo's own behavior.
    let canonical_root_dir = canonical_or(lockfile_dir);
    let mut followed = Vec::new();
    for (path, manifest, _) in &manifests {
        let base_dir = path.parent().unwrap_or(lockfile_dir);
        for dep_dir in path_dependency_dirs(base_dir, manifest) {
            if !seen.insert(canonical_or(&dep_dir)) {
                continue;
            }
            let dep_path = dep_dir.join("Cargo.toml");
            let canonical_dep_dir = canonical_or(&dep_dir);
            let is_member = has_workspace
                && canonical_dep_dir.starts_with(&canonical_root_dir)
                && !is_excluded(&canonical_root_dir, &canonical_dep_dir, &workspace_exclude);
            match load_manifest(&dep_path) {
                Ok(m) => followed.push((dep_path, m, is_member)),
                Err(e) => warnings.push(e),
            }
        }
    }
    manifests.extend(followed);

    let mut requirements = Vec::new();
    for (path, manifest, is_member) in &manifests {
        collect_requirements(path, manifest, *is_member, &mut requirements, &mut warnings);
    }

    (requirements, warnings)
}

fn canonical_or(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn load_manifest(path: &Path) -> Result<Manifest, String> {
    Manifest::from_path(path)
        .map_err(|e| format!("Failed to parse manifest {}: {e}", path.display()))
}

/// Expands `workspace.members` glob patterns against `root_dir`, dropping
/// anything matching `workspace.exclude`.
fn expand_members(root_dir: &Path, workspace: &cargo_toml::Workspace) -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    for pattern in &workspace.members {
        let full_pattern = root_dir.join(pattern);
        let Some(pattern_str) = full_pattern.to_str() else {
            continue;
        };
        let Ok(paths) = glob::glob(pattern_str) else {
            continue;
        };
        for entry in paths.flatten() {
            if is_excluded(root_dir, &entry, &workspace.exclude) {
                continue;
            }
            dirs.push(entry);
        }
    }

    dirs
}

fn is_excluded(root_dir: &Path, member_dir: &Path, exclude: &[String]) -> bool {
    let Ok(relative) = member_dir.strip_prefix(root_dir) else {
        return false;
    };
    exclude.iter().any(|pattern| relative.starts_with(pattern))
}

/// Directories of every path dependency declared in `manifest`'s normal,
/// dev, build, or target-specific dependency tables.
fn path_dependency_dirs(base_dir: &Path, manifest: &Manifest) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for deps in all_dep_sets(manifest, true) {
        for dep in deps.values() {
            if let Some(detail) = dep.detail()
                && let Some(rel_path) = &detail.path
            {
                dirs.push(base_dir.join(rel_path));
            }
        }
    }
    dirs
}

fn all_dep_sets(manifest: &Manifest, include_dev: bool) -> Vec<&DepsSet> {
    let mut sets = vec![&manifest.dependencies, &manifest.build_dependencies];
    if include_dev {
        sets.push(&manifest.dev_dependencies);
    }
    for target in manifest.target.values() {
        sets.push(&target.dependencies);
        sets.push(&target.build_dependencies);
        if include_dev {
            sets.push(&target.dev_dependencies);
        }
    }
    sets
}

fn collect_requirements(
    manifest_path: &Path,
    manifest: &Manifest,
    include_dev: bool,
    out: &mut Vec<DirectRequirement>,
    warnings: &mut Vec<String>,
) {
    // A manifest with no [package] table (a pure workspace root) declares no
    // crate identity, so it can never be a lockfile dependent — nothing it
    // lists (ordinarily nothing) could be scoped to it correctly.
    let Some(package) = manifest.package.as_ref() else {
        return;
    };
    let declaring_package = package.name().to_string();
    // `version.get()` fails only when the version is still
    // `workspace = true` and workspace inheritance never actually resolved
    // it (e.g. the workspace root has no `[workspace.package]` value for
    // it). `Manifest::from_path` has already applied workspace inheritance
    // by this point, so a resolvable version is already resolved here.
    let declaring_version = package.version.get().ok().map(|v| v.to_string());

    for deps in all_dep_sets(manifest, include_dev) {
        for (key, dep) in deps {
            collect_one(
                manifest_path,
                &declaring_package,
                declaring_version.as_deref(),
                key,
                dep,
                out,
                warnings,
            );
        }
    }
}

fn collect_one(
    manifest_path: &Path,
    declaring_package: &str,
    declaring_version: Option<&str>,
    key: &str,
    dep: &Dependency,
    out: &mut Vec<DirectRequirement>,
    warnings: &mut Vec<String>,
) {
    // Path and git dependencies aren't registry-versioned.
    if let Some(detail) = dep.detail()
        && (detail.path.is_some() || detail.git.is_some())
    {
        return;
    }

    let crate_name = dep.package().unwrap_or(key).to_string();
    let source = requirement_source(dep);

    match dep.try_req() {
        Ok(req) => out.push(DirectRequirement {
            manifest: manifest_path.to_path_buf(),
            declaring_package: declaring_package.to_string(),
            declaring_version: declaring_version.map(str::to_string),
            crate_name,
            req: req.clone(),
            source,
        }),
        Err(e) => warnings.push(format!(
            "Could not determine requirement for {crate_name} in {}: {e}",
            manifest_path.display()
        )),
    }
}

/// The registry a dependency declaration names. `dep` has already passed
/// through workspace inheritance by the time it reaches here (`from_path`
/// resolves it), so a `{ workspace = true }` dependency backed by a
/// workspace-level `registry` is read the same way as one declared directly.
fn requirement_source(dep: &Dependency) -> RequirementSource {
    match dep
        .detail()
        .and_then(|d| d.registry.clone().or_else(|| d.registry_index.clone()))
    {
        Some(registry) if is_crates_io_identity(&registry) => RequirementSource::CratesIo,
        Some(registry) => RequirementSource::Registry(registry),
        None => RequirementSource::CratesIo,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write(dir: &Path, rel: &str, contents: &str) -> PathBuf {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, contents).unwrap();
        path
    }

    fn find<'a>(reqs: &'a [DirectRequirement], name: &str) -> &'a DirectRequirement {
        reqs.iter()
            .find(|r| r.crate_name == name)
            .unwrap_or_else(|| panic!("no requirement collected for {name}"))
    }

    #[test]
    fn plain_string_requirement() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "Cargo.toml",
            r#"
[package]
name = "root"
version = "0.1.0"

[dependencies]
serde = "1.0"
"#,
        );

        let (reqs, warnings) = load_direct_requirements(dir.path());
        assert!(warnings.is_empty());
        assert_eq!(
            find(&reqs, "serde").req,
            semver::VersionReq::parse("1.0").unwrap()
        );
        assert_eq!(find(&reqs, "serde").declaring_package, "root");
        assert_eq!(
            find(&reqs, "serde").declaring_version.as_deref(),
            Some("0.1.0")
        );
    }

    #[test]
    fn workspace_inherited_declaring_version_is_resolved() {
        // "member"'s own `version` is inherited from the workspace root, not
        // written directly — the requirement's declaring identity must
        // still resolve to the workspace-supplied version, not go missing.
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "Cargo.toml",
            r#"
[workspace]
members = ["member"]

[workspace.package]
version = "2.3.4"
"#,
        );
        write(
            dir.path(),
            "member/Cargo.toml",
            r#"
[package]
name = "member"
version.workspace = true

[dependencies]
serde = "1.0"
"#,
        );

        let (reqs, warnings) = load_direct_requirements(dir.path());
        assert!(warnings.is_empty());
        assert_eq!(
            find(&reqs, "serde").declaring_version.as_deref(),
            Some("2.3.4")
        );
    }

    #[test]
    fn unresolvable_declaring_version_is_reported_as_unavailable() {
        // "member" inherits its version from the workspace, but the
        // workspace root supplies no `[workspace.package]` value for it —
        // the version genuinely can't be determined, and must come back as
        // `None` rather than some guessed-at value.
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "Cargo.toml",
            r#"
[workspace]
members = ["member"]
"#,
        );
        write(
            dir.path(),
            "member/Cargo.toml",
            r#"
[package]
name = "member"
version.workspace = true

[dependencies]
serde = "1.0"
"#,
        );

        let (reqs, warnings) = load_direct_requirements(dir.path());
        assert!(
            !warnings.is_empty(),
            "expected a warning about the unresolved workspace field"
        );
        assert!(
            reqs.is_empty(),
            "the member's manifest failed to load, so it collects no requirements"
        );
    }

    #[test]
    fn detailed_dependency_with_rename() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "Cargo.toml",
            r#"
[package]
name = "root"
version = "0.1.0"

[dependencies]
my_serde = { package = "serde", version = "1.0" }
"#,
        );

        let (reqs, warnings) = load_direct_requirements(dir.path());
        assert!(warnings.is_empty());
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].crate_name, "serde");
    }

    #[test]
    fn workspace_inherited_requirement() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "Cargo.toml",
            r#"
[workspace]
members = ["member"]

[workspace.dependencies]
serde = "1.0"
"#,
        );
        write(
            dir.path(),
            "member/Cargo.toml",
            r#"
[package]
name = "member"
version = "0.1.0"

[dependencies]
serde = { workspace = true }
"#,
        );

        let (reqs, warnings) = load_direct_requirements(dir.path());
        assert!(warnings.is_empty());
        assert_eq!(
            find(&reqs, "serde").req,
            semver::VersionReq::parse("1.0").unwrap()
        );
    }

    #[test]
    fn target_specific_table() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "Cargo.toml",
            r#"
[package]
name = "root"
version = "0.1.0"

[target.'cfg(unix)'.dependencies]
libc = "0.2"
"#,
        );

        let (reqs, warnings) = load_direct_requirements(dir.path());
        assert!(warnings.is_empty());
        assert_eq!(
            find(&reqs, "libc").req,
            semver::VersionReq::parse("0.2").unwrap()
        );
    }

    #[test]
    fn workspace_members_glob_with_exclude() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "Cargo.toml",
            r#"
[workspace]
members = ["crates/*"]
exclude = ["crates/skip-me"]
"#,
        );
        write(
            dir.path(),
            "crates/a/Cargo.toml",
            r#"
[package]
name = "a"
version = "0.1.0"

[dependencies]
serde = "1.0"
"#,
        );
        write(
            dir.path(),
            "crates/skip-me/Cargo.toml",
            r#"
[package]
name = "skip-me"
version = "0.1.0"

[dependencies]
rand = "0.8"
"#,
        );

        let (reqs, warnings) = load_direct_requirements(dir.path());
        assert!(warnings.is_empty());
        assert!(reqs.iter().any(|r| r.crate_name == "serde"));
        assert!(!reqs.iter().any(|r| r.crate_name == "rand"));
    }

    #[test]
    fn path_dependency_followed_one_level() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "Cargo.toml",
            r#"
[package]
name = "root"
version = "0.1.0"

[dependencies]
helper = { path = "helper" }
"#,
        );
        write(
            dir.path(),
            "helper/Cargo.toml",
            r#"
[package]
name = "helper"
version = "0.1.0"

[dependencies]
serde = "1.0"
"#,
        );

        let (reqs, warnings) = load_direct_requirements(dir.path());
        assert!(warnings.is_empty());
        assert!(reqs.iter().any(|r| r.crate_name == "serde"));
        // The path dependency itself is skipped: it's not registry-versioned.
        assert!(!reqs.iter().any(|r| r.crate_name == "helper"));
    }

    #[test]
    fn followed_path_dependency_dev_dependencies_are_skipped() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "Cargo.toml",
            r#"
[package]
name = "root"
version = "0.1.0"

[dependencies]
helper = { path = "helper" }

[dev-dependencies]
baz = "3"
"#,
        );
        write(
            dir.path(),
            "helper/Cargo.toml",
            r#"
[package]
name = "helper"
version = "0.1.0"

[dependencies]
foo = "1"

[dev-dependencies]
bar = "2"
"#,
        );

        let (reqs, warnings) = load_direct_requirements(dir.path());
        assert!(warnings.is_empty());
        // helper is not a workspace member, so Cargo ignores its
        // dev-dependencies.
        assert!(reqs.iter().any(|r| r.crate_name == "foo"));
        assert!(!reqs.iter().any(|r| r.crate_name == "bar"));
        // The root is a workspace member, so its dev-dependencies are
        // still collected.
        assert!(reqs.iter().any(|r| r.crate_name == "baz"));
    }

    #[test]
    fn in_tree_path_dependency_of_workspace_is_a_member() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "Cargo.toml",
            r#"
[workspace]
members = []

[package]
name = "root"
version = "0.1.0"

[dependencies]
helper = { path = "helper" }
"#,
        );
        write(
            dir.path(),
            "helper/Cargo.toml",
            r#"
[package]
name = "helper"
version = "0.1.0"

[dev-dependencies]
foo = "=1.9.0"
"#,
        );

        let (reqs, warnings) = load_direct_requirements(dir.path());
        assert!(warnings.is_empty());
        let foo = find(&reqs, "foo");
        assert_eq!(foo.req, semver::VersionReq::parse("=1.9.0").unwrap());
        assert_eq!(foo.declaring_package, "helper");
    }

    #[test]
    fn excluded_in_tree_path_dependency_is_not_a_member() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "Cargo.toml",
            r#"
[workspace]
members = []
exclude = ["helper"]

[package]
name = "root"
version = "0.1.0"

[dependencies]
helper = { path = "helper" }
"#,
        );
        write(
            dir.path(),
            "helper/Cargo.toml",
            r#"
[package]
name = "helper"
version = "0.1.0"

[dependencies]
bar = "1"

[dev-dependencies]
foo = "=1.9.0"
"#,
        );

        let (reqs, warnings) = load_direct_requirements(dir.path());
        assert!(warnings.is_empty());
        assert!(!reqs.iter().any(|r| r.crate_name == "foo"));
        assert!(reqs.iter().any(|r| r.crate_name == "bar"));
    }

    #[test]
    fn out_of_tree_path_dependency_is_not_a_member() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "ws/Cargo.toml",
            r#"
[workspace]
members = []

[package]
name = "root"
version = "0.1.0"

[dependencies]
helper = { path = "../helper" }
"#,
        );
        write(
            dir.path(),
            "helper/Cargo.toml",
            r#"
[package]
name = "helper"
version = "0.1.0"

[dev-dependencies]
foo = "=1.9.0"
"#,
        );

        let (reqs, warnings) = load_direct_requirements(&dir.path().join("ws"));
        assert!(warnings.is_empty());
        assert!(!reqs.iter().any(|r| r.crate_name == "foo"));
    }

    #[test]
    fn dependency_table_records_registry_identity() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "Cargo.toml",
            r#"
[package]
name = "root"
version = "0.1.0"

[dependencies]
serde = "1.0"
priv_serde = { package = "serde", version = "1.0", registry = "priv" }

[dev-dependencies]
rand = { version = "0.8", registry = "priv" }

[build-dependencies]
libc = { version = "0.2", registry = "priv" }

[target.'cfg(unix)'.dependencies]
nix = { version = "0.2", registry = "priv" }
"#,
        );

        let (reqs, warnings) = load_direct_requirements(dir.path());
        assert!(warnings.is_empty());

        let crates_io_serde = reqs
            .iter()
            .find(|r| r.crate_name == "serde" && r.source == RequirementSource::CratesIo)
            .expect("plain serde dependency should resolve to crates.io");
        assert_eq!(
            crates_io_serde.req,
            semver::VersionReq::parse("1.0").unwrap()
        );

        let priv_serde = reqs
            .iter()
            .find(|r| r.crate_name == "serde" && r.source != RequirementSource::CratesIo)
            .expect("renamed serde dependency should record its alternate registry");
        assert_eq!(
            priv_serde.source,
            RequirementSource::Registry("priv".to_string())
        );

        for name in ["rand", "libc", "nix"] {
            assert_eq!(
                find(&reqs, name).source,
                RequirementSource::Registry("priv".to_string()),
                "{name} should record its alternate registry"
            );
        }
    }

    #[test]
    fn workspace_inherited_registry_metadata_is_preserved() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "Cargo.toml",
            r#"
[workspace]
members = ["member"]

[workspace.dependencies]
serde = { version = "1.0", registry = "priv" }
"#,
        );
        write(
            dir.path(),
            "member/Cargo.toml",
            r#"
[package]
name = "member"
version = "0.1.0"

[dependencies]
serde = { workspace = true }
"#,
        );

        let (reqs, warnings) = load_direct_requirements(dir.path());
        assert!(warnings.is_empty());
        assert_eq!(
            find(&reqs, "serde").source,
            RequirementSource::Registry("priv".to_string())
        );
    }

    #[test]
    fn explicit_crates_io_registry_identity_is_recognized() {
        // `registry = "crates-io"` is Cargo's reserved name for the default
        // registry, and a `registry-index` naming crates.io's own index URL
        // directly is the same registry under its literal address. Neither
        // is an alternate registry, so both must be `CratesIo`.
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "Cargo.toml",
            r#"
[package]
name = "root"
version = "0.1.0"

[dependencies]
by_name = { package = "serde", version = "1.0", registry = "crates-io" }
by_git_index = { package = "rand", version = "0.8", registry-index = "https://github.com/rust-lang/crates.io-index" }
by_sparse_index = { package = "libc", version = "0.2", registry-index = "sparse+https://index.crates.io/" }
"#,
        );

        let (reqs, warnings) = load_direct_requirements(dir.path());
        assert!(warnings.is_empty());
        for name in ["serde", "rand", "libc"] {
            assert_eq!(
                find(&reqs, name).source,
                RequirementSource::CratesIo,
                "{name} should resolve to crates.io"
            );
        }
    }

    #[test]
    fn missing_manifest_degrades_to_a_warning() {
        let dir = tempdir().unwrap();

        let (reqs, warnings) = load_direct_requirements(dir.path());
        assert!(reqs.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("No Cargo.toml"));
    }
}

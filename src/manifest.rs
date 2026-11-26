use cargo_toml::{Dependency, DepsSet, Manifest};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// A manifest registry identity; aliases are not lockfile source URLs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequirementSource {
    /// The default registry or its accepted name/index identities.
    CratesIo,
    /// An alternate registry alias or raw index, retained for diagnostics.
    Registry(String),
}

/// Registry values Cargo treats as crates.io.
const CRATES_IO_REGISTRY_NAME: &str = "crates-io";
const CRATES_IO_GIT_INDEX: &str = "https://github.com/rust-lang/crates.io-index";
const CRATES_IO_SPARSE_INDEX: &str = "sparse+https://index.crates.io/";

fn is_crates_io_identity(registry: &str) -> bool {
    matches!(
        registry,
        CRATES_IO_REGISTRY_NAME | CRATES_IO_GIT_INDEX | CRATES_IO_SPARSE_INDEX
    )
}

/// A registry requirement scoped to its declaring package and registry.
/// A missing `declaring_version` is unresolved, never a wildcard.
pub struct DirectRequirement {
    pub manifest: PathBuf,
    pub declaring_package: String,
    pub declaring_version: Option<String>,
    pub crate_name: String,
    pub req: semver::VersionReq,
    pub source: RequirementSource,
}

/// Collects registry requirements from the root, members, and one-level path
/// dependencies. Members and in-tree, non-excluded path dependencies include
/// dev dependencies. Missing or invalid manifests produce warnings and are
/// excluded.
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

    let canonical_root_dir = canonical_or(lockfile_dir);
    let mut seen = HashSet::from([canonical_root_dir.clone()]);

    // `bool` marks whether the manifest is a workspace member (root or a
    // `workspace.members` entry) as opposed to a followed path dependency.
    let mut manifests: Vec<(PathBuf, Manifest, bool)> = Vec::new();
    let mut workspace: Option<cargo_toml::Workspace> = None;
    match load_manifest(&root_path) {
        Ok(root) => {
            if let Some(ws) = &root.workspace {
                workspace = Some(ws.clone());
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

    // Follow one level; only in-tree, non-excluded dependencies are members.
    let mut followed = Vec::new();
    for (path, manifest, _) in &manifests {
        let base_dir = path.parent().unwrap_or(lockfile_dir);
        for dep_dir in path_dependency_dirs(base_dir, manifest) {
            let canonical_dep_dir = canonical_or(&dep_dir);
            if !seen.insert(canonical_dep_dir.clone()) {
                continue;
            }
            let dep_path = dep_dir.join("Cargo.toml");
            let is_member = workspace.as_ref().is_some_and(|ws| {
                canonical_dep_dir.starts_with(&canonical_root_dir)
                    && !is_excluded(&canonical_root_dir, &canonical_dep_dir, ws)
            });
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
        .map_err(|e| format!("Failed to load manifest {}: {e}", path.display()))
}

/// Expands `workspace.members` glob patterns against `root_dir`, dropping
/// anything matching `workspace.exclude`. Like cargo, a pattern that yields no
/// glob results (or isn't a usable glob) is taken as a literal path; glob
/// results that aren't directories are dropped.
fn expand_members(root_dir: &Path, workspace: &cargo_toml::Workspace) -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    for pattern in &workspace.members {
        let full_pattern = root_dir.join(pattern);
        let matches = glob_matches(&full_pattern);
        let candidates = if matches.is_empty() {
            vec![full_pattern]
        } else {
            matches.into_iter().filter(|p| p.is_dir()).collect()
        };
        dirs.extend(
            candidates
                .into_iter()
                .filter(|entry| !is_excluded(root_dir, entry, workspace)),
        );
    }

    dirs
}

/// Paths matching `pattern`; empty if it is not valid UTF-8 or not a valid glob.
fn glob_matches(pattern: &Path) -> Vec<PathBuf> {
    pattern
        .to_str()
        .and_then(|s| glob::glob(s).ok())
        .map(|paths| paths.flatten().collect())
        .unwrap_or_default()
}

/// Mirrors cargo: a path is excluded when under an `exclude` entry, unless it is
/// also under a `members` entry (explicit members win over exclude).
fn is_excluded(root_dir: &Path, dir: &Path, workspace: &cargo_toml::Workspace) -> bool {
    let under = |paths: &[String]| paths.iter().any(|p| dir.starts_with(root_dir.join(p)));
    under(&workspace.exclude) && !under(&workspace.members)
}

/// Directories of every path dependency declared in `manifest`'s normal,
/// dev, build, or target-specific dependency tables.
fn path_dependency_dirs(base_dir: &Path, manifest: &Manifest) -> Vec<PathBuf> {
    all_dep_sets(manifest, true)
        .into_iter()
        .flat_map(|deps| deps.values())
        .filter_map(|dep| dep.detail()?.path.as_ref())
        .map(|rel_path| base_dir.join(rel_path))
        .collect()
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
    // A workspace root without [package] has no crate identity to scope.
    let Some(package) = manifest.package.as_ref() else {
        return;
    };
    let declaring_package = package.name().to_string();
    // Only unresolved `workspace = true` versions fail here; from_path
    // already applies resolvable workspace inheritance.
    let declaring_version = package.version.get().ok().map(|v| v.to_string());

    for deps in all_dep_sets(manifest, include_dev) {
        for (key, dep) in deps {
            // Path and git dependencies aren't registry-versioned.
            if let Some(detail) = dep.detail()
                && (detail.path.is_some() || detail.git.is_some())
            {
                continue;
            }

            let crate_name = dep.package().unwrap_or(key).to_string();
            let source = requirement_source(dep);
            match dep.try_req() {
                Ok(req) => out.push(DirectRequirement {
                    manifest: manifest_path.to_path_buf(),
                    declaring_package: declaring_package.clone(),
                    declaring_version: declaring_version.clone(),
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
    }
}

/// The registry a dependency names. `from_path` has already applied workspace
/// inheritance, so `{ workspace = true }` dependencies are read like direct ones.
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

    fn write(dir: &Path, rel: &str, contents: &str) {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    /// A manifest for package `name` whose `[dependencies]` table is `deps`.
    fn package(name: &str, deps: &str) -> String {
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n\n[dependencies]\n{deps}\n")
    }

    /// Writes `files` (relative path, contents) into a fresh directory, then
    /// loads that directory.
    fn load(files: &[(&str, &str)]) -> (Vec<DirectRequirement>, Vec<String>) {
        let dir = tempdir().unwrap();
        for (rel, contents) in files {
            write(dir.path(), rel, contents);
        }
        load_direct_requirements(dir.path())
    }

    /// Like `load`, but asserts that no warnings were produced.
    fn load_ok(files: &[(&str, &str)]) -> Vec<DirectRequirement> {
        let (reqs, warnings) = load(files);
        assert!(warnings.is_empty(), "{warnings:?}");
        reqs
    }

    fn find<'a>(reqs: &'a [DirectRequirement], name: &str) -> &'a DirectRequirement {
        reqs.iter()
            .find(|r| r.crate_name == name)
            .unwrap_or_else(|| panic!("no requirement collected for {name}"))
    }

    fn crate_names(reqs: &[DirectRequirement]) -> Vec<&str> {
        let mut names: Vec<_> = reqs.iter().map(|r| r.crate_name.as_str()).collect();
        names.sort();
        names
    }

    #[test]
    fn plain_string_requirement() {
        let reqs = load_ok(&[("Cargo.toml", &package("root", r#"serde = "1.0""#))]);

        let serde = find(&reqs, "serde");
        assert_eq!(serde.req, semver::VersionReq::parse("1.0").unwrap());
        assert_eq!(serde.declaring_package, "root");
        assert_eq!(serde.declaring_version.as_deref(), Some("0.1.0"));
    }

    #[test]
    fn workspace_inherited_declaring_version_is_resolved() {
        let member = r#"
[package]
name = "member"
version.workspace = true

[dependencies]
serde = "1.0"
"#;
        let reqs = load_ok(&[
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"member\"]\n\n[workspace.package]\nversion = \"2.3.4\"\n",
            ),
            ("member/Cargo.toml", member),
        ]);

        assert_eq!(
            find(&reqs, "serde").declaring_version.as_deref(),
            Some("2.3.4")
        );
    }

    #[test]
    fn unresolvable_inherited_version_warns_and_skips_the_member() {
        // The workspace root supplies no `[workspace.package] version`.
        let member = r#"
[package]
name = "member"
version.workspace = true

[dependencies]
serde = "1.0"
"#;
        let (reqs, warnings) = load(&[
            ("Cargo.toml", "[workspace]\nmembers = [\"member\"]\n"),
            ("member/Cargo.toml", member),
        ]);

        assert!(!warnings.is_empty(), "expected a warning for the member");
        assert!(reqs.is_empty());
    }

    #[test]
    fn workspace_inherited_dependency_keeps_version_and_registry() {
        let reqs = load_ok(&[
            (
                "Cargo.toml",
                r#"
[workspace]
members = ["member"]

[workspace.dependencies]
serde = { version = "1.0", registry = "priv" }
"#,
            ),
            (
                "member/Cargo.toml",
                &package("member", "serde = { workspace = true }"),
            ),
        ]);

        let serde = find(&reqs, "serde");
        assert_eq!(serde.req, semver::VersionReq::parse("1.0").unwrap());
        assert_eq!(
            serde.source,
            RequirementSource::Registry("priv".to_string())
        );
    }

    #[test]
    fn dependency_table_records_registry_identity() {
        let reqs = load_ok(&[(
            "Cargo.toml",
            r#"
[package]
name = "root"
version = "0.1.0"

[dependencies]
serde = "1.0"
priv_serde = { package = "serde", version = "1.0", registry = "priv" }
by_index = { version = "1.0", registry-index = "sparse+https://alt.example/index/" }

[dev-dependencies]
rand = { version = "0.8", registry = "priv" }

[build-dependencies]
libc = { version = "0.2", registry = "priv" }

[target.'cfg(unix)'.dependencies]
nix = { version = "0.2", registry = "priv" }
"#,
        )]);

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
        assert_eq!(
            find(&reqs, "by_index").source,
            RequirementSource::Registry("sparse+https://alt.example/index/".to_string())
        );
    }

    #[test]
    fn explicit_crates_io_registry_identity_is_recognized() {
        // `registry = "crates-io"` is Cargo's reserved name for the default
        // registry, and a `registry-index` naming crates.io's own index URL
        // is the same registry under its literal address.
        let reqs = load_ok(&[(
            "Cargo.toml",
            &package(
                "root",
                r#"
by_name = { package = "serde", version = "1.0", registry = "crates-io" }
by_git_index = { package = "rand", version = "0.8", registry-index = "https://github.com/rust-lang/crates.io-index" }
by_sparse_index = { package = "libc", version = "0.2", registry-index = "sparse+https://index.crates.io/" }"#,
            ),
        )]);

        for name in ["serde", "rand", "libc"] {
            assert_eq!(
                find(&reqs, name).source,
                RequirementSource::CratesIo,
                "{name} should resolve to crates.io"
            );
        }
    }

    #[test]
    fn workspace_exclude_drops_matching_glob_members() {
        // The `./` form must exclude exactly like the plain one.
        for exclude in ["crates/legacy", "./crates/legacy"] {
            let root =
                format!("[workspace]\nmembers = [\"crates/*\"]\nexclude = [\"{exclude}\"]\n");
            let reqs = load_ok(&[
                ("Cargo.toml", &root),
                ("crates/a/Cargo.toml", &package("a", r#"serde = "1.0""#)),
                (
                    "crates/legacy/Cargo.toml",
                    &package("legacy", r#"rand = "0.8""#),
                ),
            ]);

            assert_eq!(crate_names(&reqs), ["serde"], "exclude = {exclude}");
        }
    }

    #[test]
    fn explicit_member_inside_excluded_dir_is_kept() {
        let reqs = load_ok(&[
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"crates/*\", \"crates/legacy/core\"]\nexclude = [\"crates/legacy\"]\n",
            ),
            (
                "crates/legacy/Cargo.toml",
                &package("legacy", r#"rand = "0.8""#),
            ),
            (
                "crates/legacy/core/Cargo.toml",
                &package("core", r#"serde = "1.0""#),
            ),
        ]);

        assert_eq!(crate_names(&reqs), ["serde"]);
    }

    #[test]
    fn stray_file_in_members_glob_is_not_a_member() {
        let reqs = load_ok(&[
            ("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n"),
            ("crates/a/Cargo.toml", &package("a", r#"serde = "1.0""#)),
            ("crates/.DS_Store", ""),
        ]);

        assert_eq!(crate_names(&reqs), ["serde"]);
    }

    #[test]
    fn member_under_root_with_glob_metacharacters_is_found() {
        // `[1]` in the root path makes the joined member pattern match nothing;
        // cargo then uses the literal path, so we must too.
        let parent = tempdir().unwrap();
        let root = parent.path().join("proj[1]");
        write(
            &root,
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/a\"]\n",
        );
        write(
            &root,
            "crates/a/Cargo.toml",
            &package("a", r#"serde = "1.0""#),
        );

        let (reqs, warnings) = load_direct_requirements(&root);

        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(crate_names(&reqs), ["serde"]);
    }

    #[test]
    fn literal_member_with_missing_directory_warns() {
        let (_, warnings) = load(&[(
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/missing\"]\n",
        )]);

        assert!(
            warnings
                .iter()
                .any(|w| w.contains("Failed to load manifest") && w.contains("missing")),
            "{warnings:?}"
        );
    }

    #[test]
    fn path_dependency_is_followed_one_level_without_its_dev_dependencies() {
        let reqs = load_ok(&[
            (
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
            ),
            (
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
            ),
        ]);

        // helper is not a workspace member, so Cargo ignores its
        // dev-dependencies; the root's are collected. The path dependency
        // itself is not registry-versioned.
        assert_eq!(crate_names(&reqs), ["baz", "foo"]);
    }

    #[test]
    fn in_tree_path_dependency_of_workspace_is_a_member() {
        let reqs = load_ok(&[
            (
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
            ),
            (
                "helper/Cargo.toml",
                r#"
[package]
name = "helper"
version = "0.1.0"

[dev-dependencies]
foo = "=1.9.0"
"#,
            ),
        ]);

        let foo = find(&reqs, "foo");
        assert_eq!(foo.req, semver::VersionReq::parse("=1.9.0").unwrap());
        assert_eq!(foo.declaring_package, "helper");
    }

    #[test]
    fn excluded_in_tree_path_dependency_is_not_a_member() {
        let reqs = load_ok(&[
            (
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
            ),
            (
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
            ),
        ]);

        assert_eq!(crate_names(&reqs), ["bar"]);
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
        assert!(reqs.is_empty());
    }

    #[test]
    fn member_path_dependency_on_root_does_not_duplicate_root_requirements() {
        // "member" depends back on the workspace root (`app = { path = ".." }`).
        // The root is already collected as a member, so following that path
        // dependency must not collect it a second time.
        let reqs = load_ok(&[
            (
                "Cargo.toml",
                r#"
[workspace]
members = ["member"]

[package]
name = "root"
version = "0.1.0"

[dependencies]
serde = "1.0"
"#,
            ),
            (
                "member/Cargo.toml",
                &package("member", r#"app = { path = ".." }"#),
            ),
        ]);

        assert_eq!(crate_names(&reqs), ["serde"]);
    }

    #[test]
    fn missing_manifest_degrades_to_a_warning() {
        let (reqs, warnings) = load(&[]);

        assert!(reqs.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("No Cargo.toml"));
    }
}

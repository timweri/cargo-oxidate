use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// A package or dependency edge. `source` is the origin as `cargo_lock::SourceId`'s
/// canonical string, so same-name, same-version packages from different origins differ.
pub struct PackageRef {
    pub name: String,
    pub version: String,
    pub source: Option<String>,
}

/// A `Cargo.lock` entry. Path and git packages are kept so workspace members can
/// appear as dependents; `is_registry` marks the ones eligible for the age check.
pub struct Package {
    pub name: String,
    pub version: String,
    pub is_registry: bool,
    pub source: Option<String>,
    pub dependencies: Vec<PackageRef>,
}

/// The canonical lockfile path and its packages. Derive the lockfile's directory
/// from `path`, which has symlinks and `..` already resolved.
pub struct LoadedLockfile {
    pub path: PathBuf,
    pub packages: Vec<Package>,
}

/// Loads every package in a lockfile, together with its canonical path.
///
/// `path` is resolved against `working_dir` if relative, and must be a regular
/// file within `working_dir` (`..` traversal and symlink escapes included).
/// `cargo_lock` resolves each dependency edge to a concrete version itself, so
/// every `PackageRef` carries one.
pub fn load(path: &Path, working_dir: &Path) -> Result<LoadedLockfile> {
    // `join` keeps an absolute `path` as is. Canonicalizing resolves symlinks
    // and `..` components, and requires the file to exist.
    let canonical = working_dir.join(path).canonicalize().context(format!(
        "Cargo.lock path does not exist or is not accessible: {}",
        path.display()
    ))?;

    if !canonical.is_file() {
        anyhow::bail!("Cargo.lock path is not a regular file: {}", path.display());
    }

    let canonical_working_dir = working_dir
        .canonicalize()
        .context("Failed to canonicalize current directory")?;

    if !canonical.starts_with(&canonical_working_dir) {
        anyhow::bail!(
            "Cargo.lock path escapes the working directory: {}",
            path.display()
        );
    }

    let lockfile = cargo_lock::Lockfile::load(&canonical)
        .context(format!(
            "Could not load lockfile at {}",
            canonical.display()
        ))
        .context("Failed to parse Cargo.lock")?;

    let packages = lockfile
        .packages
        .into_iter()
        .map(|p| Package {
            name: p.name.as_str().to_string(),
            version: p.version.to_string(),
            is_registry: p.source.as_ref().is_some_and(|s| s.is_default_registry()),
            source: p.source.as_ref().map(|s| s.to_string()),
            dependencies: p
                .dependencies
                .iter()
                .map(|d| PackageRef {
                    name: d.name.as_str().to_string(),
                    version: d.version.to_string(),
                    source: d.source.as_ref().map(|s| s.to_string()),
                })
                .collect(),
        })
        .collect();

    Ok(LoadedLockfile {
        path: canonical,
        packages,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    const LOCKFILE_HEADER: &str = "version = 4\n";

    fn registry_entry(name: &str, version: &str) -> String {
        registry_entry_with_deps(name, version, &[])
    }

    fn registry_entry_with_deps(name: &str, version: &str, deps: &[&str]) -> String {
        let deps_line = if deps.is_empty() {
            String::new()
        } else {
            let list = deps
                .iter()
                .map(|d| format!("\"{d}\""))
                .collect::<Vec<_>>()
                .join(",\n ");
            format!("dependencies = [\n {list},\n]\n")
        };
        format!(
            r#"
[[package]]
name = "{name}"
version = "{version}"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "0000000000000000000000000000000000000000000000000000000000000000"
{deps_line}"#
        )
    }

    fn git_entry(name: &str, version: &str) -> String {
        format!(
            r#"
[[package]]
name = "{name}"
version = "{version}"
source = "git+https://github.com/example/{name}#0000000000000000000000000000000000000000"
"#
        )
    }

    fn path_entry(name: &str, version: &str) -> String {
        format!(
            r#"
[[package]]
name = "{name}"
version = "{version}"
"#
        )
    }

    fn alt_registry_entry(name: &str, version: &str) -> String {
        format!(
            r#"
[[package]]
name = "{name}"
version = "{version}"
source = "registry+https://example.com/index"
"#
        )
    }

    fn write_lockfile(dir: &Path, contents: &str) {
        std::fs::write(
            dir.join("Cargo.lock"),
            format!("{LOCKFILE_HEADER}{contents}"),
        )
        .unwrap();
    }

    /// Writes `contents` as `Cargo.lock` in a fresh directory and loads it.
    fn load_packages(contents: &str) -> Vec<Package> {
        let dir = tempdir().unwrap();
        write_lockfile(dir.path(), contents);
        load(Path::new("Cargo.lock"), dir.path()).unwrap().packages
    }

    fn find<'a>(packages: &'a [Package], name: &str) -> &'a Package {
        packages.iter().find(|p| p.name == name).unwrap()
    }

    #[test]
    fn non_registry_packages_are_kept_with_the_flag_set_and_their_sources() {
        let packages = load_packages(
            &[
                registry_entry("serde", "1.0.0"),
                git_entry("rand", "0.8.0"),
                path_entry("local-crate", "0.1.0"),
            ]
            .concat(),
        );

        assert_eq!(packages.len(), 3);
        let serde = find(&packages, "serde");
        let rand = find(&packages, "rand");
        let local = find(&packages, "local-crate");
        assert!(serde.is_registry);
        assert!(!rand.is_registry);
        assert!(!local.is_registry);
        assert!(serde.source.is_some());
        assert!(rand.source.is_some());
        assert_ne!(serde.source, rand.source);
        assert_eq!(local.source, None);
    }

    #[test]
    fn dependency_resolves_with_or_without_a_version_in_the_lockfile() {
        for dep in ["b 2.0.0", "b"] {
            let packages = load_packages(
                &[
                    registry_entry_with_deps("a", "1.0.0", &[dep]),
                    registry_entry("b", "2.0.0"),
                ]
                .concat(),
            );

            let edge = &find(&packages, "a").dependencies[0];
            assert_eq!(edge.name, "b", "{dep}");
            assert_eq!(edge.version, "2.0.0", "{dep}");
            assert_eq!(edge.source, find(&packages, "b").source, "{dep}");
        }
    }

    #[test]
    fn dependency_edges_carry_the_selected_packages_resolved_source() {
        // `cargo_lock` already resolves each dependency edge to its selected
        // package's source while parsing. A root package depends, without
        // source qualification, on a crates.io package, a git package, an
        // alternate-registry package, and a path package, plus one dangling
        // versioned edge to a package absent from the lockfile.
        let packages = load_packages(
            &[
                registry_entry_with_deps(
                    "root",
                    "1.0.0",
                    &[
                        "crates-dep 1.0.0",
                        "git-dep 1.0.0",
                        "alt-dep 1.0.0",
                        "path-dep 1.0.0",
                        "missing-dep 9.9.9",
                    ],
                ),
                registry_entry("crates-dep", "1.0.0"),
                git_entry("git-dep", "1.0.0"),
                alt_registry_entry("alt-dep", "1.0.0"),
                path_entry("path-dep", "1.0.0"),
            ]
            .concat(),
        );
        let root = find(&packages, "root");
        let edge = |name: &str| root.dependencies.iter().find(|d| d.name == name).unwrap();

        assert_eq!(
            edge("crates-dep").source,
            find(&packages, "crates-dep").source
        );
        // A git edge's source drops the commit hash that the target package's
        // own source keeps (`normalize_git_source_for_dependency`).
        assert!(edge("git-dep").source.is_some());
        assert_eq!(edge("alt-dep").source, find(&packages, "alt-dep").source);
        assert_eq!(edge("path-dep").source, None);
        assert_eq!(edge("missing-dep").source, None);
    }

    #[test]
    fn source_qualified_edge_keeps_its_declared_source_over_a_same_identity_path_package() {
        // A path package and a crates.io package share a name and version.
        // The dependent's edge to it is source-qualified, so it must keep
        // that declared source rather than resolving to the path package.
        let packages = load_packages(
            &[
                registry_entry_with_deps(
                    "root",
                    "1.0.0",
                    &["shared 1.0.0 (registry+https://github.com/rust-lang/crates.io-index)"],
                ),
                registry_entry("shared", "1.0.0"),
                path_entry("shared", "1.0.0"),
            ]
            .concat(),
        );
        let registry_shared = packages
            .iter()
            .find(|p| p.name == "shared" && p.source.is_some())
            .unwrap();

        assert_eq!(
            find(&packages, "root").dependencies[0].source,
            registry_shared.source
        );
    }

    #[test]
    fn path_outside_working_directory_is_rejected() {
        let outside = tempdir().unwrap();
        let working_dir = tempdir().unwrap();
        write_lockfile(outside.path(), &registry_entry("serde", "1.0.0"));

        let result = load(&outside.path().join("Cargo.lock"), working_dir.path());

        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escaping_working_directory_is_rejected() {
        let outside = tempdir().unwrap();
        let working_dir = tempdir().unwrap();
        write_lockfile(outside.path(), &registry_entry("serde", "1.0.0"));

        let link_path = working_dir.path().join("Cargo.lock");
        std::os::unix::fs::symlink(outside.path().join("Cargo.lock"), &link_path).unwrap();

        let result = load(Path::new("Cargo.lock"), working_dir.path());

        assert!(result.is_err());
    }

    #[test]
    fn directory_given_where_a_file_is_expected_is_rejected() {
        let working_dir = tempdir().unwrap();
        std::fs::create_dir(working_dir.path().join("Cargo.lock")).unwrap();

        let result = load(Path::new("Cargo.lock"), working_dir.path());

        assert!(result.is_err());
    }
}

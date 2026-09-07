use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Resolves a possibly-relative lockfile path against `working_dir`, without
/// touching the filesystem. Shared by `load` (which then canonicalizes and
/// validates it) and by callers that just need the lockfile's directory.
pub fn resolve_path(path: &Path, working_dir: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        working_dir.join(path)
    }
}

/// A name/version pair identifying a package, used both for lockfile entries
/// and for the dependency edges between them.
pub struct PackageRef {
    pub name: String,
    pub version: String,
}

/// An entry from `Cargo.lock`. Includes path and git packages (not just
/// crates.io ones) so that workspace members can appear as dependents in the
/// requirement graph; `is_registry` tells callers which entries are eligible
/// for the age check itself.
pub struct Package {
    pub name: String,
    pub version: String,
    pub is_registry: bool,
    pub dependencies: Vec<PackageRef>,
}

/// Loads every package recorded in a lockfile, registry and non-registry
/// alike.
///
/// `path` is the lockfile path as given by the caller (relative or
/// absolute), resolved against `working_dir` if relative. Rejects anything
/// that is not a regular file within `working_dir` (`..` traversal and
/// symlink escapes included).
///
/// `cargo_lock` resolves each dependency edge to a concrete version itself
/// (lockfiles may omit a dependency's version when only one instance of it
/// exists), so every `PackageRef` here already carries one.
pub fn load(path: &Path, working_dir: &Path) -> Result<Vec<Package>> {
    let resolved = resolve_path(path, working_dir);

    // Canonicalize to resolve symlinks and ".." components
    // (file must exist for canonicalize to succeed)
    let canonical = resolved.canonicalize().context(format!(
        "Cargo.lock path does not exist or is not accessible: {}",
        path.display()
    ))?;

    // Ensure it's a regular file
    if !canonical.is_file() {
        anyhow::bail!("Cargo.lock path is not a regular file: {}", path.display());
    }

    // Ensure the resolved path is within the working directory
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
            dependencies: p
                .dependencies
                .iter()
                .map(|d| PackageRef {
                    name: d.name.as_str().to_string(),
                    version: d.version.to_string(),
                })
                .collect(),
        })
        .collect();

    Ok(packages)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    const LOCKFILE_HEADER: &str = "version = 4\n";

    fn registry_entry(name: &str, version: &str) -> String {
        format!(
            r#"
[[package]]
name = "{name}"
version = "{version}"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "0000000000000000000000000000000000000000000000000000000000000000"
"#
        )
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

    fn write_lockfile(dir: &Path, contents: &str) -> std::path::PathBuf {
        let path = dir.join("Cargo.lock");
        std::fs::write(&path, format!("{LOCKFILE_HEADER}{contents}")).unwrap();
        path
    }

    #[test]
    fn non_registry_packages_are_kept_with_the_flag_set() {
        let dir = tempdir().unwrap();
        let contents = format!(
            "{}{}{}",
            registry_entry("serde", "1.0.0"),
            git_entry("rand", "0.8.0"),
            path_entry("local-crate", "0.1.0"),
        );
        write_lockfile(dir.path(), &contents);

        let packages = load(Path::new("Cargo.lock"), dir.path()).unwrap();

        assert_eq!(packages.len(), 3);
        let serde = packages.iter().find(|p| p.name == "serde").unwrap();
        assert!(serde.is_registry);
        let rand = packages.iter().find(|p| p.name == "rand").unwrap();
        assert!(!rand.is_registry);
        let local = packages.iter().find(|p| p.name == "local-crate").unwrap();
        assert!(!local.is_registry);
    }

    #[test]
    fn dependency_with_explicit_version_resolves() {
        let dir = tempdir().unwrap();
        let contents = format!(
            "{}{}",
            registry_entry_with_deps("a", "1.0.0", &["b 2.0.0"]),
            registry_entry("b", "2.0.0"),
        );
        write_lockfile(dir.path(), &contents);

        let packages = load(Path::new("Cargo.lock"), dir.path()).unwrap();
        let a = packages.iter().find(|p| p.name == "a").unwrap();
        assert_eq!(a.dependencies.len(), 1);
        assert_eq!(a.dependencies[0].name, "b");
        assert_eq!(a.dependencies[0].version, "2.0.0");
    }

    #[test]
    fn dependency_with_omitted_version_resolves_by_name() {
        let dir = tempdir().unwrap();
        let contents = format!(
            "{}{}",
            registry_entry_with_deps("a", "1.0.0", &["b"]),
            registry_entry("b", "2.0.0"),
        );
        write_lockfile(dir.path(), &contents);

        let packages = load(Path::new("Cargo.lock"), dir.path()).unwrap();
        let a = packages.iter().find(|p| p.name == "a").unwrap();
        assert_eq!(a.dependencies.len(), 1);
        assert_eq!(a.dependencies[0].name, "b");
        assert_eq!(a.dependencies[0].version, "2.0.0");
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

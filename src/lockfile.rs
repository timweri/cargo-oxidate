use anyhow::{Context, Result};
use std::path::Path;

/// A dependency from a lockfile, checked against the crates.io registry.
pub struct Package {
    pub name: String,
    pub version: String,
}

/// Loads the crates.io registry packages from a lockfile.
///
/// `path` is the lockfile path as given by the caller (relative or
/// absolute), resolved against `working_dir` if relative. Rejects anything
/// that is not a regular file within `working_dir` (`..` traversal and
/// symlink escapes included), then keeps only packages sourced from the
/// default registry, since path and git dependencies aren't on crates.io.
pub fn load(path: &Path, working_dir: &Path) -> Result<Vec<Package>> {
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        working_dir.join(path)
    };

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
        .filter(|p| {
            // Only check packages from crates.io registry
            p.source.as_ref().is_some_and(|s| s.is_default_registry())
        })
        .map(|p| Package {
            name: p.name.as_str().to_string(),
            version: p.version.to_string(),
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
    fn only_crates_io_registry_packages_are_kept() {
        let dir = tempdir().unwrap();
        let contents = format!(
            "{}{}{}",
            registry_entry("serde", "1.0.0"),
            git_entry("rand", "0.8.0"),
            path_entry("local-crate", "0.1.0"),
        );
        write_lockfile(dir.path(), &contents);

        let packages = load(Path::new("Cargo.lock"), dir.path()).unwrap();

        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "serde");
        assert_eq!(packages[0].version, "1.0.0");
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

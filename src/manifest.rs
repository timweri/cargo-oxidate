use anyhow::Result;
use semver::VersionReq;
use std::collections::HashMap;
use std::path::Path;

/// Parses a `Cargo.toml` and returns a map from dependency name to `VersionReq`.
///
/// Known limitations (deps falling into these cases are absent from the map and
/// will be reported as transitive by `--suggest-upgrade`):
/// - Workspace-inherited deps (`foo = { workspace = true }`) - no version string
///   to extract; workspace root is not consulted.
/// - `[dev-dependencies]`, `[build-dependencies]`, and
///   `[target.'cfg(...)'.dependencies]` are not parsed; only `[dependencies]`.
/// - Workspace root manifests with only a `[workspace]` table return empty.
/// - Renamed packages (`foo = { package = "real-foo", version = "..." }`) are
///   keyed by `foo` here but appear as `real-foo` in the lockfile, so lookups
///   miss.
pub fn parse_version_requirements(cargo_toml_path: &Path) -> HashMap<String, VersionReq> {
    match parse_version_requirements_impl(cargo_toml_path) {
        Ok(map) => map,
        Err(e) => {
            eprintln!(
                "\nWarning: failed to parse {}: {e}",
                cargo_toml_path.display()
            );
            eprintln!("  All dependencies will be treated as transitive.\n");
            HashMap::new()
        }
    }
}

fn parse_version_requirements_impl(cargo_toml_path: &Path) -> Result<HashMap<String, VersionReq>> {
    let content = std::fs::read_to_string(cargo_toml_path)?;
    let manifest: toml::Value = toml::from_str(&content)?;

    let mut requirements = HashMap::new();

    if let Some(deps) = manifest.get("dependencies").and_then(|v| v.as_table()) {
        for (name, value) in deps {
            if let Some(version_str) = extract_version_string(value) {
                if let Ok(req) = VersionReq::parse(version_str) {
                    requirements.insert(name.clone(), req);
                }
            }
        }
    }

    Ok(requirements)
}

/// Extracts the `version` string from a dependency value. Returns `None` for
/// `{ workspace = true }` and `{ git = ... }`/`{ path = ... }` entries; does
/// not resolve the `package = "..."` rename key.
fn extract_version_string(value: &toml::Value) -> Option<&str> {
    match value {
        // Simple string: serde = "1.0"
        toml::Value::String(s) => Some(s.as_str()),
        // Table with version key: serde = { version = "1.0", features = [...] }
        toml::Value::Table(t) => t.get("version").and_then(|v| v.as_str()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_parse_simple_string_versions() {
        let manifest = r#"
[dependencies]
serde = "1.0"
anyhow = "1"
"#;
        let temp = NamedTempFile::new().unwrap();
        std::fs::write(temp.path(), manifest).unwrap();

        let reqs = parse_version_requirements(temp.path());
        assert_eq!(reqs.len(), 2);
        assert!(
            reqs.get("serde")
                .unwrap()
                .matches(&"1.0.200".parse().unwrap())
        );
        assert!(
            reqs.get("anyhow")
                .unwrap()
                .matches(&"1.0.100".parse().unwrap())
        );
    }

    #[test]
    fn test_parse_table_versions() {
        let manifest = r#"
[dependencies]
serde = { version = "1.0", features = ["derive"] }
clap = { version = "4", default-features = false }
"#;
        let temp = NamedTempFile::new().unwrap();
        std::fs::write(temp.path(), manifest).unwrap();

        let reqs = parse_version_requirements(temp.path());
        assert_eq!(reqs.len(), 2);
        assert!(reqs.get("serde").is_some());
        assert!(reqs.get("clap").is_some());
    }

    #[test]
    fn test_missing_file_returns_empty() {
        let reqs = parse_version_requirements(Path::new("/nonexistent/Cargo.toml"));
        assert_eq!(reqs.len(), 0);
    }

    #[test]
    fn test_invalid_toml_returns_empty() {
        let temp = NamedTempFile::new().unwrap();
        std::fs::write(temp.path(), "not valid toml [[[").unwrap();

        let reqs = parse_version_requirements(temp.path());
        assert_eq!(reqs.len(), 0);
    }

    #[test]
    fn test_no_dependencies_section() {
        let manifest = r#"
[package]
name = "test"
version = "0.1.0"
"#;
        let temp = NamedTempFile::new().unwrap();
        std::fs::write(temp.path(), manifest).unwrap();

        let reqs = parse_version_requirements(temp.path());
        assert_eq!(reqs.len(), 0);
    }
}

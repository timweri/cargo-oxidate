use anyhow::{Context, Result};
use std::path::Path;

pub struct Package {
    pub name: String,
    pub version: String,
}

pub fn parse_lockfile(path: &Path) -> Result<Vec<Package>> {
    let lockfile = cargo_lock::Lockfile::load(path)
        .context(format!("Could not load lockfile at {}", path.display()))?;

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

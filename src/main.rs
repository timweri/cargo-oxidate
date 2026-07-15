use anyhow::{Context, Result};
use clap::Parser;
use serde::Deserialize;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

mod api;
mod cache;
mod report;
mod suggest;

#[derive(Parser, Debug)]
#[command(
    name = "cargo-oxidate",
    version,
    about = "Check Cargo dependency freshness",
    after_help = "By default, packages whose publish date cannot be determined are treated as violations. Use --exclude-missing to suppress them."
)]
struct Cli {
    /// Path to the Cargo.lock file
    #[arg(default_value = "Cargo.lock")]
    cargo_lock: PathBuf,

    /// Minimum age in days - packages newer than this are flagged (supply chain security)
    #[arg(long)]
    min_age_days: Option<u64>,

    /// Maximum age in days - packages older than this are flagged (staleness)
    #[arg(long)]
    max_age_days: Option<u64>,

    /// Comma-separated list of package names to exempt from checks
    #[arg(long, value_delimiter = ',')]
    exempt: Vec<String>,

    /// Exclude packages whose publish date cannot be determined from violations (by default they are included)
    #[arg(long)]
    exclude_missing: bool,

    /// HTTP request timeout in seconds
    #[arg(long, default_value_t = 10)]
    timeout: u64,

    /// Suggest an iterative Cargo-validated plan for "too new" violations
    #[arg(long)]
    suggest_fix: bool,

    /// Path to the response cache file (enables caching)
    #[arg(long, env = "CARGO_OXIDATE_CACHE_PATH")]
    cache_path: Option<PathBuf>,

    /// Maximum age in hours for cached all-versions responses
    #[arg(long, default_value_t = 24)]
    cache_max_age_hours: u64,
}

struct Package {
    name: String,
    version: String,
}

fn parse_lockfile(path: &Path) -> Result<Vec<Package>> {
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

fn main() -> ExitCode {
    // Filter out the "oxidate" subcommand name that cargo passes when invoked as `cargo oxidate`
    let args: Vec<String> = std::env::args()
        .enumerate()
        .filter(|(i, arg)| !(*i == 1 && arg == "oxidate"))
        .map(|(_, arg)| arg)
        .collect();
    let cli = Cli::parse_from(args);

    match run(cli) {
        Ok(has_violations) => {
            if has_violations {
                ExitCode::from(1)
            } else {
                ExitCode::from(0)
            }
        }
        Err(e) => {
            eprintln!("Error: {e:#}");
            ExitCode::from(2)
        }
    }
}

/// Checks one package's publish date and produces any violations it triggers.
/// Logs a warning to stderr when the fetch errors out.
fn check_package(
    client: &mut api::CratesIoClient,
    pkg: &Package,
    min_age_days: Option<u64>,
    max_age_days: Option<u64>,
    exclude_missing: bool,
) -> Vec<report::Violation> {
    let mut violations = Vec::new();
    let result = client.fetch_publish_date_with_retry(&pkg.name, &pkg.version);

    match result {
        Ok(Some(published)) => {
            let age_days = (chrono::Utc::now() - published).num_days();

            if let Some(min) = min_age_days
                && age_days < min as i64
            {
                violations.push(report::Violation {
                    package: pkg.name.clone(),
                    version: pkg.version.clone(),
                    kind: report::ViolationKind::TooNew,
                    age_days,
                    published: Some(published),
                });
            }

            if let Some(max) = max_age_days
                && age_days > max as i64
            {
                violations.push(report::Violation {
                    package: pkg.name.clone(),
                    version: pkg.version.clone(),
                    kind: report::ViolationKind::TooOld,
                    age_days,
                    published: Some(published),
                });
            }
        }
        Ok(None) | Err(_) => {
            if let Err(ref e) = result {
                let severity = match e {
                    api::FetchError::Retryable(_) => "transient",
                    api::FetchError::Permanent(_) => "permanent",
                };
                eprintln!(
                    "\n  Warning: {severity} error checking {}@{}: {e}",
                    pkg.name, pkg.version
                );
            }
            if !exclude_missing {
                violations.push(report::Violation {
                    package: pkg.name.clone(),
                    version: pkg.version.clone(),
                    kind: report::ViolationKind::Unknown,
                    age_days: 0,
                    published: None,
                });
            }
        }
    }

    violations
}

#[derive(Deserialize)]
struct CargoMetadata {
    packages: Vec<MetadataPackage>,
}

#[derive(Deserialize)]
struct MetadataPackage {
    manifest_path: PathBuf,
    dependencies: Vec<MetadataDependency>,
}

#[derive(Deserialize)]
struct MetadataDependency {
    name: String,
    rename: Option<String>,
    kind: Option<String>,
    target: Option<String>,
}

#[derive(Clone)]
struct DirectDependency {
    manifest_path: PathBuf,
    package: String,
    rename: Option<String>,
    kind: Option<String>,
    target: Option<String>,
    workspace_inherited: bool,
}

struct FixPlan {
    commands: Vec<String>,
    blocker: Option<String>,
}

trait FixPlanBackend {
    fn violations(&mut self, lockfile: &Path) -> Result<Vec<report::Violation>>;
    fn versions(
        &mut self,
        package: &str,
    ) -> Result<Vec<api::CrateVersionInfo>, api::FetchError>;
    fn update(
        &mut self,
        lockfile: &Path,
        package: &str,
        current_version: &str,
        candidate_version: &str,
    ) -> Result<Option<String>>;
    fn direct_dependency(&self, package: &str) -> Result<Option<DirectDependency>>;
}

struct CargoFixPlanBackend<'client, 'manifest, 'exempt> {
    client: &'client mut api::CratesIoClient,
    manifest_path: &'manifest Path,
    metadata: CargoMetadata,
    min_age_days: u64,
    max_age_days: Option<u64>,
    exclude_missing: bool,
    exempt: &'exempt HashSet<&'exempt str>,
}

impl FixPlanBackend for CargoFixPlanBackend<'_, '_, '_> {
    fn violations(&mut self, lockfile: &Path) -> Result<Vec<report::Violation>> {
        let packages = parse_lockfile(lockfile)?;
        Ok(collect_violations(
            self.client,
            &packages,
            Some(self.min_age_days),
            self.max_age_days,
            self.exclude_missing,
            self.exempt,
        ))
    }

    fn versions(&mut self, package: &str) -> Result<Vec<api::CrateVersionInfo>, api::FetchError> {
        self.client.fetch_all_versions_with_retry(package)
    }

    fn update(
        &mut self,
        lockfile: &Path,
        package: &str,
        current_version: &str,
        candidate_version: &str,
    ) -> Result<Option<String>> {
        cargo_update_temp_lock(
            self.manifest_path,
            lockfile,
            package,
            current_version,
            candidate_version,
        )
    }

    fn direct_dependency(&self, package: &str) -> Result<Option<DirectDependency>> {
        find_direct_dependency(&self.metadata, package)
    }
}

fn cargo_version_supports_isolated_lockfile(version: &str) -> bool {
    let mut parts = version.split('.');
    let major = parts.next().and_then(|part| part.parse::<u64>().ok());
    let minor = parts.next().and_then(|part| part.parse::<u64>().ok());
    matches!((major, minor), (Some(major), Some(minor)) if major > 1 || (major == 1 && minor >= 97))
}

fn cargo_supports_isolated_lockfile() -> Result<bool> {
    let output = Command::new("cargo")
        .arg("--version")
        .output()
        .context("Failed to run cargo --version")?;
    let version = String::from_utf8_lossy(&output.stdout);
    let Some(version) = version.split_whitespace().nth(1) else {
        return Ok(false);
    };
    Ok(cargo_version_supports_isolated_lockfile(version))
}

fn cargo_error(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .find(|line| line.trim_start().starts_with("error:"))
        .unwrap_or("Cargo rejected this version")
        .trim()
        .to_string()
}

fn cargo_update_temp_lock(
    manifest_path: &Path,
    temporary_lockfile: &Path,
    package: &str,
    current_version: &str,
    candidate_version: &str,
) -> Result<Option<String>> {
    let lockfile = temporary_lockfile
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    let config = format!("resolver.lockfile-path=\"{lockfile}\"");
    let output = Command::new("cargo")
        .arg("--config")
        .arg(config)
        .arg("update")
        .arg("--manifest-path")
        .arg(manifest_path)
        .args(["-p", &format!("{package}@{current_version}"), "--precise"])
        .arg(candidate_version)
        .args(["--color", "never"])
        .output()
        .context("Failed to run cargo update")?;

    if output.status.success() {
        return Ok(None);
    }

    Ok(Some(cargo_error(&output.stderr)))
}

fn load_metadata(manifest_path: &Path) -> Result<CargoMetadata> {
    let output = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps", "--manifest-path"])
        .arg(manifest_path)
        .output()
        .context("Failed to run cargo metadata")?;
    if !output.status.success() {
        anyhow::bail!("cargo metadata failed: {}", cargo_error(&output.stderr));
    }
    serde_json::from_slice(&output.stdout).context("Failed to parse cargo metadata output")
}

fn dependency_uses_workspace(manifest_path: &Path, dependency: &str) -> Result<bool> {
    let document: toml::Value = fs::read_to_string(manifest_path)
        .with_context(|| format!("Failed to read {}", manifest_path.display()))?
        .parse()
        .with_context(|| format!("Failed to parse {}", manifest_path.display()))?;
    let uses_workspace = |table: Option<&toml::value::Table>| {
        table
            .and_then(|table| table.get(dependency))
            .and_then(toml::Value::as_table)
            .and_then(|entry| entry.get("workspace"))
            .and_then(toml::Value::as_bool)
            .unwrap_or(false)
    };

    if ["dependencies", "dev-dependencies", "build-dependencies"]
        .iter()
        .any(|name| uses_workspace(document.get(name).and_then(toml::Value::as_table)))
    {
        return Ok(true);
    }

    Ok(document
        .get("target")
        .and_then(toml::Value::as_table)
        .is_some_and(|targets| {
            targets.values().any(|target| {
                let Some(target) = target.as_table() else {
                    return false;
                };
                ["dependencies", "dev-dependencies", "build-dependencies"]
                    .iter()
                    .any(|name| uses_workspace(target.get(*name).and_then(toml::Value::as_table)))
            })
        }))
}

fn find_direct_dependency(
    metadata: &CargoMetadata,
    package: &str,
) -> Result<Option<DirectDependency>> {
    for member in &metadata.packages {
        for dependency in &member.dependencies {
            if dependency.name != package {
                continue;
            }
            let dependency_name = dependency.rename.as_deref().unwrap_or(&dependency.name);
            return Ok(Some(DirectDependency {
                manifest_path: member.manifest_path.clone(),
                package: dependency.name.clone(),
                rename: dependency.rename.clone(),
                kind: dependency.kind.clone(),
                target: dependency.target.clone(),
                workspace_inherited: dependency_uses_workspace(
                    &member.manifest_path,
                    dependency_name,
                )?,
            }));
        }
    }
    Ok(None)
}

fn cargo_add_command(dependency: &DirectDependency, version: &str) -> String {
    let mut command = format!(
        "cargo add --manifest-path '{}' {}@={}",
        dependency.manifest_path.display(),
        dependency.package,
        version
    );
    if let Some(rename) = &dependency.rename {
        command.push_str(&format!(" --rename {rename}"));
    }
    match dependency.kind.as_deref() {
        Some("dev") => command.push_str(" --dev"),
        Some("build") => command.push_str(" --build"),
        _ => {}
    }
    if let Some(target) = &dependency.target {
        command.push_str(&format!(" --target '{target}'"));
    }
    command
}

fn collect_violations(
    client: &mut api::CratesIoClient,
    packages: &[Package],
    min_age_days: Option<u64>,
    max_age_days: Option<u64>,
    exclude_missing: bool,
    exempt: &HashSet<&str>,
) -> Vec<report::Violation> {
    let mut violations = Vec::new();
    for package in packages {
        if !exempt.contains(package.name.as_str()) {
            violations.extend(check_package(
                client,
                package,
                min_age_days,
                max_age_days,
                exclude_missing,
            ));
        }
        client.rate_limit();
    }
    violations
}

fn build_fix_plan(
    client: &mut api::CratesIoClient,
    cargo_lock_path: &Path,
    manifest_path: &Path,
    min_age_days: u64,
    max_age_days: Option<u64>,
    exclude_missing: bool,
    exempt: &HashSet<&str>,
) -> Result<FixPlan> {
    let temporary = tempfile::tempdir().context("Failed to create a temporary lockfile")?;
    let temporary_lockfile = temporary.path().join("Cargo.lock");
    fs::copy(cargo_lock_path, &temporary_lockfile).context("Failed to copy Cargo.lock")?;
    let metadata = load_metadata(manifest_path)?;
    let mut backend = CargoFixPlanBackend {
        client,
        manifest_path,
        metadata,
        min_age_days,
        max_age_days,
        exclude_missing,
        exempt,
    };
    build_fix_plan_with(
        &mut backend,
        &temporary_lockfile,
        manifest_path,
        min_age_days,
        max_age_days,
    )
}

fn build_fix_plan_with(
    backend: &mut impl FixPlanBackend,
    temporary_lockfile: &Path,
    manifest_path: &Path,
    min_age_days: u64,
    max_age_days: Option<u64>,
) -> Result<FixPlan> {
    let mut commands = Vec::new();
    let mut attempted = HashSet::new();

    loop {
        let violations = backend.violations(temporary_lockfile)?;
        let mut updated = false;
        let mut blockers = Vec::new();

        for violation in violations
            .iter()
            .filter(|violation| matches!(violation.kind, report::ViolationKind::TooNew))
        {
            let versions = match backend.versions(&violation.package) {
                Ok(versions) => versions,
                Err(error) => {
                    blockers.push(format!(
                        "Could not fetch versions for {}: {error}",
                        violation.package
                    ));
                    continue;
                }
            };
            let candidates = suggest::find_compliant_versions(
                &versions,
                &violation.version,
                min_age_days,
                max_age_days,
            );
            let first_candidate = candidates.first().map(|(version, _)| version.clone());
            let mut last_rejection = None;

            for (candidate, _) in candidates {
                let attempt = format!("{}@{}->{candidate}", violation.package, violation.version);
                if !attempted.insert(attempt) {
                    continue;
                }
                match backend.update(
                    temporary_lockfile,
                    &violation.package,
                    &violation.version,
                    &candidate,
                )? {
                    None => {
                        commands.push(format!(
                            "cargo update --manifest-path '{}' -p {}@{} --precise {}",
                            manifest_path.display(),
                            violation.package,
                            violation.version,
                            candidate
                        ));
                        updated = true;
                        break;
                    }
                    Some(reason) => last_rejection = Some(reason),
                }
            }

            if updated {
                break;
            }

            if let Some(candidate) = first_candidate
                && let Some(dependency) = backend.direct_dependency(&violation.package)?
            {
                if dependency.workspace_inherited {
                    return Ok(FixPlan {
                        commands,
                        blocker: Some(format!(
                            "{} is inherited from [workspace.dependencies]; update the shared requirement manually, then rerun cargo oxidate.",
                            violation.package
                        )),
                    });
                }
                commands.push(cargo_add_command(&dependency, &candidate));
                return Ok(FixPlan {
                    commands,
                    blocker: Some(
                        "Apply the cargo add command above, then rerun cargo oxidate to plan the remaining fixes."
                            .to_string(),
                    ),
                });
            }

            blockers.push(format!(
                "No compatible downgrade for {}@{}{}",
                violation.package,
                violation.version,
                last_rejection
                    .map(|reason| format!(": {reason}"))
                    .unwrap_or_default()
            ));
        }

        if !updated {
            return Ok(FixPlan {
                commands,
                blocker: (!blockers.is_empty()).then(|| blockers.join("\n")),
            });
        }
    }
}

fn run(cli: Cli) -> Result<bool> {
    // Validate that at least one threshold is set
    if cli.min_age_days.is_none() && cli.max_age_days.is_none() {
        anyhow::bail!("At least one of --min-age-days or --max-age-days must be specified");
    }

    // Validate that --suggest-fix requires --min-age-days
    if cli.suggest_fix && cli.min_age_days.is_none() {
        anyhow::bail!("--suggest-fix requires --min-age-days to be specified");
    }

    // Validate cargo-lock path
    let cargo_lock_path = {
        let path = &cli.cargo_lock;

        // Resolve the path to catch traversal
        let resolved = if path.is_absolute() {
            path.clone()
        } else {
            std::env::current_dir()
                .context("Failed to get current directory")?
                .join(path)
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

        // Ensure the resolved path is within the current working directory
        let cwd = std::env::current_dir()
            .context("Failed to get current directory")?
            .canonicalize()
            .context("Failed to canonicalize current directory")?;

        if !canonical.starts_with(&cwd) {
            anyhow::bail!(
                "Cargo.lock path escapes the working directory: {}",
                path.display()
            );
        }

        canonical
    };

    let manifest_path = cargo_lock_path
        .parent()
        .context("Cargo.lock path has no parent directory")?
        .join("Cargo.toml");
    if cli.suggest_fix && !manifest_path.is_file() {
        anyhow::bail!(
            "--suggest-fix requires Cargo.toml next to {}",
            cargo_lock_path.display()
        );
    }
    if cli.suggest_fix && !cargo_supports_isolated_lockfile()? {
        anyhow::bail!("--suggest-fix requires Cargo 1.97 or newer");
    }

    // Parse lockfile
    let packages = parse_lockfile(&cargo_lock_path).context("Failed to parse Cargo.lock")?;

    // Build API client
    let mut client = api::CratesIoClient::new(
        cli.timeout,
        cli.cache_path.as_deref(),
        cli.cache_max_age_hours,
    )?;

    // Check each package
    let mut violations = Vec::new();
    let exempt_set: HashSet<&str> = cli.exempt.iter().map(|s| s.trim()).collect();

    let total = packages.len();
    for (i, pkg) in packages.iter().enumerate() {
        if exempt_set.contains(pkg.name.as_str()) {
            continue;
        }

        eprintln!(
            "  Checking [{}/{}] {}@{}",
            i + 1,
            total,
            pkg.name,
            pkg.version
        );

        violations.extend(check_package(
            &mut client,
            pkg,
            cli.min_age_days,
            cli.max_age_days,
            cli.exclude_missing,
        ));

        client.rate_limit();
    }

    // Print report
    report::print_report(&violations);

    // Generate an iterative fix plan if requested
    if cli.suggest_fix {
        if violations
            .iter()
            .any(|violation| matches!(violation.kind, report::ViolationKind::TooNew))
        {
            eprintln!("\nBuilding an iterative Cargo-validated fix plan...");
            let plan = build_fix_plan(
                &mut client,
                &cargo_lock_path,
                &manifest_path,
                cli.min_age_days.unwrap(),
                cli.max_age_days,
                cli.exclude_missing,
                &exempt_set,
            )?;
            report::print_fix_plan(&plan.commands, plan.blocker.as_deref());
        }
    }

    client.save_cache();

    Ok(!violations.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, VecDeque};

    struct FakeFixPlanBackend {
        violation_rounds: VecDeque<Vec<report::Violation>>,
        versions: HashMap<String, Vec<api::CrateVersionInfo>>,
        fetch_failures: HashSet<String>,
        updates: VecDeque<Option<String>>,
        update_calls: Vec<(String, String, String)>,
        dependencies: HashMap<String, DirectDependency>,
    }

    impl FixPlanBackend for FakeFixPlanBackend {
        fn violations(&mut self, _: &Path) -> Result<Vec<report::Violation>> {
            Ok(self.violation_rounds.pop_front().unwrap_or_default())
        }

        fn versions(
            &mut self,
            package: &str,
        ) -> Result<Vec<api::CrateVersionInfo>, api::FetchError> {
            if self.fetch_failures.contains(package) {
                return Err(api::FetchError::Permanent("fixture fetch failure".to_string()));
            }
            Ok(self.versions.get(package).cloned().unwrap_or_default())
        }

        fn update(
            &mut self,
            _: &Path,
            package: &str,
            current_version: &str,
            candidate_version: &str,
        ) -> Result<Option<String>> {
            self.update_calls.push((
                package.to_string(),
                current_version.to_string(),
                candidate_version.to_string(),
            ));
            Ok(self.updates.pop_front().unwrap_or(None))
        }

        fn direct_dependency(&self, package: &str) -> Result<Option<DirectDependency>> {
            Ok(self.dependencies.get(package).cloned())
        }
    }

    fn too_new(package: &str, version: &str) -> report::Violation {
        report::Violation {
            package: package.to_string(),
            version: version.to_string(),
            kind: report::ViolationKind::TooNew,
            age_days: 1,
            published: None,
        }
    }

    fn version(number: &str, age_days: i64) -> api::CrateVersionInfo {
        api::CrateVersionInfo {
            num: number.to_string(),
            created_at: chrono::Utc::now() - chrono::Duration::days(age_days),
            yanked: false,
        }
    }

    fn direct_dependency(package: &str, workspace_inherited: bool) -> DirectDependency {
        DirectDependency {
            manifest_path: PathBuf::from("member/Cargo.toml"),
            package: package.to_string(),
            rename: None,
            kind: None,
            target: None,
            workspace_inherited,
        }
    }

    fn backend(rounds: Vec<Vec<report::Violation>>) -> FakeFixPlanBackend {
        FakeFixPlanBackend {
            violation_rounds: rounds.into(),
            versions: HashMap::new(),
            fetch_failures: HashSet::new(),
            updates: VecDeque::new(),
            update_calls: Vec::new(),
            dependencies: HashMap::new(),
        }
    }

    #[test]
    fn detects_workspace_inherited_dependency() {
        let directory = tempfile::tempdir().unwrap();
        let manifest = directory.path().join("Cargo.toml");
        fs::write(
            &manifest,
            r#"
[package]
name = "fixture"
version = "0.1.0"

[dependencies]
serde = { workspace = true }
"#,
        )
        .unwrap();

        assert!(dependency_uses_workspace(&manifest, "serde").unwrap());
    }

    #[test]
    fn detects_target_workspace_inherited_dependency() {
        let directory = tempfile::tempdir().unwrap();
        let manifest = directory.path().join("Cargo.toml");
        fs::write(
            &manifest,
            r#"
[package]
name = "fixture"
version = "0.1.0"

[target.'cfg(unix)'.build-dependencies]
cc = { workspace = true }
"#,
        )
        .unwrap();

        assert!(dependency_uses_workspace(&manifest, "cc").unwrap());
    }

    #[test]
    fn formats_direct_dependency_command() {
        let dependency = DirectDependency {
            manifest_path: PathBuf::from("member/Cargo.toml"),
            package: "serde".to_string(),
            rename: Some("serde_json_alias".to_string()),
            kind: Some("dev".to_string()),
            target: Some("cfg(unix)".to_string()),
            workspace_inherited: false,
        };

        assert_eq!(
            cargo_add_command(&dependency, "1.0.0"),
            "cargo add --manifest-path 'member/Cargo.toml' serde@=1.0.0 --rename serde_json_alias --dev --target 'cfg(unix)'"
        );
    }

    #[test]
    fn formats_build_dependency_command() {
        let mut dependency = direct_dependency("cc", false);
        dependency.kind = Some("build".to_string());

        assert_eq!(
            cargo_add_command(&dependency, "1.2.3"),
            "cargo add --manifest-path 'member/Cargo.toml' cc@=1.2.3 --build"
        );
    }

    #[test]
    fn recognizes_supported_cargo_versions() {
        assert!(!cargo_version_supports_isolated_lockfile("1.96.0"));
        assert!(cargo_version_supports_isolated_lockfile("1.97.0"));
        assert!(cargo_version_supports_isolated_lockfile("2.0.0"));
        assert!(!cargo_version_supports_isolated_lockfile("not-a-version"));
    }

    #[test]
    fn plans_multiple_updates_in_order() {
        let mut fake = backend(vec![
            vec![too_new("alpha", "2.0.0"), too_new("beta", "3.0.0")],
            vec![too_new("beta", "3.0.0")],
            vec![],
        ]);
        fake.versions.insert("alpha".to_string(), vec![version("1.5.0", 30)]);
        fake.versions.insert("beta".to_string(), vec![version("2.5.0", 30)]);
        fake.updates.extend([None, None]);

        let plan = build_fix_plan_with(
            &mut fake,
            Path::new("Cargo.lock"),
            Path::new("subproject/Cargo.toml"),
            7,
            None,
        )
        .unwrap();

        assert_eq!(
            plan.commands,
            [
                "cargo update --manifest-path 'subproject/Cargo.toml' -p alpha@2.0.0 --precise 1.5.0",
                "cargo update --manifest-path 'subproject/Cargo.toml' -p beta@3.0.0 --precise 2.5.0",
            ]
        );
        assert_eq!(plan.blocker, None);
        assert_eq!(
            fake.update_calls,
            [
                ("alpha".to_string(), "2.0.0".to_string(), "1.5.0".to_string()),
                ("beta".to_string(), "3.0.0".to_string(), "2.5.0".to_string()),
            ]
        );
    }

    #[test]
    fn falls_back_after_cargo_rejects_a_candidate() {
        let mut fake = backend(vec![vec![too_new("alpha", "2.0.0")], vec![]]);
        fake.versions.insert(
            "alpha".to_string(),
            vec![version("1.5.0", 30), version("1.4.0", 40)],
        );
        fake.updates.extend([Some("version conflict".to_string()), None]);

        let plan = build_fix_plan_with(
            &mut fake,
            Path::new("Cargo.lock"),
            Path::new("Cargo.toml"),
            7,
            None,
        )
        .unwrap();

        assert_eq!(
            plan.commands,
            ["cargo update --manifest-path 'Cargo.toml' -p alpha@2.0.0 --precise 1.4.0"]
        );
        assert_eq!(
            fake.update_calls,
            [
                ("alpha".to_string(), "2.0.0".to_string(), "1.5.0".to_string()),
                ("alpha".to_string(), "2.0.0".to_string(), "1.4.0".to_string()),
            ]
        );
    }

    #[test]
    fn plans_direct_dependency_change_after_rejected_candidates() {
        let mut fake = backend(vec![vec![too_new("alpha", "2.0.0")]]);
        fake.versions.insert("alpha".to_string(), vec![version("1.5.0", 30)]);
        fake.updates.push_back(Some("version conflict".to_string()));
        fake.dependencies
            .insert("alpha".to_string(), direct_dependency("alpha", false));

        let plan = build_fix_plan_with(
            &mut fake,
            Path::new("Cargo.lock"),
            Path::new("Cargo.toml"),
            7,
            None,
        )
        .unwrap();

        assert_eq!(
            plan.commands,
            ["cargo add --manifest-path 'member/Cargo.toml' alpha@=1.5.0"]
        );
        assert_eq!(
            plan.blocker.as_deref(),
            Some("Apply the cargo add command above, then rerun cargo oxidate to plan the remaining fixes.")
        );
    }

    #[test]
    fn reports_workspace_dependency_without_cargo_add() {
        let mut fake = backend(vec![vec![too_new("alpha", "2.0.0")]]);
        fake.versions.insert("alpha".to_string(), vec![version("1.5.0", 30)]);
        fake.updates.push_back(Some("version conflict".to_string()));
        fake.dependencies
            .insert("alpha".to_string(), direct_dependency("alpha", true));

        let plan = build_fix_plan_with(
            &mut fake,
            Path::new("Cargo.lock"),
            Path::new("Cargo.toml"),
            7,
            None,
        )
        .unwrap();

        assert!(plan.commands.is_empty());
        assert!(plan
            .blocker
            .as_deref()
            .unwrap()
            .contains("inherited from [workspace.dependencies]"));
    }

    #[test]
    fn reports_fetch_failure_and_missing_candidates_without_looping() {
        let mut fake = backend(vec![vec![too_new("alpha", "2.0.0"), too_new("beta", "2.0.0")]]);
        fake.fetch_failures.insert("alpha".to_string());
        fake.versions.insert("beta".to_string(), vec![version("2.0.0", 30)]);

        let plan = build_fix_plan_with(
            &mut fake,
            Path::new("Cargo.lock"),
            Path::new("Cargo.toml"),
            7,
            None,
        )
        .unwrap();

        assert!(plan.commands.is_empty());
        let blocker = plan.blocker.unwrap();
        assert!(blocker.contains("Could not fetch versions for alpha"));
        assert!(blocker.contains("No compatible downgrade for beta@2.0.0"));
        assert!(fake.update_calls.is_empty());
    }

    #[test]
    fn reports_cargo_rejection_when_no_direct_dependency_can_change() {
        let mut fake = backend(vec![vec![too_new("alpha", "2.0.0")]]);
        fake.versions
            .insert("alpha".to_string(), vec![version("1.5.0", 30)]);
        fake.updates.push_back(Some("version conflict".to_string()));

        let plan = build_fix_plan_with(
            &mut fake,
            Path::new("Cargo.lock"),
            Path::new("Cargo.toml"),
            7,
            None,
        )
        .unwrap();

        assert!(plan.commands.is_empty());
        assert_eq!(
            plan.blocker.as_deref(),
            Some("No compatible downgrade for alpha@2.0.0: version conflict")
        );
    }
}

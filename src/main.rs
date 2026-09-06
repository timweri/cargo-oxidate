use anyhow::{Context, Result};
use clap::Parser;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

mod api;
mod cache;
mod policy;
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

    /// For "too new" violations, suggest cargo update commands to downgrade to compliant versions
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
    freshness_policy: &policy::FreshnessPolicy,
    pkg: &Package,
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<report::Violation> {
    let result = client.fetch_publish_date(&pkg.name, &pkg.version);

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

    freshness_policy.evaluate(pkg, &result, now)
}

fn run(cli: Cli) -> Result<bool> {
    let freshness_policy = policy::FreshnessPolicy::new(
        cli.min_age_days,
        cli.max_age_days,
        cli.exclude_missing,
        cli.exempt,
    )?;

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
    let now = chrono::Utc::now();

    let total = packages.len();
    for (i, pkg) in packages.iter().enumerate() {
        if freshness_policy.is_exempt(&pkg.name) {
            continue;
        }

        eprintln!(
            "  Checking [{}/{}] {}@{}",
            i + 1,
            total,
            pkg.name,
            pkg.version
        );

        violations.extend(check_package(&mut client, &freshness_policy, pkg, now));
    }

    // Print report
    report::print_report(&violations);

    // Generate suggestions if requested
    if cli.suggest_fix {
        // Safe to unwrap: validated at start of run()
        let min_age = freshness_policy.min_age_days().unwrap();
        let mut suggestions = Vec::new();
        let too_new_violations: Vec<_> = violations
            .iter()
            .filter(|v| matches!(v.kind, report::ViolationKind::TooNew { .. }))
            .collect();

        if !too_new_violations.is_empty() {
            eprintln!("\nFetching version suggestions...");
            for (i, violation) in too_new_violations.iter().enumerate() {
                eprintln!(
                    "  [{}/{}] {}",
                    i + 1,
                    too_new_violations.len(),
                    violation.package
                );

                let result = client.fetch_all_versions(&violation.package);

                match result {
                    Ok(versions) => {
                        if let Some((suggested_version, age_days)) =
                            suggest::find_compliant_version(&versions, min_age)
                        {
                            suggestions.push(suggest::Suggestion {
                                package: violation.package.clone(),
                                suggested_version,
                                suggested_age_days: age_days,
                            });
                        }
                    }
                    Err(e) => {
                        eprintln!(
                            "\n  Warning: failed to fetch versions for {}: {e}",
                            violation.package
                        );
                    }
                }
            }

            report::print_suggestions(&suggestions);
        }
    }

    client.finish();

    Ok(!violations.is_empty())
}

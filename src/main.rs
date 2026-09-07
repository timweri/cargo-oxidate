use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use std::process::ExitCode;

mod api;
mod cache;
mod lockfile;
mod manifest;
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
    #[arg(long, requires = "min_age_days")]
    suggest_fix: bool,

    /// Consider prerelease versions as suggestion candidates (requires --suggest-fix). Under
    /// semver, a requirement matches a prerelease only when it names the identical
    /// major.minor.patch with a prerelease part of its own, so this flag usually changes nothing
    /// unless a dependent already tracks that exact prerelease line or the locked version is
    /// itself a prerelease — an empty result with the flag set is expected, not a bug.
    #[arg(long, requires = "suggest_fix")]
    include_prerelease: bool,

    /// Path to the response cache file (enables caching)
    #[arg(long, env = "CARGO_OXIDATE_CACHE_PATH")]
    cache_path: Option<PathBuf>,

    /// Maximum age in hours for cached all-versions responses
    #[arg(long, default_value_t = 24)]
    cache_max_age_hours: u64,
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
    pkg: &lockfile::Package,
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

    freshness_policy.evaluate(pkg, result.ok().flatten(), now)
}

fn run(cli: Cli) -> Result<bool> {
    let freshness_policy = policy::FreshnessPolicy::new(
        cli.min_age_days,
        cli.max_age_days,
        cli.exclude_missing,
        cli.exempt,
    )?;

    let suggest_min_age = cli.suggest_fix.then_some(cli.min_age_days).flatten();

    let working_dir = std::env::current_dir().context("Failed to get current directory")?;

    // Parse lockfile
    let packages = lockfile::load(&cli.cargo_lock, &working_dir)?;

    // Build API client
    let mut client = api::CratesIoClient::new(
        cli.timeout,
        cli.cache_path.as_deref(),
        cli.cache_max_age_hours,
    )?;

    // Check each package
    let mut violations = Vec::new();
    let now = chrono::Utc::now();

    let registry_packages: Vec<&lockfile::Package> =
        packages.iter().filter(|p| p.is_registry).collect();

    let total = registry_packages.len();
    for (i, pkg) in registry_packages.iter().enumerate() {
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
    if let Some(min_age) = suggest_min_age {
        let cargo_lock_path = lockfile::resolve_path(&cli.cargo_lock, &working_dir);
        let lockfile_dir = cargo_lock_path.parent().unwrap_or(&working_dir);

        let (direct_requirements, manifest_warnings) =
            manifest::load_direct_requirements(lockfile_dir);
        for warning in &manifest_warnings {
            eprintln!("  Warning: {warning}");
        }

        if let Some(outcomes) = suggest::generate_suggestions(
            &mut client,
            &violations,
            &packages,
            &direct_requirements,
            lockfile_dir,
            min_age,
            cli.include_prerelease,
            now,
        ) {
            report::print_suggestions(&outcomes);
        }
    }

    client.finish();

    Ok(!violations.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggest_fix_without_min_age_days_fails_to_parse() {
        let result =
            Cli::try_parse_from(["cargo-oxidate", "--suggest-fix", "--max-age-days", "30"]);
        assert!(result.is_err());
    }

    #[test]
    fn suggest_fix_with_min_age_days_parses() {
        let result = Cli::try_parse_from(["cargo-oxidate", "--suggest-fix", "--min-age-days", "7"]);
        assert!(result.is_ok());
    }

    #[test]
    fn include_prerelease_without_suggest_fix_fails_to_parse() {
        let result = Cli::try_parse_from([
            "cargo-oxidate",
            "--include-prerelease",
            "--min-age-days",
            "7",
        ]);
        assert!(result.is_err());
    }

    #[test]
    fn include_prerelease_with_suggest_fix_parses() {
        let result = Cli::try_parse_from([
            "cargo-oxidate",
            "--suggest-fix",
            "--min-age-days",
            "7",
            "--include-prerelease",
        ]);
        assert!(result.is_ok());
    }
}

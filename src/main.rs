use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use std::process::ExitCode;

mod api;
mod lockfile;
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

    // Parse lockfile
    let packages =
        lockfile::parse_lockfile(&cargo_lock_path).context("Failed to parse Cargo.lock")?;

    // Build API client
    let client = api::CratesIoClient::new(cli.timeout)?;

    // Check each package
    let mut violations = Vec::new();
    let exempt_set: std::collections::HashSet<&str> = cli.exempt.iter().map(|s| s.trim()).collect();

    let total = packages.len();
    for (i, pkg) in packages.iter().enumerate() {
        if exempt_set.contains(pkg.name.as_str()) {
            continue;
        }

        eprint!(
            "\r  Checking [{}/{}] {}@{}",
            i + 1,
            total,
            pkg.name,
            pkg.version
        );

        let result = client.fetch_publish_date_with_retry(&pkg.name, &pkg.version);

        match result {
            Ok(Some(published)) => {
                let age_days = (chrono::Utc::now() - published).num_days();

                if let Some(min) = cli.min_age_days
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

                if let Some(max) = cli.max_age_days
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
                if !cli.exclude_missing {
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

        // Rate limit: 100ms between requests
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    eprintln!(); // Clear progress line

    // Print report
    report::print_report(&violations);

    // Generate suggestions if requested
    if cli.suggest_fix {
        // Safe to unwrap: validated at start of run()
        let min_age = cli.min_age_days.unwrap();
        let mut suggestions = Vec::new();
        let too_new_violations: Vec<_> = violations
            .iter()
            .filter(|v| matches!(v.kind, report::ViolationKind::TooNew))
            .collect();

        if !too_new_violations.is_empty() {
            eprintln!("\nFetching version suggestions...");
            for (i, violation) in too_new_violations.iter().enumerate() {
                eprint!(
                    "\r  [{}/{}] {}",
                    i + 1,
                    too_new_violations.len(),
                    violation.package
                );

                match client.fetch_all_versions_with_retry(&violation.package) {
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

                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            eprintln!(); // Clear progress line

            report::print_suggestions(&suggestions);
        }
    }

    Ok(!violations.is_empty())
}

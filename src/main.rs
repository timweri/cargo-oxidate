use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use std::process::ExitCode;

mod api;
mod lockfile;
mod report;

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
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
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

async fn run() -> Result<bool> {
    let cli = Cli::parse();

    // Validate that at least one threshold is set
    if cli.min_age_days.is_none() && cli.max_age_days.is_none() {
        anyhow::bail!("At least one of --min-age-days or --max-age-days must be specified");
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
            anyhow::bail!(
                "Cargo.lock path is not a regular file: {}",
                path.display()
            );
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
    let packages = lockfile::parse_lockfile(&cargo_lock_path)
        .context("Failed to parse Cargo.lock")?;

    // Build API client
    let client = api::CratesIoClient::new(cli.timeout)?;

    // Check each package
    let mut violations = Vec::new();
    let exempt_set: std::collections::HashSet<&str> =
        cli.exempt.iter().map(|s| s.trim()).collect();

    let total = packages.len();
    for (i, pkg) in packages.iter().enumerate() {
        if exempt_set.contains(pkg.name.as_str()) {
            continue;
        }

        eprint!("\r  Checking [{}/{}] {}@{}", i + 1, total, pkg.name, pkg.version);

        let result = client.fetch_publish_date_with_retry(&pkg.name, &pkg.version).await;

        match result {
            Ok(Some(published)) => {
                let age_days = (chrono::Utc::now() - published).num_days();

                if let Some(min) = cli.min_age_days && age_days < min as i64 {
                    violations.push(report::Violation {
                        package: pkg.name.clone(),
                        version: pkg.version.clone(),
                        kind: report::ViolationKind::TooNew,
                        age_days,
                        published: Some(published),
                    });
                }

                if let Some(max) = cli.max_age_days && age_days > max as i64 {
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
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    eprintln!(); // Clear progress line

    // Print report
    report::print_report(&violations);

    Ok(!violations.is_empty())
}

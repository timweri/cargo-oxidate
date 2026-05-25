use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use std::process::ExitCode;

mod api;
mod lockfile;
mod report;

#[derive(Parser, Debug)]
#[command(name = "cargo-oxidate", version, about = "Check Cargo dependency freshness")]
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

    /// Flag packages whose publish date cannot be determined
    #[arg(long, default_value_t = true)]
    include_missing: bool,

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

    // Parse lockfile
    let packages = lockfile::parse_lockfile(&cli.cargo_lock)
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

        match client.fetch_publish_date(&pkg.name, &pkg.version).await {
            Ok(Some(published)) => {
                let age_days = (chrono::Utc::now() - published).num_days();

                if let Some(min) = cli.min_age_days && age_days < min as i64 {
                    violations.push(report::Violation {
                        package: pkg.name.clone(),
                        version: pkg.version.clone(),
                        kind: report::ViolationKind::TooNew,
                        age_days,
                        threshold: min,
                        published: Some(published),
                    });
                }

                if let Some(max) = cli.max_age_days && age_days > max as i64 {
                    violations.push(report::Violation {
                        package: pkg.name.clone(),
                        version: pkg.version.clone(),
                        kind: report::ViolationKind::TooOld,
                        age_days,
                        threshold: max,
                        published: Some(published),
                    });
                }
            }
            Ok(None) => {
                if cli.include_missing {
                    violations.push(report::Violation {
                        package: pkg.name.clone(),
                        version: pkg.version.clone(),
                        kind: report::ViolationKind::Unknown,
                        age_days: 0,
                        threshold: 0,
                        published: None,
                    });
                }
            }
            Err(e) => {
                eprintln!("\n  Warning: failed to check {}@{}: {}", pkg.name, pkg.version, e);
                if cli.include_missing {
                    violations.push(report::Violation {
                        package: pkg.name.clone(),
                        version: pkg.version.clone(),
                        kind: report::ViolationKind::Unknown,
                        age_days: 0,
                        threshold: 0,
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

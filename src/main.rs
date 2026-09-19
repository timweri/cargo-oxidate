use clap::{Parser, ValueEnum};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

mod api;
mod cache;
mod lockfile;
mod manifest;
mod policy;
mod report;
mod suggest;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verbosity {
    Quiet,
    Normal,
    Verbose,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
enum OutputFormat {
    #[default]
    Text,
    Json,
}

impl Verbosity {
    fn shows_start(self) -> bool {
        !matches!(self, Self::Quiet)
    }

    fn shows_progress(self) -> bool {
        matches!(self, Self::Verbose)
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "cargo-oxidate",
    version,
    about = "Check Cargo dependency freshness",
    after_help = "By default, confirmed missing publish dates are reported as violations. Use --exclude-missing to exclude them; lookup failures remain errors."
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

    /// Exclude confirmed missing publish dates from violations
    #[arg(long)]
    exclude_missing: bool,

    /// HTTP request timeout in seconds
    #[arg(long, default_value_t = 10)]
    timeout: u64,

    /// For "too new" violations, suggest cargo update commands to downgrade to compliant versions
    #[arg(long, requires = "min_age_days")]
    suggest_fix: bool,

    /// Consider prerelease versions as suggestion candidates (requires --suggest-fix)
    ///
    /// This only admits prerelease versions as candidates; it does not change
    /// requirement matching. Ordinary SemVer requirements (e.g. `^1.2`) still
    /// generally do not match prereleases, so most will still be rejected.
    #[arg(long, requires = "suggest_fix")]
    include_prerelease: bool,

    /// Path to the response cache file (enables caching)
    #[arg(long, env = "CARGO_OXIDATE_CACHE_PATH")]
    cache_path: Option<PathBuf>,

    /// Maximum age in hours for cached all-versions responses
    #[arg(long, default_value_t = 24)]
    cache_max_age_hours: u64,

    /// Suppress start and progress messages
    #[arg(long, conflicts_with = "verbose")]
    quiet: bool,

    /// Show each package as it is checked
    #[arg(long, conflicts_with = "quiet")]
    verbose: bool,

    /// Render the final result as text or JSON
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    format: OutputFormat,
}

impl Cli {
    fn verbosity(&self) -> Verbosity {
        if self.quiet {
            Verbosity::Quiet
        } else if self.verbose {
            Verbosity::Verbose
        } else {
            Verbosity::Normal
        }
    }
}

fn print_start(lockfile: &std::path::Path, policy: &report::Policy) {
    let minimum_age = policy
        .min_age_days
        .map_or_else(|| "not set".to_string(), |days| format!("{days} days"));
    let maximum_age = policy
        .max_age_days
        .map_or_else(|| "not set".to_string(), |days| format!("{days} days"));
    eprintln!(
        "Checking dependency ages in {} (minimum age: {minimum_age}; maximum age: {maximum_age}).",
        lockfile.display()
    );
}

fn failed_report(
    lockfile: PathBuf,
    policy: report::Policy,
    message: impl Into<String>,
    started: Instant,
) -> report::RunReport {
    let message = message.into();
    eprintln!("error: {message}");
    let mut report = report::RunReport::failed(lockfile, policy, message);
    report.duration = started.elapsed();
    report
}

fn setup_failure_report(
    lockfile: PathBuf,
    policy: report::Policy,
    freshness_policy: &policy::FreshnessPolicy,
    packages: &[lockfile::Package],
    message: impl Into<String>,
    started: Instant,
) -> report::RunReport {
    let message = message.into();
    eprintln!("error: {message}");
    let mut report = report::RunReport::completed(
        lockfile,
        policy,
        report::Summary {
            total_packages: packages.len(),
            ..Default::default()
        },
    );
    let summary = report
        .summary
        .as_mut()
        .expect("completed reports have coverage");
    for package in packages {
        if !package.is_registry {
            summary.unsupported_packages += 1;
        } else if freshness_policy.is_exempt(&package.name) {
            summary.exempt_packages += 1;
        } else {
            summary.not_checked_packages += 1;
        }
    }
    report.required_errors.push(report::Diagnostic {
        category: "input",
        package: None,
        version: None,
        path: None,
        message,
        retryable: false,
    });
    report.duration = started.elapsed();
    report
}

fn record_cache_warnings(
    report: &mut report::RunReport,
    warnings: impl IntoIterator<Item = cache::CacheWarning>,
) {
    for warning in warnings {
        report.warnings.push(report::Diagnostic {
            category: "cache",
            package: None,
            version: None,
            path: Some(warning.path),
            message: warning.message,
            retryable: false,
        });
    }
}

fn main() -> ExitCode {
    // Filter out the "oxidate" subcommand name that cargo passes when invoked as `cargo oxidate`
    let args: Vec<String> = std::env::args()
        .enumerate()
        .filter(|(i, arg)| !(*i == 1 && arg == "oxidate"))
        .map(|(_, arg)| arg)
        .collect();
    let cli = Cli::parse_from(args);

    let verbosity = cli.verbosity();
    let suggestions_requested = cli.suggest_fix;
    let output_format = cli.format;
    let mut report = run(cli, verbosity);
    if suggestions_requested && report.suggestions.is_none() {
        report.suggestions = Some(Vec::new());
    }
    let exit_code = report.exit_code();
    match output_format {
        OutputFormat::Text => report::print_report(&report),
        OutputFormat::Json => match report::render_json(&report) {
            Ok(json) => println!("{json}"),
            Err(error) => {
                eprintln!("error: Could not render JSON report: {error}");
                return ExitCode::from(2);
            }
        },
    }
    ExitCode::from(exit_code)
}

/// Checks all lockfile entries through a client. The generic transport keeps
/// required-check behaviour testable without a public test-only CLI switch.
fn check_packages<T: api::Transport>(
    client: &mut api::CratesIoClient<T>,
    freshness_policy: &policy::FreshnessPolicy,
    packages: &[lockfile::Package],
    supplied_lockfile: PathBuf,
    now: chrono::DateTime<chrono::Utc>,
    mut on_check: Option<&mut dyn FnMut(&lockfile::Package)>,
) -> report::RunReport {
    let started = Instant::now();
    let mut report = report::RunReport::completed(
        supplied_lockfile,
        freshness_policy.report_policy(),
        report::Summary {
            total_packages: packages.len(),
            ..Default::default()
        },
    );

    for pkg in packages {
        let summary = report
            .summary
            .as_mut()
            .expect("completed reports have coverage");
        if !pkg.is_registry {
            summary.unsupported_packages += 1;
            continue;
        }
        if freshness_policy.is_exempt(&pkg.name) {
            summary.exempt_packages += 1;
            continue;
        }

        if let Some(on_check) = on_check.as_mut() {
            on_check(pkg);
        }

        match client.fetch_publish_date(&pkg.name, &pkg.version) {
            Ok(Some(published)) => {
                summary.checked_packages += 1;
                report
                    .violations
                    .extend(freshness_policy.evaluate(pkg, Some(published), now));
            }
            Ok(None) if freshness_policy.excludes_missing() => {
                summary.excluded_missing_packages += 1;
            }
            Ok(None) => {
                summary.checked_packages += 1;
                report
                    .violations
                    .extend(freshness_policy.evaluate(pkg, None, now));
            }
            Err(error) => {
                eprintln!(
                    "error: Could not check {}@{}: {error}",
                    pkg.name, pkg.version
                );
                summary.failed_packages += 1;
                report.required_errors.push(report::Diagnostic {
                    category: "lookup",
                    package: Some(pkg.name.clone()),
                    version: Some(pkg.version.clone()),
                    path: None,
                    retryable: matches!(error, api::FetchError::Retryable(_)),
                    message: error.to_string(),
                });
            }
        }
    }

    report.duration = started.elapsed();
    report
}

fn run(cli: Cli, verbosity: Verbosity) -> report::RunReport {
    let started = Instant::now();
    let supplied_lockfile = cli.cargo_lock.clone();
    let report_policy = report::Policy::new(
        cli.min_age_days,
        cli.max_age_days,
        cli.exclude_missing,
        cli.exempt.clone(),
    );
    let freshness_policy = policy::FreshnessPolicy::new(
        cli.min_age_days,
        cli.max_age_days,
        cli.exclude_missing,
        cli.exempt,
    );
    let freshness_policy = match freshness_policy {
        Ok(policy) => policy,
        Err(error) => {
            return failed_report(supplied_lockfile, report_policy, error.to_string(), started);
        }
    };

    let suggest_min_age = cli.suggest_fix.then_some(cli.min_age_days).flatten();

    let working_dir = match std::env::current_dir() {
        Ok(directory) => directory,
        Err(error) => {
            return failed_report(
                supplied_lockfile,
                report_policy,
                format!("Failed to get current directory: {error}"),
                started,
            );
        }
    };

    // Parse lockfile
    let loaded_lockfile = match lockfile::load(&cli.cargo_lock, &working_dir) {
        Ok(lockfile) => lockfile,
        Err(error) => {
            return failed_report(
                supplied_lockfile,
                report_policy,
                format!("{error:#}"),
                started,
            );
        }
    };
    let packages = loaded_lockfile.packages;

    // Build API client
    let mut client = match api::CratesIoClient::new(
        cli.timeout,
        cli.cache_path.as_deref(),
        cli.cache_max_age_hours,
    ) {
        Ok(client) => client,
        Err(error) => {
            return setup_failure_report(
                supplied_lockfile,
                report_policy,
                &freshness_policy,
                &packages,
                error.to_string(),
                started,
            );
        }
    };
    let initial_cache_warnings = client.take_cache_warnings();
    for warning in &initial_cache_warnings {
        eprintln!("warning: {}: {}", warning.path.display(), warning.message);
    }

    if verbosity.shows_start() {
        print_start(&loaded_lockfile.path, &freshness_policy.report_policy());
    }

    let now = chrono::Utc::now();
    let mut print_check_progress = |pkg: &lockfile::Package| {
        if verbosity.shows_progress() {
            eprintln!("Checking {}@{}", pkg.name, pkg.version);
        }
    };
    let mut report = check_packages(
        &mut client,
        &freshness_policy,
        &packages,
        supplied_lockfile,
        now,
        Some(&mut print_check_progress),
    );
    record_cache_warnings(&mut report, initial_cache_warnings);

    // Generate suggestions if requested
    let has_too_new = report
        .violations
        .iter()
        .any(|violation| matches!(violation.kind, report::ViolationKind::TooNew(_)));
    if let Some(min_age) = suggest_min_age
        && has_too_new
    {
        let lockfile_dir = loaded_lockfile.path.parent().unwrap_or(&working_dir);

        let (direct_requirements, manifest_warnings) =
            manifest::load_direct_requirements(lockfile_dir);
        for warning in manifest_warnings {
            eprintln!("warning: {warning}");
            report.warnings.push(report::Diagnostic {
                category: "manifest",
                package: None,
                version: None,
                path: None,
                message: warning,
                retryable: false,
            });
        }

        let mut print_suggestion_progress = |progress: suggest::SuggestionProgress| {
            if verbosity.shows_progress() {
                eprintln!(
                    "Checking suggestion [{}/{}] {}",
                    progress.current, progress.total, progress.package
                );
            }
        };
        let outcomes = suggest::generate_suggestions(
            &mut client,
            &report.violations,
            &packages,
            &direct_requirements,
            lockfile_dir,
            min_age,
            cli.include_prerelease,
            now,
            &mut print_suggestion_progress,
        )
        .unwrap_or_default();
        for outcome in &outcomes {
            if let suggest::Outcome::Unavailable {
                package,
                locked_version,
                reason,
            } = outcome
            {
                let message = format!(
                    "Could not determine a downgrade for {package}@{locked_version}: {reason}"
                );
                eprintln!("warning: {message}");
                report.warnings.push(report::Diagnostic {
                    category: "suggestion",
                    package: Some(package.clone()),
                    version: Some(locked_version.clone()),
                    path: None,
                    message,
                    retryable: false,
                });
            }
        }
        report.suggestions = Some(outcomes);
    }

    if let Some(warning) = client.finish() {
        eprintln!("warning: {}: {}", warning.path.display(), warning.message);
        record_cache_warnings(&mut report, [warning]);
    }
    report.duration = started.elapsed();

    report
}

#[cfg(test)]
#[path = "main/tests.rs"]
mod tests;

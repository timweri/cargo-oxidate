use chrono::{DateTime, Utc};
use std::path::{Path, PathBuf};
use std::time::Duration;

mod json;
pub use json::render_json;

/// A package's publish date and its resulting age, shared by both
/// age-threshold violation kinds.
pub struct Aged {
    pub published: DateTime<Utc>,
    pub age_days: i64,
}

pub enum ViolationKind {
    TooNew(Aged),
    TooOld(Aged),
    MissingPublishDate { reason: String },
}

pub struct Violation {
    pub package: String,
    pub version: String,
    pub kind: ViolationKind,
}

/// The effective age policy used for a run. This is data rather than CLI
/// arguments so every renderer reports the same policy after normalization.
pub struct Policy {
    pub min_age_days: Option<u64>,
    pub max_age_days: Option<u64>,
    pub exclude_missing: bool,
    pub exempt: Vec<String>,
}

impl Policy {
    pub fn new(
        min_age_days: Option<u64>,
        max_age_days: Option<u64>,
        exclude_missing: bool,
        mut exempt: Vec<String>,
    ) -> Self {
        exempt
            .iter_mut()
            .for_each(|name| *name = name.trim().to_string());
        exempt.retain(|name| !name.is_empty());
        exempt.sort();
        exempt.dedup();
        Self {
            min_age_days,
            max_age_days,
            exclude_missing,
            exempt,
        }
    }
}

/// Disjoint coverage counts for lockfile package entries.
#[derive(Default)]
pub struct Summary {
    pub total_packages: usize,
    pub checked_packages: usize,
    pub exempt_packages: usize,
    pub unsupported_packages: usize,
    pub excluded_missing_packages: usize,
    pub failed_packages: usize,
    pub not_checked_packages: usize,
}

pub struct Diagnostic {
    pub category: &'static str,
    pub package: Option<String>,
    pub version: Option<String>,
    pub path: Option<PathBuf>,
    pub message: String,
    pub retryable: bool,
}

/// The complete outcome of one invocation after argument parsing. It is the
/// single source for the text report today and structured output later.
pub struct RunReport {
    /// The path supplied by the caller, retained even when resolution fails.
    pub lockfile: PathBuf,
    pub policy: Policy,
    pub summary: Option<Summary>,
    pub violations: Vec<Violation>,
    pub required_errors: Vec<Diagnostic>,
    pub warnings: Vec<Diagnostic>,
    /// None when suggestions were not requested; otherwise every requested
    /// investigation outcome is present, including an empty list.
    pub suggestions: Option<Vec<crate::suggest::Outcome>>,
    pub duration: Duration,
}

impl RunReport {
    pub fn completed(lockfile: PathBuf, policy: Policy, summary: Summary) -> Self {
        Self {
            lockfile,
            policy,
            summary: Some(summary),
            violations: Vec::new(),
            required_errors: Vec::new(),
            warnings: Vec::new(),
            suggestions: None,
            duration: Duration::ZERO,
        }
    }

    pub fn failed(lockfile: PathBuf, policy: Policy, message: impl Into<String>) -> Self {
        Self {
            lockfile,
            policy,
            summary: None,
            violations: Vec::new(),
            required_errors: vec![Diagnostic {
                category: "input",
                package: None,
                version: None,
                path: None,
                message: message.into(),
                retryable: false,
            }],
            warnings: Vec::new(),
            suggestions: None,
            duration: Duration::ZERO,
        }
    }

    pub fn exit_code(&self) -> u8 {
        if !self.required_errors.is_empty() {
            2
        } else if !self.violations.is_empty() {
            1
        } else {
            0
        }
    }
}

fn print_policy(policy: &Policy, lockfile: &Path) {
    println!("Dependency age policy");
    println!("  Lockfile: {}", lockfile.display());
    match policy.min_age_days {
        Some(days) => println!("  Minimum age: {days} days"),
        None => println!("  Minimum age: not set"),
    }
    match policy.max_age_days {
        Some(days) => println!("  Maximum age: {days} days"),
        None => println!("  Maximum age: not set"),
    }
    if !policy.exempt.is_empty() {
        println!("  Exempt packages: {}", policy.exempt.join(", "));
    }
    println!(
        "  Missing publish dates: {}",
        if policy.exclude_missing {
            "excluded"
        } else {
            "reported as violations"
        }
    );
}

fn print_summary(summary: &Summary, violations: usize, duration: Duration) {
    println!("Coverage");
    println!(
        "  packages: total={}, checked={}, exempt={}, unsupported={}, excluded missing={}, failed={}, not checked={}",
        summary.total_packages,
        summary.checked_packages,
        summary.exempt_packages,
        summary.unsupported_packages,
        summary.excluded_missing_packages,
        summary.failed_packages,
        summary.not_checked_packages,
    );
    println!("  violations: {violations}");
    println!("  elapsed: {} ms", duration.as_millis());
}

fn print_violations(violations: &[Violation], policy: &Policy) {
    if violations.is_empty() {
        return;
    }

    println!("Dependency age violations");
    for violation in violations {
        match &violation.kind {
            ViolationKind::TooNew(aged) => println!(
                "  {}@{}: Below minimum age {} days (published {}, {} days old)",
                violation.package,
                violation.version,
                policy.min_age_days.unwrap_or_default(),
                aged.published.format("%Y-%m-%d"),
                aged.age_days,
            ),
            ViolationKind::TooOld(aged) => println!(
                "  {}@{}: Above maximum age {} days (published {}, {} days old)",
                violation.package,
                violation.version,
                policy.max_age_days.unwrap_or_default(),
                aged.published.format("%Y-%m-%d"),
                aged.age_days,
            ),
            ViolationKind::MissingPublishDate { reason } => println!(
                "  {}@{}: Publish date unavailable: {reason}",
                violation.package, violation.version
            ),
        }
    }
}

/// Renders a completed or initialization-failed run. Operational messages
/// remain on stderr; this is the final result written to stdout.
pub fn print_report(report: &RunReport) {
    if !report.required_errors.is_empty() {
        println!("Dependency age check incomplete");
    } else if !report.violations.is_empty() {
        println!("Dependency age violations found");
    } else if report
        .summary
        .as_ref()
        .is_some_and(|summary| summary.checked_packages == 0)
    {
        println!("No eligible dependencies checked.");
    } else {
        println!("No dependency age violations found.");
    }
    print_policy(&report.policy, &report.lockfile);

    if let Some(summary) = &report.summary {
        print_summary(summary, report.violations.len(), report.duration);
    } else {
        println!("Coverage unavailable.");
        println!("Elapsed: {} ms", report.duration.as_millis());
    }

    if !report.required_errors.is_empty() {
        for error in &report.required_errors {
            let retryability = if error.retryable { " (retryable)" } else { "" };
            match (&error.package, &error.version) {
                (Some(package), Some(version)) => println!(
                    "  error: could not check {package}@{version}{retryability}: {}",
                    error.message,
                ),
                _ => println!("  error{retryability}: {}", error.message),
            }
        }
    }

    print_violations(&report.violations, &report.policy);

    print_warnings(&report.warnings);
    if let Some(outcomes) = &report.suggestions {
        print_suggestions(outcomes);
    }
}

fn print_warnings(warnings: &[Diagnostic]) {
    for warning in warnings {
        // An unavailable suggestion is already rendered as its own outcome.
        // Keep its diagnostic structured for JSON and stderr without
        // repeating the same message in the text result.
        if warning.category == "suggestion" {
            continue;
        }
        let context = match (&warning.package, &warning.version, &warning.path) {
            (Some(package), Some(version), _) => format!(" for {package}@{version}"),
            (_, _, Some(path)) => format!(" for {}", path.display()),
            _ => String::new(),
        };
        println!(
            "warning: {}{context}: {}",
            warning.category, warning.message
        );
    }
}

pub fn print_suggestions(outcomes: &[crate::suggest::Outcome]) {
    if outcomes.is_empty() {
        return;
    }

    let suggestions: Vec<_> = outcomes
        .iter()
        .filter_map(|outcome| match outcome {
            crate::suggest::Outcome::Suggest {
                package_spec,
                locked_version,
                suggested_version,
                suggested_age_days,
                unverified_dependents,
                ..
            } => Some((
                package_spec,
                locked_version,
                suggested_version,
                suggested_age_days,
                unverified_dependents,
            )),
            _ => None,
        })
        .collect();

    if !suggestions.is_empty() {
        println!("\nSuggested downgrades (apply top to bottom, then re-run):");
        for (
            package_spec,
            locked_version,
            suggested_version,
            suggested_age_days,
            unverified_dependents,
        ) in &suggestions
        {
            let annotation = if unverified_dependents.is_empty() {
                String::new()
            } else {
                format!(
                    " (requirement of {} unverified)",
                    unverified_dependents.join(", ")
                )
            };
            println!(
                "  cargo update -p {package_spec}@{locked_version} --precise {suggested_version} # {suggested_age_days} days old{annotation}"
            );
        }
    }

    let blocked: Vec<_> = outcomes
        .iter()
        .filter(|outcome| !matches!(outcome, crate::suggest::Outcome::Suggest { .. }))
        .collect();
    if !blocked.is_empty() {
        println!("\nOther downgrade outcomes:");
        for outcome in blocked {
            match outcome {
                crate::suggest::Outcome::Blocked {
                    package,
                    locked_version,
                    newest_compliant,
                    blocker,
                } => {
                    let source = match &blocker.version {
                        Some(version) => format!("{} {version}", blocker.name),
                        None => blocker.name.clone(),
                    };
                    let also_suggested = if blocker.also_suggested {
                        format!(
                            " ({source} also has a suggested downgrade above; applying it may unblock this, so re-run to check)"
                        )
                    } else {
                        String::new()
                    };
                    println!(
                        "  Downgrade blocked by dependency requirements for {package}@{locked_version}: newest eligible version is {newest_compliant}, but {source} requires {}{also_suggested}",
                        blocker.req
                    );
                }
                crate::suggest::Outcome::NoCompliantVersion {
                    package,
                    locked_version,
                } => println!(
                    "  No eligible downgrade found for {package}@{locked_version} within its compatible version range"
                ),
                crate::suggest::Outcome::Unavailable {
                    package,
                    locked_version,
                    reason,
                } => println!(
                    "  Could not determine a downgrade for {package}@{locked_version}: {reason}"
                ),
                crate::suggest::Outcome::Suggest { .. } => unreachable!(),
            }
        }
    }

    if !suggestions.is_empty() {
        println!(
            "\nSuggestions satisfy, on a best-effort basis, version requirements verified from Cargo.lock and your manifests. Requirements marked \"unverified\" above were not checked, and Cargo may still reject a suggestion. Source compatibility is not verified: build or test after applying."
        );
    }
}

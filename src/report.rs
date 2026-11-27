use crate::suggest::{BlockerKind, Outcome};
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

/// The normalized age policy used for a run.
#[derive(Clone)]
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

/// Package counts. Each lockfile entry contributes to one field.
#[derive(Default)]
pub struct Summary {
    pub total_packages: usize,
    pub checked_packages: usize,
    pub exempt_packages: usize,
    pub unsupported_packages: usize,
    pub excluded_missing_packages: usize,
    pub failed_packages: usize,
}

pub struct Diagnostic {
    pub category: &'static str,
    pub package: Option<String>,
    pub version: Option<String>,
    pub path: Option<PathBuf>,
    pub message: String,
    pub retryable: bool,
    pub package_spec: Option<String>,
}

impl Diagnostic {
    /// A non-retryable diagnostic with no package, version, or path.
    pub fn new(category: &'static str, message: String) -> Self {
        Self {
            category,
            package: None,
            version: None,
            path: None,
            message,
            retryable: false,
            package_spec: None,
        }
    }
}

/// The complete outcome of one invocation after argument parsing.
pub struct RunReport {
    /// The path supplied by the caller, retained even when resolution fails.
    pub lockfile: PathBuf,
    pub policy: Policy,
    pub summary: Option<Summary>,
    pub violations: Vec<Violation>,
    pub required_errors: Vec<Diagnostic>,
    pub warnings: Vec<Diagnostic>,
    /// None when suggestions were not requested. Otherwise, this contains
    /// every investigation outcome and may be empty.
    pub suggestions: Option<Vec<Outcome>>,
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
            required_errors: vec![Diagnostic::new("input", message.into())],
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
        "  packages: total={}, checked={}, exempt={}, unsupported={}, excluded missing={}, failed={}",
        summary.total_packages,
        summary.checked_packages,
        summary.exempt_packages,
        summary.unsupported_packages,
        summary.excluded_missing_packages,
        summary.failed_packages,
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
                policy
                    .min_age_days
                    .expect("too-new violations require a minimum age"),
                aged.published.format("%Y-%m-%d"),
                aged.age_days,
            ),
            ViolationKind::TooOld(aged) => println!(
                "  {}@{}: Above maximum age {} days (published {}, {} days old)",
                violation.package,
                violation.version,
                policy
                    .max_age_days
                    .expect("too-old violations require a maximum age"),
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

/// Prints the final result to standard output.
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

    print_violations(&report.violations, &report.policy);

    if let Some(outcomes) = &report.suggestions {
        print_suggestions(outcomes);
    }
}

fn print_suggestions(outcomes: &[Outcome]) {
    let suggested: Vec<String> = outcomes.iter().filter_map(suggestion_line).collect();
    let others: Vec<String> = outcomes.iter().filter_map(other_outcome_line).collect();

    if !suggested.is_empty() {
        println!(
            "\nSuggested downgrades. Apply them from top to bottom, then run this check again:"
        );
    }
    for line in &suggested {
        println!("{line}");
    }

    if !others.is_empty() {
        println!("\nOther downgrade outcomes:");
    }
    for line in &others {
        println!("{line}");
    }

    if !suggested.is_empty() {
        println!(
            "\nThese suggestions check the version requirements in Cargo.lock and your manifests. They do not run Cargo's resolver, so Cargo may reject them. The check skipped requirements marked \"unverified\". Build or test after applying each suggestion."
        );
    }
}

fn suggestion_line(outcome: &Outcome) -> Option<String> {
    let Outcome::Suggest {
        package_spec,
        locked_version,
        suggested_version,
        suggested_age_days,
        unverified_dependents,
        ..
    } = outcome
    else {
        return None;
    };
    let annotation = if unverified_dependents.is_empty() {
        String::new()
    } else {
        format!(
            " (requirement of {} unverified)",
            unverified_dependents.join(", ")
        )
    };
    let command = shell_command(&update_argv(
        package_spec,
        locked_version,
        suggested_version,
    ));
    Some(format!(
        "  {command} # {suggested_age_days} days old{annotation}"
    ))
}

/// The `cargo update` argv that applies a suggested downgrade, unquoted.
fn update_argv(package_spec: &str, locked_version: &str, suggested_version: &str) -> Vec<String> {
    vec![
        "cargo".to_string(),
        "update".to_string(),
        "-p".to_string(),
        format!("{package_spec}@{locked_version}"),
        "--precise".to_string(),
        suggested_version.to_string(),
    ]
}

/// Renders an argv as a shell command. An argument with a character outside
/// `[A-Za-z0-9_@%+=:,./-]` is put in double quotes, which sh, cmd.exe and
/// PowerShell accept. The only such argument here is a source-qualified spec,
/// which has nothing those shells expand inside double quotes.
fn shell_command(argv: &[String]) -> String {
    argv.iter()
        .map(|arg| {
            let plain = |c: char| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c);
            if arg.chars().all(plain) {
                arg.clone()
            } else {
                format!("\"{arg}\"")
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn other_outcome_line(outcome: &Outcome) -> Option<String> {
    match outcome {
        Outcome::Suggest { .. } => None,
        Outcome::Blocked {
            package_spec,
            locked_version,
            newest_compliant,
            blocker,
            ..
        } => {
            let (name_and_version, source) = match &blocker.kind {
                BlockerKind::Manifest => (blocker.name.clone(), blocker.name.clone()),
                BlockerKind::Package {
                    version,
                    package_spec,
                } => {
                    let name_and_version = format!("{} {version}", blocker.name);
                    // A bare spec leaves the short form; a qualified one names the source.
                    let source = if *package_spec == blocker.name {
                        name_and_version.clone()
                    } else {
                        format!("{package_spec}@{version}")
                    };
                    (name_and_version, source)
                }
            };
            let also_suggested = if blocker.also_suggested {
                format!(
                    " ({name_and_version} also has a suggested downgrade above; applying it may unblock this, so re-run to check)"
                )
            } else {
                String::new()
            };
            Some(format!(
                "  Downgrade blocked by dependency requirements for {package_spec}@{locked_version}: newest eligible version is {newest_compliant}, but {source} requires {}{also_suggested}",
                blocker.req
            ))
        }
        Outcome::NoCompliantVersion {
            package_spec,
            locked_version,
            ..
        } => Some(format!(
            "  No eligible downgrade found for {package_spec}@{locked_version} within its compatible version range and age limits"
        )),
        Outcome::Unavailable {
            package_spec,
            locked_version,
            reason,
            ..
        } => Some(format!(
            "  Could not determine a downgrade for {package_spec}@{locked_version}: {reason}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suggest::Blocker;

    #[test]
    fn update_argv_is_unquoted_and_shell_command_quotes_only_what_needs_it() {
        let shell = |spec: &str| shell_command(&update_argv(spec, "1.0.28", "1.0.27"));

        assert_eq!(
            update_argv("semver", "1.0.28", "1.0.27"),
            [
                "cargo",
                "update",
                "-p",
                "semver@1.0.28",
                "--precise",
                "1.0.27"
            ]
        );
        assert_eq!(
            shell("semver"),
            "cargo update -p semver@1.0.28 --precise 1.0.27"
        );
        let spec = "registry+https://example.com/index#semver";
        assert_eq!(
            update_argv(spec, "1.0.28", "1.0.27")[3],
            format!("{spec}@1.0.28")
        );
        assert_eq!(
            shell(spec),
            format!("cargo update -p \"{spec}@1.0.28\" --precise 1.0.27")
        );
    }

    #[test]
    fn non_suggest_outcome_lines_name_the_package_by_its_spec() {
        let outcomes = [
            Outcome::Blocked {
                package: "anyhow".into(),
                package_spec: "registry+https://example.com/index#anyhow".into(),
                locked_version: "1.0.102".into(),
                newest_compliant: "1.0.100".into(),
                blocker: Blocker {
                    name: "app".into(),
                    kind: BlockerKind::Manifest,
                    req: "=1.0.102".into(),
                    also_suggested: false,
                },
            },
            Outcome::NoCompliantVersion {
                package: "anyhow".into(),
                package_spec: "registry+https://example.com/index#anyhow".into(),
                locked_version: "1.0.102".into(),
            },
            Outcome::Unavailable {
                package: "anyhow".into(),
                package_spec: "registry+https://example.com/index#anyhow".into(),
                locked_version: "1.0.102".into(),
                reason: "offline".into(),
            },
        ];
        for outcome in &outcomes {
            let line = other_outcome_line(outcome).unwrap();
            assert!(
                line.contains("for registry+https://example.com/index#anyhow@1.0.102"),
                "{line}"
            );
        }
    }

    #[test]
    fn blocker_line_qualifies_the_blocker_only_when_its_spec_is_not_the_bare_name() {
        let spec = "registry+https://example.com/index#clap";
        let tail = |kind: BlockerKind, also_suggested| {
            let line = other_outcome_line(&Outcome::Blocked {
                package: "clap_derive".into(),
                package_spec: "clap_derive".into(),
                locked_version: "4.6.9".into(),
                newest_compliant: "4.6.7".into(),
                blocker: Blocker {
                    name: "clap".into(),
                    kind,
                    req: "=4.6.9".into(),
                    also_suggested,
                },
            })
            .unwrap();
            line.split_once(", but ").unwrap().1.to_string()
        };
        let package = |package_spec: &str| BlockerKind::Package {
            version: "4.6.9".into(),
            package_spec: package_spec.into(),
        };

        assert_eq!(tail(BlockerKind::Manifest, false), "clap requires =4.6.9");
        assert_eq!(tail(package("clap"), false), "clap 4.6.9 requires =4.6.9");
        assert_eq!(
            tail(package(spec), false),
            format!("{spec}@4.6.9 requires =4.6.9")
        );
        // The note keeps the short form rather than repeating the spec.
        assert_eq!(
            tail(package(spec), true),
            format!(
                "{spec}@4.6.9 requires =4.6.9 (clap 4.6.9 also has a suggested downgrade above; applying it may unblock this, so re-run to check)"
            )
        );
    }
}

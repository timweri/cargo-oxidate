use crate::suggest::Outcome;
use chrono::{DateTime, Utc};

/// A package's publish date and its resulting age, shared by both
/// age-threshold violation kinds.
pub struct Aged {
    pub published: DateTime<Utc>,
    pub age_days: i64,
}

pub enum ViolationKind {
    TooNew(Aged),
    TooOld(Aged),
    Unknown,
}

pub struct Violation {
    pub package: String,
    pub version: String,
    pub kind: ViolationKind,
}

fn print_section(header: &str, violations: &[(&Violation, &Aged)]) {
    if violations.is_empty() {
        return;
    }
    println!("  {header}");
    let days_width = violations
        .iter()
        .map(|(_, aged)| aged.age_days.to_string().len())
        .max()
        .unwrap_or(1);
    for (v, aged) in violations {
        let date_str = aged.published.format("%Y-%m-%d").to_string();
        println!(
            "    {} | {:>width$} days old | {} {}",
            date_str,
            aged.age_days,
            v.package,
            v.version,
            width = days_width
        );
    }
    println!();
}

pub fn print_report(violations: &[Violation]) {
    if violations.is_empty() {
        println!("\n✅ All dependencies pass freshness checks.");
        return;
    }

    println!("\n❌ {} dependency violation(s) found:\n", violations.len());

    // Group by kind
    let too_new: Vec<_> = violations
        .iter()
        .filter_map(|v| match &v.kind {
            ViolationKind::TooNew(aged) => Some((v, aged)),
            _ => None,
        })
        .collect();
    let too_old: Vec<_> = violations
        .iter()
        .filter_map(|v| match &v.kind {
            ViolationKind::TooOld(aged) => Some((v, aged)),
            _ => None,
        })
        .collect();
    let unknown: Vec<_> = violations
        .iter()
        .filter(|v| matches!(v.kind, ViolationKind::Unknown))
        .collect();

    print_section(
        "🚨 Too New (younger than threshold - possible supply chain risk):",
        &too_new,
    );
    print_section(
        "⏰ Too Old (older than threshold - consider updating):",
        &too_old,
    );

    if !unknown.is_empty() {
        println!("  ❓ Unknown (publish date could not be determined):");
        for v in &unknown {
            println!(
                "    {:<10} | {:>15} | {} {}",
                "unknown", "--", v.package, v.version
            );
        }
        println!();
    }
}

pub fn print_suggestions(outcomes: &[Outcome]) {
    if outcomes
        .iter()
        .all(|o| !matches!(o, Outcome::Suggest { .. }))
    {
        println!("\n⚠️  No compliant versions found for any \"too new\" violations.");
        println!("    Consider adding these packages to --exempt if they are trusted.\n");
    } else {
        println!(
            "\n💡 Suggested fixes for \"too new\" violations (apply top to bottom, then re-run):\n"
        );

        for outcome in outcomes {
            if let Outcome::Suggest {
                package,
                locked_version,
                suggested_version,
                suggested_age_days,
                unverified_dependents,
            } = outcome
            {
                let annotation = if unverified_dependents.is_empty() {
                    String::new()
                } else {
                    format!(
                        "   (requirement of {} unverified)",
                        unverified_dependents.join(", ")
                    )
                };
                println!(
                    "    cargo update -p {package}@{locked_version} --precise {suggested_version}    # {suggested_age_days} days old{annotation}"
                );
            }
        }
    }

    let blocked: Vec<&Outcome> = outcomes
        .iter()
        .filter(|o| !matches!(o, Outcome::Suggest { .. }))
        .collect();

    if !blocked.is_empty() {
        println!("\n⛔ No compatible compliant version:\n");
        for outcome in blocked {
            match outcome {
                Outcome::Blocked {
                    package,
                    locked_version,
                    newest_compliant,
                    blocker,
                } => {
                    let source = match &blocker.version {
                        Some(v) => format!("{} {v}", blocker.name),
                        None => blocker.name.clone(),
                    };
                    let also_suggested = if blocker.also_suggested {
                        format!(
                            " ({} also has a suggested downgrade above; apply it first and re-run)",
                            blocker.name
                        )
                    } else {
                        String::new()
                    };
                    println!(
                        "    {package} {locked_version}: newest compliant is {newest_compliant}, but {source} requires {}{also_suggested}",
                        blocker.req
                    );
                }
                Outcome::NoCompliantVersion {
                    package,
                    locked_version,
                } => {
                    println!(
                        "    {package} {locked_version}: no version at least the minimum age old within its compatible range"
                    );
                }
                Outcome::Suggest { .. } => unreachable!(),
            }
        }
    }

    println!(
        r#"
  Suggestions satisfy every version requirement in Cargo.lock and your manifests.
  Source compatibility is not verified: build after applying.
"#
    );
}

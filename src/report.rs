use crate::suggest;
use chrono::{DateTime, Utc};

pub enum ViolationKind {
    TooNew {
        published: DateTime<Utc>,
        age_days: i64,
    },
    TooOld {
        published: DateTime<Utc>,
        age_days: i64,
    },
    Unknown,
}

pub struct Violation {
    pub package: String,
    pub version: String,
    pub kind: ViolationKind,
}

fn print_section(header: &str, violations: &[(&Violation, DateTime<Utc>, i64)]) {
    if violations.is_empty() {
        return;
    }
    println!("  {header}");
    let days_width = violations
        .iter()
        .map(|(_, _, age_days)| age_days.to_string().len())
        .max()
        .unwrap_or(1);
    for (v, published, age_days) in violations {
        let date_str = published.format("%Y-%m-%d").to_string();
        println!(
            "    {} | {:>width$} days old | {} {}",
            date_str,
            age_days,
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
        .filter_map(|v| match v.kind {
            ViolationKind::TooNew {
                published,
                age_days,
            } => Some((v, published, age_days)),
            _ => None,
        })
        .collect();
    let too_old: Vec<_> = violations
        .iter()
        .filter_map(|v| match v.kind {
            ViolationKind::TooOld {
                published,
                age_days,
            } => Some((v, published, age_days)),
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

pub fn print_suggestions(suggestions: &[suggest::Suggestion]) {
    if suggestions.is_empty() {
        println!("\n⚠️  No compliant versions found for any \"too new\" violations.");
        println!("    Consider adding these packages to --exempt if they are trusted.\n");
        return;
    }

    println!("\n💡 Suggested fixes for \"too new\" violations:\n");

    for s in suggestions {
        println!(
            "    cargo update -p {} --precise {}    # {} days old",
            s.package, s.suggested_version, s.suggested_age_days
        );
    }

    println!(
        r#"
  Note: These suggestions pick the newest version that satisfies --min-age-days.
  They may not be compatible with your Cargo.toml version requirements.
  For transitive dependencies, run `cargo tree -i <pkg>` to find the parent.
"#
    );
}

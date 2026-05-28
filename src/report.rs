use crate::suggest;
use chrono::{DateTime, Utc};

pub enum ViolationKind {
    TooNew,
    TooOld,
    Unknown,
}

pub struct Violation {
    pub package: String,
    pub version: String,
    pub kind: ViolationKind,
    pub age_days: i64,
    pub published: Option<DateTime<Utc>>,
}

fn print_section(header: &str, violations: &[&Violation]) {
    if violations.is_empty() {
        return;
    }
    println!("  {header}");
    let days_width = violations
        .iter()
        .map(|v| v.age_days.to_string().len())
        .max()
        .unwrap_or(1);
    for v in violations {
        let date_str = v
            .published
            .map(|d| d.format("%Y-%m-%d").to_string())
            .unwrap_or_else(|| "unknown   ".to_string());
        println!(
            "    {} | {:>width$} days old | {} {}",
            date_str,
            v.age_days,
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
        .filter(|v| matches!(v.kind, ViolationKind::TooNew))
        .collect();
    let too_old: Vec<_> = violations
        .iter()
        .filter(|v| matches!(v.kind, ViolationKind::TooOld))
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

pub fn print_upgrade_suggestions(suggestions: &[suggest::UpgradeSuggestion]) {
    if suggestions.is_empty() {
        println!("\n✅ All dependencies are already at their newest compliant versions.");
        return;
    }

    let direct: Vec<_> = suggestions.iter().filter(|s| s.is_direct).collect();
    let transitive: Vec<_> = suggestions.iter().filter(|s| !s.is_direct).collect();

    println!(
        "\n⬆️  {} upgrade(s) available (constrained by --min-age-days):\n",
        suggestions.len()
    );

    if !direct.is_empty() {
        println!("  Direct dependencies:");
        for s in direct {
            println!(
                "    cargo update -p {} --precise {}    # {} -> {}, {} days old",
                s.package,
                s.suggested_version,
                s.current_version,
                s.suggested_version,
                s.suggested_age_days
            );
        }
        println!();
    }

    if !transitive.is_empty() {
        println!("  Transitive dependencies (compatibility not guaranteed):");
        for s in transitive {
            println!(
                "    cargo update -p {} --precise {}    # {} -> {}, {} days old",
                s.package,
                s.suggested_version,
                s.current_version,
                s.suggested_version,
                s.suggested_age_days
            );
        }
        println!();
    }

    println!(
        r#"  Note: Direct dependencies are validated against your Cargo.toml version requirements.
  Transitive dependencies are age-compliant but may not satisfy parent requirements.
  For transitive dependencies, run `cargo tree -i <pkg>` to find the parent.

  Known limitations:
  - Pre-release versions (-alpha, -beta, -rc, etc.) are never suggested.
  - Only [dependencies] in Cargo.toml is parsed. Deps under [dev-dependencies],
    [build-dependencies], [target.'cfg(...)'.dependencies], or inherited via
    {{ workspace = true }} may appear in the Transitive section above even
    though they are direct deps. Renamed deps (package = "...") are also
    misclassified.
  - When the lockfile contains multiple versions of the same crate, the
    `cargo update -p NAME --precise X` lines above are ambiguous. Use
    `cargo update -p NAME@VERSION --precise X` instead.
"#
    );
}

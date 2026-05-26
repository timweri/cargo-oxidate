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
    let days_width = violations.iter()
        .map(|v| v.age_days.to_string().len())
        .max()
        .unwrap_or(1);
    for v in violations {
        let date_str = v.published
            .map(|d| d.format("%Y-%m-%d").to_string())
            .unwrap_or_else(|| "unknown   ".to_string());
        println!(
            "    {} | {:>width$} days old | {} {}",
            date_str, v.age_days, v.package, v.version, width = days_width
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
    let too_new: Vec<_> = violations.iter().filter(|v| matches!(v.kind, ViolationKind::TooNew)).collect();
    let too_old: Vec<_> = violations.iter().filter(|v| matches!(v.kind, ViolationKind::TooOld)).collect();
    let unknown: Vec<_> = violations.iter().filter(|v| matches!(v.kind, ViolationKind::Unknown)).collect();

    print_section("🚨 Too New (younger than threshold - possible supply chain risk):", &too_new);
    print_section("⏰ Too Old (older than threshold - consider updating):", &too_old);

    if !unknown.is_empty() {
        println!("  ❓ Unknown (publish date could not be determined):");
        for v in &unknown {
            println!(
                "    {:<10} | {:>15} | {} {}",
                "unknown",
                "--",
                v.package,
                v.version
            );
        }
        println!();
    }
}

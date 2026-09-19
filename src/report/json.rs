use super::{Diagnostic, RunReport, Summary, Violation, ViolationKind};
use crate::suggest::Outcome;
use chrono::SecondsFormat;
use serde::Serialize;

#[derive(Serialize)]
struct JsonReport {
    schema_version: u8,
    status: &'static str,
    lockfile: String,
    policy: JsonPolicy,
    summary: JsonSummary,
    violations: Vec<JsonViolation>,
    warnings: Vec<JsonDiagnostic>,
    errors: Vec<JsonDiagnostic>,
    suggestions: Option<Vec<JsonSuggestion>>,
}

#[derive(Serialize)]
struct JsonPolicy {
    min_age_days: Option<u64>,
    max_age_days: Option<u64>,
    exclude_missing: bool,
    exempt: Vec<String>,
}

#[derive(Serialize)]
struct JsonSummary {
    total_packages: Option<usize>,
    checked_packages: Option<usize>,
    exempt_packages: Option<usize>,
    unsupported_packages: Option<usize>,
    excluded_missing_packages: Option<usize>,
    failed_packages: Option<usize>,
    not_checked_packages: Option<usize>,
    violations: usize,
    duration_ms: u128,
}

#[derive(Serialize)]
struct JsonViolation {
    package: String,
    version: String,
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    published_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    age_days: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    threshold_days: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

#[derive(Serialize)]
struct JsonDiagnostic {
    category: String,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    package: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    retryable: Option<bool>,
}

#[derive(Serialize)]
struct JsonSuggestion {
    kind: &'static str,
    package: String,
    locked_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    package_spec: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    suggested_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    suggested_age_days: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    unverified_dependents: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    newest_compliant: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    blocker: Option<JsonBlocker>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

#[derive(Serialize)]
struct JsonBlocker {
    name: String,
    version: Option<String>,
    requirement: String,
    also_suggested: bool,
}

impl JsonSummary {
    fn known(summary: &Summary, violations: usize, duration_ms: u128) -> Self {
        Self {
            total_packages: Some(summary.total_packages),
            checked_packages: Some(summary.checked_packages),
            exempt_packages: Some(summary.exempt_packages),
            unsupported_packages: Some(summary.unsupported_packages),
            excluded_missing_packages: Some(summary.excluded_missing_packages),
            failed_packages: Some(summary.failed_packages),
            not_checked_packages: Some(summary.not_checked_packages),
            violations,
            duration_ms,
        }
    }

    fn unavailable(violations: usize, duration_ms: u128) -> Self {
        Self {
            total_packages: None,
            checked_packages: None,
            exempt_packages: None,
            unsupported_packages: None,
            excluded_missing_packages: None,
            failed_packages: None,
            not_checked_packages: None,
            violations,
            duration_ms,
        }
    }
}

fn violation_json(violation: &Violation, report: &RunReport) -> JsonViolation {
    match &violation.kind {
        ViolationKind::TooNew(aged) => JsonViolation {
            package: violation.package.clone(),
            version: violation.version.clone(),
            kind: "too_new",
            published_at: Some(aged.published.to_rfc3339_opts(SecondsFormat::Secs, true)),
            age_days: Some(aged.age_days),
            threshold_days: report.policy.min_age_days,
            reason: None,
        },
        ViolationKind::TooOld(aged) => JsonViolation {
            package: violation.package.clone(),
            version: violation.version.clone(),
            kind: "too_old",
            published_at: Some(aged.published.to_rfc3339_opts(SecondsFormat::Secs, true)),
            age_days: Some(aged.age_days),
            threshold_days: report.policy.max_age_days,
            reason: None,
        },
        ViolationKind::MissingPublishDate { reason } => JsonViolation {
            package: violation.package.clone(),
            version: violation.version.clone(),
            kind: "missing_publish_date",
            published_at: None,
            age_days: None,
            threshold_days: None,
            reason: Some(reason.clone()),
        },
    }
}

fn diagnostic_json(diagnostic: &Diagnostic) -> JsonDiagnostic {
    JsonDiagnostic {
        category: diagnostic.category.to_string(),
        message: diagnostic.message.clone(),
        package: diagnostic.package.clone(),
        version: diagnostic.version.clone(),
        path: diagnostic
            .path
            .as_ref()
            .map(|path| path.display().to_string()),
        retryable: (diagnostic.category == "lookup").then_some(diagnostic.retryable),
    }
}

fn suggestion_json(outcome: &Outcome) -> JsonSuggestion {
    match outcome {
        Outcome::Suggest {
            package,
            package_spec,
            locked_version,
            suggested_version,
            suggested_age_days,
            unverified_dependents,
        } => JsonSuggestion {
            kind: "suggested",
            package: package.clone(),
            locked_version: locked_version.clone(),
            package_spec: Some(package_spec.clone()),
            command: Some(format!(
                "cargo update -p {package_spec}@{locked_version} --precise {suggested_version}"
            )),
            suggested_version: Some(suggested_version.clone()),
            suggested_age_days: Some(*suggested_age_days),
            unverified_dependents: Some(unverified_dependents.clone()),
            newest_compliant: None,
            blocker: None,
            reason: None,
        },
        Outcome::Blocked {
            package,
            locked_version,
            newest_compliant,
            blocker,
        } => JsonSuggestion {
            kind: "blocked",
            package: package.clone(),
            locked_version: locked_version.clone(),
            package_spec: None,
            command: None,
            suggested_version: None,
            suggested_age_days: None,
            unverified_dependents: None,
            newest_compliant: Some(newest_compliant.clone()),
            blocker: Some(JsonBlocker {
                name: blocker.name.clone(),
                version: blocker.version.clone(),
                requirement: blocker.req.clone(),
                also_suggested: blocker.also_suggested,
            }),
            reason: None,
        },
        Outcome::NoCompliantVersion {
            package,
            locked_version,
        } => JsonSuggestion {
            kind: "no_eligible_downgrade",
            package: package.clone(),
            locked_version: locked_version.clone(),
            package_spec: None,
            command: None,
            suggested_version: None,
            suggested_age_days: None,
            unverified_dependents: None,
            newest_compliant: None,
            blocker: None,
            reason: None,
        },
        Outcome::Unavailable {
            package,
            locked_version,
            reason,
        } => JsonSuggestion {
            kind: "unavailable",
            package: package.clone(),
            locked_version: locked_version.clone(),
            package_spec: None,
            command: None,
            suggested_version: None,
            suggested_age_days: None,
            unverified_dependents: None,
            newest_compliant: None,
            blocker: None,
            reason: Some(reason.clone()),
        },
    }
}

/// Serializes one completed CLI report as schema version 1. Additive fields
/// are permitted within this version; consumers must ignore unknown fields.
pub fn render_json(report: &RunReport) -> serde_json::Result<String> {
    let summary = report.summary.as_ref().map_or_else(
        || JsonSummary::unavailable(report.violations.len(), report.duration.as_millis()),
        |summary| {
            JsonSummary::known(
                summary,
                report.violations.len(),
                report.duration.as_millis(),
            )
        },
    );

    serde_json::to_string(&JsonReport {
        schema_version: 1,
        status: match report.exit_code() {
            0 => "passed",
            1 => "violations",
            _ => "error",
        },
        lockfile: report.lockfile.display().to_string(),
        policy: JsonPolicy {
            min_age_days: report.policy.min_age_days,
            max_age_days: report.policy.max_age_days,
            exclude_missing: report.policy.exclude_missing,
            exempt: report.policy.exempt.clone(),
        },
        summary,
        violations: report
            .violations
            .iter()
            .map(|violation| violation_json(violation, report))
            .collect(),
        warnings: report.warnings.iter().map(diagnostic_json).collect(),
        errors: report.required_errors.iter().map(diagnostic_json).collect(),
        suggestions: report
            .suggestions
            .as_ref()
            .map(|outcomes| outcomes.iter().map(suggestion_json).collect()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::{Aged, Policy, RunReport, Summary, Violation};
    use serde_json::json;

    #[test]
    fn error_status_preserves_partial_results_and_lookup_context() {
        let mut report = RunReport::completed(
            "Cargo.lock".into(),
            Policy::new(Some(14), None, false, vec![]),
            Summary {
                total_packages: 2,
                checked_packages: 1,
                failed_packages: 1,
                ..Default::default()
            },
        );
        report.violations.push(Violation {
            package: "young".into(),
            version: "1.0.0".into(),
            kind: ViolationKind::TooNew(Aged {
                published: chrono::Utc::now(),
                age_days: 1,
            }),
        });
        report.required_errors.push(Diagnostic {
            category: "lookup",
            package: Some("broken".into()),
            version: Some("1.0.0".into()),
            path: None,
            message: "Client error 400".into(),
            retryable: false,
        });

        let json: serde_json::Value = serde_json::from_str(&render_json(&report).unwrap()).unwrap();
        assert_eq!(json["schema_version"], 1);
        assert_eq!(json["status"], "error");
        assert_eq!(json["summary"]["violations"], 1);
        assert_eq!(json["errors"][0]["category"], "lookup");
        assert_eq!(json["errors"][0]["retryable"], false);
        assert_eq!(json["violations"][0]["kind"], "too_new");
    }

    #[test]
    fn failed_initialization_has_null_summary_and_suggestions() {
        let mut report = RunReport::failed(
            "missing.lock".into(),
            Policy::new(Some(14), None, false, vec![]),
            "Could not load lockfile",
        );
        report.suggestions = Some(vec![]);

        let json: serde_json::Value = serde_json::from_str(&render_json(&report).unwrap()).unwrap();
        assert_eq!(json["status"], "error");
        assert_eq!(json["summary"]["total_packages"], serde_json::Value::Null);
        assert_eq!(json["suggestions"], json!([]));
        assert_eq!(json["errors"][0]["category"], "input");
        assert!(json["errors"][0].get("retryable").is_none());
    }

    #[test]
    fn requested_suggestions_remain_an_array_when_no_downgrade_is_available() {
        let mut report = RunReport::completed(
            "Cargo.lock".into(),
            Policy::new(Some(14), None, false, vec![]),
            Summary::default(),
        );
        report.suggestions = Some(vec![Outcome::Unavailable {
            package: "widget".into(),
            locked_version: "1.0.0".into(),
            reason: "registry request failed".into(),
        }]);

        let json: serde_json::Value = serde_json::from_str(&render_json(&report).unwrap()).unwrap();
        assert_eq!(json["suggestions"][0]["kind"], "unavailable");
        assert_eq!(json["suggestions"][0]["reason"], "registry request failed");
    }

    #[test]
    fn suggestion_variants_preserve_commands_constraints_and_uncertainty() {
        let mut report = RunReport::completed(
            "Cargo.lock".into(),
            Policy::new(Some(14), None, false, vec![]),
            Summary::default(),
        );
        report.suggestions = Some(vec![
            Outcome::Suggest {
                package: "widget".into(),
                package_spec: "registry+https://example.test#index#widget".into(),
                locked_version: "2.0.0".into(),
                suggested_version: "1.9.0".into(),
                suggested_age_days: 30,
                unverified_dependents: vec!["consumer".into()],
            },
            Outcome::Blocked {
                package: "blocked".into(),
                locked_version: "2.0.0".into(),
                newest_compliant: "1.9.0".into(),
                blocker: crate::suggest::Blocker {
                    name: "consumer".into(),
                    version: Some("3.0.0".into()),
                    req: "^2".into(),
                    also_suggested: true,
                },
            },
            Outcome::NoCompliantVersion {
                package: "none".into(),
                locked_version: "2.0.0".into(),
            },
            Outcome::Unavailable {
                package: "unavailable".into(),
                locked_version: "2.0.0".into(),
                reason: "registry request failed".into(),
            },
        ]);

        let json: serde_json::Value = serde_json::from_str(&render_json(&report).unwrap()).unwrap();
        let suggestions = json["suggestions"].as_array().unwrap();
        assert_eq!(suggestions[0]["kind"], "suggested");
        assert_eq!(
            suggestions[0]["command"],
            "cargo update -p registry+https://example.test#index#widget@2.0.0 --precise 1.9.0"
        );
        assert_eq!(suggestions[0]["unverified_dependents"], json!(["consumer"]));
        assert_eq!(suggestions[1]["kind"], "blocked");
        assert_eq!(suggestions[1]["blocker"]["requirement"], "^2");
        assert_eq!(suggestions[1]["blocker"]["also_suggested"], true);
        assert_eq!(suggestions[2]["kind"], "no_eligible_downgrade");
        assert_eq!(suggestions[3]["kind"], "unavailable");
    }

    #[test]
    fn passed_zero_coverage_keeps_warnings_and_requested_empty_suggestions() {
        let mut report = RunReport::completed(
            "Cargo.lock".into(),
            Policy::new(Some(14), None, false, vec![]),
            Summary::default(),
        );
        report.warnings.push(Diagnostic {
            category: "cache",
            package: None,
            version: None,
            path: Some("responses.json".into()),
            message: "Could not save cache".into(),
            retryable: false,
        });
        report.suggestions = Some(vec![]);

        let json: serde_json::Value = serde_json::from_str(&render_json(&report).unwrap()).unwrap();
        assert_eq!(json["status"], "passed");
        assert_eq!(json["summary"]["checked_packages"], 0);
        assert_eq!(json["warnings"][0]["category"], "cache");
        assert_eq!(json["warnings"][0]["path"], "responses.json");
        assert!(json["warnings"][0].get("retryable").is_none());
        assert_eq!(json["suggestions"], json!([]));
    }
}

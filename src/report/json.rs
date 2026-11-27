use super::{Diagnostic, RunReport, Summary, Violation, ViolationKind};
use crate::suggest::{BlockerKind, Outcome};
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
    violations: usize,
    duration_ms: u128,
}

#[derive(Serialize, Default)]
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
    #[serde(skip_serializing_if = "Option::is_none")]
    package_spec: Option<String>,
}

#[derive(Serialize, Default)]
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
    /// `"package"` or `"manifest"`.
    kind: &'static str,
    name: String,
    /// Present for package blockers only.
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    /// Present for package blockers only: the bare name when unambiguous.
    #[serde(skip_serializing_if = "Option::is_none")]
    package_spec: Option<String>,
    requirement: String,
    also_suggested: bool,
}

impl JsonSummary {
    fn new(summary: Option<&Summary>, violations: usize, duration_ms: u128) -> Self {
        Self {
            total_packages: summary.map(|summary| summary.total_packages),
            checked_packages: summary.map(|summary| summary.checked_packages),
            exempt_packages: summary.map(|summary| summary.exempt_packages),
            unsupported_packages: summary.map(|summary| summary.unsupported_packages),
            excluded_missing_packages: summary.map(|summary| summary.excluded_missing_packages),
            failed_packages: summary.map(|summary| summary.failed_packages),
            violations,
            duration_ms,
        }
    }
}

fn violation_json(violation: &Violation, report: &RunReport) -> JsonViolation {
    let base = JsonViolation {
        package: violation.package.clone(),
        version: violation.version.clone(),
        ..Default::default()
    };
    match &violation.kind {
        ViolationKind::TooNew(aged) => JsonViolation {
            kind: "too_new",
            published_at: Some(aged.published.to_rfc3339_opts(SecondsFormat::Secs, true)),
            age_days: Some(aged.age_days),
            threshold_days: report.policy.min_age_days,
            ..base
        },
        ViolationKind::TooOld(aged) => JsonViolation {
            kind: "too_old",
            published_at: Some(aged.published.to_rfc3339_opts(SecondsFormat::Secs, true)),
            age_days: Some(aged.age_days),
            threshold_days: report.policy.max_age_days,
            ..base
        },
        ViolationKind::MissingPublishDate { reason } => JsonViolation {
            kind: "missing_publish_date",
            reason: Some(reason.clone()),
            ..base
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
        package_spec: diagnostic.package_spec.clone(),
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
            ..Default::default()
        },
        Outcome::Blocked {
            package,
            package_spec,
            locked_version,
            newest_compliant,
            blocker,
        } => JsonSuggestion {
            kind: "blocked",
            package: package.clone(),
            locked_version: locked_version.clone(),
            package_spec: Some(package_spec.clone()),
            newest_compliant: Some(newest_compliant.clone()),
            blocker: Some(match &blocker.kind {
                BlockerKind::Package {
                    version,
                    package_spec,
                } => JsonBlocker {
                    kind: "package",
                    name: blocker.name.clone(),
                    version: Some(version.clone()),
                    package_spec: Some(package_spec.clone()),
                    requirement: blocker.req.clone(),
                    also_suggested: blocker.also_suggested,
                },
                BlockerKind::Manifest => JsonBlocker {
                    kind: "manifest",
                    name: blocker.name.clone(),
                    version: None,
                    package_spec: None,
                    requirement: blocker.req.clone(),
                    also_suggested: blocker.also_suggested,
                },
            }),
            ..Default::default()
        },
        Outcome::NoCompliantVersion {
            package,
            package_spec,
            locked_version,
        } => JsonSuggestion {
            kind: "no_eligible_downgrade",
            package: package.clone(),
            locked_version: locked_version.clone(),
            package_spec: Some(package_spec.clone()),
            ..Default::default()
        },
        Outcome::Unavailable {
            package,
            package_spec,
            locked_version,
            reason,
        } => JsonSuggestion {
            kind: "unavailable",
            package: package.clone(),
            locked_version: locked_version.clone(),
            package_spec: Some(package_spec.clone()),
            reason: Some(reason.clone()),
            ..Default::default()
        },
    }
}

/// Serializes a CLI report as schema version 2.
pub fn render_json(report: &RunReport) -> serde_json::Result<String> {
    serde_json::to_string(&JsonReport {
        schema_version: 2,
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
        summary: JsonSummary::new(
            report.summary.as_ref(),
            report.violations.len(),
            report.duration.as_millis(),
        ),
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

    fn completed_report(summary: Summary) -> RunReport {
        RunReport::completed(
            "Cargo.lock".into(),
            Policy::new(Some(14), None, false, vec![]),
            summary,
        )
    }

    fn render(report: &RunReport) -> serde_json::Value {
        serde_json::from_str(&render_json(report).unwrap()).unwrap()
    }

    #[test]
    fn error_status_preserves_partial_results_and_lookup_context() {
        let mut report = completed_report(Summary {
            total_packages: 2,
            checked_packages: 1,
            failed_packages: 1,
            ..Default::default()
        });
        report.policy.max_age_days = Some(365);
        let published = "2026-01-02T03:04:05Z".parse().unwrap();
        report.violations = vec![
            Violation {
                package: "young".into(),
                version: "1.0.0".into(),
                kind: ViolationKind::TooNew(Aged {
                    published,
                    age_days: 1,
                }),
            },
            Violation {
                package: "old".into(),
                version: "0.1.0".into(),
                kind: ViolationKind::TooOld(Aged {
                    published,
                    age_days: 400,
                }),
            },
            Violation {
                package: "gone".into(),
                version: "0.2.0".into(),
                kind: ViolationKind::MissingPublishDate {
                    reason: "not on crates.io".into(),
                },
            },
        ];
        report.required_errors.push(Diagnostic {
            category: "lookup",
            package: Some("broken".into()),
            version: Some("1.0.0".into()),
            path: None,
            message: "Client error 400".into(),
            retryable: false,
            package_spec: None,
        });

        let json = render(&report);
        assert_eq!(json["schema_version"], 2);
        assert!(json["summary"].get("not_checked_packages").is_none());
        assert_eq!(json["status"], "error");
        assert_eq!(json["summary"]["violations"], 3);
        assert_eq!(json["errors"][0]["category"], "lookup");
        assert_eq!(json["errors"][0]["retryable"], false);
        assert_eq!(
            json["violations"],
            json!([
                {
                    "package": "young",
                    "version": "1.0.0",
                    "kind": "too_new",
                    "published_at": "2026-01-02T03:04:05Z",
                    "age_days": 1,
                    "threshold_days": 14,
                },
                {
                    "package": "old",
                    "version": "0.1.0",
                    "kind": "too_old",
                    "published_at": "2026-01-02T03:04:05Z",
                    "age_days": 400,
                    "threshold_days": 365,
                },
                {
                    "package": "gone",
                    "version": "0.2.0",
                    "kind": "missing_publish_date",
                    "reason": "not on crates.io",
                },
            ])
        );
    }

    #[test]
    fn failed_initialization_has_null_summary_and_suggestions() {
        let mut report = RunReport::failed(
            "missing.lock".into(),
            Policy::new(Some(14), None, false, vec![]),
            "Could not load lockfile",
        );
        report.suggestions = Some(vec![]);

        let json = render(&report);
        assert_eq!(json["status"], "error");
        assert_eq!(json["summary"]["total_packages"], serde_json::Value::Null);
        assert_eq!(json["suggestions"], json!([]));
        assert_eq!(json["errors"][0]["category"], "input");
        assert!(json["errors"][0].get("retryable").is_none());
    }

    #[test]
    fn suggestion_variants_preserve_commands_constraints_and_uncertainty() {
        let mut report = completed_report(Summary::default());
        report.suggestions = Some(vec![
            Outcome::Suggest {
                package: "widget".into(),
                package_spec: "registry+https://example.test/index#widget".into(),
                locked_version: "2.0.0".into(),
                suggested_version: "1.9.0".into(),
                suggested_age_days: 30,
                unverified_dependents: vec!["consumer".into()],
            },
            Outcome::Blocked {
                package: "blocked".into(),
                package_spec: "registry+https://example.test/index#blocked".into(),
                locked_version: "2.0.0".into(),
                newest_compliant: "1.9.0".into(),
                blocker: crate::suggest::Blocker {
                    name: "consumer".into(),
                    kind: BlockerKind::Package {
                        version: "3.0.0".into(),
                        package_spec: "consumer".into(),
                    },
                    req: "^2".into(),
                    also_suggested: true,
                },
            },
            Outcome::NoCompliantVersion {
                package: "none".into(),
                package_spec: "none".into(),
                locked_version: "2.0.0".into(),
            },
            Outcome::Unavailable {
                package: "unavailable".into(),
                package_spec: "registry+https://example.test/index#unavailable".into(),
                locked_version: "2.0.0".into(),
                reason: "registry request failed".into(),
            },
        ]);

        assert_eq!(
            render(&report)["suggestions"],
            json!([
                {
                    "kind": "suggested",
                    "package": "widget",
                    "locked_version": "2.0.0",
                    "package_spec": "registry+https://example.test/index#widget",
                    "command": "cargo update -p registry+https://example.test/index#widget@2.0.0 --precise 1.9.0",
                    "suggested_version": "1.9.0",
                    "suggested_age_days": 30,
                    "unverified_dependents": ["consumer"],
                },
                {
                    "kind": "blocked",
                    "package": "blocked",
                    "locked_version": "2.0.0",
                    "package_spec": "registry+https://example.test/index#blocked",
                    "newest_compliant": "1.9.0",
                    "blocker": {
                        "kind": "package",
                        "name": "consumer",
                        "version": "3.0.0",
                        "package_spec": "consumer",
                        "requirement": "^2",
                        "also_suggested": true,
                    },
                },
                {
                    "kind": "no_eligible_downgrade",
                    "package": "none",
                    "locked_version": "2.0.0",
                    "package_spec": "none",
                },
                {
                    "kind": "unavailable",
                    "package": "unavailable",
                    "locked_version": "2.0.0",
                    "package_spec": "registry+https://example.test/index#unavailable",
                    "reason": "registry request failed",
                },
            ])
        );
    }

    #[test]
    fn qualified_blocker_emits_its_package_spec() {
        let mut report = completed_report(Summary::default());
        report.suggestions = Some(vec![Outcome::Blocked {
            package: "clap_derive".into(),
            package_spec: "clap_derive".into(),
            locked_version: "4.6.9".into(),
            newest_compliant: "4.6.7".into(),
            blocker: crate::suggest::Blocker {
                name: "clap".into(),
                kind: BlockerKind::Package {
                    version: "4.6.9".into(),
                    package_spec: "registry+https://example.test/index#clap".into(),
                },
                req: "=4.6.9".into(),
                also_suggested: false,
            },
        }]);

        assert_eq!(
            render(&report)["suggestions"][0]["blocker"],
            json!({
                "kind": "package",
                "name": "clap",
                "version": "4.6.9",
                "package_spec": "registry+https://example.test/index#clap",
                "requirement": "=4.6.9",
                "also_suggested": false,
            })
        );
    }

    #[test]
    fn manifest_blocker_has_a_kind_and_neither_version_nor_package_spec() {
        let mut report = completed_report(Summary::default());
        report.suggestions = Some(vec![Outcome::Blocked {
            package: "clap_derive".into(),
            package_spec: "clap_derive".into(),
            locked_version: "4.6.9".into(),
            newest_compliant: "4.6.7".into(),
            blocker: crate::suggest::Blocker {
                name: "app/Cargo.toml".into(),
                kind: BlockerKind::Manifest,
                req: "=4.6.9".into(),
                also_suggested: false,
            },
        }]);

        assert_eq!(
            render(&report)["suggestions"][0]["blocker"],
            json!({
                "kind": "manifest",
                "name": "app/Cargo.toml",
                "requirement": "=4.6.9",
                "also_suggested": false,
            })
        );
    }

    #[test]
    fn passed_zero_coverage_keeps_warnings_and_requested_empty_suggestions() {
        let mut report = completed_report(Summary::default());
        report.warnings.push(Diagnostic {
            category: "cache",
            package: None,
            version: None,
            path: Some("responses.json".into()),
            message: "Could not save cache".into(),
            retryable: false,
            package_spec: None,
        });
        report.warnings.push(Diagnostic {
            package: Some("widget".into()),
            version: Some("2.0.0".into()),
            package_spec: Some("registry+https://example.test/index#widget".into()),
            ..Diagnostic::new("suggestion", "Could not determine a downgrade".into())
        });
        report.suggestions = Some(vec![]);

        let json = render(&report);
        assert_eq!(json["status"], "passed");
        assert_eq!(json["summary"]["checked_packages"], 0);
        assert_eq!(json["warnings"][0]["category"], "cache");
        assert_eq!(json["warnings"][0]["path"], "responses.json");
        assert!(json["warnings"][0].get("retryable").is_none());
        assert!(json["warnings"][0].get("package_spec").is_none());
        assert_eq!(
            json["warnings"][1]["package_spec"],
            "registry+https://example.test/index#widget"
        );
        assert_eq!(json["suggestions"], json!([]));
    }
}

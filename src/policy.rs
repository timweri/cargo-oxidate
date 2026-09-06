use crate::Package;
use crate::api::FetchError;
use crate::report::{Violation, ViolationKind};
use chrono::{DateTime, Utc};
use std::collections::HashSet;

/// The freshness rule: minimum and maximum age thresholds, an exempt list,
/// and what to do when a publish date cannot be determined.
///
/// Constructed once and evaluated per package. Evaluation is a pure function
/// of a package, the outcome of a publish-date lookup, and a timestamp. It
/// performs no I/O, so it is exhaustively testable without a network
/// connection.
pub struct FreshnessPolicy {
    min_age_days: Option<u64>,
    max_age_days: Option<u64>,
    exclude_missing: bool,
    exempt: HashSet<String>,
}

impl FreshnessPolicy {
    /// Fails if neither threshold is given, since a policy with no thresholds
    /// can never produce a violation.
    pub fn new(
        min_age_days: Option<u64>,
        max_age_days: Option<u64>,
        exclude_missing: bool,
        exempt: Vec<String>,
    ) -> anyhow::Result<Self> {
        if min_age_days.is_none() && max_age_days.is_none() {
            anyhow::bail!("At least one of --min-age-days or --max-age-days must be specified");
        }

        Ok(Self {
            min_age_days,
            max_age_days,
            exclude_missing,
            exempt: exempt.iter().map(|s| s.trim().to_string()).collect(),
        })
    }

    pub fn is_exempt(&self, name: &str) -> bool {
        self.exempt.contains(name)
    }

    pub fn min_age_days(&self) -> Option<u64> {
        self.min_age_days
    }

    /// Evaluates a package against the policy given the outcome of a
    /// publish-date lookup and the current time, returning the Violations
    /// triggered. A lookup that failed and a lookup that succeeded with no
    /// date are treated identically: both are "unknown".
    ///
    /// Printing a warning when a lookup failed is presentation, not policy,
    /// and stays with the caller.
    pub fn evaluate(
        &self,
        pkg: &Package,
        outcome: &Result<Option<DateTime<Utc>>, FetchError>,
        now: DateTime<Utc>,
    ) -> Vec<Violation> {
        let mut violations = Vec::new();

        let published = match outcome {
            Ok(Some(published)) => Some(*published),
            Ok(None) | Err(_) => None,
        };

        let Some(published) = published else {
            if !self.exclude_missing {
                violations.push(Violation {
                    package: pkg.name.clone(),
                    version: pkg.version.clone(),
                    kind: ViolationKind::Unknown,
                });
            }
            return violations;
        };

        let age_days = (now - published).num_days();

        if let Some(min) = self.min_age_days
            && age_days < min as i64
        {
            violations.push(Violation {
                package: pkg.name.clone(),
                version: pkg.version.clone(),
                kind: ViolationKind::TooNew {
                    published,
                    age_days,
                },
            });
        }

        if let Some(max) = self.max_age_days
            && age_days > max as i64
        {
            violations.push(Violation {
                package: pkg.name.clone(),
                version: pkg.version.clone(),
                kind: ViolationKind::TooOld {
                    published,
                    age_days,
                },
            });
        }

        violations
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn pkg() -> Package {
        Package {
            name: "serde".to_string(),
            version: "1.0.0".to_string(),
        }
    }

    fn published_days_ago(now: DateTime<Utc>, days: i64) -> DateTime<Utc> {
        now - Duration::days(days)
    }

    #[test]
    fn new_fails_without_any_threshold() {
        let result = FreshnessPolicy::new(None, None, false, vec![]);
        assert!(result.is_err());
    }

    #[test]
    fn new_succeeds_with_only_min() {
        assert!(FreshnessPolicy::new(Some(7), None, false, vec![]).is_ok());
    }

    #[test]
    fn new_succeeds_with_only_max() {
        assert!(FreshnessPolicy::new(None, Some(365), false, vec![]).is_ok());
    }

    #[test]
    fn package_exactly_at_min_age_is_compliant() {
        let now = Utc::now();
        let policy = FreshnessPolicy::new(Some(7), None, false, vec![]).unwrap();
        let outcome = Ok(Some(published_days_ago(now, 7)));

        assert!(policy.evaluate(&pkg(), &outcome, now).is_empty());
    }

    #[test]
    fn package_exactly_at_max_age_is_compliant() {
        let now = Utc::now();
        let policy = FreshnessPolicy::new(None, Some(365), false, vec![]).unwrap();
        let outcome = Ok(Some(published_days_ago(now, 365)));

        assert!(policy.evaluate(&pkg(), &outcome, now).is_empty());
    }

    #[test]
    fn package_one_day_below_min_age_is_too_new() {
        let now = Utc::now();
        let policy = FreshnessPolicy::new(Some(7), None, false, vec![]).unwrap();
        let outcome = Ok(Some(published_days_ago(now, 6)));

        let violations = policy.evaluate(&pkg(), &outcome, now);
        assert_eq!(violations.len(), 1);
        assert!(matches!(
            violations[0].kind,
            ViolationKind::TooNew { age_days: 6, .. }
        ));
    }

    #[test]
    fn package_one_day_above_max_age_is_too_old() {
        let now = Utc::now();
        let policy = FreshnessPolicy::new(None, Some(365), false, vec![]).unwrap();
        let outcome = Ok(Some(published_days_ago(now, 366)));

        let violations = policy.evaluate(&pkg(), &outcome, now);
        assert_eq!(violations.len(), 1);
        assert!(matches!(
            violations[0].kind,
            ViolationKind::TooOld { age_days: 366, .. }
        ));
    }

    #[test]
    fn both_thresholds_active_at_once() {
        let now = Utc::now();
        let policy = FreshnessPolicy::new(Some(7), Some(365), false, vec![]).unwrap();

        let too_new = policy.evaluate(&pkg(), &Ok(Some(published_days_ago(now, 1))), now);
        assert!(matches!(
            too_new.as_slice(),
            [Violation {
                kind: ViolationKind::TooNew { .. },
                ..
            }]
        ));

        let too_old = policy.evaluate(&pkg(), &Ok(Some(published_days_ago(now, 400))), now);
        assert!(matches!(
            too_old.as_slice(),
            [Violation {
                kind: ViolationKind::TooOld { .. },
                ..
            }]
        ));

        let compliant = policy.evaluate(&pkg(), &Ok(Some(published_days_ago(now, 100))), now);
        assert!(compliant.is_empty());
    }

    #[test]
    fn failed_lookup_is_unknown_by_default() {
        let now = Utc::now();
        let policy = FreshnessPolicy::new(Some(7), None, false, vec![]).unwrap();
        let outcome = Err(FetchError::Retryable("boom".to_string()));

        let violations = policy.evaluate(&pkg(), &outcome, now);
        assert_eq!(violations.len(), 1);
        assert!(matches!(violations[0].kind, ViolationKind::Unknown));
    }

    #[test]
    fn exclude_missing_suppresses_unknown() {
        let now = Utc::now();
        let policy = FreshnessPolicy::new(Some(7), None, true, vec![]).unwrap();
        let outcome = Err(FetchError::Retryable("boom".to_string()));

        assert!(policy.evaluate(&pkg(), &outcome, now).is_empty());
    }

    #[test]
    fn successful_lookup_with_no_date_is_treated_as_unknown() {
        let now = Utc::now();
        let policy = FreshnessPolicy::new(Some(7), None, false, vec![]).unwrap();
        let outcome: Result<Option<DateTime<Utc>>, FetchError> = Ok(None);

        let violations = policy.evaluate(&pkg(), &outcome, now);
        assert_eq!(violations.len(), 1);
        assert!(matches!(violations[0].kind, ViolationKind::Unknown));
    }

    #[test]
    fn exempt_names_are_trimmed() {
        let policy =
            FreshnessPolicy::new(Some(7), None, false, vec![" serde ".to_string()]).unwrap();

        assert!(policy.is_exempt("serde"));
        assert!(!policy.is_exempt(" serde "));
    }
}

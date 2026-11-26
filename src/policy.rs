use crate::lockfile::Package;
use crate::report::{Aged, Policy, Violation, ViolationKind};
use chrono::{DateTime, Utc};

/// The freshness rule: minimum and maximum age thresholds, an exempt list,
/// and what to do when a publish date cannot be determined.
///
/// Constructed once and evaluated per package. `evaluate` is a pure function
/// of a package, a publish-date lookup result, and a timestamp, so it's
/// testable without a network connection.
pub struct FreshnessPolicy {
    policy: Policy,
}

impl FreshnessPolicy {
    /// Fails if neither threshold is given, since a policy with no thresholds
    /// can never produce a violation, or if the minimum exceeds the maximum,
    /// since every package would then be flagged.
    pub fn new(policy: &Policy) -> anyhow::Result<Self> {
        if policy.min_age_days.is_none() && policy.max_age_days.is_none() {
            anyhow::bail!("At least one of --min-age-days or --max-age-days must be specified");
        }
        if let (Some(min), Some(max)) = (policy.min_age_days, policy.max_age_days)
            && min > max
        {
            anyhow::bail!("--min-age-days ({min}) must not exceed --max-age-days ({max})");
        }

        Ok(Self {
            policy: policy.clone(),
        })
    }

    pub fn is_exempt(&self, name: &str) -> bool {
        self.policy.exempt.iter().any(|exempt| exempt == name)
    }

    pub fn report_policy(&self) -> &Policy {
        &self.policy
    }

    pub fn excludes_missing(&self) -> bool {
        self.policy.exclude_missing
    }

    /// Evaluates a package against the policy given its publish date (`None`
    /// when crates.io confirms the version is missing) and the current time.
    pub fn evaluate(
        &self,
        pkg: &Package,
        published: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> Vec<Violation> {
        let violation = |kind| Violation {
            package: pkg.name.clone(),
            version: pkg.version.clone(),
            kind,
        };

        let Some(published) = published else {
            let missing = ViolationKind::MissingPublishDate {
                reason: "crates.io did not return this package version".to_string(),
            };
            return if self.policy.exclude_missing {
                vec![]
            } else {
                vec![violation(missing)]
            };
        };

        let age_days = (now - published).num_days();
        let aged = || Aged {
            published,
            age_days,
        };
        let mut violations = Vec::new();

        if self
            .policy
            .min_age_days
            .is_some_and(|min| age_days < min as i64)
        {
            violations.push(violation(ViolationKind::TooNew(aged())));
        }
        if self
            .policy
            .max_age_days
            .is_some_and(|max| age_days > max as i64)
        {
            violations.push(violation(ViolationKind::TooOld(aged())));
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
            is_registry: true,
            source: None,
            dependencies: vec![],
        }
    }

    #[test]
    fn new_validates_thresholds() {
        // (min, max, expected error message; `None` means accepted)
        let cases = [
            (
                None,
                None,
                Some("At least one of --min-age-days or --max-age-days must be specified"),
            ),
            (Some(7), None, None),
            (None, Some(365), None),
            (Some(30), Some(30), None),
            (
                Some(400),
                Some(30),
                Some("--min-age-days (400) must not exceed --max-age-days (30)"),
            ),
        ];

        for (min, max, expected) in cases {
            let result = FreshnessPolicy::new(&Policy::new(min, max, false, vec![]));
            assert_eq!(
                result.err().map(|e| e.to_string()).as_deref(),
                expected,
                "min={min:?} max={max:?}"
            );
        }
    }

    #[test]
    fn age_thresholds_are_inclusive_and_independent() {
        // (min, max, package age in days, expected violations as (kind, age))
        let cases: [(_, _, _, &[(&str, i64)]); 7] = [
            (Some(7), None, 7, &[]),
            (None, Some(365), 365, &[]),
            (Some(7), None, 6, &[("too new", 6)]),
            (None, Some(365), 366, &[("too old", 366)]),
            (Some(7), Some(365), 1, &[("too new", 1)]),
            (Some(7), Some(365), 400, &[("too old", 400)]),
            (Some(7), Some(365), 100, &[]),
        ];
        let now = Utc::now();

        for (min, max, age, expected) in cases {
            let policy = FreshnessPolicy::new(&Policy::new(min, max, false, vec![])).unwrap();
            let violations = policy.evaluate(&pkg(), Some(now - Duration::days(age)), now);
            let actual: Vec<_> = violations
                .iter()
                .map(|v| match &v.kind {
                    ViolationKind::TooNew(aged) => ("too new", aged.age_days),
                    ViolationKind::TooOld(aged) => ("too old", aged.age_days),
                    ViolationKind::MissingPublishDate { .. } => panic!("unexpected missing date"),
                })
                .collect();
            assert_eq!(actual, expected, "min={min:?} max={max:?} age={age}");
        }
    }

    #[test]
    fn missing_publish_date_is_unknown_by_default() {
        let now = Utc::now();
        let policy = FreshnessPolicy::new(&Policy::new(Some(7), None, false, vec![])).unwrap();

        let violations = policy.evaluate(&pkg(), None, now);
        assert_eq!(violations.len(), 1);
        assert!(matches!(
            violations[0].kind,
            ViolationKind::MissingPublishDate { .. }
        ));
    }

    #[test]
    fn exclude_missing_suppresses_unknown() {
        let now = Utc::now();
        let policy = FreshnessPolicy::new(&Policy::new(Some(7), None, true, vec![])).unwrap();

        assert!(policy.evaluate(&pkg(), None, now).is_empty());
    }

    #[test]
    fn exemptions_are_trimmed_deduplicated_and_sorted() {
        let exempt = [" zeta ", "alpha", "zeta"].map(String::from).to_vec();
        let policy = FreshnessPolicy::new(&Policy::new(Some(7), None, false, exempt)).unwrap();

        assert_eq!(policy.report_policy().exempt, ["alpha", "zeta"]);
        assert!(policy.is_exempt("zeta"));
        assert!(!policy.is_exempt(" zeta "));
    }
}

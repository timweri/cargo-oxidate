use crate::api::{CratesIoClient, CrateVersionInfo, Transport};
use crate::report::{Violation, ViolationKind};
use anyhow::Result;
use chrono::{DateTime, Utc};

pub struct Suggestion {
    pub package: String,
    pub suggested_version: String,
    pub suggested_age_days: i64,
}

/// Validates that `--suggest-fix` was given together with `--min-age-days`,
/// returning the minimum age to use, or `None` if `--suggest-fix` wasn't
/// requested. Lets the caller pass a plain value below instead of unwrapping
/// an `Option` it already validated.
pub fn require_min_age(suggest_fix: bool, min_age_days: Option<u64>) -> Result<Option<u64>> {
    if !suggest_fix {
        return Ok(None);
    }

    min_age_days.map(Some).ok_or_else(|| {
        anyhow::anyhow!("--suggest-fix requires --min-age-days to be specified")
    })
}

/// Finds the newest non-yanked version at least `min_age_days` old as of
/// `now`, together with its age in days.
fn find_compliant_version(
    versions: &[CrateVersionInfo],
    min_age_days: u64,
    now: DateTime<Utc>,
) -> Option<(String, i64)> {
    let min_age_threshold = now - chrono::Duration::days(min_age_days as i64);

    versions
        .iter()
        .filter(|v| !v.yanked && v.created_at <= min_age_threshold)
        .max_by_key(|v| v.created_at)
        .map(|v| {
            let age_days = (now - v.created_at).num_days();
            (v.num.clone(), age_days)
        })
}

/// Generates suggested compliant versions for every "too new" violation.
/// Returns `None` when there are no "too new" violations, so the caller
/// prints nothing; returns `Some` (possibly empty) once the flow has run.
///
/// A package whose fetch fails, or which has no compliant version, is
/// simply absent from the result rather than aborting the whole operation.
pub fn generate_suggestions<T: Transport>(
    client: &mut CratesIoClient<T>,
    violations: &[Violation],
    min_age_days: u64,
    now: DateTime<Utc>,
) -> Option<Vec<Suggestion>> {
    let too_new: Vec<&Violation> = violations
        .iter()
        .filter(|v| matches!(v.kind, ViolationKind::TooNew { .. }))
        .collect();

    if too_new.is_empty() {
        return None;
    }

    let mut suggestions = Vec::new();
    eprintln!("\nFetching version suggestions...");
    for (i, violation) in too_new.iter().enumerate() {
        eprintln!("  [{}/{}] {}", i + 1, too_new.len(), violation.package);

        match client.fetch_all_versions(&violation.package) {
            Ok(versions) => {
                if let Some((suggested_version, age_days)) =
                    find_compliant_version(&versions, min_age_days, now)
                {
                    suggestions.push(Suggestion {
                        package: violation.package.clone(),
                        suggested_version,
                        suggested_age_days: age_days,
                    });
                }
            }
            Err(e) => {
                eprintln!(
                    "\n  Warning: failed to fetch versions for {}: {e}",
                    violation.package
                );
            }
        }
    }

    Some(suggestions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap()
    }

    fn make_version(version: &str, days_ago: i64, yanked: bool) -> CrateVersionInfo {
        let created_at = now() - chrono::Duration::days(days_ago);
        CrateVersionInfo {
            num: version.to_string(),
            created_at,
            yanked,
        }
    }

    #[test]
    fn test_find_compliant_version_basic() {
        let versions = vec![
            make_version("1.0.0", 100, false),
            make_version("1.1.0", 50, false),
            make_version("1.2.0", 20, false),
            make_version("1.3.0", 5, false),
        ];

        let result = find_compliant_version(&versions, 30, now());
        assert!(result.is_some());
        let (version, age_days) = result.unwrap();
        assert_eq!(version, "1.1.0"); // Newest version older than 30 days
        assert_eq!(age_days, 50);
    }

    #[test]
    fn test_find_compliant_version_filters_yanked() {
        let versions = vec![
            make_version("1.0.0", 100, false),
            make_version("1.1.0", 50, true), // yanked
            make_version("1.2.0", 20, false),
        ];

        let result = find_compliant_version(&versions, 30, now());
        assert!(result.is_some());
        let (version, _) = result.unwrap();
        assert_eq!(version, "1.0.0"); // Skips yanked 1.1.0
    }

    #[test]
    fn test_find_compliant_version_no_compliant() {
        let versions = vec![
            make_version("1.0.0", 10, false),
            make_version("1.1.0", 5, false),
            make_version("1.2.0", 2, false),
        ];

        let result = find_compliant_version(&versions, 30, now());
        assert!(result.is_none());
    }

    #[test]
    fn test_find_compliant_version_all_yanked() {
        let versions = vec![
            make_version("1.0.0", 100, true),
            make_version("1.1.0", 50, true),
        ];

        let result = find_compliant_version(&versions, 30, now());
        assert!(result.is_none());
    }

    #[test]
    fn test_find_compliant_version_empty() {
        let versions: Vec<CrateVersionInfo> = vec![];
        let result = find_compliant_version(&versions, 30, now());
        assert!(result.is_none());
    }

    #[test]
    fn test_find_compliant_version_exact_threshold() {
        let versions = vec![
            make_version("1.0.0", 30, false),
            make_version("1.1.0", 29, false),
        ];

        let result = find_compliant_version(&versions, 30, now());
        assert!(result.is_some());
        let (version, age_days) = result.unwrap();
        assert_eq!(version, "1.0.0"); // Exactly 30 days should be compliant
        assert_eq!(age_days, 30);
    }

    #[test]
    fn test_find_compliant_version_picks_newest_compliant() {
        let versions = vec![
            make_version("1.0.0", 100, false),
            make_version("1.1.0", 90, false),
            make_version("1.2.0", 80, false),
            make_version("1.3.0", 70, false),
            make_version("1.4.0", 10, false), // Too new
        ];

        let result = find_compliant_version(&versions, 50, now());
        assert!(result.is_some());
        let (version, age_days) = result.unwrap();
        assert_eq!(version, "1.3.0"); // Newest among compliant versions
        assert_eq!(age_days, 70);
    }

    #[test]
    fn require_min_age_is_none_when_suggest_fix_not_requested() {
        assert_eq!(require_min_age(false, None).unwrap(), None);
        assert_eq!(require_min_age(false, Some(7)).unwrap(), None);
    }

    #[test]
    fn require_min_age_fails_without_min_age_days() {
        assert!(require_min_age(true, None).is_err());
    }

    #[test]
    fn require_min_age_passes_through_min_age_days() {
        assert_eq!(require_min_age(true, Some(7)).unwrap(), Some(7));
    }

    mod generate_suggestions_tests {
        use super::*;
        use crate::api::{HttpResponse, TransportError};
        use std::cell::RefCell;
        use std::collections::HashMap;
        use std::time::Duration;

        /// A minimal in-memory transport for driving the suggest flow: each
        /// crate name maps to a canned all-versions response or a transport
        /// error.
        type ScriptedResponse = Result<(u16, String), ()>;

        #[derive(Default)]
        struct FakeTransport {
            responses: RefCell<HashMap<String, ScriptedResponse>>,
        }

        impl FakeTransport {
            fn ok(&self, name: &str, versions_json: &str) {
                self.responses
                    .borrow_mut()
                    .insert(versions_url(name), Ok((200, versions_json.to_string())));
            }

            fn error(&self, name: &str) {
                self.responses
                    .borrow_mut()
                    .insert(versions_url(name), Err(()));
            }
        }

        impl Transport for FakeTransport {
            fn get(&self, url: &str) -> Result<HttpResponse, TransportError> {
                match self.responses.borrow().get(url) {
                    Some(Ok((status, body))) => Ok(HttpResponse {
                        status: *status,
                        body: body.as_bytes().to_vec(),
                    }),
                    Some(Err(())) => Err(TransportError("connection timed out".to_string())),
                    None => panic!("no scripted response for {url}"),
                }
            }
        }

        fn versions_url(name: &str) -> String {
            format!("https://crates.io/api/v1/crates/{name}")
        }

        fn versions_body(entries: &[(&str, i64, bool)], now: DateTime<Utc>) -> String {
            let versions: Vec<String> = entries
                .iter()
                .map(|(num, days_ago, yanked)| {
                    let created_at = now - chrono::Duration::days(*days_ago);
                    format!(
                        r#"{{"num":"{num}","created_at":"{}","yanked":{yanked}}}"#,
                        created_at.to_rfc3339()
                    )
                })
                .collect();
            format!(r#"{{"versions":[{}]}}"#, versions.join(","))
        }

        /// Builds a client with retry/pacing delays zeroed out, so the test
        /// suite doesn't sleep.
        fn fast_client(transport: FakeTransport) -> CratesIoClient<FakeTransport> {
            let mut client = CratesIoClient::with_transport(transport, None, 24);
            client.retry_count = 1;
            client.retry_delay = Duration::from_millis(0);
            client.pacing_delay = Duration::from_millis(0);
            client
        }

        fn too_new(package: &str) -> Violation {
            Violation {
                package: package.to_string(),
                version: "1.0.0".to_string(),
                kind: ViolationKind::TooNew {
                    published: now(),
                    age_days: 1,
                },
            }
        }

        fn too_old(package: &str) -> Violation {
            Violation {
                package: package.to_string(),
                version: "1.0.0".to_string(),
                kind: ViolationKind::TooOld {
                    published: now(),
                    age_days: 1000,
                },
            }
        }

        #[test]
        fn only_too_new_violations_are_fetched() {
            let transport = FakeTransport::default();
            transport.ok(
                "serde",
                &versions_body(&[("1.0.0", 50, false)], now()),
            );
            // "syn" has no scripted response: if it were fetched, the
            // transport would panic.
            let mut client = fast_client(transport);

            let violations = vec![too_new("serde"), too_old("syn")];
            let suggestions = generate_suggestions(&mut client, &violations, 30, now()).unwrap();

            assert_eq!(suggestions.len(), 1);
            assert_eq!(suggestions[0].package, "serde");
        }

        #[test]
        fn a_failed_fetch_does_not_abort_the_others() {
            let transport = FakeTransport::default();
            transport.error("serde");
            transport.ok(
                "syn",
                &versions_body(&[("1.0.0", 50, false)], now()),
            );
            let mut client = fast_client(transport);

            let violations = vec![too_new("serde"), too_new("syn")];
            let suggestions = generate_suggestions(&mut client, &violations, 30, now()).unwrap();

            assert_eq!(suggestions.len(), 1);
            assert_eq!(suggestions[0].package, "syn");
        }

        #[test]
        fn no_compliant_version_produces_no_suggestion() {
            let transport = FakeTransport::default();
            transport.ok(
                "serde",
                &versions_body(&[("1.0.0", 5, false)], now()),
            );
            let mut client = fast_client(transport);

            let violations = vec![too_new("serde")];
            let suggestions = generate_suggestions(&mut client, &violations, 30, now()).unwrap();

            assert!(suggestions.is_empty());
        }

        #[test]
        fn no_too_new_violations_yields_none() {
            let transport = FakeTransport::default();
            let mut client = fast_client(transport);

            let violations = vec![too_old("syn")];
            let suggestions = generate_suggestions(&mut client, &violations, 30, now());

            assert!(suggestions.is_none());
        }
    }
}

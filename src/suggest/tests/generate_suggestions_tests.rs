use super::*;
use crate::api::RetryPolicy;
use crate::api::test_support::{FakeTransport, ScriptedResponse, index_url, versions_url};
use crate::lockfile::PackageRef;
use crate::report::Aged;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::time::Duration;

trait FakeTransportExt {
    fn ok(&self, name: &str, versions_json: &str);
    fn error(&self, name: &str);
    fn index_ok(&self, name: &str, records_ndjson: &str);
    fn index_error(&self, name: &str);
}

impl FakeTransportExt for FakeTransport {
    fn ok(&self, name: &str, versions_json: &str) {
        self.push(
            &versions_url(name),
            ScriptedResponse::Http(200, versions_json.to_string()),
        );
    }

    fn error(&self, name: &str) {
        self.push(&versions_url(name), ScriptedResponse::Error);
    }

    fn index_ok(&self, name: &str, records_ndjson: &str) {
        self.push(
            &index_url(name),
            ScriptedResponse::Http(200, records_ndjson.to_string()),
        );
    }

    fn index_error(&self, name: &str) {
        self.push(&index_url(name), ScriptedResponse::Error);
    }
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
    CratesIoClient::with_transport(
        transport,
        None,
        24,
        RetryPolicy {
            retry_count: NonZeroU32::new(1).unwrap(),
            retry_delay: Duration::from_millis(0),
            pacing_delay: Duration::from_millis(0),
        },
    )
}

fn too_new(package: &str, locked_version: &str) -> Violation {
    Violation {
        package: package.to_string(),
        version: locked_version.to_string(),
        kind: ViolationKind::TooNew(Aged {
            published: now(),
            age_days: 1,
        }),
    }
}

fn too_old(package: &str) -> Violation {
    Violation {
        package: package.to_string(),
        version: "1.0.0".to_string(),
        kind: ViolationKind::TooOld(Aged {
            published: now(),
            age_days: 1000,
        }),
    }
}

const CRATES_IO_SOURCE: &str = "registry+https://github.com/rust-lang/crates.io-index";

/// A registry package. Its dependency edges default to the same
/// crates.io source as the loader resolves an unsourced edge to,
/// when — as here — the only matching name is a registry package.
fn pkg(name: &str, version: &str, deps: &[(&str, &str)]) -> Package {
    Package {
        name: name.to_string(),
        version: version.to_string(),
        is_registry: true,
        source: Some(CRATES_IO_SOURCE.to_string()),
        dependencies: deps
            .iter()
            .map(|(n, v)| PackageRef {
                name: n.to_string(),
                version: v.to_string(),
                source: Some(CRATES_IO_SOURCE.to_string()),
            })
            .collect(),
    }
}

fn non_registry_pkg(name: &str, version: &str, deps: &[(&str, &str)]) -> Package {
    Package {
        is_registry: false,
        source: None,
        ..pkg(name, version, deps)
    }
}

/// A dependent whose lockfile source is present but isn't crates.io
/// — a git or alternate-registry package. Unlike `non_registry_pkg`
/// (a local/path package, `source: None`), nothing here can read its
/// requirements: not the crates.io index (it's not on crates.io),
/// and not a local manifest (it has no manifest this crate can find).
fn external_pkg(name: &str, version: &str, source: &str, deps: &[(&str, &str)]) -> Package {
    Package {
        is_registry: false,
        source: Some(source.to_string()),
        ..pkg(name, version, deps)
    }
}

const GIT_SOURCE: &str =
    "git+https://github.com/example/app#0000000000000000000000000000000000000000";
const ALT_REGISTRY_SOURCE: &str = "registry+https://example.com/priv-index";

/// A `DirectRequirement` naming `declaring_package`/`declaring_version`
/// as the identity of the manifest that placed it — the shape
/// `load_direct_requirements` produces for a real local manifest.
fn local_requirement(
    declaring_package: &str,
    declaring_version: &str,
    crate_name: &str,
    req: &str,
) -> DirectRequirement {
    DirectRequirement {
        manifest: "/work/Cargo.toml".into(),
        declaring_package: declaring_package.to_string(),
        declaring_version: Some(declaring_version.to_string()),
        crate_name: crate_name.to_string(),
        req: VersionReq::parse(req).unwrap(),
        source: RequirementSource::CratesIo,
    }
}

/// Builds a client and dependents index from `transport`/`packages`
/// and calls production `gather_constraints` with a fixed
/// `/work` working dir — the shared shape of most of this module's
/// `gather_constraints` call sites.
fn gather(
    transport: FakeTransport,
    packages: &[Package],
    requirements: &[DirectRequirement],
    name: &str,
    locked_version: &str,
    source: Option<&str>,
) -> GatheredConstraints {
    let mut client = fast_client(transport);
    let index = build_indexes(packages).0;
    gather_constraints(
        &mut client,
        &index,
        requirements,
        Path::new("/work"),
        name,
        locked_version,
        source,
    )
}

#[test]
fn aliased_registry_requirements_follow_the_locked_version() {
    let transport = FakeTransport::default();
    transport.index_ok("app", r#"{"vers":"1.0.0","deps":[{"name":"foo_old","package":"foo","req":"^1"},{"name":"foo_new","package":"foo","req":"^2"}]}"#);
    let mut client = fast_client(transport);
    let packages = vec![pkg("app", "1.0.0", &[("foo", "1.5.0"), ("foo", "2.5.0")])];
    let index = build_indexes(&packages).0;
    for (locked, candidate) in [("1.5.0", "1.4.0"), ("2.5.0", "2.4.0")] {
        let gathered = gather_constraints(
            &mut client,
            &index,
            &[],
            Path::new("/work"),
            "foo",
            locked,
            Some(CRATES_IO_SOURCE),
        );
        assert_eq!(gathered.constraints.len(), 1);
        assert!(gathered.unverified_dependents.is_empty());
        assert!(matches!(
            walk(
                vec![(Version::parse(candidate).unwrap(), 50)],
                gathered.constraints
            ),
            WalkResult::Suggest(_, _)
        ));
    }
}

#[test]
fn unmatched_registry_requirements_are_unverified() {
    let transport = FakeTransport::default();
    transport.index_ok(
        "app",
        r#"{"vers":"1.0.0","deps":[{"name":"foo","req":"^1.5"}]}"#,
    );
    let packages = vec![pkg("app", "1.0.0", &[("foo", "1.4.0")])];
    let gathered = gather(
        transport,
        &packages,
        &[],
        "foo",
        "1.4.0",
        Some(CRATES_IO_SOURCE),
    );
    assert!(gathered.constraints.is_empty());
    assert_eq!(gathered.unverified_dependents, ["app"]);
}

#[test]
fn missing_non_registry_requirements_are_unverified() {
    let packages = vec![non_registry_pkg("git-app", "1.0.0", &[("foo", "1.5.0")])];
    let gathered = gather(
        FakeTransport::default(),
        &packages,
        &[],
        "foo",
        "1.5.0",
        Some(CRATES_IO_SOURCE),
    );
    assert!(gathered.constraints.is_empty());
    assert_eq!(gathered.unverified_dependents, ["git-app"]);
}

#[test]
fn same_named_git_dependents_report_one_unverified_label() {
    let packages = vec![
        external_pkg("parent", "1.0.0", GIT_SOURCE, &[("foo", "1.9.0")]),
        external_pkg("parent", "2.0.0", GIT_SOURCE, &[("foo", "1.9.0")]),
    ];
    let gathered = gather(
        FakeTransport::default(),
        &packages,
        &[],
        "foo",
        "1.9.0",
        Some(CRATES_IO_SOURCE),
    );
    assert_eq!(gathered.unverified_dependents, ["parent"]);
}

#[test]
fn distinct_git_dependents_are_each_reported() {
    let packages = vec![
        external_pkg("parent-a", "1.0.0", GIT_SOURCE, &[("foo", "1.9.0")]),
        external_pkg("parent-b", "1.0.0", GIT_SOURCE, &[("foo", "1.9.0")]),
    ];
    let gathered = gather(
        FakeTransport::default(),
        &packages,
        &[],
        "foo",
        "1.9.0",
        Some(CRATES_IO_SOURCE),
    );
    assert_eq!(gathered.unverified_dependents, ["parent-a", "parent-b"]);
}

#[test]
fn aliased_manifest_requirements_follow_the_locked_version() {
    let mut client = fast_client(FakeTransport::default());
    let packages = vec![non_registry_pkg(
        "app",
        "1.0.0",
        &[("foo", "1.5.0"), ("foo", "2.5.0")],
    )];
    let requirements: Vec<_> = ["^1", "^2"]
        .into_iter()
        .map(|req| DirectRequirement {
            manifest: "/work/Cargo.toml".into(),
            declaring_package: "app".to_string(),
            declaring_version: Some("1.0.0".to_string()),
            crate_name: "foo".to_string(),
            req: VersionReq::parse(req).unwrap(),
            source: RequirementSource::CratesIo,
        })
        .collect();
    let index = build_indexes(&packages).0;
    for (locked, candidate) in [("1.5.0", "1.4.0"), ("2.5.0", "2.4.0")] {
        let gathered = gather_constraints(
            &mut client,
            &index,
            &requirements,
            Path::new("/work"),
            "foo",
            locked,
            Some(CRATES_IO_SOURCE),
        );
        assert_eq!(gathered.constraints.len(), 1);
        assert!(gathered.unverified_dependents.is_empty());
        assert!(matches!(
            walk(
                vec![(Version::parse(candidate).unwrap(), 50)],
                gathered.constraints
            ),
            WalkResult::Suggest(_, _)
        ));
    }
}

#[test]
fn matching_local_identity_with_a_permissive_requirement_allows_the_downgrade() {
    // Positive control: a genuine local dependent, matched by both
    // name and version, whose manifest requirement is loose enough
    // to permit the downgrade — the ordinary case this whole path
    // exists for.
    let packages = vec![non_registry_pkg("app", "1.0.0", &[("foo", "1.5.0")])];
    let requirements = vec![local_requirement("app", "1.0.0", "foo", "^1")];

    let gathered = gather(
        FakeTransport::default(),
        &packages,
        &requirements,
        "foo",
        "1.5.0",
        Some(CRATES_IO_SOURCE),
    );
    assert_eq!(gathered.constraints.len(), 1);
    assert!(gathered.unverified_dependents.is_empty());
    assert!(matches!(
        walk(
            vec![(Version::parse("1.4.0").unwrap(), 50)],
            gathered.constraints
        ),
        WalkResult::Suggest(_, _)
    ));
}

#[test]
fn matching_local_identity_with_a_restrictive_requirement_blocks_the_downgrade() {
    // Positive control, the other direction: the same identity
    // match, but the requirement is restrictive enough to reject
    // the downgrade candidate.
    let packages = vec![non_registry_pkg("app", "1.0.0", &[("foo", "1.5.0")])];
    let requirements = vec![local_requirement("app", "1.0.0", "foo", "^1.5")];

    let gathered = gather(
        FakeTransport::default(),
        &packages,
        &requirements,
        "foo",
        "1.5.0",
        Some(CRATES_IO_SOURCE),
    );
    assert_eq!(gathered.constraints.len(), 1);
    assert!(gathered.unverified_dependents.is_empty());
    assert!(matches!(
        walk(
            vec![(Version::parse("1.4.0").unwrap(), 50)],
            gathered.constraints
        ),
        WalkResult::Blocked { .. }
    ));
}

#[test]
fn git_dependent_sharing_a_local_packages_name_and_version_stays_unverified() {
    // A git "app" and a local "app" happen to share a name and
    // version. The local one's manifest permits the downgrade, but
    // that manifest was never the git dependent's own — it must not
    // be credited with verifying it.
    let packages = vec![
        external_pkg("app", "1.0.0", GIT_SOURCE, &[("foo", "1.5.0")]),
        non_registry_pkg("app", "1.0.0", &[]),
    ];
    let requirements = vec![local_requirement("app", "1.0.0", "foo", "^1")];

    let gathered = gather(
        FakeTransport::default(),
        &packages,
        &requirements,
        "foo",
        "1.5.0",
        Some(CRATES_IO_SOURCE),
    );
    assert!(gathered.constraints.is_empty());
    assert_eq!(gathered.unverified_dependents, ["app"]);
}

#[test]
fn alternate_registry_dependent_sharing_a_local_packages_name_and_version_stays_unverified() {
    // Same shape as the git case, but the external dependent is on
    // an alternate registry instead.
    let packages = vec![
        external_pkg("app", "1.0.0", ALT_REGISTRY_SOURCE, &[("foo", "1.5.0")]),
        non_registry_pkg("app", "1.0.0", &[]),
    ];
    let requirements = vec![local_requirement("app", "1.0.0", "foo", "^1")];

    let gathered = gather(
        FakeTransport::default(),
        &packages,
        &requirements,
        "foo",
        "1.5.0",
        Some(CRATES_IO_SOURCE),
    );
    assert!(gathered.constraints.is_empty());
    assert_eq!(gathered.unverified_dependents, ["app"]);
}

#[test]
fn restrictive_unrelated_local_requirement_does_not_block_the_external_dependents_target() {
    // The local "app" shares the git dependent's name and version,
    // but has no dependency edge of its own onto "foo" at all — its
    // manifest requirement is unrelated noise. The requirement
    // matches the locked version but would block candidate 1.4.0;
    // even so, it must not block foo's downgrade for the git
    // dependent, whose own requirement can't be read at all.
    let packages = vec![
        external_pkg("app", "1.0.0", GIT_SOURCE, &[("foo", "1.5.0")]),
        non_registry_pkg("app", "1.0.0", &[]),
    ];
    let requirements = vec![local_requirement("app", "1.0.0", "foo", "^1.5")];

    let gathered = gather(
        FakeTransport::default(),
        &packages,
        &requirements,
        "foo",
        "1.5.0",
        Some(CRATES_IO_SOURCE),
    );
    assert!(gathered.constraints.is_empty());
    assert_eq!(gathered.unverified_dependents, ["app"]);
    assert!(matches!(
        walk(
            vec![(Version::parse("1.4.0").unwrap(), 50)],
            gathered.constraints
        ),
        WalkResult::Suggest(_, _)
    ));
}

#[test]
fn local_declaring_version_mismatch_neither_verifies_nor_constrains() {
    // A local "app" 2.0.0 declares a restrictive requirement on
    // "foo", but the actual lockfile dependent is a *different*
    // "app" — 1.0.0 — with an identical name. Declaring package
    // name alone must not be enough to apply this requirement.
    let packages = vec![non_registry_pkg("app", "1.0.0", &[("foo", "1.5.0")])];
    let requirements = vec![local_requirement("app", "2.0.0", "foo", "^1.5")];

    let gathered = gather(
        FakeTransport::default(),
        &packages,
        &requirements,
        "foo",
        "1.5.0",
        Some(CRATES_IO_SOURCE),
    );
    assert!(gathered.constraints.is_empty());
    assert_eq!(gathered.unverified_dependents, ["app"]);
}

#[test]
fn only_too_new_violations_are_fetched() {
    let transport = FakeTransport::default();
    transport.ok("serde", &versions_body(&[("1.0.0", 50, false)], now()));
    // "syn" has no scripted response: if it were fetched, the
    // transport would panic.
    let mut client = fast_client(transport);

    let violations = vec![too_new("serde", "1.0.0"), too_old("syn")];
    let packages = vec![pkg("serde", "1.0.0", &[])];
    let outcomes = suggestions(&mut client, &violations, &packages, &[], Path::new("/work"));

    assert_eq!(outcomes.len(), 1);
}

#[test]
fn an_unparsable_locked_version_does_not_abort_the_others() {
    let transport = FakeTransport::default();
    // "serde" has no scripted response: if its versions were fetched,
    // the transport would panic — its unparsable locked version must
    // be skipped before that.
    transport.ok(
        "syn",
        &versions_body(&[("1.0.0", 50, false), ("1.1.0", 5, false)], now()),
    );
    let mut client = fast_client(transport);

    let violations = vec![too_new("serde", "not-a-version"), too_new("syn", "1.1.0")];
    let packages = vec![pkg("serde", "not-a-version", &[]), pkg("syn", "1.1.0", &[])];
    let outcomes = suggestions(&mut client, &violations, &packages, &[], Path::new("/work"));

    assert_eq!(outcomes.len(), 1);
    assert!(matches!(&outcomes[0], Outcome::Suggest { package, .. } if package == "syn"));
}

#[test]
fn a_failed_fetch_does_not_abort_the_others() {
    let transport = FakeTransport::default();
    transport.error("serde");
    transport.ok(
        "syn",
        &versions_body(&[("1.0.0", 50, false), ("1.1.0", 5, false)], now()),
    );
    let mut client = fast_client(transport);

    let violations = vec![too_new("serde", "1.0.0"), too_new("syn", "1.1.0")];
    let packages = vec![pkg("serde", "1.0.0", &[]), pkg("syn", "1.1.0", &[])];
    let outcomes = suggestions(&mut client, &violations, &packages, &[], Path::new("/work"));

    assert_eq!(outcomes.len(), 1);
    assert!(matches!(&outcomes[0], Outcome::Suggest { package, .. } if package == "syn"));
}

#[test]
fn no_compliant_version_reports_the_no_candidate_outcome() {
    let transport = FakeTransport::default();
    transport.ok("serde", &versions_body(&[("1.0.0", 5, false)], now()));
    let mut client = fast_client(transport);

    let violations = vec![too_new("serde", "1.0.0")];
    let packages = vec![pkg("serde", "1.0.0", &[])];
    let outcomes = suggestions(&mut client, &violations, &packages, &[], Path::new("/work"));

    assert!(matches!(outcomes[0], Outcome::NoCompliantVersion { .. }));
}

#[test]
fn no_too_new_violations_yields_none() {
    let transport = FakeTransport::default();
    let mut client = fast_client(transport);

    let violations = vec![too_old("syn")];
    let outcomes = generate_suggestions(
        &mut client,
        &violations,
        &[],
        &[],
        Path::new("/work"),
        30,
        false,
        now(),
    );

    assert!(outcomes.is_none());
}

#[test]
fn transitive_dependent_requirement_blocks_the_newest_candidate() {
    // "app" depends on serde 1.5.0; serde's index says app requires ^1.5.
    let transport = FakeTransport::default();
    transport.ok(
        "serde",
        &versions_body(&[("1.5.0", 5, false), ("1.4.0", 50, false)], now()),
    );
    transport.index_ok(
        "app",
        r#"{"vers":"1.0.0","deps":[{"name":"serde","req":"^1.5"}]}"#,
    );
    let mut client = fast_client(transport);

    let violations = vec![too_new("serde", "1.5.0")];
    let packages = vec![
        pkg("serde", "1.5.0", &[]),
        pkg("app", "1.0.0", &[("serde", "1.5.0")]),
    ];
    let outcomes = suggestions(&mut client, &violations, &packages, &[], Path::new("/work"));

    match &outcomes[0] {
        Outcome::Blocked {
            newest_compliant,
            blocker,
            ..
        } => {
            assert_eq!(newest_compliant, "1.4.0");
            assert_eq!(blocker.name, "app");
            assert_eq!(blocker.req, "^1.5");
        }
        _ => panic!("expected Blocked"),
    }
}

#[test]
fn suggests_the_semver_highest_compliant_version_over_a_later_backport() {
    // 1.4.0 outranks 1.3.9 by semver even though 1.3.9 was published
    // more recently (40 days ago vs. 100), so it must be the suggestion.
    let transport = FakeTransport::default();
    transport.ok(
        "serde",
        &versions_body(&[("1.4.0", 100, false), ("1.3.9", 40, false)], now()),
    );
    let mut client = fast_client(transport);

    let violations = vec![too_new("serde", "1.5.0")];
    let packages = vec![pkg("serde", "1.5.0", &[])];
    let outcomes = suggestions(&mut client, &violations, &packages, &[], Path::new("/work"));

    match &outcomes[0] {
        Outcome::Suggest {
            suggested_version,
            suggested_age_days,
            ..
        } => {
            assert_eq!(suggested_version, "1.4.0");
            assert_eq!(*suggested_age_days, 100);
        }
        _ => panic!("expected Suggest"),
    }
}

#[test]
fn blocked_names_the_semver_highest_compliant_version_as_newest_compliant() {
    // 1.4.0 and 1.3.9 both satisfy age, but the manifest's `^1.4.1`
    // requirement rejects both. "newest_compliant" in the Blocked
    // outcome must be the semver-highest one, 1.4.0, not the more
    // recently published 1.3.9.
    let transport = FakeTransport::default();
    transport.ok(
        "serde",
        &versions_body(&[("1.4.0", 100, false), ("1.3.9", 40, false)], now()),
    );
    transport.index_ok(
        "app",
        r#"{"vers":"1.0.0","deps":[{"name":"serde","req":"^1.4.1"}]}"#,
    );
    let mut client = fast_client(transport);

    let violations = vec![too_new("serde", "1.5.0")];
    let packages = vec![
        pkg("serde", "1.5.0", &[]),
        pkg("app", "1.0.0", &[("serde", "1.5.0")]),
    ];
    let outcomes = suggestions(&mut client, &violations, &packages, &[], Path::new("/work"));

    match &outcomes[0] {
        Outcome::Blocked {
            newest_compliant,
            blocker,
            ..
        } => {
            assert_eq!(newest_compliant, "1.4.0");
            assert_eq!(blocker.name, "app");
            assert_eq!(blocker.req, "^1.4.1");
        }
        _ => panic!("expected Blocked"),
    }
}

#[test]
fn same_name_version_collision_across_sources_does_not_leak_dependents() {
    // Two packages both named "serde" locked at 1.5.0: one from
    // crates.io, one from git. "consumer" depends on the git one
    // specifically. The crates.io serde must not inherit consumer's
    // requirement just because the name and version happen to match.
    let transport = FakeTransport::default();
    transport.ok(
        "serde",
        &versions_body(&[("1.5.0", 5, false), ("1.4.0", 50, false)], now()),
    );
    // If "consumer" were (wrongly) treated as a dependent of the
    // crates.io serde, this scripted index response would be
    // fetched and its ^1.5 requirement would block the downgrade.
    transport.index_ok(
        "consumer",
        r#"{"vers":"1.0.0","deps":[{"name":"serde","req":"^1.5"}]}"#,
    );
    let mut client = fast_client(transport);

    let registry_source = "registry+https://github.com/rust-lang/crates.io-index";
    let git_source =
        "git+https://github.com/example/serde#0000000000000000000000000000000000000000";

    let packages = vec![
        Package {
            name: "serde".to_string(),
            version: "1.5.0".to_string(),
            is_registry: true,
            source: Some(registry_source.to_string()),
            dependencies: vec![],
        },
        Package {
            name: "serde".to_string(),
            version: "1.5.0".to_string(),
            is_registry: false,
            source: Some(git_source.to_string()),
            dependencies: vec![],
        },
        Package {
            name: "consumer".to_string(),
            version: "1.0.0".to_string(),
            is_registry: true,
            source: Some(registry_source.to_string()),
            dependencies: vec![PackageRef {
                name: "serde".to_string(),
                version: "1.5.0".to_string(),
                source: Some(git_source.to_string()),
            }],
        },
    ];

    let violations = vec![too_new("serde", "1.5.0")];
    let outcomes = suggestions(&mut client, &violations, &packages, &[], Path::new("/work"));

    assert!(
        matches!(&outcomes[0], Outcome::Suggest { suggested_version, .. } if suggested_version == "1.4.0"),
        "the crates.io serde must not be blocked by consumer's requirement on the git serde: {:?}",
        match &outcomes[0] {
            Outcome::Blocked { blocker, .. } => format!("Blocked by {}", blocker.name),
            _ => "other".to_string(),
        }
    );
}

#[test]
fn source_collision_yields_a_source_qualified_package_spec() {
    // Same fixture as above: a crates.io "serde" and a git "serde"
    // both locked at 1.5.0. Cargo would reject the abbreviated
    // `serde@1.5.0` spec as ambiguous, so the suggestion for the
    // registry package must qualify it with the registry source.
    let transport = FakeTransport::default();
    transport.ok(
        "serde",
        &versions_body(&[("1.5.0", 5, false), ("1.4.0", 50, false)], now()),
    );
    let mut client = fast_client(transport);

    let registry_source = "registry+https://github.com/rust-lang/crates.io-index";
    let git_source =
        "git+https://github.com/example/serde#0000000000000000000000000000000000000000";

    let packages = vec![
        Package {
            name: "serde".to_string(),
            version: "1.5.0".to_string(),
            is_registry: true,
            source: Some(registry_source.to_string()),
            dependencies: vec![],
        },
        Package {
            name: "serde".to_string(),
            version: "1.5.0".to_string(),
            is_registry: false,
            source: Some(git_source.to_string()),
            dependencies: vec![],
        },
    ];

    let violations = vec![too_new("serde", "1.5.0")];
    let outcomes = suggestions(&mut client, &violations, &packages, &[], Path::new("/work"));

    match &outcomes[0] {
        Outcome::Suggest { package_spec, .. } => {
            assert_eq!(package_spec, &format!("{registry_source}#serde"));
        }
        _ => panic!("expected serde to be Suggest"),
    }
}

#[test]
fn no_collision_keeps_the_abbreviated_package_spec() {
    let transport = FakeTransport::default();
    transport.ok(
        "serde",
        &versions_body(&[("1.5.0", 5, false), ("1.4.0", 50, false)], now()),
    );
    let mut client = fast_client(transport);

    let violations = vec![too_new("serde", "1.5.0")];
    let packages = vec![pkg("serde", "1.5.0", &[])];
    let outcomes = suggestions(&mut client, &violations, &packages, &[], Path::new("/work"));

    match &outcomes[0] {
        Outcome::Suggest { package_spec, .. } => assert_eq!(package_spec, "serde"),
        _ => panic!("expected serde to be Suggest"),
    }
}

#[test]
fn path_package_sharing_a_name_and_version_does_not_leak_dependents_to_the_registry_package() {
    // A path package "local-crate" 0.1.0 and a crates.io package of
    // the same name and version both exist. "consumer" depends on
    // the path one via an edge that omits the source, as cargo does
    // for any edge whose true target has none. Since a path
    // package's own source is always omitted too, the crates.io
    // package must not inherit consumer's requirement just because
    // the name and version happen to collide.
    let transport = FakeTransport::default();
    transport.ok(
        "local-crate",
        &versions_body(&[("1.1.0", 5, false), ("1.0.0", 50, false)], now()),
    );
    // If "consumer" were (wrongly) treated as a dependent of the
    // crates.io local-crate, this scripted index response would be
    // fetched and its ^1.1 requirement would block the downgrade.
    transport.index_ok(
        "consumer",
        r#"{"vers":"1.0.0","deps":[{"name":"local-crate","req":"^1.1"}]}"#,
    );
    let mut client = fast_client(transport);

    let registry_source = "registry+https://github.com/rust-lang/crates.io-index";

    let packages = vec![
        Package {
            name: "local-crate".to_string(),
            version: "1.1.0".to_string(),
            is_registry: true,
            source: Some(registry_source.to_string()),
            dependencies: vec![],
        },
        Package {
            name: "local-crate".to_string(),
            version: "1.1.0".to_string(),
            is_registry: false,
            source: None,
            dependencies: vec![],
        },
        Package {
            name: "consumer".to_string(),
            version: "1.0.0".to_string(),
            is_registry: true,
            source: Some(registry_source.to_string()),
            dependencies: vec![PackageRef {
                name: "local-crate".to_string(),
                version: "1.1.0".to_string(),
                source: None,
            }],
        },
    ];

    let violations = vec![too_new("local-crate", "1.1.0")];
    let outcomes = suggestions(&mut client, &violations, &packages, &[], Path::new("/work"));

    assert!(
        matches!(&outcomes[0], Outcome::Suggest { suggested_version, .. } if suggested_version == "1.0.0"),
        "the crates.io local-crate must not be blocked by consumer's requirement on the path local-crate: {:?}",
        match &outcomes[0] {
            Outcome::Blocked { blocker, .. } => format!("Blocked by {}", blocker.name),
            _ => "other".to_string(),
        }
    );
}

#[test]
fn dev_kind_edge_from_a_transitive_dependent_is_ignored() {
    let transport = FakeTransport::default();
    transport.ok(
        "serde",
        &versions_body(&[("1.5.0", 5, false), ("1.4.0", 50, false)], now()),
    );
    transport.index_ok(
        "app",
        r#"{"vers":"1.0.0","deps":[{"name":"serde","req":"^1.5","kind":"dev"}]}"#,
    );
    let mut client = fast_client(transport);

    let violations = vec![too_new("serde", "1.5.0")];
    let packages = vec![
        pkg("serde", "1.5.0", &[]),
        pkg("app", "1.0.0", &[("serde", "1.5.0")]),
    ];
    let outcomes = suggestions(&mut client, &violations, &packages, &[], Path::new("/work"));

    assert!(matches!(outcomes[0], Outcome::Suggest { .. }));
}

#[test]
fn renamed_dependency_is_matched_by_its_real_name() {
    let transport = FakeTransport::default();
    transport.ok("serde", &versions_body(&[("1.4.0", 50, false)], now()));
    transport.index_ok(
        "app",
        r#"{"vers":"1.0.0","deps":[{"name":"my_serde","package":"serde","req":"^1.5"}]}"#,
    );
    let mut client = fast_client(transport);

    let violations = vec![too_new("serde", "1.5.0")];
    let packages = vec![
        pkg("serde", "1.5.0", &[]),
        pkg("app", "1.0.0", &[("serde", "1.5.0")]),
    ];
    let outcomes = suggestions(&mut client, &violations, &packages, &[], Path::new("/work"));

    assert!(matches!(outcomes[0], Outcome::Blocked { .. }));
}

#[test]
fn alternate_registry_index_declaration_is_not_enforced() {
    // "app" declares a normal `foo = "^1"` from crates.io and a
    // same-crate alias `foo_alt = { package = "foo", version = "^1.3",
    // registry = "alt" }` from an alternate registry. The alternate
    // registry declaration cannot be resolved against the locked
    // crates.io package, so only `^1` may be enforced.
    let transport = FakeTransport::default();
    transport.ok(
        "foo",
        &versions_body(&[("1.5.0", 5, false), ("1.2.0", 50, false)], now()),
    );
    transport.index_ok(
                "app",
                r#"{"vers":"1.0.0","deps":[{"name":"foo","req":"^1"},{"name":"foo_alt","package":"foo","req":"^1.3","registry":"alt"}]}"#,
            );
    let mut client = fast_client(transport);

    let violations = vec![too_new("foo", "1.5.0")];
    let packages = vec![
        pkg("foo", "1.5.0", &[]),
        pkg("app", "1.0.0", &[("foo", "1.5.0")]),
    ];
    let outcomes = suggestions(&mut client, &violations, &packages, &[], Path::new("/work"));

    match &outcomes[0] {
        Outcome::Suggest {
            suggested_version, ..
        } => assert_eq!(suggested_version, "1.2.0"),
        _ => panic!("expected foo to be Suggest, with the alternate registry declaration ignored"),
    }
}

#[test]
fn ambiguous_optional_declaration_does_not_block_the_downgrade() {
    // "app" declares both a normal `serde = "^1"` and a disabled,
    // renamed optional `serde_new = { package = "serde", version =
    // "^1.5", optional = true }`. Both match locked serde@1.5.0, but
    // since the optional one may not even be activated, neither can
    // be enforced as a definite blocker.
    let transport = FakeTransport::default();
    transport.ok(
        "serde",
        &versions_body(&[("1.5.0", 5, false), ("1.4.0", 50, false)], now()),
    );
    transport.index_ok(
                "app",
                r#"{"vers":"1.0.0","deps":[{"name":"serde","req":"^1"},{"name":"serde_new","package":"serde","req":"^1.5","optional":true}]}"#,
            );
    let mut client = fast_client(transport);

    let violations = vec![too_new("serde", "1.5.0")];
    let packages = vec![
        pkg("serde", "1.5.0", &[]),
        pkg("app", "1.0.0", &[("serde", "1.5.0")]),
    ];
    let outcomes = suggestions(&mut client, &violations, &packages, &[], Path::new("/work"));

    match &outcomes[0] {
        Outcome::Suggest {
            suggested_version,
            unverified_dependents,
            ..
        } => {
            assert_eq!(suggested_version, "1.4.0");
            assert_eq!(unverified_dependents, &["app".to_string()]);
        }
        _ => panic!("expected serde to be Suggest, with app marked unverified"),
    }
}

#[test]
fn unique_optional_declaration_still_blocks() {
    // Only the optional, renamed declaration matches — no ambiguity,
    // so it's the unique explanation for the lockfile edge and must
    // still be enforced.
    let transport = FakeTransport::default();
    transport.ok(
        "serde",
        &versions_body(&[("1.5.0", 5, false), ("1.4.0", 50, false)], now()),
    );
    transport.index_ok(
                "app",
                r#"{"vers":"1.0.0","deps":[{"name":"serde_new","package":"serde","req":"^1.5","optional":true}]}"#,
            );
    let mut client = fast_client(transport);

    let violations = vec![too_new("serde", "1.5.0")];
    let packages = vec![
        pkg("serde", "1.5.0", &[]),
        pkg("app", "1.0.0", &[("serde", "1.5.0")]),
    ];
    let outcomes = suggestions(&mut client, &violations, &packages, &[], Path::new("/work"));

    match &outcomes[0] {
        Outcome::Blocked {
            newest_compliant,
            blocker,
            ..
        } => {
            assert_eq!(newest_compliant, "1.4.0");
            assert_eq!(blocker.name, "app");
            assert_eq!(blocker.req, "^1.5");
        }
        _ => panic!("expected serde to be Blocked by app's unique optional declaration"),
    }
}

#[test]
fn mandatory_requirements_from_different_kinds_both_block() {
    // "app" declares an unconditional, nonoptional normal
    // requirement of `^1.5` and an unconditional, nonoptional build
    // requirement of `^1` on serde. Both are always active, so both
    // are enforced — the more restrictive one (`^1.5`) rejects the
    // downgrade to 1.4.0.
    let transport = FakeTransport::default();
    transport.ok(
        "serde",
        &versions_body(&[("1.5.0", 5, false), ("1.4.0", 50, false)], now()),
    );
    transport.index_ok(
                "app",
                r#"{"vers":"1.0.0","deps":[{"name":"serde","req":"^1.5"},{"name":"serde","req":"^1","kind":"build"}]}"#,
            );
    let mut client = fast_client(transport);

    let violations = vec![too_new("serde", "1.5.0")];
    let packages = vec![
        pkg("serde", "1.5.0", &[]),
        pkg("app", "1.0.0", &[("serde", "1.5.0")]),
    ];
    let outcomes = suggestions(&mut client, &violations, &packages, &[], Path::new("/work"));

    match &outcomes[0] {
        Outcome::Blocked {
            newest_compliant,
            blocker,
            ..
        } => {
            assert_eq!(newest_compliant, "1.4.0");
            assert_eq!(blocker.name, "app");
            assert_eq!(blocker.req, "^1.5");
        }
        _ => panic!("expected serde to be Blocked by app's mandatory requirement"),
    }
}

#[test]
fn mandatory_requirement_still_blocks_alongside_uncertain_declaration() {
    // "app" declares an unconditional, nonoptional normal
    // requirement of `^1.5`, and a separate disabled, renamed
    // optional declaration matching a looser `^1`. The mandatory
    // requirement is enforced regardless of the uncertain one, even
    // though the uncertain one alone would have permitted the
    // downgrade.
    let transport = FakeTransport::default();
    transport.ok(
        "serde",
        &versions_body(&[("1.5.0", 5, false), ("1.4.0", 50, false)], now()),
    );
    transport.index_ok(
                "app",
                r#"{"vers":"1.0.0","deps":[{"name":"serde","req":"^1.5"},{"name":"serde_new","package":"serde","req":"^1","optional":true}]}"#,
            );
    let mut client = fast_client(transport);

    let violations = vec![too_new("serde", "1.5.0")];
    let packages = vec![
        pkg("serde", "1.5.0", &[]),
        pkg("app", "1.0.0", &[("serde", "1.5.0")]),
    ];
    let outcomes = suggestions(&mut client, &violations, &packages, &[], Path::new("/work"));

    match &outcomes[0] {
        Outcome::Blocked {
            newest_compliant,
            blocker,
            ..
        } => {
            assert_eq!(newest_compliant, "1.4.0");
            assert_eq!(blocker.name, "app");
            assert_eq!(blocker.req, "^1.5");
        }
        _ => panic!("expected serde to be Blocked by app's mandatory requirement"),
    }
}

#[test]
fn target_specific_requirement_is_always_enforced() {
    // "app" declares an unconditional `serde = "^1"` and a stricter
    // `serde = "^1.4"` under a target-specific table. Cargo's resolver
    // evaluates every target table regardless of the host platform, so
    // the target-specific requirement is just as mandatory as the
    // unconditional one and must be enforced alongside it.
    let transport = FakeTransport::default();
    transport.ok(
        "serde",
        &versions_body(&[("1.5.0", 5, false), ("1.3.0", 50, false)], now()),
    );
    transport.index_ok(
                "app",
                r#"{"vers":"1.0.0","deps":[{"name":"serde","req":"^1"},{"name":"serde","req":"^1.4","target":"cfg(unix)"}]}"#,
            );
    let mut client = fast_client(transport);

    let violations = vec![too_new("serde", "1.5.0")];
    let packages = vec![
        pkg("serde", "1.5.0", &[]),
        pkg("app", "1.0.0", &[("serde", "1.5.0")]),
    ];
    let outcomes = suggestions(&mut client, &violations, &packages, &[], Path::new("/work"));

    match &outcomes[0] {
        Outcome::Blocked {
            newest_compliant,
            blocker,
            ..
        } => {
            assert_eq!(newest_compliant, "1.3.0");
            assert_eq!(blocker.name, "app");
            assert_eq!(blocker.req, "^1.4");
        }
        _ => panic!("expected serde to be Blocked by app's target-specific requirement"),
    }
}

#[test]
fn identical_requirement_repeated_across_targets_still_blocks() {
    // "app" declares the same `serde = "^1.5"` requirement under two
    // target-specific tables (e.g. cfg(unix) and cfg(windows)), which
    // the index lists as two separate `deps` entries with identical
    // `req` strings. This is not the same situation as two distinct
    // declarations that might not both be active — the requirement
    // is identical either way, so it must still be enforced as a
    // definite blocker rather than merely "unverified".
    let transport = FakeTransport::default();
    transport.ok(
        "serde",
        &versions_body(&[("1.5.0", 5, false), ("1.4.0", 50, false)], now()),
    );
    transport.index_ok(
                "app",
                r#"{"vers":"1.0.0","deps":[{"name":"serde","req":"^1.5","target":"cfg(unix)"},{"name":"serde","req":"^1.5","target":"cfg(windows)"}]}"#,
            );
    let mut client = fast_client(transport);

    let violations = vec![too_new("serde", "1.5.0")];
    let packages = vec![
        pkg("serde", "1.5.0", &[]),
        pkg("app", "1.0.0", &[("serde", "1.5.0")]),
    ];
    let outcomes = suggestions(&mut client, &violations, &packages, &[], Path::new("/work"));

    match &outcomes[0] {
        Outcome::Blocked {
            newest_compliant,
            blocker,
            ..
        } => {
            assert_eq!(newest_compliant, "1.4.0");
            assert_eq!(blocker.name, "app");
            assert_eq!(blocker.req, "^1.5");
        }
        _ => panic!("expected serde to be Blocked by app's requirement, not merely unverified"),
    }
}

#[test]
fn requirement_attribution_acceptance_cases() {
    // name, second version, requirements, unverified, blocks 1.7
    let cases: &[(&str, bool, &[&str], bool, bool)] = &[
        ("overlap", true, &["^1", ">=1.8,<3"], true, false),
        ("disjoint", true, &["^1", "^2"], false, false),
        (
            "identical aliases",
            true,
            &[">=1.8,<3", ">=1.8, <3"],
            false,
            true,
        ),
        ("single version", false, &["^1", ">=1.8,<2"], false, true),
        ("optional", true, &["^1", ">=1.8,<3"], true, false),
        ("target", true, &["^1", ">=1.8,<3"], true, false),
        ("other blocker", true, &["^1", ">=1.8,<3"], true, true),
        ("other parent", false, &["^1", ">=1.8,<3"], false, true),
        ("other source", true, &["^1", ">=1.8,<3"], false, true),
        ("path sibling", false, &["^1", ">=1.8,<3"], false, true),
        ("duplicate edge", false, &["^1", ">=1.8,<3"], false, true),
        ("unreadable", false, &[], true, false),
    ];
    for registry in [true, false] {
        for &(case, second_version, reqs, unverified, blocked) in cases {
            if !registry && matches!(case, "optional" | "target") {
                continue;
            }
            let transport = FakeTransport::default();
            let mut requirements = Vec::new();
            let mut app = if registry {
                pkg("app", "1.0.0", &[("foo", "1.9.0")])
            } else {
                non_registry_pkg("app", "1.0.0", &[("foo", "1.9.0")])
            };
            let source = pkg("foo", "1.9.0", &[]).source.unwrap();
            app.dependencies[0].source = Some(source.clone());
            let mut packages = vec![pkg("foo", "1.9.0", &[])];
            if second_version {
                let mut sibling = pkg("foo", "2.0.0", &[]);
                if case == "other source" {
                    sibling = external_pkg("foo", "2.0.0", ALT_REGISTRY_SOURCE, &[]);
                }
                app.dependencies.push(PackageRef {
                    name: "foo".into(),
                    version: sibling.version.clone(),
                    source: sibling.source.clone(),
                });
                packages.push(sibling);
            }
            match case {
                "path sibling" => {
                    packages.push(non_registry_pkg("foo", "1.9.0", &[]));
                    app.dependencies.push(PackageRef {
                        name: "foo".into(),
                        version: "1.9.0".into(),
                        source: None,
                    });
                }
                "duplicate edge" => app.dependencies.push(PackageRef {
                    name: "foo".into(),
                    version: "1.9.0".into(),
                    source: Some(source.clone()),
                }),
                "other parent" => {
                    packages.push(pkg("foo", "2.0.0", &[]));
                    packages.push(pkg("other", "1.0.0", &[("foo", "2.0.0")]));
                }
                "other blocker" => {
                    if registry {
                        packages.push(pkg("other", "1.0.0", &[("foo", "1.9.0")]));
                        transport.index_ok(
                            "other",
                            r#"{"vers":"1.0.0","deps":[{"name":"foo","req":">=1.8"}]}"#,
                        );
                    } else {
                        packages.push(non_registry_pkg("other", "1.0.0", &[("foo", "1.9.0")]));
                        requirements.push(local_requirement("other", "1.0.0", "foo", ">=1.8"));
                    }
                }
                _ => {}
            }
            packages.push(app);
            if registry && case != "unreadable" {
                let deps: Vec<_> = reqs.iter().enumerate().map(|(i, req)| {
                            serde_json::json!({
                                "name": format!("alias_{i}"), "package": "foo", "req": req,
                                "optional": case == "optional" && i == 0,
                                "target": if case == "target" && i == 0 { Some("cfg(unix)") } else { None },
                            })
                        }).collect();
                transport.index_ok(
                    "app",
                    &serde_json::json!({
                        "vers": "1.0.0", "deps": deps,
                    })
                    .to_string(),
                );
            } else if registry {
                transport.index_error("app");
            } else {
                requirements.extend(
                    reqs.iter()
                        .map(|req| local_requirement("app", "1.0.0", "foo", req)),
                );
            }
            let mut client = fast_client(transport);
            let index = build_indexes(&packages).0;
            let gathered = gather_constraints(
                &mut client,
                &index,
                &requirements,
                Path::new("/work"),
                "foo",
                "1.9.0",
                Some(&source),
            );
            let expected_unverified = if unverified { vec!["app"] } else { vec![] };
            assert_eq!(
                gathered.unverified_dependents, expected_unverified,
                "{case}, registry={registry}"
            );
            if case == "disjoint" {
                assert!(
                    gathered
                        .constraints
                        .iter()
                        .any(|c| !c.req.matches(&v("0.9.0")))
                );
            }
            let result = walk(vec![(v("1.7.0"), 80)], gathered.constraints);
            assert_eq!(
                matches!(result, WalkResult::Blocked { .. }),
                blocked,
                "{case}, registry={registry}"
            );
            if case == "other blocker" {
                let WalkResult::Blocked { blocker, .. } = result else {
                    unreachable!()
                };
                assert_eq!(
                    blocker.blocker_name,
                    if registry { "other" } else { "Cargo.toml" }
                );
            } else if !blocked {
                assert!(
                    matches!(result, WalkResult::Suggest(version, 80) if version == v("1.7.0"))
                );
            }
        }
    }
}

#[test]
fn failed_index_fetch_yields_suggestion_with_unverified_annotation() {
    let transport = FakeTransport::default();
    transport.ok(
        "serde",
        &versions_body(&[("1.5.0", 5, false), ("1.4.0", 50, false)], now()),
    );
    transport.index_error("app");
    let mut client = fast_client(transport);

    let violations = vec![too_new("serde", "1.5.0")];
    let packages = vec![
        pkg("serde", "1.5.0", &[]),
        pkg("app", "1.0.0", &[("serde", "1.5.0")]),
    ];
    let outcomes = suggestions(&mut client, &violations, &packages, &[], Path::new("/work"));

    match &outcomes[0] {
        Outcome::Suggest {
            unverified_dependents,
            ..
        } => assert_eq!(unverified_dependents, &["app".to_string()]),
        _ => panic!("expected Suggest"),
    }
}

#[test]
fn a_package_locked_at_two_versions_produces_two_outcomes() {
    let transport = FakeTransport::default();
    transport.ok(
        "serde",
        &versions_body(&[("1.0.0", 50, false), ("2.0.0", 50, false)], now()),
    );
    let mut client = fast_client(transport);

    let violations = vec![too_new("serde", "1.0.0"), too_new("serde", "2.0.0")];
    let packages = vec![pkg("serde", "1.0.0", &[]), pkg("serde", "2.0.0", &[])];
    let outcomes = suggestions(&mut client, &violations, &packages, &[], Path::new("/work"));

    assert_eq!(outcomes.len(), 2);
}

#[test]
fn also_suggested_is_false_when_the_blocker_itself_has_no_suggestion() {
    // "y" blocks "target", and "y" is itself in the too-new set —
    // but y's own walk resolves to NoCompliantVersion, not Suggest,
    // so the blocked message must not claim a fix for y exists.
    let transport = FakeTransport::default();
    transport.ok(
        "target",
        &versions_body(&[("1.5.0", 5, false), ("1.4.0", 50, false)], now()),
    );
    transport.ok("y", &versions_body(&[("1.5.0", 5, false)], now()));
    transport.index_ok(
        "y",
        r#"{"vers":"1.5.0","deps":[{"name":"target","req":"^1.5"}]}"#,
    );
    let mut client = fast_client(transport);

    let violations = vec![too_new("target", "1.5.0"), too_new("y", "1.5.0")];
    let packages = vec![
        pkg("target", "1.5.0", &[]),
        pkg("y", "1.5.0", &[("target", "1.5.0")]),
    ];
    let outcomes = suggestions(&mut client, &violations, &packages, &[], Path::new("/work"));

    match &outcomes[0] {
        Outcome::Blocked { blocker, .. } => {
            assert_eq!(blocker.name, "y");
            assert!(
                !blocker.also_suggested,
                "y has no Suggest outcome of its own"
            );
        }
        _ => panic!("expected target to be Blocked"),
    }
    assert!(matches!(
        &outcomes[1],
        Outcome::NoCompliantVersion { package, .. } if package == "y"
    ));
}

#[test]
fn also_suggested_is_false_when_only_another_version_of_the_blocker_is_suggested() {
    // "foo" is locked at both 1.5.0 and 2.5.0. Only 1.5.0 resolves to
    // a Suggest; the 2.5.0 that blocks "target" has no compliant
    // version, so the blocked message must not point at the unrelated
    // 1.5.0 downgrade.
    let transport = FakeTransport::default();
    transport.ok(
        "target",
        &versions_body(&[("1.5.0", 5, false), ("1.4.0", 50, false)], now()),
    );
    transport.ok(
        "foo",
        &versions_body(
            &[
                ("1.5.0", 5, false),
                ("1.4.0", 50, false),
                ("2.5.0", 5, false),
            ],
            now(),
        ),
    );
    transport.index_ok(
        "foo",
        r#"{"vers":"2.5.0","deps":[{"name":"target","req":"^1.5"}]}"#,
    );
    let mut client = fast_client(transport);

    let violations = vec![
        too_new("target", "1.5.0"),
        too_new("foo", "1.5.0"),
        too_new("foo", "2.5.0"),
    ];
    let packages = vec![
        pkg("target", "1.5.0", &[]),
        pkg("foo", "1.5.0", &[]),
        pkg("foo", "2.5.0", &[("target", "1.5.0")]),
    ];
    let outcomes = suggestions(&mut client, &violations, &packages, &[], Path::new("/work"));

    assert!(
        matches!(&outcomes[1], Outcome::Suggest { package, locked_version, .. }
                    if package == "foo" && locked_version == "1.5.0"),
        "foo 1.5.0 should be suggested, otherwise the test proves nothing"
    );
    match &outcomes[0] {
        Outcome::Blocked { blocker, .. } => {
            assert_eq!(blocker.name, "foo");
            assert_eq!(blocker.version.as_deref(), Some("2.5.0"));
            assert!(
                !blocker.also_suggested,
                "the suggestion is for foo 1.5.0, which does not unblock foo 2.5.0"
            );
        }
        _ => panic!("expected target to be Blocked"),
    }
}

#[test]
fn manifest_constraint_is_scoped_to_the_declaring_dependent() {
    // member_a locks clap@2.5.0 and requires ^2; member_b locks a
    // different clap version and requires ^3. member_b's unrelated
    // requirement must not leak into member_a's constraint set.
    let transport = FakeTransport::default();
    transport.ok(
        "clap",
        &versions_body(&[("2.5.0", 5, false), ("2.0.0", 50, false)], now()),
    );
    let mut client = fast_client(transport);

    let direct_requirements = vec![
        crate::manifest::DirectRequirement {
            manifest: PathBuf::from("/work/member_a/Cargo.toml"),
            declaring_package: "member_a".to_string(),
            declaring_version: Some("0.1.0".to_string()),
            crate_name: "clap".to_string(),
            req: VersionReq::parse("^2").unwrap(),
            source: RequirementSource::CratesIo,
        },
        crate::manifest::DirectRequirement {
            manifest: PathBuf::from("/work/member_b/Cargo.toml"),
            declaring_package: "member_b".to_string(),
            declaring_version: Some("0.1.0".to_string()),
            crate_name: "clap".to_string(),
            req: VersionReq::parse("^3").unwrap(),
            source: RequirementSource::CratesIo,
        },
    ];
    let packages = vec![
        pkg("clap", "2.5.0", &[]),
        non_registry_pkg("member_a", "0.1.0", &[("clap", "2.5.0")]),
        non_registry_pkg("member_b", "0.1.0", &[("clap", "3.1.0")]),
    ];

    let violations = vec![too_new("clap", "2.5.0")];
    let outcomes = suggestions(
        &mut client,
        &violations,
        &packages,
        &direct_requirements,
        Path::new("/work"),
    );

    match &outcomes[0] {
        Outcome::Suggest {
            suggested_version, ..
        } => assert_eq!(suggested_version, "2.0.0"),
        _ => panic!("expected clap to be Suggest: member_b's ^3 must not apply"),
    }
}

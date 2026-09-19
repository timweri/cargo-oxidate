/// Covers a manifest crate name declared against two different
/// registries at once: an ordinary crates.io requirement and one
/// explicitly pinned to a renamed private registry. The renamed
/// declaration must never be treated as if it constrained the crates.io
/// package this flow actually suggests a downgrade for, nor may it
/// silently mark the declaring dependent as unverified when the
/// crates.io declaration alone already verifies it.
use super::*;
use crate::api::RetryPolicy;
use crate::api::test_support::{FakeTransport, ScriptedResponse, versions_url};
use crate::manifest::load_direct_requirements;
use crate::report::Aged;
use std::num::NonZeroU32;
use std::time::Duration;
use tempfile::tempdir;

const CRATES_IO_SOURCE: &str = "registry+https://github.com/rust-lang/crates.io-index";
const PRIVATE_SOURCE: &str = "registry+https://example.com/priv-index";

fn versions_body(entries: &[(&str, i64, bool)]) -> String {
    let versions: Vec<String> = entries
        .iter()
        .map(|(num, days_ago, yanked)| {
            let created_at = now() - chrono::Duration::days(*days_ago);
            format!(
                r#"{{"num":"{num}","created_at":"{}","yanked":{yanked}}}"#,
                created_at.to_rfc3339()
            )
        })
        .collect();
    format!(r#"{{"versions":[{}]}}"#, versions.join(","))
}

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

fn write_manifest(dir: &Path, contents: &str) {
    std::fs::write(dir.join("Cargo.toml"), contents).unwrap();
}

/// A "foo" package locked at 1.9.0 on each of `CRATES_IO_SOURCE` and
/// `PRIVATE_SOURCE`, both depended on by workspace member "app" via
/// source-qualified lockfile dependency edges — the shape a real
/// lockfile resolves to when a same-name/same-version package exists
/// on more than one registry.
fn packages_with_dual_source_foo() -> Vec<Package> {
    vec![
        Package {
            name: "foo".to_string(),
            version: "1.9.0".to_string(),
            is_registry: true,
            source: Some(CRATES_IO_SOURCE.to_string()),
            dependencies: vec![],
        },
        Package {
            name: "foo".to_string(),
            version: "1.9.0".to_string(),
            is_registry: false,
            source: Some(PRIVATE_SOURCE.to_string()),
            dependencies: vec![],
        },
        Package {
            name: "app".to_string(),
            version: "0.1.0".to_string(),
            is_registry: false,
            source: None,
            dependencies: vec![
                PackageRef {
                    name: "foo".to_string(),
                    version: "1.9.0".to_string(),
                    source: Some(CRATES_IO_SOURCE.to_string()),
                },
                PackageRef {
                    name: "foo".to_string(),
                    version: "1.9.0".to_string(),
                    source: Some(PRIVATE_SOURCE.to_string()),
                },
            ],
        },
    ]
}

#[test]
fn crates_io_declaration_is_enforced_while_the_renamed_registry_declaration_is_excluded() {
    // "app" depends on crates.io foo ^1.0 and a renamed
    // private-registry foo pinned to =1.9.0; both resolve to foo
    // 1.9.0 in the lockfile. Only the crates.io declaration should
    // count toward foo's suggestion: it doesn't block 1.8.0, so foo
    // should suggest it, and the =1.9.0 renamed declaration must not
    // spuriously block a package it was never written for.
    let dir = tempdir().unwrap();
    write_manifest(
        dir.path(),
        r#"
[package]
name = "app"
version = "0.1.0"

[dependencies]
foo = "^1.0"
foo_priv = { package = "foo", version = "=1.9.0", registry = "priv" }
"#,
    );
    let (direct_requirements, warnings) = load_direct_requirements(dir.path());
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");

    let transport = FakeTransport::default();
    transport.push(
        &versions_url("foo"),
        ScriptedResponse::Http(
            200,
            versions_body(&[("1.9.0", 5, false), ("1.8.0", 50, false)]),
        ),
    );
    let mut client = fast_client(transport);

    let violations = vec![too_new("foo", "1.9.0")];
    let packages = packages_with_dual_source_foo();
    let outcomes = suggestions(
        &mut client,
        &violations,
        &packages,
        &direct_requirements,
        dir.path(),
    );

    match &outcomes[0] {
        Outcome::Suggest {
            package_spec,
            suggested_version,
            unverified_dependents,
            ..
        } => {
            assert_eq!(suggested_version, "1.8.0");
            assert_eq!(package_spec, &format!("{CRATES_IO_SOURCE}#foo"));
            assert!(
                unverified_dependents.is_empty(),
                "app is verified by its crates.io ^1.0 declaration: {unverified_dependents:?}"
            );
        }
        Outcome::Blocked { blocker, .. } => panic!(
            "expected foo to be Suggest, but was Blocked by {} \
                     (the renamed-registry declaration must have leaked in)",
            blocker.name
        ),
        _ => panic!("expected foo to be Suggest"),
    }
}

#[test]
fn reverse_roles_still_block_via_the_crates_io_declaration() {
    // Same fixture, roles swapped: crates.io foo is now pinned to
    // =1.9.0 and the renamed private-registry foo carries the
    // lenient ^1.0. The crates.io pin must still block the
    // downgrade, reported as the blocker.
    let dir = tempdir().unwrap();
    write_manifest(
        dir.path(),
        r#"
[package]
name = "app"
version = "0.1.0"

[dependencies]
foo = "=1.9.0"
foo_priv = { package = "foo", version = "^1.0", registry = "priv" }
"#,
    );
    let (direct_requirements, warnings) = load_direct_requirements(dir.path());
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");

    let transport = FakeTransport::default();
    transport.push(
        &versions_url("foo"),
        ScriptedResponse::Http(
            200,
            versions_body(&[("1.9.0", 5, false), ("1.8.0", 50, false)]),
        ),
    );
    let mut client = fast_client(transport);

    let violations = vec![too_new("foo", "1.9.0")];
    let packages = packages_with_dual_source_foo();
    let outcomes = suggestions(
        &mut client,
        &violations,
        &packages,
        &direct_requirements,
        dir.path(),
    );

    match &outcomes[0] {
        Outcome::Blocked {
            newest_compliant,
            blocker,
            ..
        } => {
            assert_eq!(newest_compliant, "1.8.0");
            assert_eq!(blocker.name, "Cargo.toml");
            assert_eq!(blocker.version, None);
            assert_eq!(blocker.req, "=1.9.0");
        }
        _ => panic!("expected foo to be Blocked by the crates.io declaration"),
    }
}

#[test]
fn explicit_crates_io_registry_name_still_blocks() {
    // Same fixture as `reverse_roles_still_block_via_the_crates_io_declaration`,
    // but the crates.io declaration names its registry explicitly
    // via the reserved `crates-io` alias instead of omitting
    // `registry` altogether. It must still be recognized as
    // crates.io and enforced, not excluded as an unrecognized
    // alternate registry.
    let dir = tempdir().unwrap();
    write_manifest(
        dir.path(),
        r#"
[package]
name = "app"
version = "0.1.0"

[dependencies]
foo = { version = "=1.9.0", registry = "crates-io" }
foo_priv = { package = "foo", version = "^1.0", registry = "priv" }
"#,
    );
    let (direct_requirements, warnings) = load_direct_requirements(dir.path());
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");

    let transport = FakeTransport::default();
    transport.push(
        &versions_url("foo"),
        ScriptedResponse::Http(
            200,
            versions_body(&[("1.9.0", 5, false), ("1.8.0", 50, false)]),
        ),
    );
    let mut client = fast_client(transport);

    let violations = vec![too_new("foo", "1.9.0")];
    let packages = packages_with_dual_source_foo();
    let outcomes = suggestions(
        &mut client,
        &violations,
        &packages,
        &direct_requirements,
        dir.path(),
    );

    match &outcomes[0] {
        Outcome::Blocked {
            newest_compliant,
            blocker,
            ..
        } => {
            assert_eq!(newest_compliant, "1.8.0");
            assert_eq!(blocker.name, "Cargo.toml");
            assert_eq!(blocker.version, None);
            assert_eq!(blocker.req, "=1.9.0");
        }
        _ => panic!(
            "expected foo to be Blocked by the explicit crates-io declaration, \
                     not excluded as an unrecognized alternate registry"
        ),
    }
}

#[test]
fn a_second_crates_io_declaration_for_the_same_identity_still_blocks() {
    // Both declarations are ordinary crates.io dependencies — no
    // registry collision at all — one lenient, one restrictive.
    // Guards against the crates.io source filter collapsing
    // enforcement down to a single matching declaration.
    let dir = tempdir().unwrap();
    write_manifest(
        dir.path(),
        r#"
[package]
name = "app"
version = "0.1.0"

[dependencies]
foo = "^1.0"
foo_pinned = { package = "foo", version = "=1.9.0" }
"#,
    );
    let (direct_requirements, warnings) = load_direct_requirements(dir.path());
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    assert_eq!(direct_requirements.len(), 2);
    assert!(
        direct_requirements
            .iter()
            .all(|r| r.source == RequirementSource::CratesIo)
    );

    let transport = FakeTransport::default();
    transport.push(
        &versions_url("foo"),
        ScriptedResponse::Http(
            200,
            versions_body(&[("1.9.0", 5, false), ("1.8.0", 50, false)]),
        ),
    );
    let mut client = fast_client(transport);

    let violations = vec![too_new("foo", "1.9.0")];
    let packages = vec![
        Package {
            name: "foo".to_string(),
            version: "1.9.0".to_string(),
            is_registry: true,
            source: Some(CRATES_IO_SOURCE.to_string()),
            dependencies: vec![],
        },
        Package {
            name: "app".to_string(),
            version: "0.1.0".to_string(),
            is_registry: false,
            source: None,
            dependencies: vec![PackageRef {
                name: "foo".to_string(),
                version: "1.9.0".to_string(),
                source: Some(CRATES_IO_SOURCE.to_string()),
            }],
        },
    ];
    let outcomes = suggestions(
        &mut client,
        &violations,
        &packages,
        &direct_requirements,
        dir.path(),
    );

    match &outcomes[0] {
        Outcome::Blocked {
            newest_compliant,
            blocker,
            ..
        } => {
            assert_eq!(newest_compliant, "1.8.0");
            assert_eq!(blocker.req, "=1.9.0");
        }
        _ => panic!(
            "expected foo to be Blocked by the restrictive =1.9.0 declaration \
                     alongside the lenient ^1.0 one"
        ),
    }
}

/// "helper" is a path dependency of root "app", not a workspace
/// member. Cargo ignores the dev-dependencies of a non-member path
/// dependency, so helper's `[dev-dependencies] foo = "=1.9.0"` must
/// not block a downgrade that only helper's own `[dependencies] foo
/// = "1"` would allow.
#[test]
fn dev_dependency_of_a_non_member_path_dependency_does_not_block() {
    let dir = tempdir().unwrap();
    write_manifest(
        dir.path(),
        r#"
[package]
name = "app"
version = "0.1.0"

[dependencies]
helper = { path = "helper" }
"#,
    );
    std::fs::create_dir_all(dir.path().join("helper")).unwrap();
    write_manifest(
        &dir.path().join("helper"),
        r#"
[package]
name = "helper"
version = "0.1.0"

[dependencies]
foo = "1"

[dev-dependencies]
foo = "=1.9.0"
"#,
    );
    let (direct_requirements, warnings) = load_direct_requirements(dir.path());
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");

    let transport = FakeTransport::default();
    transport.push(
        &versions_url("foo"),
        ScriptedResponse::Http(
            200,
            versions_body(&[("1.9.0", 5, false), ("1.8.0", 50, false)]),
        ),
    );
    let mut client = fast_client(transport);

    let violations = vec![too_new("foo", "1.9.0")];
    let packages = vec![
        Package {
            name: "foo".to_string(),
            version: "1.9.0".to_string(),
            is_registry: true,
            source: Some(CRATES_IO_SOURCE.to_string()),
            dependencies: vec![],
        },
        Package {
            name: "helper".to_string(),
            version: "0.1.0".to_string(),
            is_registry: false,
            source: None,
            dependencies: vec![PackageRef {
                name: "foo".to_string(),
                version: "1.9.0".to_string(),
                source: Some(CRATES_IO_SOURCE.to_string()),
            }],
        },
        Package {
            name: "app".to_string(),
            version: "0.1.0".to_string(),
            is_registry: false,
            source: None,
            dependencies: vec![PackageRef {
                name: "helper".to_string(),
                version: "0.1.0".to_string(),
                source: None,
            }],
        },
    ];
    let outcomes = suggestions(
        &mut client,
        &violations,
        &packages,
        &direct_requirements,
        dir.path(),
    );

    match &outcomes[0] {
        Outcome::Suggest {
            suggested_version, ..
        } => {
            assert_eq!(suggested_version, "1.8.0");
        }
        Outcome::Blocked { blocker, .. } => panic!(
            "expected foo to be Suggest, but was Blocked by {} \
                     (helper's dev-dependency must be ignored: it isn't a workspace member)",
            blocker.name
        ),
        _ => panic!("expected foo to be Suggest"),
    }
}

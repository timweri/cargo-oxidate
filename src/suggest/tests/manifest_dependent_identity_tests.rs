/// Covers matching a real local dependent's manifest requirement against
/// its own locked identity (name and version), including workspace
/// version inheritance, rather than name alone.
use super::*;
use crate::api::RetryPolicy;
use crate::api::test_support::{FakeTransport, ScriptedResponse, versions_url};
use crate::manifest::load_direct_requirements;
use crate::report::Aged;
use std::num::NonZeroU32;
use std::time::Duration;
use tempfile::tempdir;

const CRATES_IO_SOURCE: &str = "registry+https://github.com/rust-lang/crates.io-index";
const GIT_SOURCE: &str =
    "git+https://github.com/example/vendor#0000000000000000000000000000000000000000";
const ALT_REGISTRY_SOURCE: &str = "registry+https://example.com/priv-index";

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
            published: now() - chrono::Duration::days(5),
            age_days: 5,
        }),
    }
}

fn write_manifest(dir: &Path, rel: &str, contents: &str) {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&path, contents).unwrap();
}

#[test]
fn workspace_inherited_declaring_version_matches_the_locked_local_dependent() {
    // "app"'s own version is inherited from the workspace root
    // rather than written directly. Its requirement on foo must
    // still be recognized as its own — matched by the resolved
    // version, not skipped for lack of one.
    let dir = tempdir().unwrap();
    write_manifest(
        dir.path(),
        "Cargo.toml",
        r#"
[workspace]
members = ["app"]

[workspace.package]
version = "0.1.0"
"#,
    );
    write_manifest(
        dir.path(),
        "app/Cargo.toml",
        r#"
[package]
name = "app"
version.workspace = true

[dependencies]
foo = "^1.0"
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
        Outcome::Suggest {
            suggested_version,
            unverified_dependents,
            ..
        } => {
            assert_eq!(suggested_version, "1.8.0");
            assert!(
                unverified_dependents.is_empty(),
                "app's workspace-inherited version should have matched: {unverified_dependents:?}"
            );
        }
        _ => panic!("expected foo to be Suggest"),
    }
}

#[test]
fn unresolvable_declaring_version_does_not_falsely_verify_the_dependent() {
    // "app" inherits its version from the workspace, but the
    // workspace root supplies none — the member's manifest fails to
    // load entirely, so no requirement is ever collected for it.
    // "app" must come back unverified, not silently treated as
    // matching by name alone.
    let dir = tempdir().unwrap();
    write_manifest(
        dir.path(),
        "Cargo.toml",
        r#"
[workspace]
members = ["app"]
"#,
    );
    write_manifest(
        dir.path(),
        "app/Cargo.toml",
        r#"
[package]
name = "app"
version.workspace = true

[dependencies]
foo = "^1.0"
"#,
    );
    let (direct_requirements, warnings) = load_direct_requirements(dir.path());
    assert!(
        !warnings.is_empty(),
        "expected a warning about app's unresolved workspace version"
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
        Outcome::Suggest {
            unverified_dependents,
            ..
        } => assert_eq!(unverified_dependents, &["app".to_string()]),
        _ => panic!("expected foo to be Suggest, with app marked unverified"),
    }
}

#[test]
fn real_local_constraint_is_enforced_while_an_external_dependent_stays_unverified() {
    // "app" is a real local dependent whose manifest permits the
    // downgrade; "vendor" is a git dependent that also locks foo at
    // the same version, but nothing here can read its requirement.
    // 1.8.0 is the newer, otherwise-preferred candidate, but app's
    // manifest rejects it, so enforcement must fall back to 1.8.5
    // while still flagging vendor as unverified.
    let dir = tempdir().unwrap();
    write_manifest(
        dir.path(),
        "Cargo.toml",
        r#"
[package]
name = "app"
version = "0.1.0"

[dependencies]
foo = "^1.8.5"
"#,
    );
    let (direct_requirements, warnings) = load_direct_requirements(dir.path());
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");

    let transport = FakeTransport::default();
    transport.push(
        &versions_url("foo"),
        ScriptedResponse::Http(
            200,
            versions_body(&[
                ("1.9.0", 5, false),
                ("1.8.0", 40, false),
                ("1.8.5", 50, false),
            ]),
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
        Package {
            name: "vendor".to_string(),
            version: "1.0.0".to_string(),
            is_registry: false,
            source: Some(GIT_SOURCE.to_string()),
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
        Outcome::Suggest {
            suggested_version,
            unverified_dependents,
            ..
        } => {
            assert_eq!(suggested_version, "1.8.5");
            assert_eq!(unverified_dependents, &["vendor".to_string()]);
        }
        _ => panic!("expected foo to be Suggest, with vendor marked unverified"),
    }
}

#[test]
fn git_dependent_sharing_a_local_packages_identity_is_not_verified_by_its_manifest() {
    // A git "app" and the local "app" share name and version; the
    // local manifest permits the downgrade but was never the git
    // dependent's own, so it must not verify it.
    let dir = tempdir().unwrap();
    write_manifest(
        dir.path(),
        "Cargo.toml",
        r#"
[package]
name = "app"
version = "1.0.0"

[dependencies]
foo = "^1.0"
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
            name: "app".to_string(),
            version: "1.0.0".to_string(),
            is_registry: false,
            source: Some(GIT_SOURCE.to_string()),
            dependencies: vec![PackageRef {
                name: "foo".to_string(),
                version: "1.9.0".to_string(),
                source: Some(CRATES_IO_SOURCE.to_string()),
            }],
        },
        Package {
            name: "app".to_string(),
            version: "1.0.0".to_string(),
            is_registry: false,
            source: None,
            dependencies: vec![],
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
            suggested_version,
            unverified_dependents,
            ..
        } => {
            assert_eq!(suggested_version, "1.8.0");
            assert_eq!(unverified_dependents, &["app".to_string()]);
        }
        _ => panic!("expected foo to be Suggest, with the git app marked unverified"),
    }
}

#[test]
fn alternate_registry_dependent_sharing_a_local_packages_identity_is_not_verified_by_its_manifest()
{
    // An alternate-registry "app" and the local "app" share name and
    // version; the local manifest permits the downgrade but was
    // never the alternate-registry dependent's own, so it must not
    // verify it.
    let dir = tempdir().unwrap();
    write_manifest(
        dir.path(),
        "Cargo.toml",
        r#"
[package]
name = "app"
version = "1.0.0"

[dependencies]
foo = "^1.0"
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
            name: "app".to_string(),
            version: "1.0.0".to_string(),
            is_registry: false,
            source: Some(ALT_REGISTRY_SOURCE.to_string()),
            dependencies: vec![PackageRef {
                name: "foo".to_string(),
                version: "1.9.0".to_string(),
                source: Some(CRATES_IO_SOURCE.to_string()),
            }],
        },
        Package {
            name: "app".to_string(),
            version: "1.0.0".to_string(),
            is_registry: false,
            source: None,
            dependencies: vec![],
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
            suggested_version,
            unverified_dependents,
            ..
        } => {
            assert_eq!(suggested_version, "1.8.0");
            assert_eq!(unverified_dependents, &["app".to_string()]);
        }
        _ => {
            panic!("expected foo to be Suggest, with the alternate-registry app marked unverified")
        }
    }
}

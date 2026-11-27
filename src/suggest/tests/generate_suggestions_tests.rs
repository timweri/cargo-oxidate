use super::*;

const CRATES_IO_SOURCE: &str = "registry+https://github.com/rust-lang/crates.io-index";
const GIT_SOURCE: &str =
    "git+https://github.com/example/app#0000000000000000000000000000000000000000";
const ALT_REGISTRY_SOURCE: &str = "registry+https://example.com/priv-index";

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

/// A local/path package (`source: None`).
fn non_registry_pkg(name: &str, version: &str, deps: &[(&str, &str)]) -> Package {
    Package {
        is_registry: false,
        source: None,
        ..pkg(name, version, deps)
    }
}

/// A git or alternate-registry package: nothing here can read its
/// requirements, neither the crates.io index nor a local manifest.
fn external_pkg(name: &str, version: &str, source: &str, deps: &[(&str, &str)]) -> Package {
    Package {
        is_registry: false,
        source: Some(source.to_string()),
        ..pkg(name, version, deps)
    }
}

/// A `DirectRequirement` naming `declaring_package`/`declaring_version`
/// as the identity of the manifest that placed it.
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

/// Calls production `gather_constraints` for crates.io `foo` at
/// `locked_version`, with a fixed `/work` working dir.
fn gather(
    transport: FakeTransport,
    packages: &[Package],
    requirements: &[DirectRequirement],
    locked_version: &str,
) -> GatheredConstraints {
    let (dependents_index, name_version_index) = build_indexes(packages);
    gather_constraints(
        &mut fast_client(transport, None),
        &dependents_index,
        &name_version_index,
        requirements,
        Path::new("/work"),
        "foo",
        locked_version,
        Some(CRATES_IO_SOURCE),
    )
}

fn reqs(gathered: &GatheredConstraints) -> Vec<String> {
    gathered
        .constraints
        .iter()
        .map(|c| c.req.to_string())
        .collect()
}

#[test]
fn ignores_violations_that_are_not_too_new() {
    let too_old = Violation {
        package: "syn".to_string(),
        version: "1.0.0".to_string(),
        kind: ViolationKind::TooOld(Aged {
            published: now(),
            age_days: 1000,
        }),
    };
    let mut no_progress = |_| {};
    let outcomes = generate_suggestions(
        &mut fast_client(FakeTransport::default(), None),
        std::slice::from_ref(&too_old),
        &[],
        &[],
        Path::new("/work"),
        MIN_AGE_30,
        false,
        now(),
        &mut no_progress,
    );
    assert!(outcomes.is_empty());

    // "syn" has no scripted response: fetching it would panic.
    let transport = FakeTransport::default();
    transport.versions("serde", &[("1.0.0", 50)]);
    let violations = [too_new("serde", "1.0.0"), too_old];
    let packages = [pkg("serde", "1.0.0", &[])];
    let outcomes = suggestions(transport, &violations, &packages, &[], Path::new("/work"));
    assert_eq!(summaries(&outcomes), ["serde: no compliant version"]);
}

#[test]
fn a_bad_violation_does_not_abort_the_others() {
    let transport = FakeTransport::default();
    // "serde" has no scripted response: its unparsable locked version
    // must be skipped before its versions are fetched.
    transport.versions_error("down");
    transport.versions("syn", &[("1.0.0", 50), ("1.1.0", 5)]);
    let violations = [
        too_new("serde", "not-a-version"),
        too_new("down", "1.0.0"),
        too_new("syn", "1.1.0"),
    ];
    let packages = [
        pkg("serde", "not-a-version", &[]),
        pkg("down", "1.0.0", &[]),
        pkg("syn", "1.1.0", &[]),
    ];
    let mut progress = Vec::new();
    let outcomes = generate_suggestions(
        &mut fast_client(transport, None),
        &violations,
        &packages,
        &[],
        Path::new("/work"),
        MIN_AGE_30,
        false,
        now(),
        &mut |p: SuggestionProgress| {
            progress.push(format!("{}/{} {}", p.current, p.total, p.package))
        },
    );
    let outcomes = summaries(&outcomes);

    assert_eq!(progress, ["1/3 serde", "2/3 down", "3/3 syn"]);
    assert_eq!(
        outcomes[0],
        "serde: unavailable (Locked version is not valid SemVer)"
    );
    assert!(outcomes[1].starts_with("down: unavailable"), "{outcomes:?}");
    assert_eq!(outcomes[2], "syn: suggest 1.0.0 (50d)");
}

#[test]
fn registry_dependent_declarations_decide_the_outcome() {
    // "app" 1.0.0 locks serde 1.5.0 and declares `deps` in its index
    // record; serde 1.4.0 is the only downgrade on offer.
    let cases = [
        (
            "plain requirement above the downgrade blocks it",
            r#"[{"name":"serde","req":"^1.5"}]"#,
            "serde: blocked at 1.4.0 by app@1.0.0 ^1.5",
        ),
        (
            "dev declaration is ignored",
            r#"[{"name":"serde","req":"^1.5","kind":"dev"}]"#,
            "serde: suggest 1.4.0 (50d) unverified: app",
        ),
        (
            "renamed dependency is matched by its real name",
            r#"[{"name":"my_serde","package":"serde","req":"^1.5"}]"#,
            "serde: blocked at 1.4.0 by app@1.0.0 ^1.5",
        ),
        (
            "alternate-registry declaration is not enforced",
            r#"[{"name":"serde","req":"^1"},{"name":"serde_alt","package":"serde","req":"^1.5","registry":"alt"}]"#,
            "serde: suggest 1.4.0 (50d)",
        ),
        (
            "optional declaration beside a mandatory one is unverified",
            r#"[{"name":"serde","req":"^1"},{"name":"serde_new","package":"serde","req":"^1.5","optional":true}]"#,
            "serde: suggest 1.4.0 (50d) unverified: app",
        ),
        (
            "unique optional declaration still blocks",
            r#"[{"name":"serde_new","package":"serde","req":"^1.5","optional":true}]"#,
            "serde: blocked at 1.4.0 by app@1.0.0 ^1.5",
        ),
        (
            "mandatory requirement still blocks beside an optional one",
            r#"[{"name":"serde","req":"^1.5"},{"name":"serde_new","package":"serde","req":"^1","optional":true}]"#,
            "serde: blocked at 1.4.0 by app@1.0.0 ^1.5",
        ),
        (
            "requirement declared optional and mandatory counts as mandatory",
            r#"[{"name":"serde_new","package":"serde","req":"^1.5","optional":true},{"name":"serde","req":"^1.5"},{"name":"serde_any","package":"serde","req":"^1"}]"#,
            "serde: blocked at 1.4.0 by app@1.0.0 ^1.5",
        ),
        (
            "unparsable requirement beside an enforced one is unverified",
            r#"[{"name":"serde","req":"^1"},{"name":"serde_bad","package":"serde","req":"not a requirement"}]"#,
            "serde: suggest 1.4.0 (50d) unverified: app",
        ),
        (
            "build declaration is enforced",
            r#"[{"name":"serde","req":"^1"},{"name":"serde","req":"^1.5","kind":"build"}]"#,
            "serde: blocked at 1.4.0 by app@1.0.0 ^1.5",
        ),
        (
            "target-specific declaration is enforced",
            r#"[{"name":"serde","req":"^1"},{"name":"serde","req":"^1.5","target":"cfg(unix)"}]"#,
            "serde: blocked at 1.4.0 by app@1.0.0 ^1.5",
        ),
        (
            "identical requirement across targets still blocks",
            r#"[{"name":"serde","req":"^1.5","target":"cfg(unix)"},{"name":"serde","req":"^1.5","target":"cfg(windows)"}]"#,
            "serde: blocked at 1.4.0 by app@1.0.0 ^1.5",
        ),
    ];
    for (case, deps, expected) in cases {
        let transport = FakeTransport::default();
        transport.versions("serde", &[("1.5.0", 5), ("1.4.0", 50)]);
        transport.index_ok("app", &format!(r#"{{"vers":"1.0.0","deps":{deps}}}"#));
        let packages = [
            pkg("serde", "1.5.0", &[]),
            pkg("app", "1.0.0", &[("serde", "1.5.0")]),
        ];
        let violations = [too_new("serde", "1.5.0")];
        let outcomes = suggestions(transport, &violations, &packages, &[], Path::new("/work"));
        assert_eq!(summary(&outcomes[0]), expected, "{case}");
    }
}

#[test]
fn a_same_name_version_package_from_another_source_does_not_leak_dependents() {
    // A crates.io serde and a twin at the same version (git, or a path
    // package with no source). "consumer" depends on the twin only; its
    // scripted ^1.5 requirement would block the crates.io serde if it
    // leaked. Cargo rejects the abbreviated `serde@1.5.0` here, so the
    // spec must be source-qualified.
    for twin_source in [Some(GIT_SOURCE), None] {
        let transport = FakeTransport::default();
        transport.versions("serde", &[("1.5.0", 5), ("1.4.0", 50)]);
        transport.index_ok(
            "consumer",
            r#"{"vers":"1.0.0","deps":[{"name":"serde","req":"^1.5"}]}"#,
        );
        let twin = Package {
            is_registry: false,
            source: twin_source.map(str::to_string),
            ..pkg("serde", "1.5.0", &[])
        };
        let mut consumer = pkg("consumer", "1.0.0", &[("serde", "1.5.0")]);
        consumer.dependencies[0].source = twin_source.map(str::to_string);
        let packages = [pkg("serde", "1.5.0", &[]), twin, consumer];
        let violations = [too_new("serde", "1.5.0")];
        let outcomes = suggestions(transport, &violations, &packages, &[], Path::new("/work"));

        assert_eq!(
            summary(&outcomes[0]),
            format!("serde: suggest 1.4.0 (50d) spec={CRATES_IO_SOURCE}#serde"),
            "twin source {twin_source:?}"
        );
    }
}

#[test]
fn same_name_version_registries_keep_their_dependents_and_manifest_requirements_apart() {
    // crates.io `anyhow 1.0.102` is too new and so is a corporate-registry
    // copy at the same version. `app` depends on the copies per row, and
    // declares `anyhow` once for each registry. Only the crates.io
    // declaration may constrain the crates.io copy, and `app` may only count
    // as its dependent when it really depends on it.
    let corp = |req| DirectRequirement {
        source: RequirementSource::Registry("corp".to_string()),
        ..local_requirement("app", "0.1.0", "anyhow", req)
    };
    let crates_io = |req| local_requirement("app", "0.1.0", "anyhow", req);
    let spec = format!("spec={CRATES_IO_SOURCE}#anyhow");
    let cases = [
        (
            "issue reproduction: app uses only the corporate copy",
            vec![ALT_REGISTRY_SOURCE],
            vec![corp("=1.0.102")],
            format!("anyhow: suggest 1.0.100 (50d) {spec}"),
        ),
        (
            "app uses both copies, the corporate one pinned",
            vec![CRATES_IO_SOURCE, ALT_REGISTRY_SOURCE],
            vec![crates_io("^1.0"), corp("=1.0.102")],
            format!("anyhow: suggest 1.0.100 (50d) {spec}"),
        ),
        (
            "control: the crates.io declaration is the pinned one",
            vec![CRATES_IO_SOURCE, ALT_REGISTRY_SOURCE],
            vec![crates_io("=1.0.102"), corp("^1.0")],
            format!("anyhow: blocked at 1.0.100 by Cargo.toml =1.0.102 {spec}"),
        ),
    ];
    for (case, app_edge_sources, requirements, expected) in cases {
        let transport = FakeTransport::default();
        transport.versions("anyhow", &[("1.0.102", 1), ("1.0.100", 50)]);
        transport.index_ok(
            "reqwest",
            r#"{"vers":"0.12.0","deps":[{"name":"anyhow","req":"^1.0"}]}"#,
        );
        let mut app = non_registry_pkg("app", "0.1.0", &[]);
        app.dependencies = app_edge_sources
            .iter()
            .map(|source| PackageRef {
                name: "anyhow".to_string(),
                version: "1.0.102".to_string(),
                source: Some(source.to_string()),
            })
            .collect();
        let packages = [
            pkg("anyhow", "1.0.102", &[]),
            external_pkg("anyhow", "1.0.102", ALT_REGISTRY_SOURCE, &[]),
            pkg("reqwest", "0.12.0", &[("anyhow", "1.0.102")]),
            app,
        ];
        let violations = [too_new("anyhow", "1.0.102")];
        let outcomes = suggestions(
            transport,
            &violations,
            &packages,
            &requirements,
            Path::new("/work"),
        );
        assert_eq!(summaries(&outcomes), [expected], "{case}");
    }
}

#[test]
fn a_blocker_duplicated_at_the_same_name_version_is_source_qualified() {
    // crates.io `app 1.0.0` pins serde; a git `app 1.0.0` shares its name and
    // version, so naming the blocker bare would be ambiguous to Cargo.
    let transport = FakeTransport::default();
    transport.versions("serde", &[("1.5.0", 5), ("1.4.0", 50)]);
    transport.index_ok(
        "app",
        r#"{"vers":"1.0.0","deps":[{"name":"serde","req":"^1.5"}]}"#,
    );
    let packages = [
        pkg("serde", "1.5.0", &[]),
        pkg("app", "1.0.0", &[("serde", "1.5.0")]),
        external_pkg("app", "1.0.0", GIT_SOURCE, &[]),
    ];
    let violations = [too_new("serde", "1.5.0")];
    let outcomes = suggestions(transport, &violations, &packages, &[], Path::new("/work"));

    assert_eq!(
        summaries(&outcomes),
        [format!(
            "serde: blocked at 1.4.0 by app@1.0.0 ^1.5 blocker_spec={CRATES_IO_SOURCE}#app"
        )]
    );
}

#[test]
fn also_suggested_stays_keyed_by_name_and_version_when_the_blocker_is_duplicated() {
    // The crates.io `app 1.1.0` blocks serde and has its own suggestion; a
    // git `app 1.1.0` shares its name and version. The block still points at
    // the app suggestion, and the blocker is source-qualified.
    let transport = FakeTransport::default();
    transport.versions("serde", &[("1.5.0", 5), ("1.4.0", 50)]);
    transport.versions("app", &[("1.1.0", 5), ("1.0.0", 50)]);
    transport.index_ok(
        "app",
        r#"{"vers":"1.1.0","deps":[{"name":"serde","req":"^1.5"}]}"#,
    );
    let packages = [
        pkg("serde", "1.5.0", &[]),
        pkg("app", "1.1.0", &[("serde", "1.5.0")]),
        external_pkg("app", "1.1.0", GIT_SOURCE, &[]),
    ];
    let violations = [too_new("serde", "1.5.0"), too_new("app", "1.1.0")];
    let outcomes = suggestions(transport, &violations, &packages, &[], Path::new("/work"));

    assert_eq!(
        summaries(&outcomes),
        [
            format!(
                "serde: blocked at 1.4.0 by app@1.1.0 ^1.5 (also suggested) blocker_spec={CRATES_IO_SOURCE}#app"
            ),
            format!("app: suggest 1.0.0 (50d) spec={CRATES_IO_SOURCE}#app"),
        ]
    );
}

#[test]
fn unavailable_outcome_for_a_duplicated_package_carries_the_qualified_spec() {
    // The invalid-SemVer early exit must still report the ambiguous spec.
    let packages = [
        pkg("anyhow", "not-semver", &[]),
        external_pkg("anyhow", "not-semver", ALT_REGISTRY_SOURCE, &[]),
    ];
    let violations = [too_new("anyhow", "not-semver")];
    let outcomes = suggestions(
        FakeTransport::default(),
        &violations,
        &packages,
        &[],
        Path::new("/work"),
    );
    assert_eq!(
        summaries(&outcomes),
        [format!(
            "anyhow: unavailable (Locked version is not valid SemVer) spec={CRATES_IO_SOURCE}#anyhow"
        )]
    );
}

#[test]
fn unavailable_outcome_for_a_failed_versions_fetch_carries_the_qualified_spec() {
    // The fetch-error arm must also report the ambiguous spec.
    let transport = FakeTransport::default();
    transport.versions_error("anyhow");
    let packages = [
        pkg("anyhow", "1.0.102", &[]),
        external_pkg("anyhow", "1.0.102", ALT_REGISTRY_SOURCE, &[]),
    ];
    let violations = [too_new("anyhow", "1.0.102")];
    let outcomes = suggestions(transport, &violations, &packages, &[], Path::new("/work"));
    assert_eq!(
        summaries(&outcomes),
        [format!(
            "anyhow: unavailable (Network error fetching versions for anyhow: connection timed out) spec={CRATES_IO_SOURCE}#anyhow"
        )]
    );
}

#[test]
fn unmatched_registry_requirements_are_unverified() {
    let transport = FakeTransport::default();
    transport.index_ok(
        "app",
        r#"{"vers":"1.0.0","deps":[{"name":"foo","req":"^1.5"}]}"#,
    );
    let packages = [pkg("app", "1.0.0", &[("foo", "1.4.0")])];
    let gathered = gather(transport, &packages, &[], "1.4.0");
    assert!(gathered.constraints.is_empty());
    assert_eq!(gathered.unverified_dependents, ["app"]);
}

#[test]
fn git_dependents_are_reported_once_per_name() {
    let packages = [
        external_pkg("parent", "1.0.0", GIT_SOURCE, &[("foo", "1.9.0")]),
        external_pkg("parent", "2.0.0", GIT_SOURCE, &[("foo", "1.9.0")]),
        external_pkg("parent-b", "1.0.0", GIT_SOURCE, &[("foo", "1.9.0")]),
    ];
    let gathered = gather(FakeTransport::default(), &packages, &[], "1.9.0");
    assert_eq!(gathered.unverified_dependents, ["parent", "parent-b"]);
}

#[test]
fn an_ambiguous_unverified_dependent_is_labelled_with_its_spec() {
    // crates.io `app 1.0.0` is verified through the index. The git `app 1.0.0`
    // shares its name and version, so its label must say which one it is. The
    // git source's commit fragment must not survive into the label, or it
    // would not be a valid pkgid.
    let transport = FakeTransport::default();
    transport.index_ok(
        "app",
        r#"{"vers":"1.0.0","deps":[{"name":"foo","req":"^1.5"}]}"#,
    );
    let packages = [
        pkg("app", "1.0.0", &[("foo", "1.9.0")]),
        external_pkg("app", "1.0.0", GIT_SOURCE, &[("foo", "1.9.0")]),
    ];
    let gathered = gather(transport, &packages, &[], "1.9.0");
    assert_eq!(reqs(&gathered), ["^1.5"]);
    assert_eq!(
        gathered.unverified_dependents,
        ["git+https://github.com/example/app#app"]
    );
}

#[test]
fn unverified_dependents_are_deduplicated_by_label() {
    // Two versions of an unambiguous git dependent collapse to its name; the
    // same name and version from two distinct sources stay distinct.
    let other_git = "git+https://github.com/example/other#1111111111111111111111111111111111111111";
    let packages = [
        external_pkg("parent", "1.0.0", GIT_SOURCE, &[("foo", "1.9.0")]),
        external_pkg("parent", "2.0.0", GIT_SOURCE, &[("foo", "1.9.0")]),
        external_pkg("twin", "1.0.0", GIT_SOURCE, &[("foo", "1.9.0")]),
        external_pkg("twin", "1.0.0", other_git, &[("foo", "1.9.0")]),
    ];
    let gathered = gather(FakeTransport::default(), &packages, &[], "1.9.0");
    assert_eq!(
        gathered.unverified_dependents,
        [
            "parent",
            "git+https://github.com/example/app#twin",
            "git+https://github.com/example/other#twin",
        ]
    );
}

#[test]
fn a_git_blockers_spec_has_a_single_fragment() {
    let blocker = Constraint {
        blocker_name: "app".to_string(),
        blocker_version: Some("1.0.0".to_string()),
        blocker_source: Some(GIT_SOURCE.to_string()),
        req: VersionReq::parse("^1.5").unwrap(),
    };
    let packages = [
        pkg("app", "1.0.0", &[]),
        external_pkg("app", "1.0.0", GIT_SOURCE, &[]),
    ];
    let spec = blocker_package_spec(&blocker, &build_indexes(&packages).1);
    assert_eq!(
        spec.as_deref(),
        Some("git+https://github.com/example/app#app")
    );
}

#[test]
fn external_dependent_sharing_a_local_packages_identity_stays_unverified() {
    // The local "app" manifest's requirement was never the external
    // "app" dependent's own, so it neither verifies nor constrains it.
    // The two share a name and version, so the external one is labelled by spec.
    for (source, label) in [
        (GIT_SOURCE, "git+https://github.com/example/app#app"),
        (
            ALT_REGISTRY_SOURCE,
            "registry+https://example.com/priv-index#app",
        ),
    ] {
        let packages = [
            external_pkg("app", "1.0.0", source, &[("foo", "1.5.0")]),
            non_registry_pkg("app", "1.0.0", &[]),
        ];
        let requirements = [local_requirement("app", "1.0.0", "foo", "^1.5")];
        let gathered = gather(FakeTransport::default(), &packages, &requirements, "1.5.0");
        assert!(gathered.constraints.is_empty(), "{source}");
        assert_eq!(gathered.unverified_dependents, [label], "{source}");
    }
}

#[test]
fn local_requirements_are_enforced_while_an_external_dependent_stays_unverified() {
    let packages = [
        non_registry_pkg("app", "0.1.0", &[("foo", "1.9.0")]),
        external_pkg("vendor", "1.0.0", GIT_SOURCE, &[("foo", "1.9.0")]),
    ];
    let requirements = [local_requirement("app", "0.1.0", "foo", "^1.8.5")];
    let gathered = gather(FakeTransport::default(), &packages, &requirements, "1.9.0");
    assert_eq!(reqs(&gathered), ["^1.8.5"]);
    assert_eq!(gathered.unverified_dependents, ["vendor"]);
}

#[test]
fn local_declaring_version_mismatch_neither_verifies_nor_constrains() {
    // A local "app" 2.0.0 declares a restrictive requirement on "foo",
    // but the actual lockfile dependent is a *different* "app" — 1.0.0.
    // Declaring package name alone must not be enough to apply it.
    let packages = [non_registry_pkg("app", "1.0.0", &[("foo", "1.5.0")])];
    let requirements = [local_requirement("app", "2.0.0", "foo", "^1.5")];
    let gathered = gather(FakeTransport::default(), &packages, &requirements, "1.5.0");
    assert!(gathered.constraints.is_empty());
    assert_eq!(gathered.unverified_dependents, ["app"]);
}

#[test]
fn only_crates_io_declarations_constrain_the_crates_io_package() {
    // "app" declares foo on crates.io and, renamed, on a private registry.
    let private = |req| DirectRequirement {
        source: RequirementSource::Registry("priv".to_string()),
        ..local_requirement("app", "0.1.0", "foo", req)
    };
    let packages = [non_registry_pkg("app", "0.1.0", &[("foo", "1.9.0")])];
    for (crates_io_req, private_req) in [("^1.0", "=1.9.0"), ("=1.9.0", "^1.0")] {
        let requirements = [
            local_requirement("app", "0.1.0", "foo", crates_io_req),
            private(private_req),
        ];
        let gathered = gather(FakeTransport::default(), &packages, &requirements, "1.9.0");
        assert_eq!(reqs(&gathered), [crates_io_req]);
        assert!(gathered.unverified_dependents.is_empty());
    }
}

#[test]
fn manifest_constraint_is_scoped_to_the_declaring_dependent() {
    // member_a locks foo 2.5.0 and requires ^2; member_b locks a
    // different foo version and requires ^2.4. member_b's requirement also
    // matches 2.5.0, but it must not leak into member_a's constraint set.
    let packages = [
        non_registry_pkg("member_a", "0.1.0", &[("foo", "2.5.0")]),
        non_registry_pkg("member_b", "0.1.0", &[("foo", "2.6.0")]),
    ];
    let requirements = [
        local_requirement("member_a", "0.1.0", "foo", "^2"),
        local_requirement("member_b", "0.1.0", "foo", "^2.4"),
    ];
    let gathered = gather(FakeTransport::default(), &packages, &requirements, "2.5.0");
    assert_eq!(reqs(&gathered), ["^2"]);
}

#[test]
fn also_suggested_follows_whether_the_blocker_itself_has_a_suggestion() {
    // "y" blocks "target" and is itself too new; whether y's own walk
    // finds a downgrade decides whether the block may claim a fix for y.
    let cases = [
        (
            &[("1.5.0", 5)][..],
            [
                "target: blocked at 1.4.0 by y@1.5.0 ^1.5",
                "y: no compliant version",
            ],
        ),
        (
            &[("1.5.0", 5), ("1.4.0", 50)],
            [
                "target: blocked at 1.4.0 by y@1.5.0 ^1.5 (also suggested)",
                "y: suggest 1.4.0 (50d)",
            ],
        ),
    ];
    for (y_versions, expected) in cases {
        let transport = FakeTransport::default();
        transport.versions("target", &[("1.5.0", 5), ("1.4.0", 50)]);
        transport.versions("y", y_versions);
        transport.index_ok(
            "y",
            r#"{"vers":"1.5.0","deps":[{"name":"target","req":"^1.5"}]}"#,
        );
        let violations = [too_new("target", "1.5.0"), too_new("y", "1.5.0")];
        let packages = [
            pkg("target", "1.5.0", &[]),
            pkg("y", "1.5.0", &[("target", "1.5.0")]),
        ];
        let outcomes = suggestions(transport, &violations, &packages, &[], Path::new("/work"));

        assert_eq!(summaries(&outcomes), expected, "y versions {y_versions:?}");
    }
}

#[test]
fn also_suggested_is_false_when_only_another_version_of_the_blocker_is_suggested() {
    // "foo" is locked at 1.5.0 and 2.5.0. Only 1.5.0 resolves to a
    // suggestion; the 2.5.0 that blocks "target" has no compliant
    // version, so the block must not point at the unrelated 1.5.0 fix.
    let transport = FakeTransport::default();
    transport.versions("target", &[("1.5.0", 5), ("1.4.0", 50)]);
    transport.versions("foo", &[("1.5.0", 5), ("1.4.0", 50), ("2.5.0", 5)]);
    transport.index_ok(
        "foo",
        r#"{"vers":"2.5.0","deps":[{"name":"target","req":"^1.5"}]}"#,
    );
    let violations = [
        too_new("target", "1.5.0"),
        too_new("foo", "1.5.0"),
        too_new("foo", "2.5.0"),
    ];
    let packages = [
        pkg("target", "1.5.0", &[]),
        pkg("foo", "1.5.0", &[]),
        pkg("foo", "2.5.0", &[("target", "1.5.0")]),
    ];
    let outcomes = suggestions(transport, &violations, &packages, &[], Path::new("/work"));

    assert_eq!(
        summaries(&outcomes),
        [
            "target: blocked at 1.4.0 by foo@2.5.0 ^1.5",
            "foo: suggest 1.4.0 (50d)",
            "foo: no compliant version"
        ]
    );
}

#[test]
fn stale_cached_index_records_missing_dependent_version_are_refetched() {
    // The cache holds "app"'s index records as of "1.0.0", but the
    // lockfile has moved to "1.0.1", which the cached entry doesn't
    // list. Looking up "1.0.1" must fall back to a live fetch rather
    // than silently treating the requirement as unreadable.
    let dir = tempfile::tempdir().unwrap();
    let cache_path = dir.path().join("cache.json");
    let mut cache = crate::cache::ResponseCache::load(Some(&cache_path));
    cache.set_index_records(
        "app",
        vec![crate::api::IndexRecord {
            vers: "1.0.0".to_string(),
            yanked: false,
            deps: vec![],
        }],
    );
    cache.save().unwrap();

    let transport = FakeTransport::default();
    transport.index_ok(
        "app",
        r#"{"vers":"1.0.1","deps":[{"name":"foo","req":"^1.5"}]}"#,
    );
    let packages = [pkg("app", "1.0.1", &[("foo", "1.5.0")])];
    let (dependents_index, name_version_index) = build_indexes(&packages);
    let gathered = gather_constraints(
        &mut fast_client(transport, Some(&cache_path)),
        &dependents_index,
        &name_version_index,
        &[],
        Path::new("/work"),
        "foo",
        "1.5.0",
        Some(CRATES_IO_SOURCE),
    );

    assert!(gathered.unverified_dependents.is_empty());
    assert_eq!(reqs(&gathered), ["^1.5"]);
}

const OVERLAPPING: &[&str] = &["^1", ">=1.8,<3"];

/// What else the lockfile holds besides `app` 1.0.0 locking crates.io
/// `foo` 1.9.0.
#[derive(Clone, Copy)]
enum Lock {
    OnlyFoo190,
    /// `app` also locks crates.io `foo` 2.0.0.
    SecondVersion,
    /// `app` also locks `foo` 2.0.0, but from another registry.
    SecondVersionFromOtherRegistry,
    /// `app` also locks a path `foo` at the same 1.9.0.
    PathFooAtSameVersion,
    /// `app` lists its crates.io `foo` 1.9.0 edge twice.
    DuplicateEdge,
    /// Some other package, not `app`, locks `foo` 2.0.0.
    OtherDependentOnSecondVersion,
    /// `app` also locks `foo` 2.0.0, and `other` requires `foo >=1.8`.
    SecondVersionAndOtherDependent,
}

/// Runs `check(gathered, registry)` with `app` declaring `foo` once per
/// entry of `reqs` (none: unreadable), first in its registry index record,
/// then in a local manifest.
fn in_both_modes(reqs: &[&str], lock: Lock, check: impl Fn(GatheredConstraints, bool)) {
    for registry in [true, false] {
        let transport = FakeTransport::default();
        let mut requirements = Vec::new();
        let mut app = if registry {
            pkg("app", "1.0.0", &[("foo", "1.9.0")])
        } else {
            non_registry_pkg("app", "1.0.0", &[("foo", "1.9.0")])
        };
        let foo_edge = |version: &str, source: Option<&str>| PackageRef {
            name: "foo".into(),
            version: version.into(),
            source: source.map(Into::into),
        };
        let mut packages = vec![pkg("foo", "1.9.0", &[])];

        let second = match lock {
            Lock::SecondVersion | Lock::SecondVersionAndOtherDependent => {
                Some(pkg("foo", "2.0.0", &[]))
            }
            Lock::SecondVersionFromOtherRegistry => {
                Some(external_pkg("foo", "2.0.0", ALT_REGISTRY_SOURCE, &[]))
            }
            _ => None,
        };
        if let Some(second) = second {
            app.dependencies
                .push(foo_edge(&second.version, second.source.as_deref()));
            packages.push(second);
        }
        match lock {
            Lock::PathFooAtSameVersion => {
                packages.push(non_registry_pkg("foo", "1.9.0", &[]));
                app.dependencies.push(foo_edge("1.9.0", None));
            }
            Lock::DuplicateEdge => app
                .dependencies
                .push(foo_edge("1.9.0", Some(CRATES_IO_SOURCE))),
            Lock::OtherDependentOnSecondVersion => {
                packages.push(pkg("foo", "2.0.0", &[]));
                packages.push(pkg("other", "1.0.0", &[("foo", "2.0.0")]));
            }
            Lock::SecondVersionAndOtherDependent if registry => {
                packages.push(pkg("other", "1.0.0", &[("foo", "1.9.0")]));
                transport.index_ok(
                    "other",
                    r#"{"vers":"1.0.0","deps":[{"name":"foo","req":">=1.8"}]}"#,
                );
            }
            Lock::SecondVersionAndOtherDependent => {
                packages.push(non_registry_pkg("other", "1.0.0", &[("foo", "1.9.0")]));
                requirements.push(local_requirement("other", "1.0.0", "foo", ">=1.8"));
            }
            _ => {}
        }
        packages.push(app);

        if !registry {
            requirements.extend(
                reqs.iter()
                    .map(|req| local_requirement("app", "1.0.0", "foo", req)),
            );
        } else if reqs.is_empty() {
            transport.index_error("app");
        } else {
            let deps: Vec<_> = reqs
                .iter()
                .enumerate()
                .map(|(i, req)| {
                    serde_json::json!({
                        "name": format!("alias_{i}"), "package": "foo", "req": req,
                    })
                })
                .collect();
            transport.index_ok(
                "app",
                &serde_json::json!({ "vers": "1.0.0", "deps": deps }).to_string(),
            );
        }
        check(
            gather(transport, &packages, &requirements, "1.9.0"),
            registry,
        );
    }
}

/// Walks the single candidate foo 1.7.0 against the gathered constraints.
fn walk_1_7(gathered: GatheredConstraints) -> WalkResult {
    walk(vec![(v("1.7.0"), 80)], gathered.constraints)
}

fn assert_suggests_1_7(result: WalkResult, registry: bool) {
    assert!(
        matches!(result, WalkResult::Suggest(version, 80) if version == v("1.7.0")),
        "registry={registry}"
    );
}

/// Asserts 1.7.0 is blocked and returns the blocking constraint.
fn blocker_of(result: WalkResult, registry: bool) -> Constraint {
    let WalkResult::Blocked { blocker, .. } = result else {
        panic!("1.7.0 was not blocked, registry={registry}");
    };
    blocker
}

/// `app` is reported unverified and enforces nothing, so 1.7.0 passes.
fn assert_unverified_and_unblocked(gathered: GatheredConstraints, registry: bool) {
    assert_eq!(
        gathered.unverified_dependents,
        ["app"],
        "registry={registry}"
    );
    assert_suggests_1_7(walk_1_7(gathered), registry);
}

/// `app` is verified and its enforced declarations block 1.7.0.
fn assert_verified_and_blocked(gathered: GatheredConstraints, registry: bool) {
    assert!(
        gathered.unverified_dependents.is_empty(),
        "registry={registry}"
    );
    blocker_of(walk_1_7(gathered), registry);
}

#[test]
fn overlapping_declarations_across_two_locked_versions_are_unverified() {
    in_both_modes(
        OVERLAPPING,
        Lock::SecondVersion,
        assert_unverified_and_unblocked,
    );
}

#[test]
fn disjoint_declarations_enforce_the_one_matching_the_locked_version() {
    in_both_modes(&["^1", "^2"], Lock::SecondVersion, |gathered, registry| {
        assert!(
            gathered.unverified_dependents.is_empty(),
            "registry={registry}"
        );
        assert_eq!(reqs(&gathered), ["^1"], "registry={registry}");
        assert_suggests_1_7(walk_1_7(gathered), registry);
    });
}

#[test]
fn identical_requirement_aliases_count_once() {
    in_both_modes(
        &[">=1.8,<3", ">=1.8, <3"],
        Lock::SecondVersion,
        assert_verified_and_blocked,
    );
}

#[test]
fn a_single_locked_version_enforces_every_declaration() {
    in_both_modes(
        &["^1", ">=1.8,<2"],
        Lock::OnlyFoo190,
        assert_verified_and_blocked,
    );
}

#[test]
fn another_dependents_requirement_still_blocks_when_app_is_unverified() {
    in_both_modes(
        OVERLAPPING,
        Lock::SecondVersionAndOtherDependent,
        |gathered, registry| {
            assert_eq!(
                gathered.unverified_dependents,
                ["app"],
                "registry={registry}"
            );
            let blocker = blocker_of(walk_1_7(gathered), registry);
            let expected = if registry { "other" } else { "Cargo.toml" };
            assert_eq!(blocker.blocker_name, expected);
        },
    );
}

#[test]
fn another_dependents_second_version_does_not_make_app_ambiguous() {
    in_both_modes(
        OVERLAPPING,
        Lock::OtherDependentOnSecondVersion,
        assert_verified_and_blocked,
    );
}

#[test]
fn a_second_version_from_another_registry_does_not_make_app_ambiguous() {
    in_both_modes(
        OVERLAPPING,
        Lock::SecondVersionFromOtherRegistry,
        assert_verified_and_blocked,
    );
}

#[test]
fn a_path_foo_at_the_same_version_does_not_make_app_ambiguous() {
    in_both_modes(
        OVERLAPPING,
        Lock::PathFooAtSameVersion,
        assert_verified_and_blocked,
    );
}

#[test]
fn a_duplicate_lock_edge_does_not_make_app_ambiguous() {
    in_both_modes(
        OVERLAPPING,
        Lock::DuplicateEdge,
        assert_verified_and_blocked,
    );
}

#[test]
fn unreadable_declarations_are_unverified() {
    in_both_modes(&[], Lock::OnlyFoo190, assert_unverified_and_unblocked);
}

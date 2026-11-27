use chrono::{Duration, Utc};
use flate2::write::GzEncoder;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use tar::Builder;
use tempfile::{TempDir, tempdir};

const BIN: &str = env!("CARGO_BIN_EXE_cargo-oxidate");
/// `(name, locked_version, locked_age_days, [(version, age_days)])`.
type CacheEntry<'a> = (&'a str, &'a str, i64, Vec<(&'a str, i64)>);

fn run_oxidate(cwd: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("cargo-oxidate should run")
}

fn run_suggest_fix(cwd: &Path, cache: &Path) -> Output {
    run_oxidate(
        cwd,
        &[
            "--min-age-days",
            "30",
            "--suggest-fix",
            "--cache-path",
            cache.to_str().unwrap(),
        ],
    )
}

fn edit_cache(path: &Path, edit: impl FnOnce(&mut serde_json::Value)) {
    let mut cache: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    edit(&mut cache);
    fs::write(path, serde_json::to_vec(&cache).unwrap()).unwrap();
}

fn write_cache(dir: &Path, entries: &[CacheEntry<'_>]) -> std::path::PathBuf {
    let now = Utc::now();
    let mut publish_dates = serde_json::Map::new();
    let mut all_versions = serde_json::Map::new();
    let mut index_records = serde_json::Map::new();

    for (name, locked, locked_age, versions) in entries {
        publish_dates.insert(
            format!("{name}/{locked}"),
            serde_json::Value::String((now - Duration::days(*locked_age)).to_rfc3339()),
        );
        all_versions.insert(
            (*name).to_string(),
            serde_json::json!({
                "fetched_at": now,
                "versions": versions.iter().map(|(version, age)| serde_json::json!({
                    "num": version,
                    "created_at": now - Duration::days(*age),
                    "yanked": false,
                })).collect::<Vec<_>>(),
            }),
        );
        index_records.insert(
            (*name).to_string(),
            serde_json::json!({
                "fetched_at": now,
                "records": [{ "vers": locked, "yanked": false, "deps": [] }],
            }),
        );
    }

    let path = dir.join("responses.json");
    fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "publish_dates": publish_dates,
            "all_versions": all_versions,
            "index_records": index_records,
        }))
        .unwrap(),
    )
    .unwrap();
    path
}

fn copied_fixture() -> TempDir {
    let temp = tempdir().unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/suggest_fix_e2e");
    fs::create_dir(temp.path().join("app")).unwrap();
    for file in ["Cargo.toml", "Cargo.lock", "app/Cargo.toml"] {
        fs::copy(fixture.join(file), temp.path().join(file)).unwrap();
    }
    temp
}

#[test]
fn suggest_fix_cli_reports_mixed_outcomes_and_preserves_project_files() {
    let project = copied_fixture();
    let cache = write_cache(
        project.path(),
        &[
            ("alpha", "1.5.0", 2, vec![("1.5.0", 2), ("1.4.0", 80)]),
            ("beta", "1.5.0", 2, vec![("1.5.0", 2), ("1.4.0", 80)]),
            ("gamma", "1.5.0", 2, vec![("1.5.0", 2), ("1.4.0", 80)]),
            ("delta", "1.5.0", 2, vec![("1.5.0", 2)]),
            ("consumer", "2.0.0", 100, vec![("2.0.0", 100)]),
        ],
    );
    edit_cache(&cache, |cache| {
        cache["index_records"]["consumer"]["records"] = serde_json::json!([{
            "vers": "2.0.0",
            "yanked": false,
            "deps": [
                { "name": "gamma", "req": "^1.5", "kind": null, "target": null, "optional": false, "package": null }
            ]
        }]);
    });

    let manifest_before = fs::read(project.path().join("Cargo.toml")).unwrap();
    let lock_before = fs::read(project.path().join("Cargo.lock")).unwrap();
    let output = run_suggest_fix(project.path(), &cache);
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("cargo update -p alpha@1.5.0 --precise 1.4.0"),
        "stdout was:\n{stdout}"
    );
    assert!(
        stdout.contains(
            "  Downgrade blocked by dependency requirements for beta@1.5.0: newest eligible version is 1.4.0, but app/Cargo.toml requires ^1.5\n"
        ),
        "stdout was:\n{stdout}"
    );
    assert!(
        stdout.contains(
            "  Downgrade blocked by dependency requirements for gamma@1.5.0: newest eligible version is 1.4.0, but consumer 2.0.0 requires ^1.5\n"
        ),
        "stdout was:\n{stdout}"
    );
    assert!(
        stdout.contains(
            "No eligible downgrade found for delta@1.5.0 within its compatible version range and age limits"
        ),
        "stdout was:\n{stdout}"
    );
    assert!(stdout.contains("They do not run Cargo's resolver"));
    for package in ["beta", "gamma", "delta"] {
        assert!(
            !stdout
                .lines()
                .any(|line| line.contains("cargo update") && line.contains(&format!("{package}@"))),
            "no command expected for {package}; stdout was:\n{stdout}"
        );
    }
    assert_eq!(
        fs::read(project.path().join("Cargo.toml")).unwrap(),
        manifest_before
    );
    assert_eq!(
        fs::read(project.path().join("Cargo.lock")).unwrap(),
        lock_before
    );
}

#[test]
fn suggest_fix_cli_reports_excluded_lower_versions_as_no_eligible_downgrade() {
    // (lower candidate, its age in days, yanked, extra args)
    let cases: [(_, _, _, &[&str]); 3] = [
        ("1.4.0", 80, true, &[]),
        ("1.4.0-beta.1", 80, false, &[]),
        ("1.0.0", 1000, false, &["--max-age-days", "365"]),
    ];
    for (candidate, age, yanked, extra_args) in cases {
        let project = copied_fixture();
        let cache = write_cache(
            project.path(),
            &[
                ("alpha", "1.5.0", 100, vec![]),
                ("beta", "1.5.0", 100, vec![]),
                ("gamma", "1.5.0", 100, vec![]),
                ("delta", "1.5.0", 2, vec![("1.5.0", 2), (candidate, age)]),
                ("consumer", "2.0.0", 100, vec![]),
            ],
        );
        edit_cache(&cache, |cache| {
            cache["all_versions"]["delta"]["versions"][1]["yanked"] = serde_json::json!(yanked);
        });

        let cache_path = cache.to_str().unwrap();
        let args = [
            &[
                "--min-age-days",
                "30",
                "--suggest-fix",
                "--cache-path",
                cache_path,
            ][..],
            extra_args,
        ]
        .concat();
        let output = run_oxidate(project.path(), &args);

        assert_eq!(output.status.code(), Some(1), "candidate {candidate}");
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(
            stdout.lines().any(|line| {
                line
                    == "  No eligible downgrade found for delta@1.5.0 within its compatible version range and age limits"
            }),
            "candidate {candidate}, stdout was:\n{stdout}"
        );
        assert!(
            !stdout
                .lines()
                .any(|line| line.trim_start().starts_with("cargo update -p delta@")),
            "candidate {candidate}, stdout was:\n{stdout}"
        );
    }
}

#[test]
fn suggest_fix_cli_retains_best_effort_qualification() {
    let project = copied_fixture();
    let lock = fs::read_to_string(project.path().join("Cargo.lock")).unwrap();
    let epsilon = "\n[[package]]\nname = \"epsilon\"\nversion = \"1.5.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"0000000000000000000000000000000000000000000000000000000000000000\"\n";
    fs::write(
        project.path().join("Cargo.lock"),
        lock.replace(
            " \"gamma\",\n]",
            " \"gamma\",\n \"epsilon 1.5.0 (registry+https://github.com/rust-lang/crates.io-index)\",\n]",
        ) + epsilon,
    )
    .unwrap();
    let cache = write_cache(
        project.path(),
        &[
            ("alpha", "1.5.0", 2, vec![("1.5.0", 2), ("1.4.0", 80)]),
            ("beta", "1.5.0", 2, vec![("1.5.0", 2), ("1.4.0", 80)]),
            ("gamma", "1.5.0", 2, vec![("1.5.0", 2), ("1.4.0", 80)]),
            ("delta", "1.5.0", 2, vec![("1.5.0", 2)]),
            ("epsilon", "1.5.0", 2, vec![("1.5.0", 2), ("1.4.0", 80)]),
            ("consumer", "2.0.0", 100, vec![("2.0.0", 100)]),
        ],
    );
    edit_cache(&cache, |cache| {
        // The record must be for the locked `2.0.0`, or the cache treats it as
        // a miss and the run reaches out to the network. The unparseable `req`
        // is what leaves the requirement unverified.
        cache["index_records"]["consumer"]["records"] = serde_json::json!([{
            "vers": "2.0.0",
            "yanked": false,
            "deps": [
                { "name": "gamma", "req": "not a requirement", "kind": null, "target": null, "optional": false, "package": null }
            ]
        }]);
    });
    let output = run_suggest_fix(project.path(), &cache);
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("requirement of consumer unverified"),
        "stdout was:\n{stdout}"
    );
    assert!(
        stdout.contains("They do not run Cargo's resolver"),
        "stdout was:\n{stdout}"
    );
    assert!(
        stdout.contains("Apply them from top to bottom, then run this check again"),
        "stdout was:\n{stdout}"
    );
    assert!(
        stdout.contains("Build or test after applying each suggestion"),
        "stdout was:\n{stdout}"
    );
}

#[test]
fn violations_without_suggest_fix_print_no_downgrade_output() {
    let project = copied_fixture();
    let cache = write_cache(
        project.path(),
        &[
            ("alpha", "1.5.0", 2, vec![("1.5.0", 2), ("1.4.0", 80)]),
            ("beta", "1.5.0", 100, vec![]),
            ("gamma", "1.5.0", 100, vec![]),
            ("delta", "1.5.0", 100, vec![]),
            ("consumer", "2.0.0", 100, vec![]),
        ],
    );
    let output = run_oxidate(
        project.path(),
        &[
            "--min-age-days",
            "30",
            "--cache-path",
            cache.to_str().unwrap(),
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("alpha@1.5.0: Below minimum age 30 days"),
        "stdout was:\n{stdout}"
    );
    for absent in [
        "cargo update",
        "Suggested downgrades",
        "Other downgrade outcomes",
    ] {
        assert!(!stdout.contains(absent), "stdout was:\n{stdout}");
    }
}

#[test]
fn suggest_fix_over_a_compliant_registry_package_prints_no_command() {
    let project = copied_fixture();
    let cache = write_cache(
        project.path(),
        &[
            ("alpha", "1.5.0", 100, vec![]),
            ("beta", "1.5.0", 100, vec![]),
            ("gamma", "1.5.0", 100, vec![]),
            ("delta", "1.5.0", 100, vec![]),
            ("consumer", "2.0.0", 100, vec![]),
        ],
    );
    let output = run_suggest_fix(project.path(), &cache);
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("No dependency age violations found."),
        "stdout was:\n{stdout}"
    );
    assert!(!stdout.contains("cargo update"), "stdout was:\n{stdout}");
}

#[test]
fn suggest_fix_without_min_age_days_is_a_usage_error() {
    let project = copied_fixture();
    let output = run_oxidate(project.path(), &["--suggest-fix"]);
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("--min-age-days"), "stderr was:\n{stderr}");
}

#[cfg(unix)]
#[test]
fn suggest_fix_uses_manifest_beside_the_symlinks_real_lockfile() {
    // `decoy/Cargo.lock` is a symlink to `real/Cargo.lock`. Manifest discovery
    // must follow the canonical path, or the decoy's permissive manifest
    // (which doesn't declare `alpha`) would let the downgrade through.
    let project = tempdir().unwrap();
    let real_dir = project.path().join("real");
    let decoy_dir = project.path().join("decoy");
    fs::create_dir_all(&real_dir).unwrap();
    fs::create_dir_all(&decoy_dir).unwrap();

    fs::write(
        real_dir.join("Cargo.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nalpha = \"^1.5\"\n",
    )
    .unwrap();
    fs::write(
        real_dir.join("Cargo.lock"),
        "version = 4\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\n \"alpha\",\n]\n\n[[package]]\nname = \"alpha\"\nversion = \"1.5.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"0000000000000000000000000000000000000000000000000000000000000000\"\n",
    )
    .unwrap();

    fs::write(
        decoy_dir.join("Cargo.toml"),
        "[package]\nname = \"decoy\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::os::unix::fs::symlink(real_dir.join("Cargo.lock"), decoy_dir.join("Cargo.lock")).unwrap();

    let cache = write_cache(
        project.path(),
        &[("alpha", "1.5.0", 2, vec![("1.5.0", 2), ("1.4.0", 80)])],
    );
    let output = run_oxidate(
        project.path(),
        &[
            "decoy/Cargo.lock",
            "--min-age-days",
            "30",
            "--suggest-fix",
            "--cache-path",
            cache.to_str().unwrap(),
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        !stdout.contains("cargo update -p alpha@1.5.0 --precise 1.4.0"),
        "forbidden downgrade command was suggested; stdout was:\n{stdout}"
    );
    assert!(
        stdout.contains("Downgrade blocked by dependency requirements for alpha@1.5.0")
            && stdout.contains("Cargo.toml")
            && stdout.contains("^1.5"),
        "expected the real manifest's restriction on alpha to be reported; stdout was:\n{stdout}"
    );
}

#[test]
fn suggest_fix_with_no_too_new_violations_does_not_warn_about_a_missing_manifest() {
    // No Cargo.toml beside the lockfile, and no registry packages to
    // check, so there is nothing to suggest a fix for. `--suggest-fix`
    // must not still load (and warn about) direct requirements when
    // there are no "too new" violations to act on.
    let project = tempdir().unwrap();
    fs::write(
        project.path().join("Cargo.lock"),
        "version = 4\n\n[[package]]\nname = \"clean\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();

    let output = run_oxidate(project.path(), &["--suggest-fix", "--min-age-days", "30"]);
    assert!(output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(!stderr.contains("No Cargo.toml"), "stderr was:\n{stderr}");
}

fn write_crate_source(dir: &Path, name: &str, version: &str) {
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(
        dir.join("Cargo.toml"),
        format!("[package]\nname = \"{name}\"\nversion = \"{version}\"\nedition = \"2021\"\n"),
    )
    .unwrap();
    fs::write(dir.join("src/lib.rs"), "").unwrap();
}

fn write_local_registry(registry: &Path, name: &str, version: &str) {
    let source = registry.join(".build").join(format!("{name}-{version}"));
    write_crate_source(&source, name, version);
    let mut bytes = Vec::new();
    let encoder = GzEncoder::new(&mut bytes, flate2::Compression::default());
    let mut tar = Builder::new(encoder);
    tar.append_dir_all(format!("{name}-{version}"), &source)
        .unwrap();
    tar.into_inner().unwrap().finish().unwrap();
    fs::write(registry.join(format!("{name}-{version}.crate")), &bytes).unwrap();
    let checksum = Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let shard = registry.join("index").join(&name[..2]).join(&name[2..4]);
    fs::create_dir_all(&shard).unwrap();
    let entry = serde_json::json!({"name": name, "vers": version, "deps": [], "cksum": checksum, "features": {}, "yanked": false});
    use std::io::Write;
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(shard.join(name))
        .unwrap()
        .write_all(format!("{entry}\n").as_bytes())
        .unwrap();
}

/// Cargo with an empty `CARGO_HOME`, so no user-global configuration applies.
fn isolated_cargo(project: &Path, cargo_home: &Path) -> Command {
    let mut command = Command::new("cargo");
    command.env("CARGO_HOME", cargo_home).current_dir(project);
    command
}

#[test]
fn printed_source_qualified_command_updates_only_the_registry_package() {
    let root = tempdir().unwrap();
    let registry = root.path().join("registry");
    let project = root.path().join("project");
    let cargo_home = root.path().join("cargo-home");
    fs::create_dir(&cargo_home).unwrap();
    write_local_registry(&registry, "semver", "1.0.28");
    write_local_registry(&registry, "semver", "1.0.27");
    write_crate_source(&project.join("vendor-semver"), "semver", "1.0.28");
    fs::create_dir_all(project.join(".cargo")).unwrap();
    fs::create_dir_all(project.join("src")).unwrap();
    fs::write(project.join("src/main.rs"), "fn main() {}\n").unwrap();
    fs::write(project.join(".cargo/config.toml"), format!("[source.local-vendor]\nlocal-registry = \"{}\"\n\n[source.crates-io]\nreplace-with = \"local-vendor\"\n", registry.display())).unwrap();
    fs::write(project.join("Cargo.toml"), "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nsemver = \"1\"\nsemver-path = { package = \"semver\", path = \"vendor-semver\" }\n").unwrap();
    let generated = isolated_cargo(&project, &cargo_home)
        .args(["generate-lockfile", "--offline"])
        .output()
        .unwrap();
    assert!(
        generated.status.success(),
        "{}",
        String::from_utf8_lossy(&generated.stderr)
    );

    // The abbreviated spec really is ambiguous in this fixture; otherwise the
    // printed source-qualified command below proves nothing.
    let lock_before = fs::read_to_string(project.join("Cargo.lock")).unwrap();
    let abbreviated = isolated_cargo(&project, &cargo_home)
        .args([
            "update",
            "-p",
            "semver@1.0.28",
            "--precise",
            "1.0.27",
            "--offline",
        ])
        .output()
        .unwrap();
    assert!(
        !abbreviated.status.success()
            && String::from_utf8_lossy(&abbreviated.stderr).contains("ambiguous"),
        "expected the abbreviated spec to be ambiguous in this fixture: {}",
        String::from_utf8_lossy(&abbreviated.stderr)
    );
    assert_eq!(
        fs::read_to_string(project.join("Cargo.lock")).unwrap(),
        lock_before,
        "the failed abbreviated update must leave Cargo.lock unchanged"
    );

    let cache = write_cache(
        &project,
        &[("semver", "1.0.28", 2, vec![("1.0.28", 2), ("1.0.27", 80)])],
    );
    let output = run_suggest_fix(&project, &cache);
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        !stdout.contains("Other downgrade outcomes"),
        "stdout was:\n{stdout}"
    );
    let printed = stdout
        .lines()
        .find(|line| line.trim_start().starts_with("cargo update "))
        .unwrap_or_else(|| panic!("a rendered cargo update command; stdout was:\n{stdout}"));

    let output = run_oxidate(
        &project,
        &[
            "--min-age-days",
            "30",
            "--suggest-fix",
            "--format",
            "json",
            "--cache-path",
            cache.to_str().unwrap(),
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let suggestion = &json["suggestions"][0];
    let command = suggestion["command"].as_str().unwrap();
    let argv: Vec<&str> = suggestion["command_argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|arg| arg.as_str().unwrap())
        .collect();
    assert!(
        printed.trim_start().starts_with(&format!("{command} # ")),
        "printed: {printed}\ncommand: {command}"
    );
    assert!(
        argv.iter().any(|arg| arg.contains('#')),
        "expected source-qualified package spec: {argv:?}"
    );

    // The shell string and the argv must each work from a fresh project.
    if cfg!(unix) {
        let by_shell = root.path().join("by-shell");
        copy_dir(&project, &by_shell);
        let applied = Command::new("sh")
            .args(["-c", &format!("{command} --offline")])
            .env("CARGO_HOME", &cargo_home)
            .current_dir(&by_shell)
            .output()
            .unwrap();
        assert!(
            applied.status.success(),
            "{}",
            String::from_utf8_lossy(&applied.stderr)
        );
        assert_only_the_registry_package_was_downgraded(&by_shell);
    }

    let by_argv = root.path().join("by-argv");
    copy_dir(&project, &by_argv);
    let applied = isolated_cargo(&by_argv, &cargo_home)
        .args(&argv[1..])
        .arg("--offline")
        .output()
        .unwrap();
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    assert_only_the_registry_package_was_downgraded(&by_argv);
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn assert_only_the_registry_package_was_downgraded(project: &Path) {
    let lock = cargo_lock::Lockfile::load(project.join("Cargo.lock")).unwrap();
    assert!(
        lock.packages
            .iter()
            .any(|package| package.name.as_str() == "semver"
                && package.version.to_string() == "1.0.27"
                && package.source.is_some())
    );
    assert!(
        lock.packages
            .iter()
            .any(|package| package.name.as_str() == "semver"
                && package.version.to_string() == "1.0.28"
                && package.source.is_none())
    );
}

#[test]
fn suggest_fix_cli_qualifies_an_ambiguous_blocker_in_text_and_json() {
    // `consumer 2.0.0` is locked from both crates.io and a git source, so the
    // blocker's abbreviated `consumer 2.0.0` would not identify which one
    // pins `gamma`.
    let project = tempdir().unwrap();
    fs::write(
        project.path().join("Cargo.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(
        project.path().join("Cargo.lock"),
        "version = 4\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\n \"consumer 2.0.0 (git+https://example.com/consumer#0000000000000000000000000000000000000000)\",\n \"consumer 2.0.0 (registry+https://github.com/rust-lang/crates.io-index)\",\n]\n\n[[package]]\nname = \"consumer\"\nversion = \"2.0.0\"\nsource = \"git+https://example.com/consumer#0000000000000000000000000000000000000000\"\n\n[[package]]\nname = \"consumer\"\nversion = \"2.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"0000000000000000000000000000000000000000000000000000000000000000\"\ndependencies = [\n \"gamma\",\n]\n\n[[package]]\nname = \"gamma\"\nversion = \"1.5.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"0000000000000000000000000000000000000000000000000000000000000000\"\n",
    )
    .unwrap();
    let cache = write_cache(
        project.path(),
        &[
            ("gamma", "1.5.0", 2, vec![("1.5.0", 2), ("1.4.0", 80)]),
            ("consumer", "2.0.0", 100, vec![("2.0.0", 100)]),
        ],
    );
    edit_cache(&cache, |cache| {
        cache["index_records"]["consumer"]["records"] = serde_json::json!([{
            "vers": "2.0.0",
            "yanked": false,
            "deps": [
                { "name": "gamma", "req": "^1.5", "kind": null, "target": null, "optional": false, "package": null }
            ]
        }]);
    });
    let spec = "registry+https://github.com/rust-lang/crates.io-index#consumer";

    let output = run_suggest_fix(project.path(), &cache);
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains(&format!("but {spec}@2.0.0 requires ^1.5\n")),
        "stdout was:\n{stdout}"
    );

    let output = run_oxidate(
        project.path(),
        &[
            "--min-age-days",
            "30",
            "--suggest-fix",
            "--format",
            "json",
            "--cache-path",
            cache.to_str().unwrap(),
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let blocker = &json["suggestions"][0]["blocker"];
    assert_eq!(blocker["kind"], "package", "json was:\n{json:#}");
    assert_eq!(blocker["version"], "2.0.0", "json was:\n{json:#}");
    assert_eq!(blocker["package_spec"], spec, "json was:\n{json:#}");
}

#[test]
fn suggest_fix_cli_labels_an_ambiguous_unverified_dependent_by_spec() {
    // crates.io `app 1.0.0` is verified through its index record, while the
    // git `app 1.0.0` cannot be, and shares its name and version. Only the git
    // one's label must be source-qualified, with a single `#`.
    let project = tempdir().unwrap();
    fs::write(
        project.path().join("Cargo.toml"),
        "[package]\nname = \"root\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    let git = "git+https://example.com/app#0000000000000000000000000000000000000000";
    let registry = "registry+https://github.com/rust-lang/crates.io-index";
    let checksum = "0".repeat(64);
    fs::write(
        project.path().join("Cargo.lock"),
        format!(
            "version = 4\n\n[[package]]\nname = \"root\"\nversion = \"0.1.0\"\ndependencies = [\n \"app 1.0.0 ({git})\",\n \"app 1.0.0 ({registry})\",\n]\n\n[[package]]\nname = \"app\"\nversion = \"1.0.0\"\nsource = \"{git}\"\ndependencies = [\n \"gamma\",\n]\n\n[[package]]\nname = \"app\"\nversion = \"1.0.0\"\nsource = \"{registry}\"\nchecksum = \"{checksum}\"\ndependencies = [\n \"gamma\",\n]\n\n[[package]]\nname = \"gamma\"\nversion = \"1.5.0\"\nsource = \"{registry}\"\nchecksum = \"{checksum}\"\n"
        ),
    )
    .unwrap();
    let cache = write_cache(
        project.path(),
        &[
            ("gamma", "1.5.0", 2, vec![("1.5.0", 2), ("1.4.0", 80)]),
            ("app", "1.0.0", 100, vec![("1.0.0", 100)]),
        ],
    );
    edit_cache(&cache, |cache| {
        cache["index_records"]["app"]["records"] = serde_json::json!([{
            "vers": "1.0.0",
            "yanked": false,
            "deps": [
                { "name": "gamma", "req": "^1.0", "kind": null, "target": null, "optional": false, "package": null }
            ]
        }]);
    });
    let label = "git+https://example.com/app#app";

    let output = run_suggest_fix(project.path(), &cache);
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains(&format!("(requirement of {label} unverified)")),
        "stdout was:\n{stdout}"
    );

    let output = run_oxidate(
        project.path(),
        &[
            "--min-age-days",
            "30",
            "--suggest-fix",
            "--format",
            "json",
            "--cache-path",
            cache.to_str().unwrap(),
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        json["suggestions"][0]["unverified_dependents"],
        serde_json::json!([label]),
        "json was:\n{json:#}"
    );
}

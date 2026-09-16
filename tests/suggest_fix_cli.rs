use chrono::{Duration, Utc};
use flate2::write::GzEncoder;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use tar::Builder;
use tempfile::{TempDir, tempdir};

const BIN: &str = env!("CARGO_BIN_EXE_cargo-oxidate");
type CacheEntry<'a> = (&'a str, &'a str, i64, Vec<(&'a str, i64)>);

fn run_oxidate(cwd: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("cargo-oxidate should run")
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
    fs::copy(
        fixture.join("app/Cargo.toml"),
        temp.path().join("Cargo.toml"),
    )
    .unwrap();
    fs::copy(fixture.join("Cargo.lock"), temp.path().join("Cargo.lock")).unwrap();
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
    let mut cache_json: serde_json::Value =
        serde_json::from_slice(&fs::read(&cache).unwrap()).unwrap();
    cache_json["index_records"]["consumer"]["records"] = serde_json::json!([{
        "vers": "2.0.0",
        "yanked": false,
        "deps": [
            { "name": "gamma", "req": "^1.5", "kind": null, "target": null, "optional": false, "package": null }
        ]
    }]);
    fs::write(&cache, serde_json::to_vec(&cache_json).unwrap()).unwrap();

    let manifest_before = fs::read(project.path().join("Cargo.toml")).unwrap();
    let lock_before = fs::read(project.path().join("Cargo.lock")).unwrap();
    let output = run_oxidate(
        project.path(),
        &[
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
        stdout.contains("cargo update -p alpha@1.5.0 --precise 1.4.0"),
        "stdout was:\n{stdout}"
    );
    assert!(
        stdout.contains("beta 1.5.0") && stdout.contains("Cargo.toml") && stdout.contains("^1.5"),
        "stdout was:\n{stdout}"
    );
    assert!(
        stdout.contains("gamma 1.5.0")
            && stdout.contains("consumer 2.0.0")
            && stdout.contains("^1.5")
    );
    assert!(stdout.contains("delta 1.5.0") && stdout.contains("no version"));
    assert!(stdout.contains("best-effort"));
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
fn suggest_fix_cli_retains_best_effort_qualification() {
    let project = tempdir().unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/suggest_fix_e2e");
    fs::copy(
        fixture.join("app/Cargo.toml"),
        project.path().join("Cargo.toml"),
    )
    .unwrap();
    let lock = fs::read_to_string(fixture.join("Cargo.lock")).unwrap();
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
    let mut cache_json: serde_json::Value =
        serde_json::from_slice(&fs::read(&cache).unwrap()).unwrap();
    cache_json["index_records"]["consumer"]["records"] = serde_json::json!([{
        "vers": "1.0.0", "yanked": false, "deps": []
    }]);
    fs::write(&cache, serde_json::to_vec(&cache_json).unwrap()).unwrap();
    let output = run_oxidate(
        project.path(),
        &[
            "--min-age-days",
            "30",
            "--suggest-fix",
            "--cache-path",
            cache.to_str().unwrap(),
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("best-effort"), "stdout was:\n{stdout}");
}

#[test]
fn ordinary_and_invalid_cli_runs_keep_their_exit_contract() {
    let project = tempdir().unwrap();
    fs::write(
        project.path().join("Cargo.toml"),
        "[package]\nname = \"clean\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(
        project.path().join("Cargo.lock"),
        "version = 4\n\n[[package]]\nname = \"clean\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();

    let clean = run_oxidate(project.path(), &["--min-age-days", "30"]);
    assert!(clean.status.success());
    assert!(
        !String::from_utf8(clean.stdout)
            .unwrap()
            .contains("Suggested fixes")
    );

    let invalid = run_oxidate(project.path(), &["--suggest-fix"]);
    assert_eq!(invalid.status.code(), Some(2));
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

#[test]
fn printed_source_qualified_command_updates_only_the_registry_package() {
    let root = tempdir().unwrap();
    let registry = root.path().join("registry");
    let project = root.path().join("project");
    write_local_registry(&registry, "semver", "1.0.28");
    write_local_registry(&registry, "semver", "1.0.27");
    write_crate_source(&project.join("vendor-semver"), "semver", "1.0.28");
    fs::create_dir_all(project.join(".cargo")).unwrap();
    fs::create_dir_all(project.join("src")).unwrap();
    fs::write(project.join("src/main.rs"), "fn main() {}\n").unwrap();
    fs::write(project.join(".cargo/config.toml"), format!("[source.local-vendor]\nlocal-registry = \"{}\"\n\n[source.crates-io]\nreplace-with = \"local-vendor\"\n", registry.display())).unwrap();
    fs::write(project.join("Cargo.toml"), "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nsemver = \"1\"\nsemver-path = { package = \"semver\", path = \"vendor-semver\" }\n").unwrap();
    let generated = Command::new("cargo")
        .args(["generate-lockfile", "--offline"])
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(
        generated.status.success(),
        "{}",
        String::from_utf8_lossy(&generated.stderr)
    );

    let cache = write_cache(
        &project,
        &[("semver", "1.0.28", 2, vec![("1.0.28", 2), ("1.0.27", 80)])],
    );
    let output = run_oxidate(
        &project,
        &[
            "--min-age-days",
            "30",
            "--suggest-fix",
            "--cache-path",
            cache.to_str().unwrap(),
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout).unwrap();
    let line = stdout
        .lines()
        .find(|line| line.trim_start().starts_with("cargo update "))
        .unwrap_or_else(|| panic!("a rendered cargo update command; stdout was:\n{stdout}"));
    let command = line.split(" #").next().unwrap().trim();
    let args: Vec<_> = command.split_whitespace().skip(1).collect();
    assert!(
        args.iter().any(|arg| arg.contains('#')),
        "expected source-qualified package spec: {command}"
    );
    let applied = Command::new("cargo")
        .args(args)
        .arg("--offline")
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );

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

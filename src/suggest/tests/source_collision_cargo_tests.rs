/// Builds a real Cargo project with a source collision — a crates.io
/// package and a path package sharing a name and locked version — and
/// runs a real `cargo` against the spec `build_package_spec` produces,
/// to verify it's the source-qualified pkgid Cargo itself expects,
/// rather than merely a string this crate assumes is valid.
use super::*;
use sha2::{Digest, Sha256};
use std::process::Command;

const CRATE_NAME: &str = "semver";
const CRATE_VERSION: &str = "1.0.28";

/// Writes a minimal crate (`Cargo.toml` + `src/lib.rs`) at `dir`.
fn write_crate_source(dir: &Path, name: &str, version: &str) {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        format!("[package]\nname = \"{name}\"\nversion = \"{version}\"\nedition = \"2021\"\n"),
    )
    .unwrap();
    std::fs::write(dir.join("src/lib.rs"), "").unwrap();
}

/// Packs `crate_dir` (already containing a `{name}-{version}`
/// top-level directory) into a `.crate` tarball, Cargo's own
/// publish format.
fn pack_crate_tarball(crate_dir: &Path, name: &str, version: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let encoder = flate2::write::GzEncoder::new(&mut bytes, flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        builder
            .append_dir_all(format!("{name}-{version}"), crate_dir)
            .unwrap();
        builder.finish().unwrap();
    }
    bytes
}

/// Assembles a local-registry source (see Cargo's source-replacement
/// docs) at `registry_dir`, containing one crate. Local-registry
/// index entries are sharded by name length: a 4+ character name
/// shards under its first two, then next two, characters.
fn write_local_registry(registry_dir: &Path, name: &str, version: &str) {
    let build_dir = registry_dir
        .join(".build")
        .join(format!("{name}-{version}"));
    write_crate_source(&build_dir, name, version);
    let tarball = pack_crate_tarball(&build_dir, name, version);

    std::fs::write(
        registry_dir.join(format!("{name}-{version}.crate")),
        &tarball,
    )
    .unwrap();

    let cksum = Sha256::digest(&tarball)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let shard = registry_dir
        .join("index")
        .join(&name[0..2])
        .join(&name[2..4]);
    std::fs::create_dir_all(&shard).unwrap();
    std::fs::write(
                shard.join(name),
                format!(
                    r#"{{"name":"{name}","vers":"{version}","deps":[],"cksum":"{cksum}","features":{{}},"yanked":false}}"#
                ),
            )
            .unwrap();
}

fn run_cargo(args: &[&str], cwd: &Path) -> std::process::Output {
    Command::new("cargo")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("failed to run cargo")
}

#[test]
fn qualified_spec_resolves_where_the_abbreviated_spec_is_ambiguous() {
    let root = tempfile::tempdir().unwrap();
    let registry_dir = root.path().join("registry");
    let workspace_dir = root.path().join("workspace");

    write_local_registry(&registry_dir, CRATE_NAME, CRATE_VERSION);
    write_crate_source(
        &workspace_dir.join("vendor-semver"),
        CRATE_NAME,
        CRATE_VERSION,
    );

    std::fs::create_dir_all(workspace_dir.join(".cargo")).unwrap();
    std::fs::write(
                workspace_dir.join(".cargo/config.toml"),
                format!(
                    "[source.local-vendor]\nlocal-registry = \"{}\"\n\n[source.crates-io]\nreplace-with = \"local-vendor\"\n",
                    registry_dir.display()
                ),
            )
            .unwrap();
    std::fs::create_dir_all(workspace_dir.join("src")).unwrap();
    std::fs::write(workspace_dir.join("src/main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(
                workspace_dir.join("Cargo.toml"),
                format!(
                    "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n{CRATE_NAME} = \"{CRATE_VERSION}\"\n{CRATE_NAME}-path = {{ package = \"{CRATE_NAME}\", path = \"vendor-semver\" }}\n"
                ),
            )
            .unwrap();

    let lock = run_cargo(&["generate-lockfile", "--offline"], &workspace_dir);
    assert!(
        lock.status.success(),
        "generate-lockfile failed: {}",
        String::from_utf8_lossy(&lock.stderr)
    );

    let packages = crate::lockfile::load(Path::new("Cargo.lock"), &workspace_dir)
        .unwrap()
        .packages;
    let target_source = packages
        .iter()
        .find(|p| p.name == CRATE_NAME && p.is_registry)
        .and_then(|p| p.source.as_deref());
    let is_ambiguous = packages
        .iter()
        .filter(|p| p.name == CRATE_NAME && p.version == CRATE_VERSION)
        .count()
        > 1;
    let spec = build_package_spec(CRATE_NAME, target_source, is_ambiguous);
    assert!(
        spec.contains('#'),
        "expected a source-qualified spec for a name/version collision, got {spec}"
    );

    // The abbreviated spec really is ambiguous in this fixture —
    // otherwise the qualified spec above proves nothing.
    let abbreviated = run_cargo(
        &[
            "update",
            "--offline",
            "-p",
            &format!("{CRATE_NAME}@{CRATE_VERSION}"),
            "--precise",
            CRATE_VERSION,
        ],
        &workspace_dir,
    );
    assert!(
        !abbreviated.status.success()
            && String::from_utf8_lossy(&abbreviated.stderr).contains("ambiguous"),
        "expected the abbreviated spec to be ambiguous in this fixture: {}",
        String::from_utf8_lossy(&abbreviated.stderr)
    );

    let qualified = run_cargo(
        &[
            "update",
            "--offline",
            "-p",
            &format!("{spec}@{CRATE_VERSION}"),
            "--precise",
            CRATE_VERSION,
        ],
        &workspace_dir,
    );
    assert!(
        qualified.status.success(),
        "expected the source-qualified spec to resolve without an ambiguous-specification error: {}",
        String::from_utf8_lossy(&qualified.stderr)
    );
}

# cargo-oxidate

Check `Cargo.lock` for packages that are too new (supply chain risk) or too old (staleness/CVE risk).

## Installation

```sh
cargo install cargo-oxidate
```

## Usage

```sh
# As a cargo subcommand
cargo oxidate --min-age-days 14 --max-age-days 730

# Direct invocation
cargo-oxidate Cargo.lock --min-age-days 14 --max-age-days 730
```

## Options

| Flag | Description |
|------|-------------|
| `--min-age-days N` | Flag packages newer than N days (supply chain security) |
| `--max-age-days N` | Flag packages older than N days (staleness) |
| `--exempt pkg1,pkg2` | Comma-separated packages to skip |
| `--exclude-missing` | Don't flag packages with unknown publish dates |
| `--timeout N` | HTTP timeout in seconds (default: 10) |
| `--suggest-fix` | For "too new" violations, suggest `cargo update` commands to downgrade |
| `--suggest-upgrade` | For all lockfile packages, suggest `cargo update` commands to upgrade to the newest compliant version |
| `--cache-path PATH` | Enable response caching at PATH (or set `CARGO_OXIDATE_CACHE_PATH`) |
| `--cache-max-age-hours N` | Max age for cached version listings (default: 24) |

At least one of `--min-age-days` or `--max-age-days` must be specified.

## Exit Codes

- `0` — No violations found
- `1` — Violations detected
- `2` — Runtime error

## Caching

Repeat runs can reuse crates.io API responses by passing `--cache-path`:

```sh
cargo oxidate --cache-path .cache/oxidate.json --min-age-days 14
```

Per-version publish dates are cached indefinitely (they're immutable on crates.io). Per-crate version listings expire after `--cache-max-age-hours` (default 24h) so newly published versions are picked up.

## GitHub Action

This tool is also available as a GitHub Action. See [examples/usage.yml](examples/usage.yml) or use it in your workflow:

```yaml
- uses: timweri/cargo-oxidate@v0.1.5
  with:
    min-age-days: 14
    max-age-days: 730
    cache-responses: true  # default; set to 'false' to disable
```

When `cache-responses` is enabled (the default), the action wires up `actions/cache` keyed on the `Cargo.lock` hash so subsequent runs skip already-fetched crates.io responses.

## --suggest-upgrade limitations

`--suggest-upgrade` (which requires `--min-age-days`) walks every package in
`Cargo.lock` and suggests the newest age-compliant, non-yanked version. The
following corner cases are not currently handled, and affected packages are
either misclassified as transitive or produce ambiguous `cargo update` lines:

- **Pre-release versions are never suggested.** Anything with a `-tag` suffix
  (`-alpha`, `-alpha.1`, `-beta.2`, `-rc.1`, `-pre`, `-0.x`) is filtered out.
  There is no opt-in flag.
- **Only `[dependencies]` is parsed.** Deps under `[dev-dependencies]`,
  `[build-dependencies]`, or `[target.'cfg(...)'.dependencies]` are treated as
  transitive (with the "compatibility not guaranteed" warning) even though
  they are direct deps.
- **Workspace inheritance is not resolved.** Deps declared as
  `foo = { workspace = true }` resolve to no version string and are treated as
  transitive.
- **Workspace root manifests** with only a `[workspace]` table cause every
  package to be reported as transitive. Run from a member crate instead.
- **Renamed packages.** `foo = { package = "real-foo", version = "..." }`
  keys the requirement under `foo`, but the lockfile lists `real-foo`, so the
  match misses and `real-foo` is treated as transitive.
- **Multiple versions of the same crate.** When the lockfile contains
  e.g. `rand 0.8` and `rand 0.9`, the suggested `cargo update -p rand
  --precise X` lines are ambiguous; use `cargo update -p rand@VERSION
  --precise X` instead.

For transitive deps, run `cargo tree -i <pkg>` to find the parent that
constrains the version.

## License

MIT

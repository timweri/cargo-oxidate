# cargo-oxidate

Check `Cargo.lock` for packages that are too new (supply chain risk) or too old (staleness/CVE risk).

## Installation

```sh
cargo install cargo-oxidate --locked
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
| `--include-prerelease` | Consider prerelease versions as suggestion candidates (requires `--suggest-fix`); ordinary SemVer requirements (e.g. `^1.2`) still generally don't match prereleases, so most will still be rejected |
| `--cache-path PATH` | Enable response caching at PATH (or set `CARGO_OXIDATE_CACHE_PATH`) |
| `--cache-max-age-hours N` | Max age for cached version listings (default: 24) |
| `--quiet` | Suppress start and per-package progress messages |
| `--verbose` | Show each package as it is checked |
| `--format text\|json` | Render the final result as text (default) or JSON |

At least one of `--min-age-days` or `--max-age-days` must be specified.

## CI output

The final result is written to standard output. A concise start message and
operational diagnostics are written to standard error. Use `--verbose` to see
each package as it is checked, or `--quiet` to suppress start and progress
messages while retaining warnings, errors, and the final result. The two flags
cannot be combined.

Use `--format json` for automation. It writes one newline-terminated JSON
document to standard output; progress and operational diagnostics remain on
standard error. Schema version 1 permits additive fields, and consumers should
ignore fields they do not recognize.

### JSON schema

JSON output has these top-level fields: `schema_version`, `status`, `lockfile`,
`policy`, `summary`, `violations`, `warnings`, `errors`, and `suggestions`.
`status` is `passed`, `violations`, or `error`, matching exit codes 0, 1, and 2.
The `summary` always includes the violation count and duration in milliseconds;
its package-count fields are `null` when the lockfile could not be loaded.

Each violation has `package`, `version`, and `kind`. Age violations also include
`published_at`, `age_days`, and `threshold_days`; missing publish dates include
`reason`. Diagnostics include `category` and `message`, plus package, version,
or path context when available. Lookup diagnostics include `retryable`.

`suggestions` is `null` unless `--suggest-fix` was requested. Once requested it
is always an array, including when no downgrade investigation was needed. Its
entries use `suggested`, `blocked`, `no_eligible_downgrade`, or `unavailable`
`kind` values and carry the applicable command, blocker, uncertainty, or
failure details. Fields may be added within schema version 1; removing a field
or changing its type or meaning requires a new schema version.

For example:

```json
{"schema_version":1,"status":"passed","lockfile":"Cargo.lock","policy":{"min_age_days":14,"max_age_days":null,"exclude_missing":false,"exempt":[]},"summary":{"total_packages":1,"checked_packages":1,"exempt_packages":0,"unsupported_packages":0,"excluded_missing_packages":0,"failed_packages":0,"not_checked_packages":0,"violations":0,"duration_ms":4},"violations":[],"warnings":[],"errors":[],"suggestions":null}
```

A lockfile that cannot be loaded still produces one document after parsing:

```json
{"schema_version":1,"status":"error","lockfile":"missing.lock","policy":{"min_age_days":14,"max_age_days":null,"exclude_missing":false,"exempt":[]},"summary":{"total_packages":null,"checked_packages":null,"exempt_packages":null,"unsupported_packages":null,"excluded_missing_packages":null,"failed_packages":null,"not_checked_packages":null,"violations":0,"duration_ms":0},"violations":[],"warnings":[],"errors":[{"category":"input","message":"Failed to parse Cargo.lock"}],"suggestions":null}
```

`--exclude-missing` excludes only confirmed missing publish dates. Registry
lookup and response failures remain errors so CI can distinguish incomplete
checks from age-policy violations. Cache read and write failures are warnings;
the freshness check continues and its exit result is unchanged.

## `--suggest-fix`

`--suggest-fix` prints a `cargo update --precise` command for the newest eligible downgrade of
each package that is too new. It checks dependency requirements it can verify from `Cargo.lock`
and workspace manifests.

A candidate must be old enough, not yanked, older than the locked version, and in its compatible
version zone. The zone keeps the same major version, except that `0.x` keeps the same minor and
`0.0.x` keeps the same patch. Prereleases are excluded unless you pass `--include-prerelease` or
the locked version is itself a prerelease.

Registry requirements come from the crates.io index. An optional registry declaration that cannot
be confirmed active is shown as unverified. Target-specific registry declarations are enforced.
For local and workspace manifests, the tool does not determine feature or target activation, so it
treats every declared requirement, including optional and target-specific ones, as mandatory.

Suggestions are best effort. The tool does not run Cargo's resolver or build your project, so Cargo
can still reject a suggested command. Apply suggestions in order, then run the command again and
run your tests. It reports whether a downgrade is blocked by a dependency
requirement, no eligible downgrade exists, or a downgrade could not be
determined because its version metadata was unavailable.

## Exit Codes

- `0` — No dependency age violations found
- `1` — Dependency age violations found
- `2` — A required check could not complete or input was invalid

## Caching

Repeat runs can reuse crates.io API responses by passing `--cache-path`:

```sh
cargo oxidate --cache-path .cache/oxidate.json --min-age-days 14
```

Per-version publish dates are cached indefinitely (they're immutable on crates.io). Per-crate version listings expire after `--cache-max-age-hours` (default 24h) so newly published versions are picked up.

## GitHub Action

This tool is also available as a GitHub Action. See [examples/usage.yml](examples/usage.yml) or use it in your workflow:

```yaml
- uses: timweri/cargo-oxidate@v0.1.8
  with:
    min-age-days: 14
    max-age-days: 730
    cache-responses: true  # default; set to 'false' to disable
```

When `cache-responses` is enabled (the default), the action caches crates.io responses using the selected `Cargo.lock` hash. Its compiled binary is cached separately by the referenced cargo-oxidate action version, so dependency changes in the consuming repository do not trigger a rebuild of this tool.

## License

MIT

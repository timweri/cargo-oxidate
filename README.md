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

When `cache-responses` is enabled (the default), the action caches crates.io responses using the selected `Cargo.lock` hash. Its compiled binary is cached separately by the referenced cargo-oxidate action version, so dependency changes in the consuming repository do not trigger a rebuild of this tool.

## License

MIT

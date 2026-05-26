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

At least one of `--min-age-days` or `--max-age-days` must be specified.

## Exit Codes

- `0` — No violations found
- `1` — Violations detected
- `2` — Runtime error

## GitHub Action

This tool is also available as a GitHub Action. See [examples/usage.yml](examples/usage.yml) or use it in your workflow:

```yaml
- uses: timweri/cargo-oxidate@v1
  with:
    min-age-days: 14
    max-age-days: 730
```

## License

MIT

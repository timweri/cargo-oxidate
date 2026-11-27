# 1. JSON output moves to schema version 2

Date: 2026-11-26

## Context

`--format json` carries a `schema_version`. The rule set with version 1 is that
fields may be added within a version, but removing a field or changing what a
field means needs a new version.

Three wanted changes break that rule: a field that never told the truth should
go, a list of names should hold package specs where a name is ambiguous, and a
command string should become safe to paste into any shell. The crate has few
users, so correctness was preferred to keeping version 1 alive.

## Decision

Bump `schema_version` to 2 and make every breaking change in that one step.

Changes from version 1:

- `summary.not_checked_packages` is removed. Nothing wrote it, so it was
  always 0. The text coverage line drops `not checked=N` with it.
- `suggestions[].unverified_dependents` entries are labels, not names. An
  entry is the bare crate name when that name and version are unique in the
  lockfile, and the source-qualified spec `{source}#{name}` when they are not.
- `suggestions[].blocker.kind` is new: `"package"` or `"manifest"`.
  - A package blocker always has `version` and `package_spec`. The spec is the
    bare name when unambiguous. In version 1 it was present only when
    ambiguous.
  - A manifest blocker has neither. Its `name` is the path of a `Cargo.toml`.
- `suggestions[].command_argv` is new: the `cargo update` command as an
  argument array, with no quoting.
- `suggestions[].command` is a shell string. An argument is single-quoted
  when it contains a character outside `[A-Za-z0-9_@%+=:,./-]`, which in
  practice means a source-qualified spec. In version 1 nothing was quoted, and
  the `#` in such a spec broke under zsh with `EXTENDED_GLOB`.
- A package spec never contains two `#`. The `#<commit>` that a git source
  carries in `Cargo.lock` is dropped before `#{name}` is appended.

## Consequences

- A version 1 consumer must update. It should check `schema_version` and read
  `command_argv` rather than splitting `command` on spaces.
- Commands and labels for packages whose name and version are unique in the
  lockfile are byte-identical to version 1.
- The version 1 rule still holds for version 2: additions are allowed, and the
  next removal or change of meaning needs version 3.

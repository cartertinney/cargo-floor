# cargo-floor

Find the lowest verified dependency versions required by your Rust library's
current source. Consumers resolve from the dependency requirements you publish,
not your lockfile, so a passing local build can hide incorrect minimums.

`cargo floor` checks dependency releases against your source to catch minimums
that are too low for the APIs you use and lower unnecessary minimums so consumers
can keep compatible older versions.

## Install

```sh
cargo install --path .
```

## Usage

Run from a library or workspace:

```sh
cargo floor
cargo floor -p my-library --features extras
cargo floor --no-default-features --target x86_64-unknown-linux-gnu
cargo floor --manifest-path path/to/Cargo.toml
cargo floor --exhaustive
cargo +stable floor
```

Binary search is the default for speed. It assumes compatibility generally
improves with newer releases and can miss an earlier working release after a
regression. Use `--exhaustive` to check eligible releases oldest first instead.

Each run checks one feature/target configuration using the selected toolchain.
Run separately for the configurations you support; `--all-features` selects a
union of features, not every combination. Use `--package` for an ambiguous
workspace and `--target` when Cargo configuration defines multiple targets.

Progress is written to stderr as each phase and candidate starts. Operations
lasting more than ten seconds show elapsed-time updates; these indicate the
operation is still running, not a completion estimate. Use `--quiet` to suppress
progress without hiding warnings, errors, or the final report.

## Apply changes

By default, discovery is read-only. Review the report, then apply verified
requirement changes:

```sh
cargo floor --apply
```

Only verified requirement values are edited. Comments, other dependency
attributes, and the original `Cargo.lock` are preserved.

## JSON output

```sh
cargo floor --format json
```

The report is written to stdout; progress and diagnostics go to stderr. Use
`cargo floor --help` for all options.

Results are compilation evidence for the selected configuration, not a guarantee
of runtime correctness or every permitted version combination. Only run on
trusted projects: Cargo can execute build scripts and procedural macros.

## Documentation

- [Behavior, design, and limitations](doc/design.md)
- [Development and scenario coverage](doc/development.md)

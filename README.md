# cargo-floor

Find the lowest verified dependency versions required by your Rust library's
current source. Consumers resolve from the dependency requirements you publish,
not your lockfile, so a passing local build can hide incorrect minimums.

`cargo floor` checks dependency releases against your source to catch minimums
that are too low for the APIs you use and lower unnecessary minimums so consumers
can keep compatible older versions.

## Install

```sh
cargo install cargo-floor --locked
```

## Usage

Run from a library or workspace:

```sh
cargo floor
cargo floor -p my-library --features extras
cargo floor --no-default-features --target x86_64-unknown-linux-gnu
cargo floor --manifest-path path/to/Cargo.toml
cargo +stable floor
```

Each run checks one feature/target configuration using the selected toolchain.
Run separately for the configurations you support; `--all-features` selects a
union of features, not every combination. Use `--package` for an ambiguous
workspace and `--target` when Cargo configuration defines multiple targets.

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

The report is written to stdout; diagnostics go to stderr. Use
`cargo floor --help` for all options.

Results are compilation evidence for the selected configuration, not a guarantee
of runtime correctness or every permitted version combination. Only run on
trusted projects: Cargo can execute build scripts and procedural macros.

## Documentation

- [Behavior, design, and limitations](doc/design.md)
- [Development and scenario coverage](doc/development.md)
- [Manual release checklist](doc/release.md)

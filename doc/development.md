# Development

## Checks

```sh
cargo test --tests
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

## Continuous integration

GitHub Actions runs the locked validation matrix on pull requests, pushes to
`main`, manual dispatches, and every Monday at 06:17 UTC (`17 6 * * 1`).
The schedule becomes active after merge and runs from the default branch,
picking up stable Rust updates without updating `Cargo.lock`. The matrix covers
stable Rust on Linux, macOS, and Windows, plus Rust 1.85.0 on Linux, including
tests, packaged installs, and formatting/Clippy checks on stable Linux.

## Test fixtures

The end-to-end suite in [tests/](../tests/) uses real Cargo compilations against
deterministic local registry fixtures and loopback-only sparse HTTP servers with
isolated Cargo homes. It does not depend on public registry releases or external
network access. There are no unit tests.

## Scenario coverage

- Library API usage, local/dependency features, effective rustflags, aliases,
  custom library names, procedural macros, and host/target compilation roles.
- Independently compiled floor witnesses and actual acceptance/rejection at
  lower and upper requirement boundaries.
- Logarithmic default binary searches, exhaustive ordering, cached probes,
  non-monotonic histories, fallback behavior, and search-completeness reporting.
- Failed final witnesses, unsupported requirements/sources, locked yanked
  releases, corrupt archives, and malformed or unavailable sparse indexes.
- Workspace selection/isolation, configuration precedence, byte-preserving
  edits, synchronized concurrent manifest changes, and filesystem exclusions.

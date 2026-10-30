# Development

## Checks

```sh
cargo test --tests
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

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

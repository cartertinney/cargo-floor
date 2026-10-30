# Development

## Source installation

Build and install from a source checkout:

```sh
git clone https://github.com/cartertinney/cargo-floor.git
cd cargo-floor
cargo install --path . --locked
cargo floor --help
```

For an existing checkout, run the install command from the repository root.
Ensure Cargo's installation directory (normally `$HOME/.cargo/bin`) is on `PATH`.

## Checks

Run from the repository root:

```sh
cargo check --locked
cargo test --locked --tests
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
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
- Failed final witnesses, unsupported requirements/sources, locked yanked
  releases, corrupt archives, and malformed or unavailable sparse indexes.
- Workspace selection/isolation, configuration precedence, byte-preserving
  edits, synchronized concurrent manifest changes, and filesystem exclusions.

## Releases

Follow the [manual release checklist](release.md) before publishing. It includes
license/metadata review, Rust version and platform validation, and packaged
installation checks.

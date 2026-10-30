# Manual release checklist

Complete these manual maintainer gates for each release. Run commands from the
repository root against the same clean commit intended for publication; resolve
missing or failing gates before proceeding.

## Review and validation

- [ ] Review the intended version, package description, README, repository links,
  license expression, matching license files, and dependency license obligations.
  Merge any required license/metadata changes before packaging.
- [ ] Run the locked checks on stable Rust:

  ```sh
  cargo check --locked
  cargo test --locked --tests
  cargo fmt --check
  cargo clippy --locked --all-targets -- -D warnings
  ```

- [ ] Validate the advertised Rust 1.85 minimum, including the locked dependencies:

  ```sh
  rustup toolchain install 1.85.0 --profile minimal
  cargo +1.85.0 check --locked
  cargo +1.85.0 test --locked --tests
  ```

  Resolve dependency/MSRV mismatches before release; do not bypass the lockfile or
  Rust version requirement to make these checks pass.
- [ ] Require passing CI on the release commit for every platform advertised as
  supported. Merge and run any missing platform coverage before publication;
  one local build is not evidence for other platforms.

## Package and install

- [ ] Inspect the file list for the expected README, license files, and
  `Cargo.lock`, and for accidental credentials or unrelated files. Run the
  publication dry-run without uploading:

  ```sh
  cargo package --locked --list
  cargo publish --locked --dry-run --registry crates-io
  ```

  Do not skip verification or allow a dirty tree.
- [ ] Smoke-test a clean install from the packaged archive, not the checkout.
  This Unix-shell example recreates only `target/release-smoke`, uses a fresh
  Cargo home/build/install directory, and installs with the advertised MSRV.
  Fresh output directories make the version-independent archive glob unambiguous.

  ```sh
  (
    set -eu
    smoke="$PWD/target/release-smoke"
    rm -rf "$smoke"
    cargo package --locked --target-dir "$smoke/package-build"
    mkdir -p "$smoke/source"
    tar -xzf "$smoke"/package-build/package/cargo-floor-*.crate -C "$smoke/source"
    export CARGO_HOME="$smoke/cargo-home"
    export CARGO_TARGET_DIR="$smoke/build"
    cargo +1.85.0 install --path "$smoke"/source/cargo-floor-* \
      --locked --root "$smoke/install"
    "$smoke/install/bin/cargo-floor" --version
    PATH="$smoke/install/bin:$PATH" cargo floor --help
  )
  ```

## Publish manually

- [ ] Authenticate to crates.io using a securely configured
  [Cargo credential provider](https://doc.rust-lang.org/cargo/reference/registry-authentication.html)
  outside the repository. Confirm the account and publishing permission without
  exposing credentials. Never commit credentials or paste them into commands,
  issues, PRs, or logs.
- [ ] After all gates pass and a maintainer explicitly authorizes publication,
  run this command manually. It performs the actual public upload:

  ```sh
  cargo publish --locked --registry crates-io
  ```

- [ ] Verify the crates.io listing and the [README installation instructions](../README.md)
  in a clean environment: `cargo install cargo-floor --locked`, then
  `cargo floor --help`.

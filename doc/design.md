# Behavior and design

`cargo floor` compiles real dependency candidates to infer direct dependency
lower bounds. It does not infer a globally minimal lockfile.

## Why use this for a library?

Consumers resolve your library's dependencies from its published requirements,
not its lockfile. A passing local build can therefore hide incorrect minimums.

`cargo floor` helps you:

- **Lower unnecessary minimums**, letting consumers keep compatible older
  versions while still allowing newer releases.
- **Raise too-low minimums** when your source uses APIs introduced after the
  declared minimum.

Applications typically manage one locked dependency graph; libraries also need
accurate requirements for downstream consumers.

## How it works

1. Copy the workspace into a temporary directory, retaining the original
   invocation's Cargo configuration and registry source replacements.
2. Isolate the selected workspace package and remove development-dependency
   constraints from the copied manifests, preserving local feature definitions
   and forwarding to retained normal/build dependencies. Remove unused build
   dependencies from copied packages that have no build script.
   Confirm `cargo check --lib` passes.
3. Enumerate stable, non-yanked releases within each direct registry
   dependency's SemVer compatibility line, preserving existing upper bounds
   and dependency attributes.
4. Probe releases oldest first, pinning only the dependency under examination.
   Other in-scope direct dependencies can resolve within expanded search
   windows; transitive dependencies resolve normally.
5. Recheck each successful candidate against the final proposed requirements,
   and check that the proposed manifest builds without candidate pins.

Compilation, rather than API-name matching or assumptions about monotonic
compatibility, is the authority. Normal and build dependencies are included.
Optional and target-specific dependencies not built in the selected
configuration are explicitly reported as inactive. Test/example/benchmark
requirements do not establish the published library's floor.
Cargo evaluates the selected target's dependency conditions, including effective
configuration and environment rustflags. Build dependencies retain Cargo's
host-versus-target behavior; normal artifacts alone do not verify unused build
dependency entries. Separate Cargo-backed condition probes distinguish inactive
declarations even when they share a compiled package with another dependency.
Inactive original requirements remain unchanged.

For example, if `foo 1.2` requires `bar >=2.5`, but `foo 1.4` supports
`bar >=2.1`, the tool can propose:

```toml
[dependencies]
foo = "1.2.0"
bar = "2.1.0"
```

These are independently witnessed lower bounds. They need not occur in the
same lockfile: Cargo enforces `foo`'s declared constraints. The report retains
the actual compiled package versions for each successful probe.

## Safety and limitations

- Report-only is the default. Original manifests, lockfiles, and source files
  are not edited during discovery. `--apply` changes only verified dependency
  requirement values, preserving TOML comments and other attributes. It
  intentionally leaves `Cargo.lock` unchanged.
- Results mean **lowest verified under the tested configuration**. A failed
  Cargo resolution or compilation rejects that tested graph, not every
  conceivable graph containing the candidate. A passing result is not proof
  that every permitted version combination works, nor a runtime test.
- Advisories, security policy, and Rust MSRV discovery are outside v1.
  Run `cargo audit` separately for advisory checking.
- Search stays within one existing SemVer compatibility line. Exact pins,
  prerelease requirements, unbounded requirements, and ranges spanning
  compatibility lines are explicitly unsupported.
- Workspace members with explicit requirements are supported. Shared
  `workspace = true` dependencies, path/Git dependencies, dependency overrides
  (`[patch]`, `[replace]`, Cargo path overrides), and directory sources are
  explicitly reported as unsupported rather than treated as registry evidence.
  Unsupported requirements remain unchanged.
- Public sparse HTTP(S) registries and Cargo `local-registry` source
  replacements are supported. Git-based registry indexes other than the
  canonical crates.io index are not supported. Sparse index enumeration
  requires network access; offline discovery requires a local-registry source.
  Authenticated registries and custom Cargo HTTP/TLS configuration are not
  supported for release enumeration.
- Directory symlinks and special files in the copied workspace are rejected.
  External path dependencies remain external, are not searched, and are not
  edited by the tool.
- Only run this tool on trusted projects: Cargo checks can execute build scripts
  and procedural macros. Temporary directories are not a security sandbox.
- Release metadata is cached and build artifacts are reused within a run.
  There is no search budget or persistent search cache in v1. Large release
  histories can require many compilations.

## Machine-readable reports

JSON output (`--format json`) is one object on stdout, with `schema_version = 1`,
the selected configuration, dependency statuses, proposed requirements, probe
outcomes/diagnostics, actual compiled package versions, and whether edits were
applied. Diagnostics go to stderr. Unsupported/unverified entries make the report
`incomplete`; failed baselines, invalid input, and failed final verification exit
nonzero without a successful discovery report.

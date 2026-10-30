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
4. Search eligible releases using binary search by default or an oldest-first
   scan with `--exhaustive`, pinning only the dependency under examination.
   Other in-scope direct dependencies can resolve within expanded search
   windows; transitive dependencies resolve normally.
5. Recheck each selected floor witness against the final proposed requirements,
   and check that the proposed manifest builds without candidate pins.

Actual compilation verifies each candidate; binary search assumes monotonic
compatibility when deciding which other releases to skip.
Normal and build dependencies are included.
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

## Search strategies

The default binary search tests the oldest eligible release first. If it works,
no earlier candidate needs checking. Otherwise, the search confirms a passing
upper endpoint, preferring the version resolved by the baseline build, and
bisects the remaining interval. Each candidate is probed at most once during
discovery; final witness verification remains a separate build.

The two releases immediately below the located boundary are also checked.
If sampled results demonstrate non-monotonic compatibility, the search falls
back to an exhaustive check below the best passing candidate. It also falls
back to an oldest-first scan when no passing upper endpoint can be established.
These fallbacks are described in verbose progress output.

Normally this uses logarithmically many probes, plus a small number of endpoint
and neighbor checks. A fallback can still require a linear scan. Untested
regressions can hide earlier working releases even when sampled results look
monotonic; neighbor checks do not prove minimality.

`--exhaustive` checks every eligible release before the first passing candidate
in ascending version order. It does not check every newer release, every
dependency combination, or runtime behavior.

Both strategies require an actual passing build for the selected floor and
retain the same final requirements/witness validation and opt-in edit behavior.

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

Human-readable dependency rows show only the original and proposed requirements.
Exact verified versions and per-dependency search evidence remain available in
JSON output rather than repeated on every row.

JSON output (`--format json`) is one object on stdout, with `schema_version = 1`,
the selected configuration, dependency statuses, proposed requirements, probe
outcomes/diagnostics, actual compiled package versions, and whether edits were
applied. `configuration.search_strategy` records `binary` or `exhaustive`.
Each dependency's `earlier_candidates_checked` is `true` when every earlier
eligible release was probed and failed, `false` when some were skipped, and
`null` when no floor was verified. This describes the tested graphs, not every
conceivable resolution for each version.
Progress and diagnostics go to stderr. Unsupported/unverified entries make the report
`incomplete`; failed baselines, invalid input, and failed final verification exit
nonzero without a successful discovery report.

## Progress reporting

Default progress is compact ASCII text on stderr: short preparation, baseline,
and final-verification status lines, plus one dotted line per searched
dependency. Each completed candidate probe adds a dot; a long-running operation
adds another activity dot every ten seconds, even if Cargo is silent.
For example:

```text
Preparing workspace ready
Checking baseline passed
api [normal]... 1.2.0 (3 probes)
Final verification. passed
```

The final probe count excludes heartbeat dots and final witness verification.
Dots indicate activity, not a percentage or an estimate of remaining work.
Incomplete searches end with `unverified`; a failed operation closes its line
with `failed` before the error diagnostics, rather than looking successful.

`--verbose` (`-v`) enables detailed preparation, resolution, compilation,
candidate results, and fallback messages, retaining the `cargo-floor:` log
prefix that compact progress omits. Candidate numbers are positions
within the eligible release list and can jump during binary search.
Long verbose operations show elapsed-time updates every ten seconds. These
updates indicate that an operation has not returned, not that useful work or
completion is guaranteed. Cargo diagnostics remain captured for failure
reporting and JSON probe evidence.

`--quiet` (`-q`) suppresses progress only. Warnings, errors, and the final human
or JSON report remain enabled. Quiet and verbose modes are mutually exclusive.
All modes leave report stdout unchanged, so it can still be redirected or
consumed by another program.

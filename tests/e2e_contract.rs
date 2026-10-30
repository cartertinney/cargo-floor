//! Proposal semantics, final witnesses, unsupported inputs, and file safety.

mod support;

use semver::VersionReq;
use serde_json::Value;
use std::fs;
use std::process::Command;
use support::{
    BuildGate, Fixture, Release, RunningFloor, assert_failure, assert_success, assert_unsupported,
    dependency, parse_report, quoted, verified,
};

const OLD: &str = "pub fn old() -> u32 { 1 }\n";
const NEW: &str = "pub fn old() -> u32 { 1 }\npub fn needed() -> u32 { 2 }\n";

fn publish_api(fixture: &Fixture, name: &str) {
    for (version, source) in [("1.0.0", OLD), ("1.2.0", NEW), ("1.8.0", NEW)] {
        fixture.publish(Release::new(name, version, source));
    }
}

fn selected_version(fixture: &Fixture, package: &str) -> String {
    let output = fixture.cargo(&["metadata", "--offline", "--format-version", "1"]);
    assert_success(&output);
    let metadata: Value = serde_json::from_slice(&output.stdout).unwrap();
    metadata["packages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|candidate| candidate["name"] == package)
        .unwrap()["version"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn proposal_case(
    name: &str,
    original: &str,
    versions: &[(&str, &str)],
    minimum: &str,
    highest: &str,
    excluded: &[&str],
) {
    let fixture = Fixture::new(name);
    for (version, source) in versions {
        fixture.publish(Release::new("api", version, source));
    }
    fixture.project(
        "sample",
        &format!("[dependencies]\napi = {}\n", quoted(original)),
        "pub fn value() -> u32 { api::needed() }\n",
    );
    fixture.check(&[]);
    let untouched = fixture.snapshot(&["Cargo.lock", "src/lib.rs"]);
    let report = fixture.floor(&["--apply"]);
    let api = verified(&report, "api", "api", original, minimum);
    let proposed = api["proposed_requirement"].as_str().unwrap();
    assert!(
        fixture
            .read("Cargo.toml")
            .windows(quoted(proposed).len())
            .any(|bytes| bytes == quoted(proposed).as_bytes()),
        "apply must install the reported requirement",
    );
    fixture.assert_unchanged(&untouched);
    let requirement = VersionReq::parse(proposed).unwrap();
    assert!(requirement.matches(&minimum.parse().unwrap()), "{api:#}");
    assert!(requirement.matches(&highest.parse().unwrap()), "{api:#}");
    for version in excluded {
        assert!(
            !requirement.matches(&version.parse().unwrap()),
            "proposal broadened an upper bound or admitted {version} below its floor: {api:#}",
        );
    }
    fixture.assert_floor_witness(&report, api, &[]);
    assert_success(&fixture.cargo(&["update", "--offline"]));
    assert_eq!(selected_version(&fixture, "api"), highest);
    fixture.check(&[]);
    for version in excluded {
        let output = fixture.cargo(&["update", "--offline", "-p", "api", "--precise", version]);
        assert_failure(
            &output,
            "Cargo must reject releases excluded by the applied requirement",
        );
    }
}

#[test]
fn caret_proposal_admits_its_floor_but_not_lower_or_next_major_releases() {
    proposal_case(
        "proposal-caret",
        "^1.8",
        &[
            ("1.0.0", OLD),
            ("1.2.0", NEW),
            ("1.8.0", NEW),
            ("1.9.0", NEW),
            ("2.0.0", NEW),
        ],
        "1.2.0",
        "1.9.0",
        &["1.0.0", "2.0.0"],
    );
}

#[test]
fn tilde_patch_requirement_keeps_its_original_minor_ceiling_after_lowering() {
    proposal_case(
        "proposal-tilde",
        "~1.8.4",
        &[
            ("1.0.0", OLD),
            ("1.2.0", NEW),
            ("1.8.4", NEW),
            ("1.8.9", NEW),
            ("1.9.0", NEW),
        ],
        "1.2.0",
        "1.8.9",
        &["1.0.0", "1.9.0"],
    );
}

#[test]
fn zero_major_wildcard_keeps_its_compatibility_line_and_excludes_below_floor() {
    proposal_case(
        "proposal-wildcard-zero",
        "0.2.*",
        &[
            ("0.1.9", NEW),
            ("0.2.0", OLD),
            ("0.2.2", NEW),
            ("0.2.8", NEW),
            ("0.3.0", NEW),
        ],
        "0.2.2",
        "0.2.8",
        &["0.1.9", "0.2.0", "0.3.0"],
    );
}

#[test]
fn explicit_inclusive_upper_bound_remains_inclusive_not_one_patch_wider() {
    proposal_case(
        "proposal-inclusive",
        ">=1.6, <=1.8.0",
        &[
            ("1.0.0", OLD),
            ("1.2.0", NEW),
            ("1.6.0", NEW),
            ("1.8.0", NEW),
            ("1.8.1", NEW),
        ],
        "1.2.0",
        "1.8.0",
        &["1.0.0", "1.8.1"],
    );
}

#[test]
fn exclusive_lower_comparator_is_replaced_by_an_inclusive_verified_floor() {
    proposal_case(
        "proposal-exclusive",
        ">1.6, <1.8",
        &[
            ("1.0.0", OLD),
            ("1.2.0", NEW),
            ("1.6.0", NEW),
            ("1.7.0", NEW),
            ("1.8.0", NEW),
        ],
        "1.2.0",
        "1.7.0",
        &["1.0.0", "1.8.0"],
    );
}

#[test]
fn final_requirements_that_invalidate_an_independent_witness_abort_without_applying() {
    let fixture = Fixture::new("invalidated-final-witness");
    fixture.publish(Release::new("bar", "2.0.0", "pub fn common() -> u32 { 0 }"));
    for version in ["2.5.0", "2.8.0"] {
        fixture.publish(Release::new(
            "bar",
            version,
            "pub fn common() -> u32 { 0 }\npub fn needed() -> u32 { 5 }",
        ));
    }
    fixture.publish(
        Release::new("foo", "1.0.0", "pub fn value() -> u32 { bar::common() }")
            .dependency("bar", ">=2.0, <2.5"),
    );
    // The latest foo intentionally declares an overly loose requirement.
    // Cargo will select it for bar 2.0, then fail compilation, not backtrack.
    fixture.publish(
        Release::new("foo", "1.8.0", "pub fn value() -> u32 { bar::needed() }")
            .dependency("bar", "2"),
    );
    fixture.project(
        "sample",
        "[dependencies]\nfoo = \"1.8\"\nbar = \"2.8\"\n",
        "pub fn value() -> u32 { foo::value() + bar::common() }\n",
    );
    fixture.check(&[]);
    let snapshot = fixture.snapshot(&["Cargo.toml", "Cargo.lock", "src/lib.rs"]);
    for args in [
        vec!["floor", "--format", "json"],
        vec!["floor", "--format", "json", "--apply"],
    ] {
        let output = fixture.floor_raw(&args);
        assert_failure(&output, "final lower bounds invalidate the foo 1.0 witness");
        let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
        assert!(
            (stderr.contains("witness") || stderr.contains("final") || stderr.contains("proposed"))
                && (stderr.contains("foo") || stderr.contains("bar")),
            "stderr must distinguish final witness invalidation from baseline failure: {stderr}",
        );
        fixture.assert_unchanged(&snapshot);
    }
}

#[test]
fn unsupported_requirement_families_stay_unchanged_during_safe_partial_apply() {
    let fixture = Fixture::new("unsupported-requirement-families");
    let unsupported = [
        ("pinned", "=1.8.0"),
        ("preview", "1.8.0-beta.1"),
        ("unbounded", "*"),
        ("open_ended", ">=1.8"),
        ("cross_line", ">=1, <3"),
        ("upper_only", "<2"),
    ];
    let mut dependencies = String::from("[dependencies]\ngood = \"1.8\"\n");
    let mut expression = String::from("good::needed()");
    publish_api(&fixture, "good");
    for (name, requirement) in unsupported {
        publish_api(&fixture, name);
        fixture.publish(Release::new(name, "2.0.0", NEW));
        dependencies.push_str(&format!("{name} = {}\n", quoted(requirement)));
        expression.push_str(&format!(" + {name}::needed()"));
    }
    fixture.project(
        "sample",
        &dependencies,
        &format!("pub fn value() -> u32 {{ {expression} }}"),
    );
    fixture.check(&[]);
    let snapshot = fixture.snapshot(&["Cargo.toml", "Cargo.lock", "src/lib.rs"]);
    let report_only = fixture.floor(&[]);
    fixture.assert_unchanged(&snapshot);
    for (name, original) in unsupported {
        assert_unsupported(&report_only, name, original);
    }
    let before = String::from_utf8(fixture.read("Cargo.toml")).unwrap();
    let untouched = fixture.snapshot(&["Cargo.lock", "src/lib.rs"]);
    let report = fixture.floor(&["--apply"]);
    assert_eq!(report["applied"], true, "{report:#}");
    let good = verified(&report, "good", "good", "1.8", "1.2.0");
    for (name, original) in unsupported {
        assert_unsupported(&report, name, original);
    }
    assert_eq!(
        fixture.read("Cargo.toml"),
        before
            .replacen(
                "good = \"1.8\"",
                &format!(
                    "good = {}",
                    quoted(good["proposed_requirement"].as_str().unwrap())
                ),
                1,
            )
            .as_bytes(),
    );
    fixture.assert_unchanged(&untouched);
    fixture.check(&[]);
}

#[test]
fn inherited_workspace_requirements_are_not_mistaken_for_package_local_evidence() {
    let fixture = Fixture::new("workspace-inherited-partial");
    publish_api(&fixture, "shared");
    publish_api(&fixture, "direct");
    fixture.write(
        "Cargo.toml",
        "[workspace]\nmembers = [\"alpha\", \"beta\"]\ndefault-members = [\"alpha\"]\n\
         resolver = \"2\"\n[workspace.dependencies]\nshared = \"1.8\"\n",
    );
    fixture.write(
        "alpha/Cargo.toml",
        "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
         [dependencies]\nshared = { workspace = true }\ndirect = \"1.8\"\n",
    );
    fixture.write(
        "alpha/src/lib.rs",
        "pub fn value() -> u32 { shared::needed() + direct::needed() }\n",
    );
    fixture.write(
        "beta/Cargo.toml",
        "[package]\nname = \"beta\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
         [dependencies]\nshared.workspace = true\n",
    );
    fixture.write(
        "beta/src/lib.rs",
        "pub fn value() -> u32 { shared::needed() }\n",
    );
    fixture.check(&["-p", "alpha"]);
    let untouched = fixture.snapshot(&[
        "Cargo.toml",
        "Cargo.lock",
        "alpha/src/lib.rs",
        "beta/Cargo.toml",
        "beta/src/lib.rs",
    ]);
    let report = fixture.floor(&["--apply"]);
    assert_eq!(report["package"], "alpha", "{report:#}");
    assert_eq!(report["applied"], true, "{report:#}");
    assert_unsupported(&report, "shared", "1.8");
    verified(&report, "direct", "direct", "1.8", "1.2.0");
    assert!(
        String::from_utf8(fixture.read("alpha/Cargo.toml"))
            .is_ok_and(|manifest| manifest.contains("workspace = true"))
    );
    fixture.assert_unchanged(&untouched);
    fixture.check(&["-p", "alpha"]);
}

#[test]
fn directory_sources_are_incomplete_and_never_treated_as_registry_release_evidence() {
    let fixture = Fixture::new("unsupported-directory-source");
    publish_api(&fixture, "api");
    fixture.directory_source();
    fixture.project(
        "sample",
        "[dependencies]\napi = \"1.8\"\n",
        "pub fn value() -> u32 { api::needed() }\n",
    );
    fixture.check(&[]);
    let snapshot = fixture.snapshot(&["Cargo.toml", "Cargo.lock", "src/lib.rs"]);
    let report = fixture.floor(&["--apply"]);
    assert_unsupported(&report, "api", "1.8");
    assert_eq!(report["applied"], false, "{report:#}");
    fixture.assert_unchanged(&snapshot);
}

#[test]
fn a_local_git_dependency_is_explicitly_unsupported_and_not_edited() {
    let fixture = Fixture::new("unsupported-local-git");
    fixture.write(
        "git-api/Cargo.toml",
        "[package]\nname = \"git_api\"\nversion = \"1.8.0\"\nedition = \"2021\"\n",
    );
    fixture.write("git-api/src/lib.rs", NEW);
    let repository = fixture.project.join("git-api");
    for args in [
        vec!["init", "--quiet"],
        vec!["add", "Cargo.toml", "src/lib.rs"],
        vec![
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-m",
            "fixture",
        ],
    ] {
        assert_success(
            &Command::new("git")
                .args(args)
                .current_dir(&repository)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .output()
                .unwrap(),
        );
    }
    fixture.project(
        "sample",
        &format!(
            "[dependencies]\ngit_api = {{ version = \"1.8\", git = {} }}\n",
            quoted(&format!("file://{}", repository.display())),
        ),
        "pub fn value() -> u32 { git_api::needed() }\n",
    );
    // Prime only this file:// repository; all registry sources remain local.
    // Cargo refuses even a local Git checkout until its cache is populated.
    assert_success(
        &fixture
            .cargo_command(&["check", "--lib"])
            .env("CARGO_NET_OFFLINE", "false")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap(),
    );
    fixture.check(&[]);
    let snapshot = fixture.snapshot(&[
        "Cargo.toml",
        "Cargo.lock",
        "src/lib.rs",
        "git-api/Cargo.toml",
        "git-api/src/lib.rs",
    ]);
    let report = fixture.floor(&["--apply"]);
    assert_unsupported(&report, "git_api", "1.8");
    assert_eq!(report["applied"], false);
    fixture.assert_unchanged(&snapshot);
}

#[test]
fn cargo_path_overrides_cannot_establish_a_published_registry_floor() {
    let fixture = Fixture::new("unsupported-path-override");
    fixture.publish(Release::new("api", "1.8.0", OLD));
    fixture.write(
        "override-api/Cargo.toml",
        "[package]\nname = \"api\"\nversion = \"1.8.0\"\nedition = \"2021\"\n",
    );
    fixture.write("override-api/src/lib.rs", NEW);
    fixture.write(".cargo/config.toml", "paths = [\"override-api\"]\n");
    fixture.project(
        "sample",
        "[dependencies]\napi = \"1.8\"\n",
        "pub fn value() -> u32 { api::needed() }\n",
    );
    fixture.check(&[]);
    let snapshot = fixture.snapshot(&[
        "Cargo.toml",
        "Cargo.lock",
        "src/lib.rs",
        ".cargo/config.toml",
        "override-api/Cargo.toml",
        "override-api/src/lib.rs",
    ]);
    let report = fixture.floor(&["--apply"]);
    assert_unsupported(&report, "api", "1.8");
    assert_eq!(report["applied"], false);
    fixture.assert_unchanged(&snapshot);
}

#[test]
fn locked_yanked_only_working_release_cannot_produce_a_verified_or_applied_floor() {
    let fixture = Fixture::new("locked-yanked-witness");
    publish_api(&fixture, "good");
    fixture.publish(Release::new("api", "1.0.0", OLD));
    fixture.publish(Release::new("api", "1.8.0", NEW));
    fixture.project(
        "sample",
        "[dependencies]\napi = \"1.8\"\ngood = \"1.8\"\n",
        "pub fn value() -> u32 { api::needed() + good::needed() }\n",
    );
    fixture.check(&[]);
    fixture.yank("api", "1.8.0");
    fixture.check(&["--locked"]);
    let snapshot = fixture.snapshot(&["Cargo.toml", "Cargo.lock", "src/lib.rs"]);
    let output = fixture.floor_raw(&["floor", "--format", "json"]);
    if output.status.success() {
        let report = parse_report(&output);
        let api = dependency(&report, "api");
        assert_eq!(report["status"], "incomplete", "{report:#}");
        assert_eq!(api["status"], "unverified", "{api:#}");
        assert!(api["lowest_verified"].is_null(), "{api:#}");
        assert_eq!(api["proposed_requirement"], "1.8", "{api:#}");
        assert!(
            api["probes"]
                .as_array()
                .unwrap()
                .iter()
                .all(|probe| { probe["version"] != "1.8.0" && probe["outcome"] != "passed" }),
            "{api:#}",
        );
    } else {
        assert_failure(
            &output,
            "a yanked lockfile is not stable non-yanked evidence",
        );
        let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
        assert!(
            stderr.contains("yanked")
                || stderr.contains("unverified")
                || stderr.contains("no stable"),
            "{stderr}",
        );
    }
    fixture.assert_unchanged(&snapshot);
    let apply = fixture.floor_raw(&["floor", "--format", "json", "--apply"]);
    assert_failure(
        &apply,
        "unverified active floors must never be partially applied",
    );
    fixture.assert_unchanged(&snapshot);
}

#[test]
fn no_eligible_stable_candidates_is_not_success_shaped_json() {
    let fixture = Fixture::new("no-eligible-candidates");
    fixture.publish(Release::new("api", "1.8.0", NEW));
    fixture.publish(Release::new("api", "1.9.0-beta.1", NEW));
    fixture.project(
        "sample",
        "[dependencies]\napi = \"1.8\"\n",
        "pub fn value() -> u32 { api::needed() }\n",
    );
    fixture.check(&[]);
    fixture.yank("api", "1.8.0");
    let snapshot = fixture.snapshot(&["Cargo.toml", "Cargo.lock", "src/lib.rs"]);
    let output = fixture.floor_raw(&["floor", "--format", "json", "--apply"]);
    assert_failure(
        &output,
        "yanked and prerelease releases cannot verify a stable floor",
    );
    let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
    assert!(
        stderr.contains("yanked") || stderr.contains("unverified") || stderr.contains("no stable"),
        "{stderr}",
    );
    fixture.assert_unchanged(&snapshot);
}

#[test]
fn unselected_workspace_member_constraints_and_sources_do_not_raise_the_floor() {
    let fixture = Fixture::new("unrelated-workspace-constraint");
    publish_api(&fixture, "api");
    fixture.write(
        "Cargo.toml",
        "[workspace]\nmembers = [\"selected\", \"unrelated\"]\nresolver = \"2\"\n",
    );
    fixture.write(
        "selected/Cargo.toml",
        "[package]\nname = \"selected\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
         [dependencies]\napi = \"1.8\"\n",
    );
    fixture.write(
        "selected/src/lib.rs",
        "pub fn value() -> u32 { api::needed() }\n",
    );
    fixture.write(
        "unrelated/Cargo.toml",
        "[package]\nname = \"unrelated\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
         [dependencies]\napi = \"=1.8.0\"\n",
    );
    fixture.write(
        "unrelated/src/lib.rs",
        "compile_error!(\"must not compile an unrelated member\");",
    );
    fixture.check(&["-p", "selected"]);
    let untouched = fixture.snapshot(&[
        "Cargo.toml",
        "Cargo.lock",
        "selected/src/lib.rs",
        "unrelated/Cargo.toml",
        "unrelated/src/lib.rs",
    ]);
    let report = fixture.floor(&["-p", "selected", "--apply"]);
    verified(&report, "api", "api", "1.8", "1.2.0");
    assert_eq!(report["package"], "selected");
    fixture.assert_unchanged(&untouched);
    fixture.check(&["-p", "selected"]);
}

#[test]
fn virtual_workspace_single_default_is_selected_but_ambiguous_defaults_are_errors() {
    let fixture = Fixture::new("virtual-default-selection");
    publish_api(&fixture, "api");
    let workspace = "[workspace]\nmembers = [\"alpha\", \"beta\"]\ndefault-members = [\"alpha\"]\nresolver = \"2\"\n";
    fixture.write("Cargo.toml", workspace);
    for (name, call) in [("alpha", "old"), ("beta", "needed")] {
        fixture.write(
            &format!("{name}/Cargo.toml"),
            &format!(
                "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
                 [dependencies]\napi = \"1.8\"\n"
            ),
        );
        fixture.write(
            &format!("{name}/src/lib.rs"),
            &format!("pub fn value() -> u32 {{ api::{call}() }}\n"),
        );
    }
    fixture.check(&[]);
    let report = fixture.floor(&[]);
    assert_eq!(report["package"], "alpha");
    verified(&report, "api", "api", "1.8", "1.0.0");
    fixture.write(
        "Cargo.toml",
        &workspace.replace(
            "default-members = [\"alpha\"]",
            "default-members = [\"alpha\", \"beta\"]",
        ),
    );
    let snapshot = fixture.snapshot(&[
        "Cargo.toml",
        "Cargo.lock",
        "alpha/Cargo.toml",
        "beta/Cargo.toml",
    ]);
    let output = fixture.floor_raw(&["floor", "--format", "json", "--apply"]);
    assert_failure(
        &output,
        "ambiguous virtual workspace selection must require --package",
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("package") || stderr.contains("select"),
        "{stderr}"
    );
    fixture.assert_unchanged(&snapshot);
}

#[test]
fn nonvirtual_workspace_manifest_selects_its_root_library_not_a_different_default_member() {
    let fixture = Fixture::new("nonvirtual-workspace-root");
    publish_api(&fixture, "api");
    fixture.project(
        "root_library",
        "[workspace]\nmembers = [\"child\"]\ndefault-members = [\"child\"]\nresolver = \"2\"\n\
         [dependencies]\napi = \"1.8\"\n",
        "pub fn value() -> u32 { api::old() }\n",
    );
    fixture.write(
        "child/Cargo.toml",
        "[package]\nname = \"child\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
         [dependencies]\napi = \"1.8\"\n",
    );
    fixture.write(
        "child/src/lib.rs",
        "pub fn value() -> u32 { api::needed() }\n",
    );
    fixture.check(&["-p", "root_library"]);
    let report = fixture.floor(&[]);
    assert_eq!(report["package"], "root_library", "{report:#}");
    verified(&report, "api", "api", "1.8", "1.0.0");
    let child = fixture.floor(&["-p", "child"]);
    verified(&child, "api", "api", "1.8", "1.2.0");
}

#[test]
fn nested_invocation_retains_relative_source_replacement_chains_and_spaces() {
    let fixture = Fixture::new("relative registry paths with spaces");
    publish_api(&fixture, "api");
    fixture.project(
        "sample",
        "[dependencies]\napi = \"1.8\"\n",
        "pub fn value() -> u32 { api::needed() }\n",
    );
    fixture.write(
        ".cargo/config.toml",
        "[source.crates-io]\nreplace-with = \"bridge\"\n\
         [source.bridge]\nregistry = \"sparse+http://127.0.0.1:9/never-used/\"\nreplace-with = \"last\"\n\
         [source.last]\nlocal-registry = \"../registry\"\n",
    );
    fixture.write("nested/deep/keep", "fixture");
    let invocation = fixture.project.join("nested/deep");
    assert_success(
        &fixture
            .cargo_command(&[
                "check",
                "--offline",
                "--lib",
                "--manifest-path",
                "../../Cargo.toml",
            ])
            .current_dir(&invocation)
            .output()
            .unwrap(),
    );
    let snapshot = fixture.snapshot(&[
        "Cargo.toml",
        "Cargo.lock",
        "src/lib.rs",
        ".cargo/config.toml",
    ]);
    let output = fixture
        .floor_command(&[
            "floor",
            "--format",
            "json",
            "--manifest-path",
            "../../Cargo.toml",
        ])
        .current_dir(&invocation)
        .output()
        .unwrap();
    let report = parse_report(&output);
    verified(&report, "api", "api", "1.8", "1.2.0");
    fixture.assert_unchanged(&snapshot);
}

#[test]
fn scoped_build_target_env_overrides_a_single_configured_target() {
    let fixture = Fixture::new("scoped-env-target");
    publish_api(&fixture, "api");
    let host = fixture.host_target();
    fixture.project(
        "sample",
        &format!("[target.'{host}'.dependencies]\napi = \"1.8\"\n"),
        "pub fn value() -> u32 { api::needed() }\n",
    );
    fixture.write(
        ".cargo/config.toml",
        "[build]\ntarget = \"not-a-real-target\"\n",
    );
    assert_success(
        &fixture
            .cargo_command(&["check", "--offline", "--lib"])
            .env("CARGO_BUILD_TARGET", &host)
            .output()
            .unwrap(),
    );
    let output = fixture
        .floor_command(&["floor", "--format", "json"])
        .env("CARGO_BUILD_TARGET", &host)
        .output()
        .unwrap();
    let report = parse_report(&output);
    assert_eq!(report["configuration"]["target"], host, "{report:#}");
    verified(&report, "api", "api", "1.8", "1.2.0");
}

#[test]
fn invalid_selected_features_and_ambiguous_target_matrices_are_nonmutating_errors() {
    let fixture = Fixture::new("invalid-selected-config");
    publish_api(&fixture, "api");
    fixture.project(
        "sample",
        "[dependencies]\napi = \"1.8\"\n",
        "pub fn value() -> u32 { api::needed() }\n",
    );
    fixture.check(&[]);
    let snapshot = fixture.snapshot(&["Cargo.toml", "Cargo.lock", "src/lib.rs"]);
    let output = fixture.floor_raw(&[
        "floor",
        "--format",
        "json",
        "--features",
        "missing-feature",
        "--apply",
    ]);
    assert_failure(
        &output,
        "invalid feature selection is not a discovery result",
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("missing-feature"));
    fixture.assert_unchanged(&snapshot);
    let host = fixture.host_target();
    fixture.write(
        ".cargo/config.toml",
        &format!(
            "[build]\ntarget = [{}, \"not-a-real-target\"]\n",
            quoted(&host)
        ),
    );
    let snapshot = fixture.snapshot(&[
        "Cargo.toml",
        "Cargo.lock",
        "src/lib.rs",
        ".cargo/config.toml",
    ]);
    let output = fixture.floor_raw(&["floor", "--format", "json", "--apply"]);
    assert_failure(
        &output,
        "a target matrix must not silently select one target",
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("target"));
    fixture.assert_unchanged(&snapshot);
}

#[test]
fn apply_preserves_crlf_quote_styles_dotted_forms_comments_and_multiple_value_spans() {
    let fixture = Fixture::new("crlf-multiple-spans");
    for name in ["api", "other_api", "builder"] {
        publish_api(&fixture, name);
    }
    let manifest = r#"# Keep CRLF and quotes.
[package]
name = 'sample'
version = '0.1.0'
edition = '2021'
[dependencies]
dotted.package = 'api'
dotted.version = '1.8' # first span
dotted.default-features = false
inline = { version = "1.8", package = "other_api", default-features = false } # second span
[build-dependencies.builder]
version = '1.8' # third span
[package.metadata.fixture]
not_a_dependency = '1.8'
"#
    .replace('\n', "\r\n");
    fixture.write("Cargo.toml", &manifest);
    fixture.write(
        "src/lib.rs",
        "pub fn value() -> u32 { dotted::needed() + inline::needed() }\n",
    );
    fixture.write(
        "build.rs",
        "fn main() { assert_eq!(builder::needed(), 2); }\n",
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            fixture.project.join("Cargo.toml"),
            fs::Permissions::from_mode(0o640),
        )
        .unwrap();
    }
    fixture.check(&[]);
    let untouched = fixture.snapshot(&["Cargo.lock", "src/lib.rs", "build.rs"]);
    let report = fixture.floor(&["--apply"]);
    let dotted = verified(&report, "dotted", "api", "1.8", "1.2.0");
    let inline = verified(&report, "inline", "other_api", "1.8", "1.2.0");
    let builder = verified(&report, "builder", "builder", "1.8", "1.2.0");
    let expected = manifest
        .replacen(
            "dotted.version = '1.8'",
            &format!(
                "dotted.version = '{}'",
                dotted["proposed_requirement"].as_str().unwrap()
            ),
            1,
        )
        .replacen(
            "version = \"1.8\"",
            &format!(
                "version = {}",
                quoted(inline["proposed_requirement"].as_str().unwrap())
            ),
            1,
        )
        .replacen(
            "version = '1.8' # third",
            &format!(
                "version = '{}' # third",
                builder["proposed_requirement"].as_str().unwrap()
            ),
            1,
        );
    assert_eq!(fixture.read("Cargo.toml"), expected.as_bytes());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(fixture.project.join("Cargo.toml"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o640,
        );
    }
    fixture.assert_unchanged(&untouched);
    fixture.check(&[]);
}

#[test]
fn already_correct_requirements_make_apply_a_byte_preserving_no_op() {
    let fixture = Fixture::new("no-op-apply");
    publish_api(&fixture, "api");
    fixture.project(
        "sample",
        "[dependencies]\napi = '1.2.0' # already verified\n",
        "pub fn value() -> u32 { api::needed() }\n",
    );
    fixture.check(&[]);
    let snapshot = fixture.snapshot(&["Cargo.toml", "Cargo.lock", "src/lib.rs"]);
    let report = fixture.floor(&["--apply"]);
    verified(&report, "api", "api", "1.2.0", "1.2.0");
    assert_eq!(report["applied"], false, "{report:#}");
    fixture.assert_unchanged(&snapshot);
    fixture.assert_floor_witness(&report, dependency(&report, "api"), &[]);
}

#[test]
fn concurrent_manifest_edits_are_observed_with_real_build_script_synchronization() {
    let fixture = Fixture::new("concurrent-manifest-edit");
    publish_api(&fixture, "api");
    fixture.project(
        "sample",
        "[dependencies]\napi = \"1.8\"\n",
        "pub fn value() -> u32 { api::needed() }\n",
    );
    fixture.write("build.rs", BuildGate::script());
    fixture.check(&[]);
    let untouched = fixture.snapshot(&["Cargo.lock", "src/lib.rs", "build.rs"]);
    let original = String::from_utf8(fixture.read("Cargo.toml")).unwrap();
    let mut gate = BuildGate::new();
    let mut command = fixture.floor_command(&["floor", "--format", "json", "--apply"]);
    command.env("FLOOR_TEST_GATE", gate.address()).env(
        "FLOOR_TEST_GATE_MARKER",
        fixture.scratch_path().join("once"),
    );
    let running = RunningFloor::spawn(&mut command);
    gate.wait_ready();
    // The initial manifest has already been read and copied at this handshake.
    let concurrent = format!("{original}\n# concurrently added and must survive\n");
    fixture.write("Cargo.toml", &concurrent);
    gate.release();
    let output = running.finish();
    assert_failure(
        &output,
        "discovery must not overwrite edits made while Cargo was running",
    );
    let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
    assert!(
        stderr.contains("manifest")
            && (stderr.contains("changed") || stderr.contains("concurrent")),
        "{stderr}",
    );
    assert_eq!(fixture.read("Cargo.toml"), concurrent.as_bytes());
    fixture.assert_unchanged(&untouched);
}

#[cfg(unix)]
#[test]
fn documented_directory_symlink_rejection_is_explicit_and_nonmutating() {
    let fixture = Fixture::new("reject-directory-symlink");
    publish_api(&fixture, "api");
    fixture.project(
        "sample",
        "[dependencies]\napi = \"1.8\"\n",
        "pub fn value() -> u32 { api::needed() }\n",
    );
    fixture.check(&[]);
    fixture.write("real-directory/keep", "unchanged");
    std::os::unix::fs::symlink("real-directory", fixture.project.join("linked-directory")).unwrap();
    let snapshot = fixture.snapshot(&[
        "Cargo.toml",
        "Cargo.lock",
        "src/lib.rs",
        "real-directory/keep",
    ]);
    let output = fixture.floor_raw(&["floor", "--format", "json", "--apply"]);
    assert_failure(&output, "directory symlinks are documented as unsupported");
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .to_lowercase()
            .contains("symlink")
    );
    assert!(
        fs::symlink_metadata(fixture.project.join("linked-directory"))
            .unwrap()
            .is_symlink()
    );
    fixture.assert_unchanged(&snapshot);
}

#[cfg(unix)]
#[test]
fn documented_special_file_rejection_does_not_read_or_replace_a_unix_socket() {
    let fixture = Fixture::new("special");
    publish_api(&fixture, "api");
    fixture.project(
        "sample",
        "[dependencies]\napi = \"1.8\"\n",
        "pub fn value() -> u32 { api::needed() }\n",
    );
    fixture.check(&[]);
    let _socket = std::os::unix::net::UnixListener::bind(fixture.project.join("s")).unwrap();
    let snapshot = fixture.snapshot(&["Cargo.toml", "Cargo.lock", "src/lib.rs"]);
    let output = fixture.floor_raw(&["floor", "--format", "json", "--apply"]);
    assert_failure(&output, "special files are documented as unsupported");
    let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
    assert!(
        stderr.contains("special") || stderr.contains("socket"),
        "{stderr}"
    );
    fixture.assert_unchanged(&snapshot);
}

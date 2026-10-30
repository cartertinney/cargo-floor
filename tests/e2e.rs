mod support;

use serde_json::Value;
use support::{
    Fixture, Release, assert_failure, assert_inactive, assert_resolved, assert_success, dependency,
    parse_report, probe, quoted, verified,
};

const OLD_API: &str = "pub fn old() -> u32 { 1 }\n";
const NEW_API: &str = "pub fn old() -> u32 { 1 }\npub fn needed() -> u32 { 2 }\n";

fn api_fixture(name: &str, requirement: &str) -> Fixture {
    let fixture = Fixture::new(name);
    for (version, source) in [("1.0.0", OLD_API), ("1.2.0", NEW_API), ("1.8.0", NEW_API)] {
        fixture.publish(Release::new("api", version, source));
    }
    fixture.project(
        "sample",
        &format!("[dependencies]\napi = {}\n", quoted(requirement)),
        "pub fn value() -> u32 { api::needed() }\n",
    );
    fixture
}

#[test]
fn lowers_overstated_floor_with_forwarded_and_direct_invocation() {
    let fixture = api_fixture("lower", "1.8");
    fixture.check(&[]);

    let help = fixture.floor_raw(&["--help"]);
    assert_success(&help);
    let help = String::from_utf8(help.stdout).unwrap();
    assert!(help.contains("--manifest-path") && help.contains("--apply"));

    let report = fixture.floor(&[]);
    assert_eq!(report["package"], "sample");
    assert_eq!(report["applied"], false);
    let api = verified(&report, "api", "api", "1.8", "1.2.0");
    probe(api, "1.0.0", "build_failed");

    let manifest = fixture.project.join("Cargo.toml");
    let direct = parse_report(&fixture.floor_raw(&[
        "--format",
        "json",
        "--manifest-path",
        manifest.to_str().unwrap(),
    ]));
    verified(&direct, "api", "api", "1.8", "1.2.0");
}

#[test]
fn raises_understated_floor_from_current_library_source() {
    let fixture = api_fixture("raise", "1.0");
    fixture.check(&[]);
    let report = fixture.floor(&[]);
    let api = verified(&report, "api", "api", "1.0", "1.2.0");
    probe(api, "1.0.0", "build_failed");
}

#[test]
fn exhaustive_search_finds_oldest_passing_release_despite_later_compile_regression() {
    let fixture = Fixture::new("nonmonotonic");
    for (version, source) in [
        ("1.0.0", OLD_API),
        ("1.1.0", NEW_API),
        (
            "1.2.0",
            "compile_error!(\"a real intermediate regression\");",
        ),
        (
            "1.3.0",
            "compile_error!(\"the regression persists for another release\");",
        ),
        ("1.4.0", NEW_API),
    ] {
        fixture.publish(Release::new("api", version, source));
    }
    fixture.project(
        "sample",
        "[dependencies]\napi = \"1.4\"\n",
        "pub fn value() -> u32 { api::needed() }\n",
    );
    fixture.check(&[]);
    let report = fixture.floor(&["--exhaustive"]);
    let api = verified(&report, "api", "api", "1.4", "1.1.0");
    probe(api, "1.0.0", "build_failed");
}

#[test]
fn independent_minima_use_separate_resolver_witnesses_not_an_all_minima_lockfile() {
    let fixture = Fixture::new("independent");
    for version in ["2.1.0", "2.5.0"] {
        fixture.publish(Release::new("bar", version, "pub fn value() -> u32 { 2 }"));
    }
    fixture.publish(
        Release::new("foo", "1.2.0", "pub fn value() -> u32 { bar::value() }")
            .dependency("bar", "^2.5"),
    );
    fixture.publish(
        Release::new("foo", "1.4.0", "pub fn value() -> u32 { bar::value() }")
            .dependency("bar", "^2.1"),
    );
    fixture.project(
        "sample",
        "[dependencies]\nfoo = \"1.4\"\nbar = \"2.5\"\n",
        "pub fn value() -> u32 { foo::value() + bar::value() }\n",
    );
    fixture.check(&[]);
    let report = fixture.floor(&["--apply"]);
    assert_eq!(report["applied"], true);
    let foo = verified(&report, "foo", "foo", "1.4", "1.2.0");
    let bar = verified(&report, "bar", "bar", "2.5", "2.1.0");
    let foo_witness = probe(foo, "1.2.0", "passed");
    assert_resolved(foo_witness, "foo", "1.2.0");
    assert_resolved(foo_witness, "bar", "2.5.0");
    let bar_witness = probe(bar, "2.1.0", "passed");
    assert_resolved(bar_witness, "foo", "1.4.0");
    assert_resolved(bar_witness, "bar", "2.1.0");
    fixture.check(&[]);
}

#[test]
fn report_only_preserves_manifest_lockfile_sources_and_absent_lockfiles() {
    let fixture = api_fixture("report-only", "1.8");
    fixture.write("src/untouched.rs", "// Deliberately not a module.\n");
    fixture.check(&[]);
    let snapshot =
        fixture.snapshot(&["Cargo.toml", "Cargo.lock", "src/lib.rs", "src/untouched.rs"]);
    let report = fixture.floor(&[]);
    verified(&report, "api", "api", "1.8", "1.2.0");
    assert_eq!(report["applied"], false);
    fixture.assert_unchanged(&snapshot);

    std::fs::remove_file(fixture.project.join("Cargo.lock")).unwrap();
    let report = fixture.floor(&[]);
    verified(&report, "api", "api", "1.8", "1.2.0");
    assert!(
        !fixture.project.join("Cargo.lock").exists(),
        "report-only discovery must not create an original-project lockfile",
    );
}

#[test]
fn apply_edits_only_requirement_values_preserving_alias_features_and_comments() {
    let fixture = Fixture::new("surgical-apply");
    for (version, source) in [("1.0.0", OLD_API), ("1.2.0", NEW_API), ("1.8.0", NEW_API)] {
        fixture.publish(
            Release::new(
                "real_api",
                version,
                &format!(
                    "#[cfg(feature = \"poison\")]\ncompile_error!(\"defaults must stay disabled\");\n\
                     #[cfg(feature = \"gate\")]\nmod enabled {{ {source} }}\n\
                     #[cfg(feature = \"gate\")]\npub use enabled::*;\n"
                ),
            )
            .feature("default", &["poison"])
            .feature("gate", &[])
            .feature("poison", &[]),
        );
    }
    let manifest = r#"# Preserve this header.
[package]
name = "sample"
version = "0.1.0"
edition = "2021"
description = "untouched"

[dependencies]
# Keep this renamed dependency's attributes.
renamed = { package = "real_api", version = "1.8", default-features = false, features = ["gate"] } # trailing comment

[features]
default = []
local_setting = []

[package.metadata.fixture]
answer = 42
"#;
    fixture.write("Cargo.toml", manifest);
    fixture.write(
        "src/lib.rs",
        "pub fn value() -> u32 { renamed::needed() }\n",
    );
    fixture.check(&[]);
    let untouched = fixture.snapshot(&["Cargo.lock", "src/lib.rs"]);
    let report = fixture.floor(&["--apply"]);
    assert_eq!(report["applied"], true);
    let renamed = verified(&report, "renamed", "real_api", "1.8", "1.2.0");
    let proposed = renamed["proposed_requirement"].as_str().unwrap();
    let expected = manifest.replacen(
        "version = \"1.8\"",
        &format!("version = {}", quoted(proposed)),
        1,
    );
    assert_eq!(fixture.read("Cargo.toml"), expected.as_bytes());
    fixture.assert_unchanged(&untouched);
    fixture.check(&[]);
}

#[test]
fn excludes_yanked_prerelease_and_other_compatibility_lines() {
    let fixture = Fixture::new("candidate-filter");
    fixture.publish(Release::new("api", "0.9.0", NEW_API));
    fixture.publish(Release::new("api", "1.0.0", NEW_API).yanked());
    fixture.publish(Release::new("api", "1.1.0-beta.1", NEW_API));
    fixture.publish(Release::new("api", "1.1.0", OLD_API));
    fixture.publish(Release::new("api", "1.2.0", NEW_API));
    fixture.publish(Release::new("api", "1.9.0", NEW_API));
    fixture.publish(Release::new("api", "2.0.0", NEW_API));
    fixture.project(
        "sample",
        "[dependencies]\napi = \"1.9\"\n",
        "pub fn value() -> u32 { api::needed() }\n",
    );
    fixture.check(&[]);
    let report = fixture.floor(&[]);
    let api = verified(&report, "api", "api", "1.9", "1.2.0");
    probe(api, "1.1.0", "build_failed");
    for probe in api["probes"].as_array().unwrap() {
        assert!(
            !["0.9.0", "1.0.0", "1.1.0-beta.1", "2.0.0"]
                .contains(&probe["version"].as_str().unwrap()),
            "ineligible release was probed: {probe:#}",
        );
    }
}

#[test]
fn optional_dependencies_are_inactive_until_selected_features_enable_them() {
    let fixture = Fixture::new("optional");
    for (version, source) in [("1.0.0", OLD_API), ("1.2.0", NEW_API), ("1.8.0", NEW_API)] {
        fixture.publish(
            Release::new(
                "optics",
                version,
                &format!(
                    "#[cfg(feature = \"gate\")]\nmod enabled {{ {source} }}\n\
                          #[cfg(feature = \"gate\")]\npub use enabled::*;"
                ),
            )
            .feature("gate", &[]),
        );
    }
    fixture.project(
        "sample",
        r#"[dependencies]
optics = { version = "1.8", optional = true, default-features = false, features = ["gate"] }

[features]
default = []
opt = ["dep:optics"]
spare = []
"#,
        r#"#[cfg(feature = "opt")]
pub fn value() -> u32 { optics::needed() }
#[cfg(not(feature = "opt"))]
pub fn value() -> u32 { 0 }
"#,
    );
    fixture.check(&[]);
    fixture.check(&["--features", "opt,spare"]);
    assert_inactive(&fixture.floor(&[]), "optics");
    let enabled = fixture.floor(&["--features", "opt,spare"]);
    verified(&enabled, "optics", "optics", "1.8", "1.2.0");
    let all = fixture.floor(&["--all-features"]);
    verified(&all, "optics", "optics", "1.8", "1.2.0");
}

#[test]
fn no_default_features_changes_the_library_floor() {
    let fixture = api_fixture("no-default-features", "1.8");
    fixture.project(
        "sample",
        r#"[dependencies]
api = "1.8"

[features]
default = ["modern"]
modern = []
"#,
        r#"#[cfg(feature = "modern")]
pub fn value() -> u32 { api::needed() }
#[cfg(not(feature = "modern"))]
pub fn value() -> u32 { api::old() }
"#,
    );
    fixture.check(&[]);
    fixture.check(&["--no-default-features"]);
    verified(&fixture.floor(&[]), "api", "api", "1.8", "1.2.0");
    verified(
        &fixture.floor(&["--no-default-features"]),
        "api",
        "api",
        "1.8",
        "1.0.0",
    );
}

#[test]
fn dev_dependency_constraints_and_test_only_apis_do_not_raise_library_floor() {
    let fixture = Fixture::new("dev-isolation");
    fixture.publish(Release::new(
        "utility",
        "1.0.0",
        "pub fn library_api() -> u32 { 1 }",
    ));
    fixture.publish(Release::new(
        "utility",
        "1.5.0",
        "pub fn library_api() -> u32 { 1 }\npub fn test_api() -> u32 { 5 }",
    ));
    fixture.project(
        "sample",
        "[dependencies]\nutility = \"1.5\"\n\n[dev-dependencies]\nutility = \"1.5\"\n",
        "pub fn value() -> u32 { utility::library_api() }\n",
    );
    fixture.write(
        "tests/new_api.rs",
        "#[test]\nfn newer_test_only_api() { assert_eq!(utility::test_api(), 5); }\n",
    );
    assert_success(&fixture.cargo(&["test", "--offline", "--no-run"]));
    let report = fixture.floor(&[]);
    let utility = verified(&report, "utility", "utility", "1.5", "1.0.0");
    assert_eq!(utility["kind"], "normal");
    assert_eq!(
        report["dependencies"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["name"] == "utility" && entry["status"] == "verified")
            .count(),
        1,
    );
}

#[test]
fn build_dependency_floor_is_verified_by_the_real_build_script() {
    let fixture = Fixture::new("build-dependency");
    for (version, source) in [
        ("1.0.0", "pub fn old() -> u32 { 0 }"),
        ("1.3.0", "pub fn number() -> u32 { 3 }"),
        ("1.8.0", "pub fn number() -> u32 { 3 }"),
    ] {
        fixture.publish(Release::new("builders", version, source));
    }
    fixture.project(
        "sample",
        "[build-dependencies]\nbuilders = \"1.8\"\n",
        "include!(concat!(env!(\"OUT_DIR\"), \"/value.rs\"));\n\
         pub fn value() -> u32 { VALUE }\n",
    );
    fixture.write(
        "build.rs",
        r#"fn main() {
    let output = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    std::fs::write(
        output.join("value.rs"),
        format!("pub const VALUE: u32 = {};", builders::number()),
    ).unwrap();
}
"#,
    );
    fixture.check(&[]);
    let report = fixture.floor(&[]);
    let builders = verified(&report, "builders", "builders", "1.8", "1.3.0");
    assert_eq!(builders["kind"], "build");
    probe(builders, "1.0.0", "build_failed");
}

#[test]
fn baseline_failure_and_invalid_arguments_are_actionable_and_do_not_apply() {
    let fixture = api_fixture("bad-baseline", "1.8");
    fixture.write(
        "src/lib.rs",
        "pub fn value() -> u32 { api::missing_api() }\n",
    );
    let baseline = fixture.cargo(&["check", "--lib", "--offline"]);
    assert_failure(&baseline, "the real fixture baseline must fail to compile");
    let snapshot = fixture.snapshot(&["Cargo.toml", "Cargo.lock", "src/lib.rs"]);
    let output = fixture.floor_raw(&["floor", "--format", "json", "--apply"]);
    assert_failure(&output, "a failing library baseline must abort discovery");
    let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
    assert!(
        stderr.contains("baseline") || stderr.contains("initial"),
        "stderr should identify the failing initial build: {stderr}",
    );
    fixture.assert_unchanged(&snapshot);

    let invalid = fixture.floor_raw(&["floor", "--definitely-not-an-option"]);
    assert_failure(&invalid, "unknown arguments must not be ignored");
    assert!(
        String::from_utf8_lossy(&invalid.stderr).contains("definitely-not-an-option"),
        "stderr must identify the invalid argument",
    );
}

#[test]
fn workspace_selection_uses_the_selected_library_and_rejects_unknown_or_binary_packages() {
    let fixture = Fixture::new("workspace");
    for (version, source) in [("1.0.0", OLD_API), ("1.2.0", NEW_API), ("1.8.0", NEW_API)] {
        fixture.publish(Release::new("api", version, source));
    }
    fixture.write(
        "Cargo.toml",
        "[workspace]\nmembers = [\"alpha\", \"beta\", \"bin-only\"]\nresolver = \"2\"\n",
    );
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
    fixture.write(
        "bin-only/Cargo.toml",
        "[package]\nname = \"bin-only\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    fixture.write("bin-only/src/main.rs", "fn main() {}\n");
    fixture.check(&["-p", "alpha"]);
    fixture.check(&["-p", "beta"]);
    let alpha = fixture.floor(&["-p", "alpha"]);
    assert_eq!(alpha["package"], "alpha");
    verified(&alpha, "api", "api", "1.8", "1.0.0");
    let beta = fixture.floor(&["--package", "beta"]);
    assert_eq!(beta["package"], "beta");
    verified(&beta, "api", "api", "1.8", "1.2.0");

    let beta_manifest = fixture.project.join("beta/Cargo.toml");
    let selected_by_manifest = fixture.floor(&["--manifest-path", beta_manifest.to_str().unwrap()]);
    assert_eq!(selected_by_manifest["package"], "beta");
    verified(&selected_by_manifest, "api", "api", "1.8", "1.2.0");

    let unknown = fixture.floor_raw(&["floor", "--package", "not-a-member"]);
    assert_failure(&unknown, "unknown workspace packages must be rejected");
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("not-a-member"));
    let binary = fixture.floor_raw(&["floor", "-p", "bin-only"]);
    assert_failure(&binary, "a selected package must have a library target");
    assert!(
        String::from_utf8_lossy(&binary.stderr)
            .to_lowercase()
            .contains("lib"),
        "stderr must explain the absent library",
    );
}

#[test]
fn path_and_patched_dependencies_are_explicitly_unsupported_not_registry_floors() {
    let fixture = Fixture::new("unsupported-sources");
    for version in ["1.0.0", "1.5.0"] {
        fixture.publish(Release::new(
            "patched_dep",
            version,
            "pub fn registry_only() -> u32 { 0 }",
        ));
    }
    for name in ["local_dep", "patched_dep"] {
        fixture.write(
            &format!("vendor/{name}/Cargo.toml"),
            &format!("[package]\nname = \"{name}\"\nversion = \"1.5.0\"\nedition = \"2021\"\n"),
        );
        fixture.write(
            &format!("vendor/{name}/src/lib.rs"),
            "pub fn local_only() -> u32 { 1 }\n",
        );
    }
    fixture.project(
        "sample",
        r#"[dependencies]
local_dep = { version = "1.5", path = "vendor/local_dep" }
patched_dep = "1.5"

[patch.crates-io]
patched_dep = { path = "vendor/patched_dep" }
"#,
        "pub fn value() -> u32 { local_dep::local_only() + patched_dep::local_only() }\n",
    );
    fixture.check(&[]);
    let report = fixture.floor(&[]);
    for name in ["local_dep", "patched_dep"] {
        let entry = dependency(&report, name);
        assert_eq!(entry["status"], "unsupported", "{entry:#}");
        assert!(entry["lowest_verified"].is_null(), "{entry:#}");
        assert!(
            entry["probes"]
                .as_array()
                .unwrap()
                .iter()
                .all(|probe| probe["outcome"] != "passed"),
            "path overrides cannot prove a registry release's API: {entry:#}",
        );
    }
}

#[test]
fn lowering_a_bounded_range_preserves_its_upper_bound() {
    let fixture = Fixture::new("bounded-range");
    for (version, source) in [
        ("0.9.0", NEW_API),
        ("1.0.0", OLD_API),
        ("1.1.0", NEW_API),
        ("1.2.0", NEW_API),
        ("1.6.0", NEW_API),
        ("1.7.0", NEW_API),
        ("2.0.0", NEW_API),
    ] {
        fixture.publish(Release::new("bounded", version, source));
    }
    fixture.project(
        "sample",
        "[dependencies]\nbounded = \">=1.2, <1.7\"\n",
        "pub fn value() -> u32 { bounded::needed() }\n",
    );
    fixture.check(&[]);
    let report = fixture.floor(&["--apply"]);
    let bounded = verified(&report, "bounded", "bounded", ">=1.2, <1.7", "1.1.0");
    for probe in bounded["probes"].as_array().unwrap() {
        assert!(
            !["0.9.0", "1.7.0", "2.0.0"].contains(&probe["version"].as_str().unwrap()),
            "release outside the original compatibility/upper bounds was probed: {probe:#}",
        );
    }
    assert_success(&fixture.cargo(&["update", "--offline"]));
    let metadata = fixture.cargo(&["metadata", "--offline", "--format-version", "1"]);
    assert_success(&metadata);
    let metadata: Value = serde_json::from_slice(&metadata.stdout).unwrap();
    let selected = metadata["packages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|package| package["name"] == "bounded")
        .unwrap();
    assert_eq!(
        selected["version"], "1.6.0",
        "the applied requirement must still exclude 1.7 and the next major",
    );
    fixture.check(&[]);
}

#[test]
fn explicit_target_keeps_inapplicable_target_dependencies_inactive() {
    let fixture = Fixture::new("target-selection");
    for (version, source) in [("1.0.0", OLD_API), ("1.2.0", NEW_API), ("1.8.0", NEW_API)] {
        fixture.publish(Release::new("target_api", version, source));
    }
    fixture.publish(Release::new("never_active", "1.8.0", OLD_API));
    let target = fixture.host_target();
    fixture.project(
        "sample",
        &format!(
            "[target.'{target}'.dependencies]\ntarget_api = \"1.8\"\n\
             [target.'cfg(any())'.dependencies]\nnever_active = \"1.8\"\n"
        ),
        "pub fn value() -> u32 { target_api::needed() }\n",
    );
    fixture.check(&["--target", &target]);
    let report = fixture.floor(&["--target", &target]);
    verified(&report, "target_api", "target_api", "1.8", "1.2.0");
    assert_inactive(&report, "never_active");
}

#[test]
fn missing_requested_dependency_features_are_resolution_failures_not_api_successes() {
    let fixture = Fixture::new("missing-feature");
    fixture.publish(Release::new("gated", "1.0.0", NEW_API));
    for version in ["1.3.0", "1.8.0"] {
        fixture.publish(Release::new("gated", version, NEW_API).feature("gate", &[]));
    }
    fixture.project(
        "sample",
        "[dependencies]\ngated = { version = \"1.8\", features = [\"gate\"] }\n",
        "pub fn value() -> u32 { gated::needed() }\n",
    );
    fixture.check(&[]);
    let report = fixture.floor(&[]);
    let gated = verified(&report, "gated", "gated", "1.8", "1.3.0");
    let rejected = probe(gated, "1.0.0", "resolution_failed");
    assert!(
        rejected["diagnostic"].as_str().unwrap().contains("gate"),
        "{rejected:#}"
    );
}

#[test]
fn zero_major_dependencies_keep_their_semver_compatibility_boundaries() {
    let fixture = Fixture::new("zero-major");
    for (version, source) in [
        ("0.1.9", NEW_API),
        ("0.2.0", OLD_API),
        ("0.2.2", NEW_API),
        ("0.2.8", NEW_API),
        ("0.3.0", NEW_API),
    ] {
        fixture.publish(Release::new("api", version, source));
    }
    for version in ["0.0.1", "0.0.2"] {
        fixture.publish(Release::new("tiny", version, "pub fn value() -> u32 { 0 }"));
    }
    fixture.project(
        "sample",
        "[dependencies]\napi = \"0.2.8\"\ntiny = \"0.0.2\"\n",
        "pub fn value() -> u32 { api::needed() + tiny::value() }\n",
    );
    fixture.check(&[]);
    let report = fixture.floor(&[]);
    let api = verified(&report, "api", "api", "0.2.8", "0.2.2");
    for rejected in ["0.1.9", "0.3.0"] {
        assert!(
            api["probes"]
                .as_array()
                .unwrap()
                .iter()
                .all(|probe| probe["version"] != rejected)
        );
    }
    let tiny = verified(&report, "tiny", "tiny", "0.0.2", "0.0.2");
    assert!(
        tiny["probes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|probe| probe["version"] != "0.0.1")
    );
}

#[test]
fn configured_rustc_executable_names_are_resolved_on_path() {
    let fixture = api_fixture("configured-rustc", "1.8");
    fixture.write(".cargo/config.toml", "[build]\nrustc = \"rustc\"\n");
    fixture.check(&[]);
    verified(&fixture.floor(&[]), "api", "api", "1.8", "1.2.0");
}

#[test]
fn apply_preserves_a_manifest_without_a_final_newline() {
    let fixture = api_fixture("no-final-newline", "1.8");
    let manifest = String::from_utf8(fixture.read("Cargo.toml")).unwrap();
    let manifest = manifest.trim_end_matches('\n');
    fixture.write("Cargo.toml", manifest);
    fixture.check(&[]);
    let report = fixture.floor(&["--apply"]);
    let api = verified(&report, "api", "api", "1.8", "1.2.0");
    let expected = manifest.replacen(
        "\"1.8\"",
        &quoted(api["proposed_requirement"].as_str().unwrap()),
        1,
    );
    assert_eq!(fixture.read("Cargo.toml"), expected.as_bytes());
}

#[test]
fn explicit_target_overrides_a_configured_target_matrix() {
    let fixture = api_fixture("configured-targets", "1.8");
    let target = fixture.host_target();
    fixture.write(
        ".cargo/config.toml",
        &format!(
            "[build]\ntarget = [{}, \"wasm32-unknown-unknown\"]\n",
            quoted(&target)
        ),
    );
    fixture.check(&["--target", &target]);
    verified(
        &fixture.floor(&["--target", &target]),
        "api",
        "api",
        "1.8",
        "1.2.0",
    );
}

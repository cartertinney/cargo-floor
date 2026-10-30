//! Source/configuration fidelity and dependency-edge identity regressions.

mod support;

use serde_json::Value;
use support::{
    Fixture, Release, assert_failure, assert_inactive, assert_resolved, assert_success, dependency,
    dependency_kind, parse_report, probe, quoted, verified,
};

const OLD: &str = "pub fn old() -> u32 { 1 }\n";
const NEW: &str = "pub fn old() -> u32 { 1 }\npub fn needed() -> u32 { 2 }\n";

fn publish_api(fixture: &Fixture, name: &str) {
    for (version, source) in [("1.0.0", OLD), ("1.2.0", NEW), ("1.8.0", NEW)] {
        fixture.publish(Release::new(name, version, source));
    }
}

#[test]
fn local_feature_named_like_a_removed_dev_dependency_keeps_its_original_meaning() {
    let fixture = Fixture::new("dev-name-local-feature");
    publish_api(&fixture, "api");
    fixture.publish(Release::new("modern", "1.0.0", ""));
    fixture.project(
        "sample",
        r#"[dependencies]
api = "1.8"
[dev-dependencies]
modern = "1"
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
    let original = fixture.snapshot(&["Cargo.toml", "Cargo.lock", "src/lib.rs"]);
    let report = fixture.floor(&[]);
    fixture.assert_unchanged(&original);
    // JSON alone is not evidence: compile the proposal with the ORIGINAL defaults.
    fixture.assert_floor_witness(&report, dependency(&report, "api"), &[]);
    verified(&report, "api", "api", "1.8", "1.2.0");
    let without_defaults = fixture.floor(&["--no-default-features"]);
    let api = verified(&without_defaults, "api", "api", "1.8", "1.0.0");
    fixture.assert_floor_witness(&without_defaults, api, &["--no-default-features"]);
}

#[test]
fn target_normal_dependency_sharing_a_dev_name_retains_activation_and_weak_forwarding() {
    let fixture = Fixture::new("target-dev-forwarding");
    for (version, source) in [("1.0.0", OLD), ("1.2.0", NEW), ("1.8.0", NEW)] {
        fixture.publish(
            Release::new(
                "api",
                version,
                &format!(
                    "#[cfg(feature = \"modern\")]\nmod enabled {{ {source} }}\n\
                          #[cfg(feature = \"modern\")]\npub use enabled::*;\n"
                ),
            )
            .feature("modern", &[]),
        );
    }
    let target = fixture.host_target();
    fixture.project(
        "sample",
        &format!(
            "[target.'{target}'.dependencies]\n\
             api = {{ version = \"1.8\", optional = true, default-features = false }}\n\
             [dev-dependencies]\napi = \"1\"\n\
             [features]\ndefault = [\"activate\"]\n\
             activate = [\"dep:api\", \"api?/modern\"]\n"
        ),
        r#"#[cfg(feature = "activate")]
pub fn value() -> u32 { api::needed() }
#[cfg(not(feature = "activate"))]
pub fn value() -> u32 { 0 }
"#,
    );
    fixture.check(&[]);
    let report = fixture.floor(&[]);
    let api = verified(&report, "api", "api", "1.8", "1.2.0");
    fixture.assert_floor_witness(&report, api, &[]);
    assert_inactive(&fixture.floor(&["--no-default-features"]), "api");
}

#[test]
fn cargo_build_rustflags_can_activate_a_custom_cfg_dependency() {
    let fixture = Fixture::new("config-effective-cfg");
    publish_api(&fixture, "api");
    fixture.project(
        "sample",
        "[target.'cfg(audit_mode)'.dependencies]\napi = \"1.8\"\n",
        "pub fn value() -> u32 { api::needed() }\n",
    );
    fixture.write(
        ".cargo/config.toml",
        "[build]\nrustflags = [\"--cfg\", \"audit_mode\"]\n",
    );
    fixture.check(&[]);
    let report = fixture.floor(&[]);
    let api = verified(&report, "api", "api", "1.8", "1.2.0");
    assert_resolved(probe(api, "1.2.0", "passed"), "api", "1.2.0");
    fixture.assert_floor_witness(&report, api, &[]);
}

fn rustflags_precedence(encoded: bool) {
    let fixture = Fixture::new(if encoded { "encoded-cfg" } else { "env-cfg" });
    for name in ["config_api", "env_api", "encoded_api"] {
        publish_api(&fixture, name);
    }
    fixture.project(
        "sample",
        r#"[target.'cfg(config_mode)'.dependencies]
config_api = "1.8"
[target.'cfg(env_mode)'.dependencies]
env_api = "1.8"
[target.'cfg(encoded_mode)'.dependencies]
encoded_api = "1.8"
"#,
        r#"#[cfg(config_mode)]
pub fn config() -> u32 { config_api::needed() }
#[cfg(env_mode)]
pub fn env() -> u32 { env_api::needed() }
#[cfg(encoded_mode)]
pub fn encoded() -> u32 { encoded_api::needed() }
#[cfg(not(any(config_mode, env_mode, encoded_mode)))]
compile_error!("the selected rustflags were lost");
"#,
    );
    fixture.write(
        ".cargo/config.toml",
        "[build]\nrustflags = [\"--cfg\", \"config_mode\"]\n",
    );
    let configure = |command: &mut std::process::Command| {
        command.env("RUSTFLAGS", "--cfg env_mode");
        if encoded {
            command.env("CARGO_ENCODED_RUSTFLAGS", "--cfg\u{1f}encoded_mode");
        }
    };
    let mut baseline = fixture.cargo_command(&["check", "--offline", "--lib"]);
    configure(&mut baseline);
    assert_success(&baseline.output().unwrap());
    let mut command = fixture.floor_command(&["floor", "--format", "json"]);
    configure(&mut command);
    let report = parse_report(&command.output().unwrap());
    let selected = if encoded { "encoded_api" } else { "env_api" };
    let active = verified(&report, selected, selected, "1.8", "1.2.0");
    fixture.assert_floor_witness_with(&report, active, &[], configure);
    for name in ["config_api", "env_api", "encoded_api"] {
        if name != selected {
            assert_inactive(&report, name);
        }
    }
}

#[test]
fn scoped_rustflags_override_cargo_config_without_leaking_to_other_tests() {
    rustflags_precedence(false);
}

#[test]
fn encoded_rustflags_override_both_rustflags_and_cargo_config() {
    rustflags_precedence(true);
}

#[test]
fn target_specific_rustflags_take_precedence_over_build_rustflags() {
    let fixture = Fixture::new("target-config-cfg");
    publish_api(&fixture, "api");
    let target = fixture.host_target();
    fixture.project(
        "sample",
        "[target.'cfg(audit_mode)'.dependencies]\napi = \"1.8\"\n",
        "#[cfg(ignored_mode)]\ncompile_error!(\"build flags should be overridden\");\n\
         pub fn value() -> u32 { api::needed() }\n",
    );
    fixture.write(
        ".cargo/config.toml",
        &format!(
            "[build]\nrustflags = [\"--cfg\", \"ignored_mode\"]\n\
             [target.{target}]\nrustflags = [\"--cfg\", \"audit_mode\"]\n"
        ),
    );
    fixture.check(&["--target", &target]);
    let report = fixture.floor(&["--target", &target]);
    let api = verified(&report, "api", "api", "1.8", "1.2.0");
    fixture.assert_floor_witness(&report, api, &["--target", &target]);
}

#[test]
fn implicit_import_name_comes_from_the_dependency_library_target_not_package_name() {
    let fixture = Fixture::new("implicit-library-name");
    for (version, source) in [("1.0.0", OLD), ("1.2.0", NEW), ("1.8.0", NEW)] {
        fixture.publish(
            Release::new("package-api", version, source).manifest("[lib]\nname = \"actual_api\"\n"),
        );
    }
    fixture.project(
        "sample",
        "[dependencies]\npackage-api = \"1.8\"\n",
        "pub fn value() -> u32 { actual_api::needed() }\n",
    );
    fixture.check(&[]);
    let report = fixture.floor(&[]);
    let api = verified(&report, "package-api", "package-api", "1.8", "1.2.0");
    fixture.assert_floor_witness(&report, api, &[]);
}

#[test]
fn explicit_aliases_of_one_package_keep_distinct_compatibility_lines_and_witnesses() {
    let fixture = Fixture::new("multiple-aliased-versions");
    for (version, source) in [
        ("0.4.0", OLD),
        ("0.4.8", OLD),
        ("1.0.0", OLD),
        ("1.2.0", NEW),
        ("1.8.0", NEW),
    ] {
        fixture.publish(
            Release::new("package-api", version, source).manifest("[lib]\nname = \"actual_api\"\n"),
        );
    }
    fixture.project(
        "sample",
        r#"[dependencies]
legacy = { package = "package-api", version = "0.4.8" }
current = { package = "package-api", version = "1.8" }
"#,
        "pub fn value() -> u32 { legacy::old() + current::needed() }\n",
    );
    fixture.check(&[]);
    let untouched = fixture.snapshot(&["Cargo.lock", "src/lib.rs"]);
    let report = fixture.floor(&["--apply"]);
    let legacy = verified(&report, "legacy", "package-api", "0.4.8", "0.4.0");
    let current = verified(&report, "current", "package-api", "1.8", "1.2.0");
    assert_resolved(probe(legacy, "0.4.0", "passed"), "package-api", "1.8.0");
    assert_resolved(probe(current, "1.2.0", "passed"), "package-api", "0.4.8");
    let manifest = String::from_utf8(fixture.read("Cargo.toml")).unwrap();
    assert_eq!(manifest.matches("package = \"package-api\"").count(), 2);
    fixture.assert_unchanged(&untouched);
    fixture.check(&[]);
}

#[test]
fn normal_artifacts_do_not_verify_a_same_named_build_edge_without_a_build_script() {
    let fixture = Fixture::new("unused-build-edge");
    publish_api(&fixture, "api");
    fixture.publish(Release::new(
        "unused_builder",
        "1.8.0",
        "compile_error!(\"there is no build script to compile this dependency\");",
    ));
    fixture.project(
        "sample",
        "[dependencies]\napi = \"1.8\"\n\
         [build-dependencies]\napi = \"1.8\"\nunused_builder = \"1.8\"\n",
        "pub fn value() -> u32 { api::needed() }\n",
    );
    fixture.check(&[]);
    let report = fixture.floor(&["--apply"]);
    let normal = dependency_kind(&report, "api", "normal");
    assert_eq!(normal["status"], "verified", "{normal:#}");
    assert_eq!(normal["lowest_verified"], "1.2.0", "{normal:#}");
    let build = dependency_kind(&report, "api", "build");
    assert_eq!(build["status"], "inactive", "{build:#}");
    assert_eq!(build["proposed_requirement"], "1.8", "{build:#}");
    assert!(build["probes"].as_array().unwrap().is_empty(), "{build:#}");
    assert_inactive(&report, "unused_builder");
    let applied = String::from_utf8(fixture.read("Cargo.toml")).unwrap();
    assert!(
        applied.contains("[build-dependencies]\napi = \"1.8\"\n"),
        "{applied}"
    );
    fixture.check(&[]);
}

#[test]
fn explicit_target_checks_build_scripts_on_host_and_libraries_with_target_flags() {
    let fixture = Fixture::new("host-and-target-build");
    publish_api(&fixture, "api");
    publish_api(&fixture, "builder");
    let host = fixture.host_target();
    fixture.project(
        "sample",
        &format!(
            "[target.'{host}'.dependencies]\napi = \"1.8\"\n\
             [target.'{host}'.build-dependencies]\nbuilder = \"1.8\"\n"
        ),
        "#[cfg(not(library_target))]\ncompile_error!(\"target flags were lost\");\n\
         include!(concat!(env!(\"OUT_DIR\"), \"/generated.rs\"));\n\
         pub fn value() -> u32 { api::needed() + GENERATED }\n",
    );
    fixture.write(
        ".cargo/config.toml",
        &format!("[target.{host}]\nrustflags = [\"--cfg\", \"library_target\"]\n"),
    );
    fixture.write(
        "build.rs",
        r#"#[cfg(library_target)]
compile_error!("target rustflags must not be used to compile host build scripts");
fn main() {
    assert_eq!(std::env::var("HOST").unwrap(), std::env::var("TARGET").unwrap());
    let output = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    std::fs::write(
        output.join("generated.rs"),
        format!("const GENERATED: u32 = {};", builder::needed()),
    ).unwrap();
}
"#,
    );
    fixture.check(&["--target", &host]);
    let report = fixture.floor(&["--target", &host]);
    verified(&report, "api", "api", "1.8", "1.2.0");
    let builder = verified(&report, "builder", "builder", "1.8", "1.2.0");
    assert_eq!(builder["kind"], "build");
    probe(builder, "1.0.0", "build_failed");
    fixture.assert_floor_witness(&report, builder, &["--target", &host]);
}

#[test]
fn real_proc_macro_dependency_and_nonstandard_library_path_are_compiled() {
    let fixture = Fixture::new("proc-macro-api");
    for (version, macro_name) in [("1.0.0", "old"), ("1.2.0", "needed"), ("1.8.0", "needed")] {
        fixture.publish(
            Release::new(
                "macro-package",
                version,
                &format!(
                    "extern crate proc_macro;\n\
                     #[proc_macro]\npub fn {macro_name}(_: proc_macro::TokenStream) \
                     -> proc_macro::TokenStream {{ \"pub fn value() -> u32 {{ 2 }}\".parse().unwrap() }}\n"
                ),
            )
            .manifest("[lib]\nproc-macro = true\nname = \"macro_library\"\n"),
        );
    }
    fixture.project(
        "sample",
        "[lib]\nname = \"custom_root\"\npath = \"library/root.rs\"\n\
         [dependencies]\nmacros = { package = \"macro-package\", version = \"1.8\" }\n",
        "compile_error!(\"Cargo must honor the configured lib path\");",
    );
    fixture.write("library/root.rs", "macros::needed!();\n");
    fixture.check(&[]);
    let report = fixture.floor(&[]);
    let macros = verified(&report, "macros", "macro-package", "1.8", "1.2.0");
    fixture.assert_floor_witness(&report, macros, &[]);
}

#[test]
fn proc_macro_root_package_uses_real_trait_type_and_macro_apis() {
    let fixture = Fixture::new("proc-macro-root");
    let old = "pub struct Token;\npub trait Render { fn old(&self) -> &'static str; }\n";
    let new = "pub struct Token;\npub trait Render { fn render(&self) -> &'static str; }\n\
               impl Render for Token { fn render(&self) -> &'static str { \"fn generated() {}\" } }\n\
               #[macro_export]\nmacro_rules! token { () => { $crate::Token } }\n";
    for (version, source) in [("1.0.0", old), ("1.2.0", new), ("1.8.0", new)] {
        fixture.publish(Release::new("typed_api", version, source));
    }
    fixture.project(
        "sample",
        "[lib]\nproc-macro = true\n[dependencies]\ntyped_api = \"1.8\"\n",
        "extern crate proc_macro;\nuse typed_api::Render;\n\
         #[proc_macro]\npub fn generate(_: proc_macro::TokenStream) -> proc_macro::TokenStream {\n\
             typed_api::token!().render().parse().unwrap()\n}\n",
    );
    fixture.check(&[]);
    let report = fixture.floor(&[]);
    let api = verified(&report, "typed_api", "typed_api", "1.8", "1.2.0");
    fixture.assert_floor_witness(&report, api, &[]);
}

#[test]
fn ffi_library_crate_forms_do_not_inherit_binary_example_or_benchmark_api_floors() {
    let fixture = Fixture::new("ffi-library-only");
    publish_api(&fixture, "api");
    fixture.project(
        "sample",
        "[lib]\ncrate-type = [\"cdylib\", \"staticlib\"]\n\
         [dependencies]\napi = \"1.8\"\n",
        "#[no_mangle]\npub extern \"C\" fn value() -> u32 { api::old() }\n",
    );
    for path in ["src/main.rs", "examples/modern.rs", "benches/modern.rs"] {
        fixture.write(path, "fn main() { assert_eq!(api::needed(), 2); }\n");
    }
    assert_success(&fixture.cargo(&["check", "--offline", "--all-targets"]));
    let untouched = fixture.snapshot(&[
        "Cargo.lock",
        "src/lib.rs",
        "src/main.rs",
        "examples/modern.rs",
        "benches/modern.rs",
    ]);
    let report = fixture.floor(&["--apply"]);
    let api = verified(&report, "api", "api", "1.8", "1.0.0");
    fixture.assert_unchanged(&untouched);
    fixture.assert_floor_witness(&report, api, &[]);
    assert_success(&fixture.cargo(&["update", "--offline", "-p", "api", "--precise", "1.0.0"]));
    fixture.check(&["--locked"]);
    assert_failure(
        &fixture.cargo(&["check", "--offline", "--all-targets"]),
        "non-library targets deliberately need newer APIs and must not establish the library floor",
    );
}

#[test]
fn transitive_dependencies_resolve_normally_not_to_artificial_minimal_versions() {
    let fixture = Fixture::new("normal-transitive-resolution");
    fixture.publish(Release::new("transitive", "1.0.0", OLD));
    fixture.publish(Release::new(
        "transitive",
        "1.5.0",
        "pub fn replacement() -> u32 { 5 }",
    ));
    fixture.publish(
        Release::new(
            "api",
            "1.0.0",
            "pub fn value() -> u32 { transitive::old() }",
        )
        .dependency("transitive", "1"),
    );
    for version in ["1.2.0", "1.8.0"] {
        fixture.publish(
            Release::new(
                "api",
                version,
                "pub fn value() -> u32 { transitive::replacement() }",
            )
            .dependency("transitive", "1"),
        );
    }
    fixture.project(
        "sample",
        &format!("[dependencies]\napi = {}\n", quoted("1.8")),
        "pub fn value() -> u32 { api::value() }\n",
    );
    fixture.check(&[]);
    let report: Value = fixture.floor(&[]);
    let api = verified(&report, "api", "api", "1.8", "1.2.0");
    probe(api, "1.0.0", "build_failed");
    assert_resolved(probe(api, "1.2.0", "passed"), "transitive", "1.5.0");
    fixture.assert_floor_witness(&report, api, &[]);
}

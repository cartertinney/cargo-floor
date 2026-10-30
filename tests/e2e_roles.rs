//! A compiled package ID must not prove an inactive host or target dependency edge.

mod support;

use semver::VersionReq;
use support::{Fixture, Release, assert_resolved, dependency_kind, probe};

fn role_fixture(name: &str, normal_active: bool) -> (Fixture, String) {
    let fixture = Fixture::new(name);
    let wrong_role = if normal_active {
        "not(floor_role_mode)"
    } else {
        "floor_role_mode"
    };
    for (version, api) in [
        ("1.0.0", "pub fn old() -> u32 { 1 }"),
        (
            "1.2.0",
            "pub fn old() -> u32 { 1 }\npub fn needed() -> u32 { 2 }",
        ),
        (
            "1.8.0",
            "pub fn old() -> u32 { 1 }\npub fn needed() -> u32 { 2 }",
        ),
    ] {
        fixture.publish(Release::new(
            "api",
            version,
            &format!(
                "#[cfg({wrong_role})]\n\
                 compile_error!(\"api must not compile in the inactive dependency role\");\n{api}\n"
            ),
        ));
    }
    let host = fixture.host_target();
    fixture.write(
        ".cargo/config.toml",
        &format!("[target.{host}]\nrustflags = [\"--cfg\", \"floor_role_mode\"]\n"),
    );
    (fixture, host)
}

fn assert_roles(fixture: &Fixture, host: &str, active_kind: &str, inactive_kind: &str) {
    fixture.check(&["--target", host]);
    let snapshot = fixture.snapshot(&[
        "Cargo.toml",
        "Cargo.lock",
        "src/lib.rs",
        "build.rs",
        ".cargo/config.toml",
    ]);
    let report = fixture.floor(&["--target", host]);
    fixture.assert_unchanged(&snapshot);
    let active = dependency_kind(&report, "api", active_kind);
    assert_eq!(active["package"], "api", "{active:#}");
    assert_eq!(active["original_requirement"], "1.8", "{active:#}");
    assert_eq!(active["status"], "verified", "{active:#}");
    assert_eq!(active["lowest_verified"], "1.2.0", "{active:#}");
    probe(active, "1.0.0", "build_failed");
    assert_resolved(probe(active, "1.2.0", "passed"), "api", "1.2.0");
    let requirement = VersionReq::parse(active["proposed_requirement"].as_str().unwrap()).unwrap();
    assert!(requirement.matches(&"1.2.0".parse().unwrap()), "{active:#}");
    assert!(
        !requirement.matches(&"1.0.0".parse().unwrap()),
        "{active:#}"
    );
    fixture.assert_floor_witness(&report, active, &["--target", host]);

    let inactive = dependency_kind(&report, "api", inactive_kind);
    assert_eq!(inactive["status"], "inactive", "{inactive:#}");
    assert_eq!(inactive["original_requirement"], "1", "{inactive:#}");
    assert_eq!(inactive["proposed_requirement"], "1", "{inactive:#}");
    assert!(inactive["lowest_verified"].is_null(), "{inactive:#}");
    assert!(
        inactive["probes"].as_array().unwrap().is_empty(),
        "a package compiled in the other role is not a witness for this edge: {inactive:#}",
    );
}

#[test]
fn target_cfg_normal_artifact_does_not_verify_a_cfg_disabled_host_build_edge() {
    let (fixture, host) = role_fixture("normal-artifact-build-edge", true);
    fixture.project(
        "sample",
        "[target.'cfg(floor_role_mode)'.dependencies]\napi = \"1.8\"\n\
         [target.'cfg(floor_role_mode)'.build-dependencies]\napi = \"1\"\n",
        "#[cfg(not(floor_role_mode))]\n\
         compile_error!(\"the library must receive target rustflags\");\n\
         pub fn value() -> u32 { api::needed() }\n",
    );
    fixture.write(
        "build.rs",
        "#[cfg(floor_role_mode)]\n\
         compile_error!(\"host build scripts must not receive target rustflags\");\n\
         fn main() {\n\
             assert_eq!(std::env::var(\"HOST\").unwrap(), std::env::var(\"TARGET\").unwrap());\n\
         }\n",
    );
    assert_roles(&fixture, &host, "normal", "build");
}

#[test]
fn host_build_artifact_does_not_verify_a_cfg_disabled_target_normal_edge() {
    let (fixture, host) = role_fixture("build-artifact-normal-edge", false);
    fixture.project(
        "sample",
        "[target.'cfg(not(floor_role_mode))'.dependencies]\napi = \"1\"\n\
         [build-dependencies]\napi = \"1.8\"\n",
        "#[cfg(not(floor_role_mode))]\n\
         compile_error!(\"the library must receive target rustflags\");\n\
         include!(concat!(env!(\"OUT_DIR\"), \"/value.rs\"));\n\
         pub fn value() -> u32 { GENERATED }\n",
    );
    fixture.write(
        "build.rs",
        r#"#[cfg(floor_role_mode)]
compile_error!("host build scripts must not receive target rustflags");
fn main() {
    assert_eq!(std::env::var("HOST").unwrap(), std::env::var("TARGET").unwrap());
    let output = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    std::fs::write(
        output.join("value.rs"),
        format!("const GENERATED: u32 = {};", api::needed()),
    ).unwrap();
}
"#,
    );
    assert_roles(&fixture, &host, "build", "normal");
}

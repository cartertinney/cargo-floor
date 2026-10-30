//! Real Cargo resolution through local and loopback-only sparse registries.

mod support;

use support::{
    Fixture, IndexFault, Release, assert_failure, assert_resolved, dependency, probe, verified,
};

const OLD: &str = "pub fn old() -> u32 { 1 }\n";
const NEW: &str = "pub fn old() -> u32 { 1 }\npub fn needed() -> u32 { 2 }\n";

#[test]
fn loopback_sparse_registry_enumerates_real_archives_and_all_crate_index_path_shapes() {
    let mut fixture = Fixture::new("loopback-sparse-success");
    let mut dependencies = String::from("[dependencies]\n");
    let mut calls = Vec::new();
    for name in ["a", "ab", "abc", "long_api"] {
        // Publication order must not become probe order.
        for (version, source) in [("1.8.0", NEW), ("1.0.0", OLD), ("1.2.0", NEW)] {
            fixture.publish(Release::new(name, version, source));
        }
        fixture.publish(Release::new(name, "1.1.0", NEW).yanked());
        fixture.publish(Release::new(name, "1.1.0-beta.1", NEW));
        dependencies.push_str(&format!("{name} = \"1.8\"\n"));
        calls.push(format!("{name}::needed()"));
    }
    fixture.project(
        "sample",
        &dependencies,
        &format!("pub fn value() -> u32 {{ {} }}\n", calls.join(" + ")),
    );
    let server = fixture.sparse_registry();
    fixture.check(&[]);
    let untouched = fixture.snapshot(&["Cargo.lock", "src/lib.rs"]);
    let report = fixture.floor(&["--apply"]);
    assert_eq!(report["status"], "verified", "{report:#}");
    for name in ["a", "ab", "abc", "long_api"] {
        let entry = verified(&report, name, name, "1.8", "1.2.0");
        probe(entry, "1.0.0", "build_failed");
        assert_resolved(probe(entry, "1.2.0", "passed"), name, "1.2.0");
        assert!(
            entry["probes"]
                .as_array()
                .unwrap()
                .iter()
                .all(|probe| probe["version"] != "1.1.0" && probe["version"] != "1.1.0-beta.1"),
            "{entry:#}",
        );
    }
    fixture.assert_unchanged(&untouched);
    fixture.assert_floor_witness(&report, dependency(&report, "a"), &[]);
    fixture.check(&[]);
    let requests = server.requests();
    for path in [
        "/index/1/a",
        "/index/2/ab",
        "/index/3/a/abc",
        "/index/lo/ng/long_api",
    ] {
        assert_eq!(
            server
                .enumerations()
                .iter()
                .filter(|requested| *requested == path)
                .count(),
            1,
            "release enumeration should use the real sparse path once: {requests:#?}",
        );
    }
    assert!(
        requests
            .iter()
            .any(|(path, _)| path == "/index/config.json"),
        "{requests:#?}"
    );
    assert!(
        requests
            .iter()
            .any(|(path, _)| path == "/crates/a/1.2.0/download"),
        "Cargo must download and compile the actual minimum archive: {requests:#?}",
    );
}

fn sparse_failure(fault: IndexFault) {
    let mut fixture = Fixture::new("loopback-sparse-error");
    for (version, source) in [("1.0.0", OLD), ("1.2.0", NEW), ("1.8.0", NEW)] {
        fixture.publish(Release::new("api", version, source));
    }
    fixture.project(
        "sample",
        "[dependencies]\napi = \"1.8\"\n",
        "pub fn value() -> u32 { api::needed() }\n",
    );
    let server = fixture.sparse_registry();
    fixture.check(&[]);
    server.fault_enumeration("api", fault);
    let snapshot = fixture.snapshot(&["Cargo.toml", "Cargo.lock", "src/lib.rs"]);
    let output = fixture.floor_raw(&["floor", "--format", "json", "--apply"]);
    assert_failure(
        &output,
        "registry enumeration errors must abort without applying or reporting success",
    );
    let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
    assert!(
        stderr.contains("api") && (stderr.contains("index") || stderr.contains("registry")),
        "identify the dependency and failing registry operation: {stderr}",
    );
    match fault {
        IndexFault::Malformed => assert!(
            stderr.contains("parse") || stderr.contains("json") || stderr.contains("line"),
            "{stderr}",
        ),
        IndexFault::Unavailable => assert!(
            stderr.contains("503") || stderr.contains("fetch") || stderr.contains("unavailable"),
            "{stderr}",
        ),
    }
    assert!(
        server
            .enumerations()
            .iter()
            .any(|path| path == "/index/3/a/api"),
        "the valid real-Cargo baseline must reach discovery's injected registry fault",
    );
    fixture.assert_unchanged(&snapshot);
}

#[test]
fn malformed_loopback_sparse_index_is_an_actionable_nonmutating_error() {
    sparse_failure(IndexFault::Malformed);
}

#[test]
fn unavailable_loopback_sparse_index_is_an_actionable_nonmutating_error() {
    sparse_failure(IndexFault::Unavailable);
}

#[test]
fn cached_sparse_baseline_does_not_make_offline_enumeration_look_verified() {
    let mut fixture = Fixture::new("loopback-sparse-offline");
    fixture.publish(Release::new("api", "1.8.0", NEW));
    fixture.project(
        "sample",
        "[dependencies]\napi = \"1.8\"\n",
        "pub fn value() -> u32 { api::needed() }\n",
    );
    let server = fixture.sparse_registry();
    fixture.check(&[]);
    let snapshot = fixture.snapshot(&["Cargo.toml", "Cargo.lock", "src/lib.rs"]);
    let output = fixture
        .floor_command(&["floor", "--format", "json", "--apply"])
        .env("CARGO_NET_OFFLINE", "true")
        .output()
        .unwrap();
    assert_failure(
        &output,
        "offline sparse enumeration is explicitly unsupported even with a cached baseline",
    );
    let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
    assert!(
        stderr.contains("offline") && stderr.contains("local"),
        "{stderr}"
    );
    assert!(
        server.enumerations().is_empty(),
        "offline enumeration must fail without an HTTP discovery request",
    );
    fixture.assert_unchanged(&snapshot);
}

#[test]
fn named_sparse_and_replaced_default_registries_keep_same_package_versions_distinct() {
    let alternate = Fixture::new("alternate-sparse-source");
    for (version, source) in [("1.0.0", OLD), ("1.3.0", NEW), ("1.8.0", NEW)] {
        alternate.publish(Release::new("api", version, source));
    }
    let server = support::SparseRegistry::new(alternate.registry_path().to_owned());
    let mut fixture = Fixture::new("multiple-registry-sources");
    for (version, source) in [("1.0.0", OLD), ("1.2.0", NEW), ("1.8.0", NEW)] {
        fixture.publish(Release::new("api", version, source));
    }
    fixture.named_sparse_registry("alternate", &server);
    fixture.project(
        "sample",
        r#"[dependencies]
local = { package = "api", version = "1.8" }
remote = { package = "api", version = "1.8", registry = "alternate" }
"#,
        "pub fn value() -> u32 { local::needed() + remote::needed() }\n",
    );
    fixture.check(&[]);
    let untouched = fixture.snapshot(&["Cargo.lock", "src/lib.rs", ".cargo/config.toml"]);
    let report = fixture.floor(&["--apply"]);
    assert_eq!(
        report["baseline"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|package| { package["name"] == "api" && package["version"] == "1.8.0" })
            .count(),
        2,
        "real Cargo must compile api 1.8 from both distinct sources: {report:#}",
    );
    let local = verified(&report, "local", "api", "1.8", "1.2.0");
    let remote = verified(&report, "remote", "api", "1.8", "1.3.0");
    for (entry, version, expected_source) in [
        (
            local,
            "1.2.0",
            "registry+https://github.com/rust-lang/crates.io-index".to_owned(),
        ),
        (remote, "1.3.0", server.index_url()),
    ] {
        let witness = probe(entry, version, "passed");
        assert!(
            witness["resolved"]
                .as_array()
                .unwrap()
                .iter()
                .any(|package| {
                    package["name"] == "api"
                        && package["version"] == version
                        && package["source"].as_str().is_some_and(|source| {
                            source
                                .trim_start_matches("registry+")
                                .trim_start_matches("sparse+")
                                .trim_end_matches('/')
                                == expected_source
                                    .trim_start_matches("registry+")
                                    .trim_start_matches("sparse+")
                                    .trim_end_matches('/')
                        })
                }),
            "witness must come from the dependency's actual registry: {witness:#}",
        );
    }
    let applied = String::from_utf8(fixture.read("Cargo.toml")).unwrap();
    assert!(applied.contains("registry = \"alternate\""), "{applied}");
    assert_eq!(applied.matches("package = \"api\"").count(), 2);
    fixture.assert_unchanged(&untouched);
    fixture.check(&[]);
}

#[test]
fn a_corrupt_old_candidate_archive_is_rejected_before_a_real_later_witness() {
    let fixture = Fixture::new("corrupt-candidate-archive");
    for (version, source) in [("1.0.0", NEW), ("1.2.0", NEW), ("1.8.0", NEW)] {
        fixture.publish(Release::new("api", version, source));
    }
    fixture.project(
        "sample",
        "[dependencies]\napi = \"1.8\"\n",
        "pub fn value() -> u32 { api::needed() }\n",
    );
    fixture.check(&[]);
    std::fs::write(
        fixture.registry_path().join("api-1.0.0.crate"),
        b"not the indexed archive",
    )
    .unwrap();
    let snapshot = fixture.snapshot(&["Cargo.toml", "Cargo.lock", "src/lib.rs"]);
    let report = fixture.floor(&[]);
    let api = verified(&report, "api", "api", "1.8", "1.2.0");
    let failed = probe(api, "1.0.0", "resolution_failed");
    assert!(
        !failed["diagnostic"].as_str().unwrap().trim().is_empty(),
        "{failed:#}"
    );
    assert_resolved(probe(api, "1.2.0", "passed"), "api", "1.2.0");
    fixture.assert_unchanged(&snapshot);
    fixture.assert_floor_witness(&report, api, &[]);
}

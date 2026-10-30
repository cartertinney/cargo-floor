mod support;

use serde_json::Value;
use std::collections::BTreeSet;
use support::{Fixture, Release, assert_failure, assert_success, parse_report, verified};

const OLD: &str = "pub fn old() -> u32 { 1 }";
const NEW: &str = "pub fn needed() -> u32 { 2 }";
const BROKEN: &str = "compile_error!(\"historical dependency regression\");";

fn history(name: &str, sources: &[&str]) -> Fixture {
    let fixture = Fixture::new(name);
    for (minor, source) in sources.iter().enumerate() {
        fixture.publish(Release::new("api", &format!("1.{minor}.0"), source));
    }
    fixture.project(
        "sample",
        &format!("[dependencies]\napi = \"1.{}\"\n", sources.len() - 1),
        "pub fn value() -> u32 { api::needed() }\n",
    );
    fixture
}

fn assert_no_repeated_candidates(entry: &Value) {
    let probes = entry["probes"].as_array().unwrap();
    let versions: BTreeSet<_> = probes
        .iter()
        .map(|probe| probe["version"].as_str().unwrap())
        .collect();
    assert_eq!(versions.len(), probes.len(), "{entry:#}");
}

#[test]
fn default_binary_search_finds_a_late_floor_with_logarithmic_probes() {
    let mut sources = vec![OLD; 256];
    sources[173..].fill(NEW);
    let fixture = history("binary-large-history", &sources);
    fixture.check(&[]);
    let snapshot = fixture.snapshot(&["Cargo.toml", "Cargo.lock", "src/lib.rs"]);
    let report = fixture.floor(&[]);
    assert_eq!(report["configuration"]["search_strategy"], "binary");
    let api = verified(&report, "api", "api", "1.255", "1.173.0");
    assert!(
        api["probes"].as_array().unwrap().len() <= 12,
        "binary search must not build hundreds of releases: {api:#}"
    );
    assert_eq!(api["earlier_candidates_checked"], false, "{api:#}");
    assert_no_repeated_candidates(api);
    fixture.assert_unchanged(&snapshot);
    fixture.assert_floor_witness(&report, api, &[]);
}

#[test]
fn exhaustive_flag_scans_every_earlier_release_oldest_first() {
    let mut sources = vec![OLD; 16];
    sources[12..].fill(NEW);
    let fixture = history("exhaustive-search", &sources);
    let report = fixture.floor(&["--exhaustive"]);
    assert_eq!(report["configuration"]["search_strategy"], "exhaustive");
    let api = verified(&report, "api", "api", "1.15", "1.12.0");
    assert_eq!(api["earlier_candidates_checked"], true, "{api:#}");
    let versions: Vec<_> = api["probes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|probe| probe["version"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        versions,
        (0..=12)
            .map(|minor| format!("1.{minor}.0"))
            .collect::<Vec<_>>()
    );
    fixture.assert_floor_witness(&report, api, &[]);
}

#[test]
fn oldest_passing_candidate_finishes_after_one_probe_in_either_mode() {
    for arguments in [vec![], vec!["--exhaustive"]] {
        let fixture = history("oldest-search-candidate", &[NEW; 16]);
        let report = fixture.floor(&arguments);
        let api = verified(&report, "api", "api", "1.15", "1.0.0");
        assert_eq!(api["probes"].as_array().unwrap().len(), 1);
        assert_eq!(api["earlier_candidates_checked"], true, "{api:#}");
    }
}

#[test]
fn binary_search_handles_single_and_adjacent_candidates_without_duplicates() {
    for sources in [&[NEW][..], &[OLD, NEW][..]] {
        let fixture = history("small-search-history", sources);
        let report = fixture.floor(&[]);
        let expected = format!("1.{}.0", sources.len() - 1);
        let requirement = format!("1.{}", sources.len() - 1);
        let api = verified(&report, "api", "api", &requirement, &expected);
        assert_eq!(api["probes"].as_array().unwrap().len(), sources.len());
        assert_eq!(api["earlier_candidates_checked"], true, "{api:#}");
        assert_no_repeated_candidates(api);
    }
}

#[test]
fn binary_reports_skipped_earlier_releases_in_a_nonmonotonic_history() {
    let fixture = history(
        "binary-nonmonotonic",
        &[OLD, NEW, BROKEN, BROKEN, BROKEN, BROKEN, BROKEN, NEW, NEW],
    );
    let binary = fixture.floor(&[]);
    let api = verified(&binary, "api", "api", "1.8", "1.7.0");
    assert_eq!(api["earlier_candidates_checked"], false, "{api:#}");
    fixture.assert_floor_witness(&binary, api, &[]);

    let exhaustive = fixture.floor(&["--exhaustive"]);
    let api = verified(&exhaustive, "api", "api", "1.8", "1.1.0");
    assert_eq!(api["earlier_candidates_checked"], true, "{api:#}");

    let human = fixture.floor_raw(&["floor", "--quiet"]);
    assert_success(&human);
    let stdout = String::from_utf8_lossy(&human.stdout);
    assert!(
        stdout.contains("binary") && stdout.contains("--exhaustive"),
        "{stdout}"
    );
    assert!(
        stdout.contains("assum") && stdout.contains("earlier"),
        "{stdout}"
    );
}

#[test]
fn binary_search_caveat_is_printed_once_after_all_dependency_rows() {
    let fixture = history(
        "single-search-caveat",
        &[OLD, OLD, OLD, OLD, OLD, OLD, NEW, NEW, NEW],
    );
    fixture.publish(Release::new("helper", "1.0.0", NEW));
    fixture.publish(Release::new("spare", "1.0.0", NEW));
    fixture.project(
        "sample",
        "[dependencies]\napi = \"1.8\"\nhelper = \"1\"\n\
         spare = { version = \"1\", optional = true }\n",
        "pub fn value() -> u32 { api::needed() + helper::needed() }\n",
    );
    let output = fixture.floor_raw(&["floor", "--quiet"]);
    assert_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let caveat = "Binary search assumes monotonic compatibility";
    assert_eq!(stdout.matches(caveat).count(), 1, "{stdout}");
    let caveat_position = stdout.find(caveat).unwrap();
    for row in ["api [normal]", "helper [normal]", "spare [normal]"] {
        assert!(stdout.find(row).unwrap() < caveat_position, "{stdout}");
    }

    let exhaustive = fixture.floor_raw(&["floor", "--quiet", "--exhaustive"]);
    assert_success(&exhaustive);
    let stdout = String::from_utf8_lossy(&exhaustive.stdout);
    assert!(!stdout.contains(caveat), "{stdout}");
}

#[test]
fn human_dependency_rows_are_concise_while_json_keeps_search_evidence() {
    let fixture = history(
        "concise-dependency-rows",
        &[OLD, OLD, OLD, OLD, OLD, OLD, NEW, NEW, NEW],
    );
    fixture.publish(Release::new("helper", "1.0.0", NEW));
    fixture.project(
        "sample",
        "[dependencies]\napi = \"1.8\"\nhelper = \"1\"\n",
        "pub fn value() -> u32 { api::needed() + helper::needed() }\n",
    );
    let human = fixture.floor_raw(&["floor", "--quiet"]);
    assert_success(&human);
    let stdout = String::from_utf8_lossy(&human.stdout);
    for expected in ["  api [normal]: 1.8 -> 1.6.0", "  helper [normal]: 1 -> 1"] {
        assert!(stdout.lines().any(|line| line == expected), "{stdout}");
    }

    let report = fixture.floor(&[]);
    let api = verified(&report, "api", "api", "1.8", "1.6.0");
    assert_eq!(api["earlier_candidates_checked"], false, "{api:#}");
    let helper = verified(&report, "helper", "helper", "1", "1.0.0");
    assert_eq!(helper["earlier_candidates_checked"], true, "{helper:#}");
}

#[test]
fn sampled_regressions_trigger_an_exhaustive_check_below_the_found_candidate() {
    let fixture = history(
        "binary-observed-regression",
        &[OLD, NEW, BROKEN, NEW, BROKEN, NEW, NEW, NEW, NEW],
    );
    let output = fixture.floor_raw(&["floor", "--format", "json", "--verbose"]);
    let report = parse_report(&output);
    let api = verified(&report, "api", "api", "1.8", "1.1.0");
    assert_eq!(api["earlier_candidates_checked"], true, "{api:#}");
    assert_no_repeated_candidates(api);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("non-monotonic") && stderr.contains("exhaustive"),
        "{stderr}"
    );
}

#[test]
fn binary_confirms_the_baseline_endpoint_instead_of_testing_every_newer_release() {
    let fixture = history("binary-baseline-endpoint", &[OLD, NEW, NEW, NEW, NEW, NEW]);
    fixture.project(
        "sample",
        "[dependencies]\napi = \"1\"\n",
        "pub fn value() -> u32 { api::needed() }\n",
    );
    assert_success(&fixture.cargo(&["update", "--offline", "-p", "api", "--precise", "1.2.0"]));
    fixture.check(&[]);
    let report = fixture.floor(&[]);
    let api = verified(&report, "api", "api", "1", "1.1.0");
    assert!(
        api["probes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|probe| !["1.3.0", "1.4.0", "1.5.0"].contains(&probe["version"].as_str().unwrap())),
        "{api:#}"
    );
    assert_no_repeated_candidates(api);
}

#[test]
fn failed_binary_endpoints_do_not_claim_that_untested_older_versions_all_failed() {
    let fixture = history(
        "binary-no-passing-endpoint",
        &[OLD, NEW, BROKEN, BROKEN, NEW],
    );
    assert_success(&fixture.cargo(&["update", "--offline", "-p", "api", "--precise", "1.4.0"]));
    fixture.yank("api", "1.4.0");
    fixture.check(&[]);
    let snapshot = fixture.snapshot(&["Cargo.toml", "Cargo.lock", "src/lib.rs"]);
    let output = fixture.floor_raw(&["floor", "--format", "json", "--apply", "--verbose"]);
    assert_failure(&output, "a broken final resolution must never be applied");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("endpoint") && stderr.contains("exhaustive"),
        "the failed upper endpoint needs an explicit fallback: {stderr}"
    );
    assert!(
        stderr.contains("api 1.1.0 verified"),
        "the fallback must discover the working intermediate release: {stderr}"
    );
    fixture.assert_unchanged(&snapshot);
}

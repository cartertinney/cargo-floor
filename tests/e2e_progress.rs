mod support;

use std::io::Read;
use std::process::Output;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};
use support::{
    BuildGate, Fixture, Release, RunningFloor, assert_failure, assert_success, parse_report,
    verified,
};

fn fixture(name: &str) -> Fixture {
    let fixture = Fixture::new(name);
    for (version, source) in [
        ("1.0.0", "pub fn old() -> u32 { 1 }"),
        ("1.2.0", "pub fn needed() -> u32 { 2 }"),
        ("1.8.0", "pub fn needed() -> u32 { 2 }"),
    ] {
        fixture.publish(Release::new("api", version, source));
    }
    fixture.project(
        "sample",
        "[dependencies]\napi = \"1.8\"\n",
        "pub fn value() -> u32 { api::needed() }\n",
    );
    fixture
}

fn wait_for_output(
    receiver: &Receiver<Vec<u8>>,
    captured: &mut Vec<u8>,
    expected: &str,
    timeout: Duration,
) {
    let deadline = Instant::now() + timeout;
    loop {
        if String::from_utf8_lossy(captured).contains(expected) {
            return;
        }
        let chunk = receiver
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap_or_else(|error| {
                panic!("expected live progress containing {expected:?}: {error}")
            });
        captured.extend_from_slice(&chunk);
    }
}

fn gated_run(
    fixture: &Fixture,
    arguments: &[&str],
    expected: &str,
    heartbeat: Option<&str>,
) -> Output {
    fixture.check(&[]);
    let snapshot = fixture.snapshot(&["Cargo.toml", "Cargo.lock", "src/lib.rs"]);
    let mut gate = BuildGate::new();
    let mut command = fixture.floor_command(arguments);
    command.env("FLOOR_TEST_GATE", gate.address()).env(
        "FLOOR_TEST_GATE_MARKER",
        fixture.scratch_path().join("once"),
    );
    let mut running = RunningFloor::spawn(&mut command);
    let mut stderr = running.take_stderr();
    let (sender, receiver) = mpsc::channel();
    let reader = thread::spawn(move || {
        let mut captured = Vec::new();
        let mut buffer = [0; 1024];
        loop {
            let count = stderr.read(&mut buffer).expect("read live progress");
            if count == 0 {
                break;
            }
            captured.extend_from_slice(&buffer[..count]);
            sender
                .send(buffer[..count].to_vec())
                .expect("progress receiver is open");
        }
        captured
    });
    gate.wait_ready();
    let mut captured = Vec::new();
    wait_for_output(&receiver, &mut captured, expected, Duration::from_secs(3));
    if let Some(heartbeat) = heartbeat {
        wait_for_output(&receiver, &mut captured, heartbeat, Duration::from_secs(15));
    }
    gate.release();
    let mut output = running.finish();
    output.stderr = reader.join().expect("join live progress reader");
    assert_success(&output);
    fixture.assert_unchanged(&snapshot);
    output
}

#[test]
fn human_progress_arrives_before_the_baseline_build_finishes() {
    let fixture = fixture("compact-live-baseline");
    fixture.write("build.rs", BuildGate::script());
    let output = gated_run(&fixture, &["floor"], "Checking baseline", None);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Dependency floors for sample"), "{stdout}");
    assert!(!stdout.contains("Compiling baseline library"), "{stdout}");
    assert!(!String::from_utf8_lossy(&output.stderr).contains("cargo-floor:"));
}

#[test]
fn compact_long_builds_emit_live_dots_without_polluting_json() {
    let fixture = fixture("compact-heartbeat");
    fixture.write("build.rs", BuildGate::script());
    let output = gated_run(
        &fixture,
        &["floor", "--format", "json"],
        "Checking baseline",
        Some("Checking baseline."),
    );
    verified(&parse_report(&output), "api", "api", "1.8", "1.2.0");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("still compiling") && !stderr.contains("s elapsed"),
        "{stderr}"
    );
}

#[test]
fn verbose_long_builds_keep_phase_details_and_elapsed_time() {
    let fixture = fixture("verbose-heartbeat");
    fixture.write("build.rs", BuildGate::script());
    let output = gated_run(
        &fixture,
        &["floor", "--format", "json", "--verbose"],
        "Compiling baseline library",
        Some("still compiling baseline library"),
    );
    verified(&parse_report(&output), "api", "api", "1.8", "1.2.0");
    assert!(String::from_utf8_lossy(&output.stderr).contains("s elapsed"));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("cargo-floor: Compiling baseline library")
    );
}

#[test]
fn default_package_progress_is_compact_live_and_keeps_json_stdout_clean() {
    let fixture = Fixture::new("compact-live-package");
    fixture.publish(
        Release::new("api", "1.0.0", "pub fn old() -> u32 { 1 }")
            .file("build.rs", BuildGate::script()),
    );
    for version in ["1.2.0", "1.8.0"] {
        fixture.publish(Release::new("api", version, "pub fn needed() -> u32 { 2 }"));
    }
    fixture.publish(Release::new(
        "helper",
        "1.0.0",
        "pub fn needed() -> u32 { 2 }",
    ));
    fixture.project(
        "sample",
        "[dependencies]\napi = \"1.8\"\nhelper = \"1\"\n",
        "pub fn value() -> u32 { api::needed() + helper::needed() }\n",
    );
    let output = gated_run(
        &fixture,
        &["floor", "--format", "json"],
        "api [normal]",
        None,
    );
    let report = parse_report(&output);
    verified(&report, "api", "api", "1.8", "1.2.0");
    verified(&report, "helper", "helper", "1", "1.0.0");
    let stderr = String::from_utf8_lossy(&output.stderr);
    for (name, version, count) in [("api", "1.2.0", 3), ("helper", "1.0.0", 1)] {
        let prefix = format!("{name} [normal]");
        let rows: Vec<_> = stderr
            .lines()
            .filter(|line| line.starts_with(&prefix))
            .collect();
        assert_eq!(rows.len(), 1, "{stderr}");
        let row = rows[0].strip_prefix(&prefix).unwrap();
        assert!(row.starts_with(&".".repeat(count)), "{row}");
        let unit = if count == 1 { "probe" } else { "probes" };
        assert!(
            row.ends_with(&format!(" {version} ({count} {unit})")),
            "{row}"
        );
    }
    for hidden in [
        "Testing ",
        "Resolving ",
        "Compiling ",
        "Fetching ",
        "candidate 1/",
    ] {
        assert!(!stderr.contains(hidden), "{stderr}");
    }
    assert!(
        !stderr.contains('\r') && !stderr.contains('\u{1b}'),
        "{stderr}"
    );
    assert!(!stderr.contains("cargo-floor:"), "{stderr}");
}

#[test]
fn progress_identifies_candidates_rejections_and_final_verification() {
    let fixture = fixture("candidate-progress");
    let output = fixture.floor_raw(&["floor", "--format", "json", "-v"]);
    verified(&parse_report(&output), "api", "api", "1.8", "1.2.0");
    let stderr = String::from_utf8_lossy(&output.stderr);
    for expected in [
        "Preparing isolated library workspace",
        "Resolving baseline library",
        "Fetching releases for api",
        "Testing api 1.0.0 (candidate 1/3)",
        "api 1.0.0 rejected: build failed",
        "Testing api 1.2.0 (candidate 2/3)",
        "api 1.2.0 verified",
        "Compiling final witness for api 1.2.0",
        "Compiling final proposed requirements",
    ] {
        assert!(stderr.contains(expected), "missing {expected:?}:\n{stderr}");
    }
}

#[test]
fn compact_baseline_failures_end_the_progress_row_before_the_error() {
    let fixture = fixture("compact-failed-baseline");
    fixture.write(
        "src/lib.rs",
        "compile_error!(\"fixture baseline failure\");",
    );
    let output = fixture.floor_raw(&["floor", "--format", "json"]);
    assert_failure(
        &output,
        "a failed baseline must not look like successful progress",
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let baseline = stderr
        .lines()
        .find(|line| line.starts_with("Checking baseline"))
        .unwrap();
    assert!(baseline.ends_with(" failed"), "{stderr}");
    assert!(
        stderr.contains("\nerror: baseline library build failed"),
        "{stderr}"
    );
    assert!(stderr.contains("fixture baseline failure"), "{stderr}");
}

#[test]
fn verbose_and_quiet_are_explicitly_conflicting_options() {
    let fixture = fixture("conflicting-progress-modes");
    let output = fixture.floor_raw(&["floor", "--verbose", "--quiet"]);
    assert_failure(&output, "contradictory output modes must be rejected");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--verbose") && stderr.contains("--quiet"),
        "{stderr}"
    );
}

#[test]
fn quiet_mode_preserves_json_report_without_progress() {
    let fixture = fixture("quiet-progress");
    let output = fixture.floor_raw(&["floor", "--format", "json", "--quiet"]);
    verified(&parse_report(&output), "api", "api", "1.8", "1.2.0");
    assert!(output.stderr.is_empty(), "{output:?}");
}

#[test]
fn quiet_mode_does_not_hide_baseline_failures() {
    let fixture = fixture("quiet-baseline-failure");
    fixture.write(
        "src/lib.rs",
        "compile_error!(\"fixture baseline failure\");",
    );
    let output = fixture.floor_raw(&["floor", "--format", "json", "--quiet"]);
    assert_failure(&output, "quiet must not hide a failed baseline");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("baseline") && stderr.contains("fixture baseline failure"),
        "{stderr}"
    );
    assert!(!stderr.contains("cargo-floor:"), "{stderr}");
}

#[test]
fn quiet_mode_does_not_hide_unsupported_dependency_warnings() {
    let fixture = fixture("quiet-unsupported");
    fixture.project(
        "sample",
        "[dependencies]\napi = \"=1.8.0\"\n",
        "pub fn value() -> u32 { api::needed() }\n",
    );
    let output = fixture.floor_raw(&["floor", "--format", "json", "-q"]);
    let report = parse_report(&output);
    assert_eq!(report["status"], "incomplete");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("warning: api:"), "{stderr}");
    assert!(!stderr.contains("cargo-floor:"), "{stderr}");
}

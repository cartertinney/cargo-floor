mod support;

use std::io::{BufRead, BufReader};
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

fn wait_for_line(receiver: &Receiver<String>, expected: &str, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    loop {
        let line = receiver
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap_or_else(|error| {
                panic!("expected live progress containing {expected:?}: {error}")
            });
        if line.contains(expected) {
            return line;
        }
    }
}

fn gated_run(json: bool, wait_for_elapsed: bool) -> Output {
    let fixture = fixture("live-progress");
    fixture.write("build.rs", BuildGate::script());
    fixture.check(&[]);
    let snapshot = fixture.snapshot(&["Cargo.toml", "Cargo.lock", "src/lib.rs", "build.rs"]);
    let mut gate = BuildGate::new();
    let mut command = fixture.floor_command(if json {
        &["floor", "--format", "json"]
    } else {
        &["floor"]
    });
    command.env("FLOOR_TEST_GATE", gate.address()).env(
        "FLOOR_TEST_GATE_MARKER",
        fixture.scratch_path().join("once"),
    );
    let mut running = RunningFloor::spawn(&mut command);
    let stderr = running.take_stderr();
    let (sender, receiver) = mpsc::channel();
    let reader = thread::spawn(move || {
        let mut captured = Vec::new();
        for line in BufReader::new(stderr).lines() {
            let line = line.expect("read live progress");
            captured.extend_from_slice(line.as_bytes());
            captured.push(b'\n');
            sender.send(line).expect("progress receiver is open");
        }
        captured
    });
    gate.wait_ready();
    wait_for_line(
        &receiver,
        "Compiling baseline library",
        Duration::from_secs(3),
    );
    if wait_for_elapsed {
        let line = wait_for_line(
            &receiver,
            "still compiling baseline library",
            Duration::from_secs(15),
        );
        assert!(line.contains("s elapsed"), "{line}");
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
    let output = gated_run(false, false);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Dependency floors for sample"), "{stdout}");
    assert!(!stdout.contains("Compiling baseline library"), "{stdout}");
}

#[test]
fn json_progress_arrives_live_and_long_builds_report_elapsed_time() {
    let output = gated_run(true, true);
    verified(&parse_report(&output), "api", "api", "1.8", "1.2.0");
}

#[test]
fn progress_identifies_candidates_rejections_and_final_verification() {
    let fixture = fixture("candidate-progress");
    let output = fixture.floor_raw(&["floor", "--format", "json"]);
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

//! The conformance report binary: format, honesty of the status column, and
//! clean failure on a bad IR.

use std::path::PathBuf;
use std::process::Command;

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_opurom-runtime-adapters")
}

fn example(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join(name)
}

#[test]
fn the_report_lists_every_runtime_with_an_honest_status() {
    let output = Command::new(binary())
        .arg(example("todo-fullstack.opuir.json"))
        .output()
        .expect("the report binary runs");
    assert!(output.status.success(), "the report must exit cleanly");
    let report = String::from_utf8(output.stdout).expect("the report is UTF-8");

    assert!(
        report.contains("name | status | checks | result"),
        "{report}"
    );
    for row in [
        "native | Verified | 7 |",
        "node | Verified | 7 |",
        "edge | Verified | 7 |",
        "embedded | Verified | 7 |",
        "serverless-cloud | Planned | 0 | no deployment attempted",
    ] {
        assert!(report.contains(row), "missing row {row:?} in\n{report}");
    }
    // The report must not claim a cloud deployment.
    assert!(!report.contains("deployed to"), "{report}");
}

#[test]
fn a_plain_v01_ir_produces_a_report_with_an_empty_model_set() {
    let output = Command::new(binary())
        .arg(example("plain.opuir.json"))
        .output()
        .expect("the report binary runs");
    assert!(output.status.success(), "the report must exit cleanly");
    let report = String::from_utf8(output.stdout).expect("the report is UTF-8");
    assert!(report.contains("models: 0  api routes: 0"), "{report}");
    assert!(
        report.contains("serverless-cloud | Planned | 0 | no deployment attempted"),
        "{report}"
    );
}

#[test]
fn a_bad_ir_fails_with_a_readable_diagnostic() {
    let output = Command::new(binary())
        .arg(example("does-not-exist.opuir.json"))
        .output()
        .expect("the report binary runs");
    assert!(!output.status.success(), "a missing IR must fail");
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(stderr.contains("cannot read"), "{stderr}");
}

#[test]
fn the_binary_refuses_extra_arguments() {
    let output = Command::new(binary())
        .arg(example("todo-fullstack.opuir.json"))
        .arg("extra")
        .output()
        .expect("the report binary runs");
    assert!(!output.status.success(), "extra arguments must fail");
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(stderr.contains("usage:"), "{stderr}");
}

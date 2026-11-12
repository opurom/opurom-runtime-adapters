//! `opurom-runtime-adapters <fullstack-ir.json>` prints a runtime conformance
//! report.
//!
//! The report is honest about what was and was not attempted. Every `Verified`
//! row means "this runtime passed the local conformance checks on this
//! machine". The `serverless-cloud` row is always `Planned`: no cloud
//! deployment is attempted anywhere in this project.

use opurom_runtime_adapters::adapters::{Edge, Embedded, NativeRust, NodeJs};
use opurom_runtime_adapters::report::{compare, run_checks};
use opurom_runtime_adapters::{sample_checks, FullstackIr, RuntimeAdapter, RuntimeReport, Status};
use std::fmt::Write as _;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 1 {
        eprintln!("usage: opurom-runtime-adapters <fullstack-ir.json>");
        if args.len() > 1 {
            eprintln!("error: expected exactly one argument, got {}", args.len());
        }
        return ExitCode::FAILURE;
    }
    let path = &args[0];

    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => {
            eprintln!("error: cannot read {path}: {error}");
            return ExitCode::FAILURE;
        }
    };
    let ir = match FullstackIr::parse(&text) {
        Ok(ir) => ir,
        Err(error) => {
            eprintln!("error: {path}: {error}");
            return ExitCode::FAILURE;
        }
    };

    let checks = sample_checks(&ir);
    let mut rows: Vec<RuntimeReport> = Vec::new();
    let mut failures: Vec<String> = Vec::new();

    // The native runtime is the reference: every other runtime must produce
    // byte-identical canonical responses.
    let mut native = NativeRust::new(ir.clone());
    let reference = match run_checks(&mut native, &checks) {
        Ok(bytes) => {
            rows.push(RuntimeReport {
                name: native.name().to_owned(),
                status: Status::Verified,
                checks: checks.len(),
                result: "pass (reference runtime)".to_owned(),
            });
            Some(bytes)
        }
        Err(error) => {
            rows.push(RuntimeReport {
                name: native.name().to_owned(),
                status: Status::Scaffold,
                checks: 0,
                result: format!("fail ({error})"),
            });
            failures.push(format!("native: {error}"));
            None
        }
    };
    let _ = native.shutdown();

    // The Node.js runtime: a real subprocess.
    let mut node = NodeJs::new(ir.clone());
    rows.push(measure(
        &mut node,
        "node",
        &checks,
        reference.as_deref(),
        "byte-identical to native",
        &mut failures,
    ));
    let _ = node.shutdown();

    // The edge runtime: a pure function, zero IO.
    let mut edge = Edge::new(ir.clone());
    rows.push(measure(
        &mut edge,
        "edge",
        &checks,
        reference.as_deref(),
        "byte-identical to native; local function only, not deployed",
        &mut failures,
    ));
    let _ = edge.shutdown();

    // The embedded runtime: single-threaded, fixed capacity.
    let mut embedded = Embedded::new(ir.clone());
    rows.push(measure(
        &mut embedded,
        "embedded",
        &checks,
        reference.as_deref(),
        "byte-identical to native; fixed capacity, no heap growth",
        &mut failures,
    ));
    let _ = embedded.shutdown();

    // Always present, always Planned: no cloud deployment is attempted here.
    rows.push(RuntimeReport {
        name: "serverless-cloud".to_owned(),
        status: Status::Planned,
        checks: 0,
        result: "no deployment attempted".to_owned(),
    });

    print_report(&ir, &rows, &failures);
    if failures.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Run the checks against one runtime and turn the outcome into a report row.
///
/// A runtime is only `Verified` when it ran every check *and* its canonical
/// bytes match the reference runtime's bytes.
fn measure(
    adapter: &mut dyn RuntimeAdapter,
    name: &str,
    checks: &[opurom_runtime_adapters::Check],
    reference: Option<&[Vec<u8>]>,
    pass_note: &str,
    failures: &mut Vec<String>,
) -> RuntimeReport {
    let bytes = match run_checks(adapter, checks) {
        Ok(bytes) => bytes,
        Err(error) => {
            failures.push(format!("{name}: {error}"));
            return RuntimeReport {
                name: name.to_owned(),
                status: Status::Scaffold,
                checks: 0,
                result: format!("fail ({error})"),
            };
        }
    };
    let Some(reference) = reference else {
        let detail = format!("{name}: no reference runtime to compare against");
        failures.push(detail);
        return RuntimeReport {
            name: name.to_owned(),
            status: Status::Scaffold,
            checks: bytes.len(),
            result: "fail (no reference runtime)".to_owned(),
        };
    };
    let diffs = compare("native", reference, name, &bytes);
    if diffs.is_empty() {
        return RuntimeReport {
            name: name.to_owned(),
            status: Status::Verified,
            checks: checks.len(),
            result: format!("pass ({pass_note})"),
        };
    }
    for diff in &diffs {
        failures.push(diff.detail.clone());
    }
    RuntimeReport {
        name: name.to_owned(),
        status: Status::Scaffold,
        checks: checks.len(),
        result: format!("fail ({} mismatching checks)", diffs.len()),
    }
}

fn print_report(ir: &FullstackIr, rows: &[RuntimeReport], failures: &[String]) {
    let mut out = String::new();
    let _ = writeln!(out, "Opurom runtime conformance report");
    let _ = writeln!(
        out,
        "app: {}",
        if ir.app.is_empty() { "-" } else { &ir.app }
    );
    let _ = writeln!(
        out,
        "models: {}  api routes: {}",
        ir.models.len(),
        ir.api.len()
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "name | status | checks | result");
    let _ = writeln!(out, "--- | ------ | ------ | ------");
    for row in rows {
        let _ = writeln!(
            out,
            "{} | {} | {} | {}",
            row.name,
            row.status.as_str(),
            row.checks,
            row.result
        );
    }
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "Verified means: local conformance checks passed on this machine only."
    );
    let _ = writeln!(
        out,
        "Not verified: real serverless/edge cloud deployment, Node.js service bundling,"
    );
    let _ = writeln!(
        out,
        "PostgreSQL/MySQL drivers, multi-tenant isolation, latency/fps benchmarks."
    );
    if !failures.is_empty() {
        let _ = writeln!(out);
        let _ = writeln!(out, "failures:");
        for failure in failures {
            let _ = writeln!(out, "- {failure}");
        }
    }
    print!("{out}");
}

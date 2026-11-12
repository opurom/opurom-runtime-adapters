//! The conformance harness: run the same checks against several runtimes and
//! compare the canonical response bytes.

use crate::ir::FullstackIr;
use crate::logic::sample_checks;
use crate::{AdapterError, RuntimeAdapter};

/// One runtime row of the conformance report.
#[derive(Clone, Debug)]
pub struct RuntimeReport {
    pub name: String,
    pub status: crate::Status,
    pub checks: usize,
    pub result: String,
}

/// A failed check.
#[derive(Clone, Debug)]
pub struct CheckFailure {
    pub runtime: String,
    pub check: String,
    pub detail: String,
}

/// Run every check against one adapter. Each check runs against the adapter's
/// current state, so the checks are stateful in the documented order.
pub fn run_checks(
    adapter: &mut dyn RuntimeAdapter,
    checks: &[crate::Check],
) -> Result<Vec<Vec<u8>>, AdapterError> {
    let mut out = Vec::with_capacity(checks.len());
    for check in checks {
        let response = adapter.invoke(&check.request)?;
        out.push(response.canonical_bytes());
    }
    Ok(out)
}

/// Compare the canonical bytes of every runtime against the reference
/// runtime's bytes.
pub fn compare(
    reference_name: &str,
    reference: &[Vec<u8>],
    runtime_name: &str,
    candidate: &[Vec<u8>],
) -> Vec<CheckFailure> {
    let mut failures = Vec::new();
    if reference.len() != candidate.len() {
        failures.push(CheckFailure {
            runtime: runtime_name.to_owned(),
            check: "check-count".into(),
            detail: format!(
                "{runtime_name} ran {} checks, {reference_name} ran {}",
                candidate.len(),
                reference.len()
            ),
        });
        return failures;
    }
    for (index, (expected, actual)) in reference.iter().zip(candidate.iter()).enumerate() {
        if expected != actual {
            failures.push(CheckFailure {
                runtime: runtime_name.to_owned(),
                check: format!("check-{index}"),
                detail: format!(
                    "{runtime_name} response differs from {reference_name}: expected {}, got {}",
                    String::from_utf8_lossy(expected),
                    String::from_utf8_lossy(actual)
                ),
            });
        }
    }
    failures
}

/// Build the checks for an IR.
pub fn checks_for(ir: &FullstackIr) -> Vec<crate::Check> {
    sample_checks(ir)
}

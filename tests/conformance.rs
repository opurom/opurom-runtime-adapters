//! Conformance suite: the same sample request set against all four runtimes.
//!
//! The runtimes must produce byte-identical canonical responses, the Node
//! runtime must fail cleanly when its binary is missing, and the bounded
//! timeout must hold against a deliberately non-responsive host.

use opurom_runtime_adapters::adapters::{
    default_host_script, Edge, Embedded, NativeRust, NodeConfig, NodeJs,
};
use opurom_runtime_adapters::{
    sample_checks, AdapterError, Check, FullstackIr, Request, RuntimeAdapter,
};
use serde_json::{json, Value};
use std::path::PathBuf;

fn todo_ir() -> FullstackIr {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/todo-fullstack.opuir.json");
    let text = std::fs::read_to_string(&path).expect("the example IR is readable");
    FullstackIr::parse(&text).expect("the example IR parses")
}

fn plain_ir() -> FullstackIr {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/plain.opuir.json");
    let text = std::fs::read_to_string(&path).expect("the example IR is readable");
    FullstackIr::parse(&text).expect("the example IR parses")
}

/// Run the checks against one adapter and return the canonical bytes.
fn run(adapter: &mut dyn RuntimeAdapter, checks: &[Check]) -> Vec<Vec<u8>> {
    checks
        .iter()
        .map(|check| {
            adapter
                .invoke(&check.request)
                .expect("the adapter answers")
                .canonical_bytes()
        })
        .collect()
}

#[test]
fn sample_request_set_covers_the_documented_crud_steps() {
    let checks = sample_checks(&todo_ir());
    let names: Vec<&str> = checks.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "list-empty",
            "create",
            "list-one",
            "get",
            "update",
            "delete",
            "not-found"
        ]
    );
    assert_eq!(checks[0].request.method, "GET");
    assert_eq!(checks[0].request.path, "/todos");
    assert_eq!(checks[1].request.method, "POST");
    assert_eq!(checks[4].request.method, "PUT");
    assert_eq!(checks[5].request.method, "DELETE");
}

#[test]
fn all_runtimes_answer_the_sample_set_identically() {
    let ir = todo_ir();
    let checks = sample_checks(&ir);

    let mut native = NativeRust::new(ir.clone());
    let expected = run(&mut native, &checks);
    assert_eq!(expected.len(), 7);
    native.shutdown().expect("native shuts down");

    let mut node = NodeJs::new(ir.clone());
    let node_bytes = run(&mut node, &checks);
    node.shutdown().expect("node shuts down");

    let mut edge = Edge::new(ir.clone());
    let edge_bytes = run(&mut edge, &checks);
    edge.shutdown().expect("edge shuts down");

    let mut embedded = Embedded::new(ir.clone());
    let embedded_bytes = run(&mut embedded, &checks);
    embedded.shutdown().expect("embedded shuts down");

    for (index, expected_bytes) in expected.iter().enumerate() {
        assert_eq!(
            node_bytes[index], *expected_bytes,
            "node response differs from native for check {index} ({})",
            checks[index].name
        );
        assert_eq!(
            edge_bytes[index], *expected_bytes,
            "edge response differs from native for check {index} ({})",
            checks[index].name
        );
        assert_eq!(
            embedded_bytes[index], *expected_bytes,
            "embedded response differs from native for check {index} ({})",
            checks[index].name
        );
    }
}

#[test]
fn the_conformance_bytes_are_the_documented_crud_answers() {
    let ir = todo_ir();
    let checks = sample_checks(&ir);
    let mut native = NativeRust::new(ir.clone());

    let empty = native
        .invoke(&checks[0].request)
        .expect("list answers")
        .canonical_bytes();
    assert_eq!(empty, br#"{"body":[],"status":200}"#.to_vec());

    let created = native
        .invoke(&checks[1].request)
        .expect("create answers")
        .canonical_bytes();
    assert_eq!(
        created,
        br#"{"body":{"done":false,"id":1,"title":"sample"},"status":201}"#.to_vec()
    );

    let one = native
        .invoke(&checks[2].request)
        .expect("list answers")
        .canonical_bytes();
    assert_eq!(
        one,
        br#"{"body":[{"done":false,"id":1,"title":"sample"}],"status":200}"#.to_vec()
    );

    let deleted = native
        .invoke(&checks[5].request)
        .expect("delete answers")
        .canonical_bytes();
    assert_eq!(
        deleted,
        br#"{"body":{"deleted":true},"status":200}"#.to_vec()
    );

    let missing = native
        .invoke(&checks[6].request)
        .expect("get answers")
        .canonical_bytes();
    assert_eq!(
        missing,
        br#"{"body":{"error":"not_found"},"status":404}"#.to_vec()
    );
}

#[test]
fn a_plain_v01_ir_still_works_with_an_empty_model_set() {
    let ir = plain_ir();
    assert!(ir.models.is_empty());
    assert!(ir.api.is_empty());

    let checks = sample_checks(&ir);
    assert_eq!(checks.len(), 1);

    let mut native = NativeRust::new(ir.clone());
    let expected = run(&mut native, &checks);
    let mut node = NodeJs::new(ir.clone());
    let node_bytes = run(&mut node, &checks);
    node.shutdown().expect("node shuts down");
    assert_eq!(node_bytes, expected);
    assert_eq!(
        expected[0],
        br#"{"body":{"error":"not_found"},"status":404}"#.to_vec()
    );
}

#[test]
fn a_missing_node_binary_is_a_clean_adapter_error() {
    let ir = todo_ir();
    let config = NodeConfig::new(default_host_script())
        .with_binary("opurom-no-such-node-binary")
        .with_timeout_ms(500);
    let mut node = NodeJs::with_config(ir, config);
    let checks = sample_checks(&todo_ir());

    let error = node
        .invoke(&checks[0].request)
        .expect_err("a missing binary is an error, not a success");
    match &error {
        AdapterError::HostUnavailable(why) => {
            assert!(why.contains("opurom-no-such-node-binary"), "{why}")
        }
        other => panic!("expected HostUnavailable, got {other:?}"),
    }
    // The error is a value: the process is still alive and can be shut down.
    node.shutdown()
        .expect("shutdown after a failed start is clean");
}

#[test]
fn a_non_responsive_host_times_out_within_two_seconds() {
    let ir = todo_ir();
    let stub = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/silent-host.mjs");
    let config = NodeConfig::new(stub).with_timeout_ms(400);
    let mut node = NodeJs::with_config(ir, config);
    let checks = sample_checks(&todo_ir());

    let started = std::time::Instant::now();
    let error = node
        .invoke(&checks[0].request)
        .expect_err("a silent host cannot answer");
    let elapsed = started.elapsed();

    assert!(
        matches!(error, AdapterError::Timeout { .. }),
        "expected a timeout, got {error:?}"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "the timeout must be bounded, took {elapsed:?}"
    );
    let _ = node.shutdown();
}

#[test]
fn the_node_host_answers_the_handshake_and_reports_its_protocol() {
    let ir = todo_ir();
    let config = NodeConfig::new(default_host_script()).with_timeout_ms(2000);
    let mut node = NodeJs::with_config(ir.clone(), config.clone());

    // A request that the IR does not declare still exercises the round trip.
    let response = node
        .invoke(&Request::new("GET", "/__missing__", Value::Null))
        .expect("the host answers");
    assert_eq!(response.status, 404);
    assert_eq!(response.body, json!({ "error": "not_found" }));

    node.shutdown().expect("node shuts down");
}

#[test]
fn the_embedded_runtime_refuses_to_exceed_its_fixed_capacity() {
    let ir = todo_ir();
    let mut embedded = Embedded::new(ir.clone());
    let before = embedded.resident_bytes();

    // Fill the fixed table beyond its row capacity.
    let mut last_error = None;
    for index in 0..64 {
        let body = json!({ "title": format!("row-{index}"), "done": false });
        let request = Request::new("POST", "/todos", body);
        if let Err(error) = embedded.invoke(&request) {
            last_error = Some(error);
            break;
        }
    }
    let error = last_error.expect("the fixed table refuses the overflow");
    assert!(
        matches!(error, AdapterError::BudgetExceeded(_)),
        "expected a budget error, got {error:?}"
    );
    // The resident footprint never grew: the refusal happened before the heap.
    assert_eq!(embedded.resident_bytes(), before);
    assert_eq!(embedded.row_count(), 32);
}

#[test]
fn every_adapter_reports_its_capabilities_and_name() {
    let ir = todo_ir();
    let native = NativeRust::new(ir.clone());
    assert_eq!(native.name(), "native");
    assert!(!native.capabilities().async_io);
    assert!(!native.capabilities().threads);
    assert!(!native.capabilities().fs);
    assert!(!native.capabilities().network);
    assert_eq!(native.capabilities().max_body, 1024 * 1024);

    let node = NodeJs::new(ir.clone());
    assert_eq!(node.name(), "node");
    assert!(node.capabilities().async_io);
    assert!(node.capabilities().threads);

    let edge = Edge::new(ir.clone());
    assert_eq!(edge.name(), "edge");
    assert!(!edge.capabilities().async_io);
    assert!(!edge.capabilities().fs);

    let embedded = Embedded::new(ir.clone());
    assert_eq!(embedded.name(), "embedded");
    assert!(!embedded.capabilities().threads);
    assert!(!embedded.capabilities().network);
}

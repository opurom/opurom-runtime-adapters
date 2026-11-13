//! Conformance suite: the same sample request set against all four runtimes,
//! plus a table of error-path and list-order cases checked byte-for-byte.
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

/// One parity case: a request trace run on a fresh adapter. Every response is
/// compared across all four runtimes; the last one is also pinned to the exact
/// canonical bytes in `expected`.
struct ParityCase {
    name: &'static str,
    requests: Vec<Request>,
    expected: &'static [u8],
}

const NOT_FOUND: &[u8] = br#"{"body":{"error":"not_found"},"status":404}"#;
const INVALID_BODY: &[u8] = br#"{"body":{"error":"invalid_body"},"status":400}"#;
const INVALID_BODY_TITLE: &[u8] =
    br#"{"body":{"error":"invalid_body","field":"title"},"status":400}"#;
const INVALID_FIELD_TITLE: &[u8] =
    br#"{"body":{"error":"invalid_field","field":"title"},"status":400}"#;
const INVALID_FIELD_DONE: &[u8] =
    br#"{"body":{"error":"invalid_field","field":"done"},"status":400}"#;
const INVALID_FIELD_ID: &[u8] = br#"{"body":{"error":"invalid_field","field":"id"},"status":400}"#;
const CONFLICT: &[u8] = br#"{"body":{"error":"conflict"},"status":409}"#;
const EMPTY_LIST: &[u8] = br#"{"body":[],"status":200}"#;
const CREATED_X: &[u8] = br#"{"body":{"done":false,"id":1,"title":"x"},"status":201}"#;
const CREATED_Y: &[u8] = br#"{"body":{"done":false,"id":2,"title":"y"},"status":201}"#;
const ORDERED_LIST: &[u8] = br#"{"body":[{"done":false,"id":1,"title":"a"},{"done":false,"id":3,"title":"c"},{"done":false,"id":4,"title":"d"}],"status":200}"#;
const INSERTION_ORDER_LIST: &[u8] = br#"{"body":[{"done":false,"id":9,"title":"b"},{"done":false,"id":3,"title":"a"}],"status":200}"#;

fn get(path: &str) -> Request {
    Request::new("GET", path, Value::Null)
}

fn post(body: Value) -> Request {
    Request::new("POST", "/todos", body)
}

fn put(path: &str, body: Value) -> Request {
    Request::new("PUT", path, body)
}

fn delete(path: &str) -> Request {
    Request::new("DELETE", path, Value::Null)
}

/// Error paths (not-found ids, invalid payloads, duplicate ids, empty
/// collections) and deterministic list order, one case per row.
fn parity_cases() -> Vec<ParityCase> {
    let row = json!({ "title": "x", "done": false });
    vec![
        // Not-found ids.
        ParityCase {
            name: "unknown-route",
            requests: vec![get("/__missing__")],
            expected: NOT_FOUND,
        },
        ParityCase {
            name: "get-missing-id",
            requests: vec![get("/todos/999")],
            expected: NOT_FOUND,
        },
        ParityCase {
            name: "get-malformed-id",
            requests: vec![get("/todos/not-a-number")],
            expected: NOT_FOUND,
        },
        ParityCase {
            name: "get-non-canonical-id",
            requests: vec![get("/todos/01")],
            expected: NOT_FOUND,
        },
        ParityCase {
            name: "get-negative-id",
            requests: vec![get("/todos/-1")],
            expected: NOT_FOUND,
        },
        ParityCase {
            name: "update-missing-id",
            requests: vec![put("/todos/999", json!({ "title": "x" }))],
            expected: NOT_FOUND,
        },
        ParityCase {
            name: "delete-missing-id",
            requests: vec![delete("/todos/999")],
            expected: NOT_FOUND,
        },
        ParityCase {
            name: "get-after-delete",
            requests: vec![post(row.clone()), delete("/todos/1"), get("/todos/1")],
            expected: NOT_FOUND,
        },
        ParityCase {
            name: "delete-twice",
            requests: vec![post(row.clone()), delete("/todos/1"), delete("/todos/1")],
            expected: NOT_FOUND,
        },
        // Invalid payloads.
        ParityCase {
            name: "create-null-body",
            requests: vec![post(Value::Null)],
            expected: INVALID_BODY,
        },
        ParityCase {
            name: "create-array-body",
            requests: vec![post(json!([]))],
            expected: INVALID_BODY,
        },
        ParityCase {
            name: "create-string-body",
            requests: vec![post(json!("nope"))],
            expected: INVALID_BODY,
        },
        ParityCase {
            name: "create-number-body",
            requests: vec![post(json!(7))],
            expected: INVALID_BODY,
        },
        ParityCase {
            name: "create-bool-body",
            requests: vec![post(json!(true))],
            expected: INVALID_BODY,
        },
        ParityCase {
            name: "create-empty-object",
            requests: vec![post(json!({}))],
            expected: INVALID_BODY_TITLE,
        },
        ParityCase {
            name: "create-missing-field",
            requests: vec![post(json!({ "done": false }))],
            expected: INVALID_BODY_TITLE,
        },
        ParityCase {
            name: "create-wrong-field-type",
            requests: vec![post(json!({ "title": 5, "done": false }))],
            expected: INVALID_FIELD_TITLE,
        },
        ParityCase {
            name: "create-wrong-bool-type",
            requests: vec![post(json!({ "title": "x", "done": 1 }))],
            expected: INVALID_FIELD_DONE,
        },
        ParityCase {
            name: "create-wrong-key-type",
            requests: vec![post(json!({ "id": "x", "title": "x" }))],
            expected: INVALID_FIELD_ID,
        },
        ParityCase {
            name: "update-null-body",
            requests: vec![post(row.clone()), put("/todos/1", Value::Null)],
            expected: INVALID_BODY,
        },
        ParityCase {
            name: "update-array-body",
            requests: vec![post(row.clone()), put("/todos/1", json!([]))],
            expected: INVALID_BODY,
        },
        ParityCase {
            name: "update-wrong-field-type",
            requests: vec![post(row.clone()), put("/todos/1", json!({ "title": 5 }))],
            expected: INVALID_FIELD_TITLE,
        },
        // Duplicate ids.
        ParityCase {
            name: "duplicate-explicit-id",
            requests: vec![
                post(json!({ "id": 1, "title": "x" })),
                post(json!({ "id": 1, "title": "y" })),
            ],
            expected: CONFLICT,
        },
        ParityCase {
            name: "duplicate-auto-id",
            requests: vec![post(row.clone()), post(json!({ "title": "y" }))],
            expected: CREATED_Y,
        },
        ParityCase {
            name: "recreate-deleted-id",
            requests: vec![
                post(json!({ "id": 1, "title": "x" })),
                delete("/todos/1"),
                post(json!({ "id": 1, "title": "x" })),
            ],
            expected: CREATED_X,
        },
        // Empty collections.
        ParityCase {
            name: "empty-list",
            requests: vec![get("/todos")],
            expected: EMPTY_LIST,
        },
        ParityCase {
            name: "empty-list-after-delete",
            requests: vec![post(row), delete("/todos/1"), get("/todos")],
            expected: EMPTY_LIST,
        },
        // Deterministic list order: embedded must reuse a freed slot at the
        // tail, and no runtime may sort rows by key.
        ParityCase {
            name: "list-order-after-slot-reuse",
            requests: vec![
                post(json!({ "id": 2, "title": "b" })),
                post(json!({ "id": 1, "title": "a" })),
                post(json!({ "id": 3, "title": "c" })),
                delete("/todos/2"),
                post(json!({ "id": 4, "title": "d" })),
                get("/todos"),
            ],
            expected: ORDERED_LIST,
        },
        ParityCase {
            name: "list-order-is-insertion-order",
            requests: vec![
                post(json!({ "id": 9, "title": "b" })),
                post(json!({ "id": 3, "title": "a" })),
                get("/todos"),
            ],
            expected: INSERTION_ORDER_LIST,
        },
    ]
}

fn parity_adapters(ir: &FullstackIr) -> Vec<Box<dyn RuntimeAdapter>> {
    vec![
        Box::new(NativeRust::new(ir.clone())),
        Box::new(NodeJs::new(ir.clone())),
        Box::new(Edge::new(ir.clone())),
        Box::new(Embedded::new(ir.clone())),
    ]
}

fn run_requests(adapter: &mut dyn RuntimeAdapter, requests: &[Request]) -> Vec<Vec<u8>> {
    let checks: Vec<Check> = requests
        .iter()
        .map(|request| Check {
            name: String::new(),
            request: request.clone(),
        })
        .collect();
    run(adapter, &checks)
}

#[test]
fn all_runtimes_share_the_error_path_and_list_order_bytes() {
    let ir = todo_ir();
    let cases = parity_cases();
    let expected_names = ["native", "node", "edge", "embedded"];

    for case in &cases {
        let mut answers: Vec<(String, Vec<Vec<u8>>)> = Vec::new();
        for mut adapter in parity_adapters(&ir) {
            let bytes = run_requests(adapter.as_mut(), &case.requests);
            let name = adapter.name().to_owned();
            adapter.shutdown().expect("the adapter shuts down");
            answers.push((name, bytes));
        }
        let names: Vec<&str> = answers.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, expected_names);

        let reference = &answers[0].1;
        for (name, bytes) in &answers[1..] {
            assert_eq!(
                bytes, reference,
                "{name} differs from native for case {}",
                case.name
            );
        }
        let last = reference.last().expect("every case has a final response");
        assert_eq!(
            last.as_slice(),
            case.expected,
            "wrong bytes for case {}",
            case.name
        );
    }
    assert_eq!(cases.len(), 29);
}

/// Item route with the key segment first; dispatch then depends on binding by
/// name rather than by segment position.
fn nested_ir() -> FullstackIr {
    let value = json!({
        "ir": "0.1.0",
        "app": "NestedTodos",
        "models": [
            {
                "name": "Todo",
                "fields": [
                    { "name": "id", "type": "int", "primary": true, "default": null },
                    { "name": "title", "type": "string", "primary": false, "default": null },
                    { "name": "done", "type": "bool", "primary": false, "default": false }
                ]
            }
        ],
        "api": [
            { "path": "/todos", "model": "Todo", "handlers": ["list", "create"] },
            {
                "path": "/todos/:id/details/:section",
                "model": "Todo",
                "handlers": ["get", "update", "delete"]
            }
        ]
    });
    FullstackIr::from_value(&value).expect("the nested IR parses")
}

const NESTED_ROW_X: &[u8] = br#"{"body":{"done":false,"id":1,"title":"x"},"status":200}"#;
const NESTED_ROW_Y: &[u8] = br#"{"body":{"done":false,"id":1,"title":"y"},"status":200}"#;
const NESTED_DELETED: &[u8] = br#"{"body":{"deleted":true},"status":200}"#;

fn nested_trace() -> Vec<Request> {
    vec![
        post(json!({ "title": "x", "done": false })),
        get("/todos/1/details/general"),
        get("/todos/1/details/other"),
        put("/todos/1/details/general", json!({ "title": "y" })),
        get("/todos/999/details/general"),
        get("/todos/1/summary/general"),
        delete("/todos/1/details/general"),
        get("/todos/1/details/general"),
    ]
}

fn nested_expected() -> Vec<&'static [u8]> {
    vec![
        CREATED_X,
        NESTED_ROW_X,
        NESTED_ROW_X,
        NESTED_ROW_Y,
        NOT_FOUND,
        NOT_FOUND,
        NESTED_DELETED,
        NOT_FOUND,
    ]
}

#[test]
fn a_two_parameter_route_dispatches_from_the_named_key() {
    let mut native = NativeRust::new(nested_ir());
    let requests = nested_trace();
    let expected = nested_expected();
    for (index, request) in requests.iter().enumerate() {
        let bytes = native
            .invoke(request)
            .expect("the adapter answers")
            .canonical_bytes();
        assert_eq!(
            bytes.as_slice(),
            expected[index],
            "wrong response for step {index} ({})",
            request.path
        );
    }
    native.shutdown().expect("native shuts down");
}

#[test]
fn all_runtimes_agree_on_two_parameter_routes() {
    let ir = nested_ir();
    let requests = nested_trace();
    let mut answers: Vec<Vec<Vec<u8>>> = Vec::new();
    for mut adapter in parity_adapters(&ir) {
        let name = adapter.name().to_owned();
        let bytes = run_requests(adapter.as_mut(), &requests);
        adapter.shutdown().expect("the adapter shuts down");
        assert_eq!(
            bytes[1].as_slice(),
            NESTED_ROW_X,
            "{name} does not dispatch the item route"
        );
        answers.push(bytes);
    }
    let reference = &answers[0];
    for (offset, bytes) in answers[1..].iter().enumerate() {
        let name = ["node", "edge", "embedded"][offset];
        assert_eq!(bytes, reference, "{name} differs from native");
    }
    for (index, expected) in nested_expected().iter().enumerate() {
        assert_eq!(reference[index].as_slice(), *expected, "step {index}");
    }
}

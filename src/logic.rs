//! The one business logic: CRUD over an in-memory table, with routes and row
//! shapes derived from the IR `api` and `models` arrays.
//!
//! `MapStore`, `FixedStore` and the Node.js host all implement this same
//! contract. The conformance suite compares canonical response bytes, so
//! any semantic drift between runtimes is a test failure.
//!
//! Responses follow framework rules: list -> array, create -> row,
//! get/update -> row or 404, delete -> `{"deleted":true}` or 404, unknown path
//! -> 404 JSON. Body size is capped by the runtime's `Capabilities::max_body`.

use crate::ir::{matches_kind, FieldKind, FullstackIr, IrApi, IrModel};
use crate::store::Store;
use crate::{AdapterError, Request, Response};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq)]
pub struct Check {
    pub name: String,
    pub request: Request,
}

struct KeyInfo {
    name: String,
    kind: FieldKind,
    implicit: bool,
}

fn key_info(model: &IrModel) -> KeyInfo {
    if let Some(field) = model.fields.iter().find(|f| f.primary) {
        KeyInfo {
            name: field.name.clone(),
            kind: field.kind,
            implicit: false,
        }
    } else {
        // the framework implementation: zero primary fields is legal, an auto rowid is used.
        KeyInfo {
            name: "id".into(),
            kind: FieldKind::Int,
            implicit: true,
        }
    }
}

fn segments(path: &str) -> Vec<&str> {
    path.split('/').filter(|s| !s.is_empty()).collect()
}

// Bind by name, not by position, so the key field's parameter wins among several.
fn find_route<'a>(
    ir: &'a FullstackIr,
    request_segments: &[&str],
) -> Option<(&'a IrApi, BTreeMap<String, String>)> {
    for api in &ir.api {
        let parts = segments(&api.path);
        if parts.len() != request_segments.len() {
            continue;
        }
        let mut params = BTreeMap::new();
        let mut matched = true;
        for (part, segment) in parts.iter().zip(request_segments.iter()) {
            if let Some(name) = part.strip_prefix(':') {
                params.insert(name.to_owned(), (*segment).to_owned());
            } else if *part != *segment {
                matched = false;
                break;
            }
        }
        if matched {
            return Some((api, params));
        }
    }
    None
}

/// Canonical key rule: an int key is the decimal spelling of its i64 value, so
/// leading zeros and `+1` are not keys. A string key is used verbatim.
fn parse_key(kind: FieldKind, raw: &str) -> Option<String> {
    match kind {
        FieldKind::Int => {
            let number: i64 = raw.parse().ok()?;
            if number.to_string() != raw {
                return None;
            }
            Some(number.to_string())
        }
        _ => Some(raw.to_owned()),
    }
}

fn typed_key(kind: FieldKind, key: &str) -> Value {
    if kind == FieldKind::Int {
        Value::from(key.parse::<i64>().unwrap_or(0))
    } else {
        Value::from(key)
    }
}

fn next_id(store: &dyn Store, model: &str, key_name: &str) -> i64 {
    store
        .list(model)
        .iter()
        .filter_map(|row| row.get(key_name).and_then(Value::as_i64))
        .max()
        .unwrap_or(0)
        + 1
}

fn not_found() -> Response {
    Response::new(404, json!({ "error": "not_found" }))
}

fn invalid_body(field: Option<&str>) -> Response {
    let mut object = Map::new();
    object.insert("error".into(), Value::from("invalid_body"));
    if let Some(field) = field {
        object.insert("field".into(), Value::from(field));
    }
    Response::new(400, Value::Object(object))
}

fn invalid_field(field: &str) -> Response {
    Response::new(400, json!({ "error": "invalid_field", "field": field }))
}

fn build_row(
    model: &IrModel,
    key: &KeyInfo,
    key_value: &str,
    body: &Map<String, Value>,
    existing: Option<&Value>,
) -> Result<Value, Response> {
    let mut row = Map::new();
    for field in &model.fields {
        if field.name == key.name {
            row.insert(field.name.clone(), typed_key(key.kind, key_value));
            continue;
        }
        if let Some(value) = body.get(&field.name) {
            if !matches_kind(field.kind, value) {
                return Err(invalid_field(&field.name));
            }
            row.insert(field.name.clone(), value.clone());
        } else if let Some(old) = existing.and_then(|row| row.get(&field.name)) {
            row.insert(field.name.clone(), old.clone());
        } else if let Some(default) = &field.default {
            row.insert(field.name.clone(), default.clone());
        } else if existing.is_none() {
            return Err(invalid_body(Some(&field.name)));
        }
    }
    if key.implicit {
        row.insert(key.name.clone(), typed_key(key.kind, key_value));
    }
    Ok(Value::Object(row))
}

pub fn handle(
    ir: &FullstackIr,
    store: &mut dyn Store,
    req: &Request,
    max_body: usize,
) -> Result<Response, AdapterError> {
    // Serializing a well-formed serde_json::Value cannot fail.
    let body_len = serde_json::to_vec(&req.body)
        .expect("serde_json::Value is always serializable")
        .len();
    if body_len > max_body {
        return Ok(Response::new(413, json!({ "error": "payload_too_large" })));
    }

    let request_segments = segments(&req.path);
    let Some((api, params)) = find_route(ir, &request_segments) else {
        return Ok(not_found());
    };
    let verb = match (req.method.as_str(), params.is_empty()) {
        ("GET", true) => "list",
        ("GET", false) => "get",
        ("POST", _) => "create",
        ("PUT", _) => "update",
        ("DELETE", _) => "delete",
        _ => return Ok(not_found()),
    };
    if !api.handlers.iter().any(|handler| handler == verb) {
        return Ok(not_found());
    }
    let Some(model) = ir.model(&api.model) else {
        return Ok(not_found());
    };
    let key = key_info(model);
    let item_key = match params.get(&key.name) {
        Some(raw) => match parse_key(key.kind, raw) {
            Some(value) => Some(value),
            None => return Ok(not_found()),
        },
        None => None,
    };
    if matches!(verb, "get" | "update" | "delete") && item_key.is_none() {
        return Ok(not_found());
    }

    match verb {
        "list" => Ok(Response::new(200, Value::Array(store.list(&model.name)))),
        "create" => {
            let Some(body) = req.body.as_object() else {
                return Ok(invalid_body(None));
            };
            let key_value = match key.kind {
                FieldKind::Int => match body.get(&key.name) {
                    Some(value) if value.as_i64().is_some() => {
                        value.as_i64().unwrap_or(0).to_string()
                    }
                    Some(_) => return Ok(invalid_field(&key.name)),
                    None => next_id(&*store, &model.name, &key.name).to_string(),
                },
                _ => match body.get(&key.name) {
                    Some(Value::String(value)) => value.clone(),
                    Some(_) => return Ok(invalid_field(&key.name)),
                    None => return Ok(invalid_body(Some(&key.name))),
                },
            };
            if store.get(&model.name, &key_value).is_some() {
                return Ok(Response::new(409, json!({ "error": "conflict" })));
            }
            let row = match build_row(model, &key, &key_value, body, None) {
                Ok(row) => row,
                Err(response) => return Ok(response),
            };
            store.insert(&model.name, &key_value, row.clone())?;
            Ok(Response::new(201, row))
        }
        "get" => {
            let key_value = item_key.unwrap_or_default();
            match store.get(&model.name, &key_value) {
                Some(row) => Ok(Response::new(200, row)),
                None => Ok(not_found()),
            }
        }
        "update" => {
            let Some(body) = req.body.as_object() else {
                return Ok(invalid_body(None));
            };
            let key_value = item_key.unwrap_or_default();
            let Some(existing) = store.get(&model.name, &key_value) else {
                return Ok(not_found());
            };
            let row = match build_row(model, &key, &key_value, body, Some(&existing)) {
                Ok(row) => row,
                Err(response) => return Ok(response),
            };
            if !store.update(&model.name, &key_value, row.clone())? {
                return Ok(not_found());
            }
            Ok(Response::new(200, row))
        }
        "delete" => {
            let key_value = item_key.unwrap_or_default();
            if !store.remove(&model.name, &key_value) {
                return Ok(not_found());
            }
            Ok(Response::new(200, json!({ "deleted": true })))
        }
        _ => Ok(not_found()),
    }
}

fn sample_value(kind: FieldKind, updated: bool) -> Value {
    match kind {
        FieldKind::Int => json!(if updated { 2 } else { 1 }),
        FieldKind::String => json!(if updated { "sample-updated" } else { "sample" }),
        // The create sample is `false`, the update sample is `true`: the two
        // differ, so an update really changes the row.
        FieldKind::Bool => json!(updated),
    }
}

fn sample_body(model: &IrModel, key: &KeyInfo) -> Map<String, Value> {
    let mut body = Map::new();
    for field in &model.fields {
        if field.name == key.name {
            // Int keys are auto-assigned; string keys must be supplied.
            if key.kind != FieldKind::Int {
                body.insert(field.name.clone(), Value::from("sample-key"));
            }
            continue;
        }
        body.insert(field.name.clone(), sample_value(field.kind, false));
    }
    body
}

fn update_body(model: &IrModel, key: &KeyInfo) -> Map<String, Value> {
    let mut body = Map::new();
    if let Some(field) = model.fields.iter().find(|f| f.name != key.name) {
        body.insert(field.name.clone(), sample_value(field.kind, true));
    }
    body
}

fn item_path(
    ir: &FullstackIr,
    model_name: &str,
    base: &str,
    key: &KeyInfo,
    create_body: &Map<String, Value>,
) -> String {
    let key_value = match create_body.get(&key.name) {
        Some(Value::String(value)) => value.clone(),
        Some(value) if value.as_i64().is_some() => value.as_i64().unwrap_or(0).to_string(),
        _ => "1".to_owned(),
    };
    let Some(entry) = ir.api.iter().find(|api| {
        api.model == model_name
            && api.path.contains(':')
            && api.handlers.iter().any(|handler| handler == "get")
    }) else {
        return format!("{base}/1");
    };
    let mut path = String::new();
    for part in segments(&entry.path) {
        path.push('/');
        if part.starts_with(':') {
            path.push_str(&key_value);
        } else {
            path.push_str(part);
        }
    }
    path
}

/// Seven stateful checks derived from the IR: list empty, create, list one,
/// get, update, delete, 404. A plain 0.1.0 IR (no `models`/`api`) yields one
/// structural check so every runtime is still exercised.
pub fn sample_checks(ir: &FullstackIr) -> Vec<Check> {
    let structural = || {
        vec![Check {
            name: "not-found".into(),
            request: Request::new("GET", "/__missing__", Value::Null),
        }]
    };
    let Some(api) = ir.api.iter().find(|api| {
        !api.path.contains(':')
            && api.handlers.iter().any(|h| h == "list")
            && api.handlers.iter().any(|h| h == "create")
    }) else {
        return structural();
    };
    let Some(model) = ir.model(&api.model) else {
        return structural();
    };
    let key = key_info(model);
    let create_body = sample_body(model, &key);
    let update_body = update_body(model, &key);
    let base = api.path.trim_end_matches('/');
    let item = item_path(ir, &api.model, base, &key, &create_body);
    let get = |path: &str| Request::new("GET", path, Value::Null);
    vec![
        Check {
            name: "list-empty".into(),
            request: get(base),
        },
        Check {
            name: "create".into(),
            request: Request::new("POST", base, Value::Object(create_body)),
        },
        Check {
            name: "list-one".into(),
            request: get(base),
        },
        Check {
            name: "get".into(),
            request: get(&item),
        },
        Check {
            name: "update".into(),
            request: Request::new("PUT", &item, Value::Object(update_body)),
        },
        Check {
            name: "delete".into(),
            request: Request::new("DELETE", &item, Value::Null),
        },
        Check {
            name: "not-found".into(),
            request: get(&item),
        },
    ]
}

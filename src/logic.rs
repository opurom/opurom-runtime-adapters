//! The one business logic: CRUD over an in-memory table, with routes and row
//! shapes derived from the IR `api` and `models` arrays.
//!
//! `MapStore`, `FixedStore` and the Node.js host all implement this same
//! specification. The conformance suite compares canonical response bytes, so
//! any semantic drift between runtimes is a test failure.
//!
//! Responses follow framework rules: list -> array, create -> row,
//! get/update -> row or 404, delete -> `{"deleted":true}` or 404, unknown path
//! -> 404 JSON, known path with no route for the method -> 405 JSON with an
//! `Allow` list of the path's registered methods, `HEAD` served by the `GET`
//! route. Body size is capped by the runtime's `Capabilities::max_body`.

use crate::ir::{matches_kind, FieldKind, FullstackIr, IrApi, IrModel};
use crate::store::Store;
use crate::{AdapterError, LayerOptions, Request, Response};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};

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

/// The parameters bound by one api pattern, or `None` when the pattern does
/// not match. Parameters bind by name, not by position, so the key field's
/// parameter wins among several.
fn match_pattern(path: &str, request_segments: &[&str]) -> Option<BTreeMap<String, String>> {
    let parts = segments(path);
    if parts.len() != request_segments.len() {
        return None;
    }
    let mut params = BTreeMap::new();
    for (part, segment) in parts.iter().zip(request_segments.iter()) {
        if let Some(name) = part.strip_prefix(':') {
            params.insert(name.to_owned(), (*segment).to_owned());
        } else if *part != *segment {
            return None;
        }
    }
    Some(params)
}

/// The handler verb a request method resolves to, mirroring the framework
/// router: a `HEAD` request is served by the matching `GET` route (this
/// JSON-level ABI returns the handler's response; the wire layer omits the
/// body), and a method no route registers resolves to `None`.
fn verb_for(method: &str, has_params: bool) -> Option<&'static str> {
    match method {
        "GET" | "HEAD" => Some(if has_params { "get" } else { "list" }),
        "POST" => Some("create"),
        "PUT" => Some("update"),
        "DELETE" => Some("delete"),
        _ => None,
    }
}

/// The HTTP method a declared handler verb registers, mirroring the framework
/// router's route table. Unknown handler verbs register nothing.
fn handler_method(handler: &str) -> Option<&'static str> {
    match handler {
        "list" | "get" => Some("GET"),
        "create" => Some("POST"),
        "update" => Some("PUT"),
        "delete" => Some("DELETE"),
        _ => None,
    }
}

/// The `Allow` value for a matched path: every method the matching patterns
/// register, deduped, in the fixed order the framework router emits.
fn allow_value(methods: &BTreeSet<&'static str>) -> String {
    ["GET", "HEAD", "POST", "PUT", "DELETE"]
        .iter()
        .filter(|method| methods.contains(**method))
        .copied()
        .collect::<Vec<_>>()
        .join(", ")
}

/// Outcome of matching a request against the IR `api` array, mirroring the
/// framework router: every matching pattern is considered, a pattern that
/// matches but declares no handler for the resolved verb answers 405 with an
/// `Allow` list of the path's registered methods, and only a path no pattern
/// matches is 404.
enum Route<'a> {
    Matched {
        api: &'a IrApi,
        params: BTreeMap<String, String>,
        verb: &'static str,
    },
    MethodNotAllowed {
        allow: String,
    },
    NotFound,
}

fn resolve_route<'a>(ir: &'a FullstackIr, request_segments: &[&str], method: &str) -> Route<'a> {
    let mut path_matched = false;
    let mut registered: BTreeSet<&'static str> = BTreeSet::new();
    for api in &ir.api {
        let Some(params) = match_pattern(&api.path, request_segments) else {
            continue;
        };
        path_matched = true;
        for handler in &api.handlers {
            if let Some(registered_method) = handler_method(handler) {
                registered.insert(registered_method);
                // `HEAD` is served by the `GET` route, so it appears alongside it.
                if registered_method == "GET" {
                    registered.insert("HEAD");
                }
            }
        }
        let Some(verb) = verb_for(method, !params.is_empty()) else {
            continue;
        };
        if api.handlers.iter().any(|handler| handler == verb) {
            return Route::Matched { api, params, verb };
        }
    }
    if path_matched {
        Route::MethodNotAllowed {
            allow: allow_value(&registered),
        }
    } else {
        Route::NotFound
    }
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

fn method_not_allowed(allow: &str) -> Response {
    Response::new(405, json!({ "error": "method_not_allowed" })).with_header("allow", allow)
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

/// The media-type rule shared by every runtime: the base type before any `;`
/// parameters, compared case-insensitively. An absent header is JSON by
/// default; anything else is refused with 415 before route matching.
fn is_json_media(content_type: &str) -> bool {
    let base = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    base == "application/json"
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
    options: &LayerOptions,
) -> Result<Response, AdapterError> {
    // The preflight answer bypasses routing and auth entirely (mirroring the
    // framework), authentication gates everything else before routing, and
    // every answer then walks the response stages in the framework's fixed
    // order: CORS before ETag.
    let mut response = match cors_preflight(req, options) {
        Some(preflight) => preflight,
        None if !auth_exempt(req, options) && !auth_authorized(req, options) => {
            Response::new(401, json!({ "error": "unauthorized" }))
        }
        None => route_response(ir, store, req, max_body)?,
    };
    stage_cors(&mut response, options);
    Ok(stage_etag(response, req))
}

/// True when the request may skip the token check: authentication is
/// disabled, or the request is `GET /health`, mirroring the framework.
fn auth_exempt(req: &Request, options: &LayerOptions) -> bool {
    options.auth_token.is_none() || (req.method == "GET" && req.path == "/health")
}

/// True when the request carries a matching `Authorization: Bearer` value,
/// compared in time that depends only on the lengths (mirroring the
/// framework's digest-then-XOR gate).
fn auth_authorized(req: &Request, options: &LayerOptions) -> bool {
    let Some(token) = options.auth_token.as_deref() else {
        return true;
    };
    req.headers
        .get("authorization")
        .and_then(|raw| raw.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("Bearer"))
        .is_some_and(|(_, candidate)| constant_time_eq(candidate.as_bytes(), token.as_bytes()))
}

/// Compare two byte strings in time that depends only on their lengths,
/// mirroring the framework's `constant_time_eq`: both sides digest to a fixed
/// width and XOR-accumulate without an early exit, and the length mismatch is
/// folded into the verdict so unequal-length tokens stay rejected.
fn constant_time_eq(expected: &[u8], provided: &[u8]) -> bool {
    use sha2::{Digest, Sha256};
    let length_mismatch = usize::from(expected.len() ^ provided.len());
    let left = Sha256::digest(expected);
    let right = Sha256::digest(provided);
    let mut difference = length_mismatch;
    for (a, b) in left.iter().zip(right.iter()) {
        difference |= usize::from(a ^ b);
    }
    difference == 0
}

/// The fixed-allow-list CORS preflight answer, mirroring the framework: when
/// CORS is configured, any `OPTIONS` request gets a bodyless `204` with the
/// allow headers, whatever path it targets.
fn cors_preflight(req: &Request, options: &LayerOptions) -> Option<Response> {
    let origin = options.cors_origin.as_deref()?;
    if req.method != "OPTIONS" {
        return None;
    }
    Some(Response {
        status: 204,
        body: Value::Null,
        headers: [
            ("access-control-allow-origin", origin.to_owned()),
            (
                "access-control-allow-methods",
                "GET, POST, PUT, DELETE, OPTIONS".to_owned(),
            ),
            (
                "access-control-allow-headers",
                "Authorization, Content-Type".to_owned(),
            ),
            ("vary", "Origin".to_owned()),
        ]
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value))
        .collect(),
    })
}

/// Stage 2 of the response pipeline, mirroring the framework: a configured
/// origin is stamped onto every answer that does not carry one, and the
/// negotiation is recorded with `Vary: Origin`.
fn stage_cors(response: &mut Response, options: &LayerOptions) {
    let Some(origin) = options.cors_origin.as_deref() else {
        return;
    };
    if !response.headers.contains_key("access-control-allow-origin") {
        response
            .headers
            .insert("access-control-allow-origin".to_owned(), origin.to_owned());
    }
    add_vary(response, "Origin");
}

/// Merge one value into `Vary`, mirroring the framework's `add_vary`: an
/// existing header is appended to as a comma list without duplicates.
fn add_vary(response: &mut Response, value: &str) {
    match response.headers.get_mut("vary") {
        Some(current) => {
            if !current.split(',').any(|field| field.trim() == value) {
                current.push_str(", ");
                current.push_str(value);
            }
        }
        None => {
            response.headers.insert("vary".to_owned(), value.to_owned());
        }
    }
}

/// The routing and CRUD pipeline: validation, health, dispatch.
fn route_response(
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

    if let Some(content_type) = &req.content_type {
        if !is_json_media(content_type) {
            return Ok(Response::new(
                415,
                json!({ "error": "unsupported_media_type" }),
            ));
        }
    }

    // framework rules: `/health` is a registered GET route answering with the
    // app identity, so it is answered before model route matching and a method
    // it does not serve is a 405 like any other known pattern.
    if req.path == "/health" {
        if matches!(req.method.as_str(), "GET" | "HEAD") {
            return Ok(Response::new(
                200,
                json!({ "ok": true, "app": ir.app, "version": "0.1.0" }),
            ));
        }
        return Ok(method_not_allowed("GET, HEAD"));
    }

    let request_segments = segments(&req.path);
    let (api, params, verb) = match resolve_route(ir, &request_segments, &req.method) {
        Route::Matched { api, params, verb } => (api, params, verb),
        Route::MethodNotAllowed { allow } => return Ok(method_not_allowed(&allow)),
        Route::NotFound => return Ok(not_found()),
    };
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

/// Response bodies larger than this many bytes would be gzip-compressed by the
/// host transport, which a `304` must record with `Vary: Accept-Encoding`.
const GZIP_THRESHOLD_BYTES: usize = 1024;

/// Deterministic, non-cryptographic FNV-1a validator for the canonical JSON
/// body bytes, mirroring the framework's `etag_for`: a quoted 64-bit lowercase
/// hex spelling.
fn etag_for(body: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in body {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("\"{hash:016x}\"")
}

/// Weak/list comparison for `If-None-Match`, mirroring the framework: a
/// `*` matches everything, weak prefixes are stripped before comparing, and a
/// malformed validator simply never matches (staying a `200`).
fn if_none_match_matches(raw: &str, etag: &str) -> bool {
    raw.split(',').any(|entry| {
        let candidate = entry.trim();
        let strong = candidate
            .strip_prefix("W/")
            .or_else(|| candidate.strip_prefix("w/"))
            .unwrap_or(candidate)
            .trim();
        candidate == "*" || strong == etag
    })
}

/// True when the request offers gzip with a nonzero quality, mirroring the
/// framework's `accepts_gzip` over the lowercase header map.
fn accepts_gzip(headers: &std::collections::BTreeMap<String, String>) -> bool {
    headers.get("accept-encoding").is_some_and(|raw| {
        raw.split(',').any(|entry| {
            let mut fields = entry.trim().split(';');
            let coding = fields.next().unwrap_or_default().trim();
            if !coding.eq_ignore_ascii_case("gzip") {
                return false;
            }
            !fields.any(|parameter| {
                let Some((name, value)) = parameter.trim().split_once('=') else {
                    return false;
                };
                name.trim().eq_ignore_ascii_case("q") && value.trim().parse::<f32>() == Ok(0.0)
            })
        })
    })
}

/// The ETag stage, mirroring the framework: every successful non-empty answer
/// carries the body-hash validator, and a matching `If-None-Match` on GET
/// becomes a bodyless `304` that keeps only validator and caching headers.
/// Compression itself stays at the host transport layer, so answers are the
/// pre-compression representation and never carry `Content-Encoding`.
fn stage_etag(mut response: Response, req: &Request) -> Response {
    if !(200..300).contains(&response.status) || response.body.is_null() {
        return response;
    }
    let body = response.canonical_body_bytes();
    response.headers.insert("etag".to_owned(), etag_for(&body));
    let compresses = accepts_gzip(&req.headers) && body.len() > GZIP_THRESHOLD_BYTES;
    if req.method == "GET" {
        if let Some(raw) = req.headers.get("if-none-match") {
            if if_none_match_matches(raw, &response.headers["etag"]) {
                response.status = 304;
                response.body = Value::String(String::new());
                response.headers.retain(|name, _| {
                    name == "etag"
                        || name == "vary"
                        || name == "cache-control"
                        || name.starts_with("access-control-")
                });
                if compresses {
                    add_vary(&mut response, "Accept-Encoding");
                }
            }
        }
    }
    response
}

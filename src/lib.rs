//! Opurom backend runtime adapters.
//!
//! This crate proves one narrow claim: the same CRUD business logic, derived
//! from the `models`/`api` arrays of a full-stack IR, produces byte-identical
//! responses on four different local runtimes. It is a conformance harness,
//! not a deployment: no cloud, no serverless platform, no database, and no
//! Node.js service bundle is built here.
//!
//! The ABI in this file is a *backend runtime* ABI. It is deliberately distinct
//! from the 0.1.0 `PlatformAdapter` ABI in `opurom-examples/the framework implementation`,
//! which describes UI hosts (windowing, input, presentation).

pub mod adapters;
pub mod ir;
pub mod logic;
pub mod report;
pub mod store;

pub use adapters::{
    default_host_script, Edge, Embedded, NativeRust, NodeConfig, NodeJs, DEFAULT_TIMEOUT_MS,
};
pub use ir::FullstackIr;
pub use logic::{sample_checks, Check};
pub use report::{CheckFailure, RuntimeReport};

/// Request body cap, 1 MiB (framework rules).
pub const MAX_BODY_BYTES: usize = 1024 * 1024;

use serde_json::{json, Value};
use std::fmt;

/// Operator-level HTTP features, the adapter-side counterpart of the
/// framework's environment configuration. Everything defaults to off, exactly
/// like the framework defaults, and never travels in the IR: the IR is
/// application-defined, this is operator configuration.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LayerOptions {
    /// The fixed CORS origin emitted verbatim in
    /// `Access-Control-Allow-Origin`; the request's `Origin` header is never
    /// used to build a response header. `None` disables CORS, so `OPTIONS`
    /// stays an ordinary routed method.
    pub cors_origin: Option<String>,
}

impl LayerOptions {
    /// Enable CORS with one fixed origin, e.g. "*" or "https://app.example".
    pub fn with_cors_origin(mut self, origin: &str) -> Self {
        self.cors_origin = Some(origin.to_owned());
        self
    }
}

/// A JSON-shaped request. `body` is `null` for requests without a payload.
/// `content_type` mirrors the request Content-Type header; `None` means the
/// header was absent, which every runtime treats as JSON.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub body: Value,
    pub content_type: Option<String>,
    /// Request headers other than the Content-Type, keyed by lowercase name.
    /// `BTreeMap` keeps them sorted for the wire frame.
    pub headers: std::collections::BTreeMap<String, String>,
}

impl Request {
    pub fn new(method: &str, path: &str, body: Value) -> Self {
        Self {
            method: method.to_owned(),
            path: path.to_owned(),
            body,
            content_type: None,
            headers: std::collections::BTreeMap::new(),
        }
    }

    /// Set the request Content-Type header, e.g. "application/json".
    pub fn with_content_type(mut self, content_type: &str) -> Self {
        self.content_type = Some(content_type.to_owned());
        self
    }

    /// Set one request header, replacing any previous value for the name.
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.insert(name.to_lowercase(), value.to_owned());
        self
    }

    pub fn to_value(&self) -> Value {
        json!({
            "method": self.method,
            "path": self.path,
            "body": self.body,
            "contentType": self.content_type,
            "headers": self.headers,
        })
    }
}

/// A JSON-shaped response: an HTTP-style status, a JSON body and a header map.
///
/// Header names are lowercase and `BTreeMap` keeps them sorted, so the map has
/// one canonical spelling. [`Response::new`] stamps every response with
/// `content-type: application/json`.
#[derive(Clone, Debug, PartialEq)]
pub struct Response {
    pub status: u16,
    pub body: Value,
    pub headers: std::collections::BTreeMap<String, String>,
}

impl Response {
    pub fn new(status: u16, body: Value) -> Self {
        let mut headers = std::collections::BTreeMap::new();
        headers.insert("content-type".to_owned(), "application/json".to_owned());
        Self {
            status,
            body,
            headers,
        }
    }

    /// Attach one more header, preserving the lowercase canonical form. The
    /// first header that a response gains after the builder is [`Response::new`].
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.insert(name.to_owned(), value.to_owned());
        self
    }

    /// The canonical JSON bytes of the body alone: the byte form the ETag
    /// validator hashes and the compression threshold measures.
    fn canonical_body_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(&self.body).expect("serde_json::Value is always serializable")
    }

    /// Canonical wire form used for byte comparisons.
    ///
    /// `serde_json::Value` objects are backed by a `BTreeMap` with the default
    /// feature set, so object keys are sorted. The Node host emits the same
    /// ordering with its own canonical stringifier, making the bytes directly
    /// comparable across runtimes.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        // Serializing a well-formed serde_json::Value cannot fail.
        let body =
            serde_json::to_vec(&self.body).expect("serde_json::Value is always serializable");
        let headers = serde_json::to_vec(&self.headers).expect("a BTreeMap always serializes");
        let mut out = Vec::with_capacity(body.len() + headers.len() + 40);
        out.extend_from_slice(b"{\"body\":");
        out.extend_from_slice(&body);
        out.extend_from_slice(b",\"headers\":");
        out.extend_from_slice(&headers);
        out.extend_from_slice(format!(",\"status\":{}}}", self.status).as_bytes());
        out
    }
}

/// What a runtime is able to do. These describe the runtime, not what the
/// sample checks happen to use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    pub async_io: bool,
    pub threads: bool,
    pub fs: bool,
    pub network: bool,
    pub max_body: usize,
}

/// Verification status of a runtime row in the conformance report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Every local conformance check passed. Local conformance only.
    Verified,
    /// The adapter exists but its checks did not all pass on this run.
    Scaffold,
    /// No adapter, no checks, no deployment attempted.
    Planned,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Verified => "Verified",
            Status::Scaffold => "Scaffold",
            Status::Planned => "Planned",
        }
    }
}

/// Adapter failures are values, never panics across the boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdapterError {
    /// The runtime executable could not be started (missing node binary, ...).
    HostUnavailable(String),
    /// Pipe or process I/O failure.
    Io(String),
    /// The runtime spoke something other than the agreed line protocol.
    Protocol(String),
    /// A bounded wait for a runtime reply expired.
    Timeout { limit_ms: u64 },
    /// A fixed-capacity store refused an over-budget write.
    BudgetExceeded(String),
    /// The IR is not usable as a full-stack IR.
    Ir(String),
}

impl fmt::Display for AdapterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AdapterError::HostUnavailable(why) => write!(f, "runtime unavailable: {why}"),
            AdapterError::Io(why) => write!(f, "runtime I/O error: {why}"),
            AdapterError::Protocol(why) => write!(f, "runtime protocol error: {why}"),
            AdapterError::Timeout { limit_ms } => {
                write!(f, "runtime timed out after {limit_ms} ms")
            }
            AdapterError::BudgetExceeded(why) => write!(f, "fixed budget exceeded: {why}"),
            AdapterError::Ir(why) => write!(f, "invalid IR: {why}"),
        }
    }
}

impl std::error::Error for AdapterError {}

/// The one ABI every backend runtime implements.
pub trait RuntimeAdapter {
    fn name(&self) -> &str;
    fn capabilities(&self) -> Capabilities;
    fn invoke(&mut self, req: &Request) -> Result<Response, AdapterError>;
    fn shutdown(&mut self) -> Result<(), AdapterError>;
}

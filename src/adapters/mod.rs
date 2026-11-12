//! The four backend runtime adapters.
//!
//! - [`NativeRust`]: the reference, in-process.
//! - [`NodeJs`]: a real `node` subprocess speaking line-delimited JSON.
//! - [`Edge`]: a pure request -> response function, zero IO, no deployment.
//! - [`Embedded`]: single-threaded, fixed-capacity storage.

mod edge;
mod embedded;
mod native;
mod node;

pub use edge::Edge;
pub use embedded::Embedded;
pub use native::NativeRust;
pub use node::{default_host_script, NodeConfig, NodeJs, DEFAULT_TIMEOUT_MS};

use crate::AdapterError;
use serde_json::Value;

/// Parse a runtime response body and require it to be canonical JSON.
///
/// Canonical means exactly what `serde_json::to_vec` produces for the parsed
/// value: object keys sorted, no insignificant whitespace. The Node host emits
/// the same form, so this check turns "the runtime produced *a* JSON value"
/// into "the runtime produced *the* bytes this runtime family agrees on".
pub fn parse_canonical_body(text: &str) -> Result<Value, AdapterError> {
    let parsed: Value = serde_json::from_str(text)
        .map_err(|e| AdapterError::Protocol(format!("response body is not JSON: {e}")))?;
    // Serializing a well-formed serde_json::Value cannot fail.
    let canonical = serde_json::to_vec(&parsed).expect("serde_json::Value is always serializable");
    if canonical != text.as_bytes() {
        return Err(AdapterError::Protocol(
            "response body is not canonical JSON (keys not sorted or extra whitespace)".into(),
        ));
    }
    Ok(parsed)
}

//! Rust-side capability implementations shared by the in-process runtimes.
//!
//! Capability storage is a dedicated store, separate from the HTTP CRUD store,
//! so a capability key cannot collide with an IR model row. Values are stored
//! as scalar cells (or compact JSON text for composite values), which fits the
//! embedded fixed-value budget as well as the map-backed runtimes.

use crate::store::Store;
use crate::{capability_codes, AdapterError, CapabilityCall, CapabilityError, Value, APP_VERSION};
use serde_json::json;

const STORAGE_MODEL: &str = "__capability_storage";

fn invalid_argument(what: &str) -> CapabilityError {
    CapabilityError::new(
        capability_codes::INVALID_ARGUMENT,
        format!("storage call needs a string {what}"),
        Value::Null,
    )
}

fn method_not_implemented(method: &str, capability: &str) -> CapabilityError {
    CapabilityError::new(
        capability_codes::UNIMPLEMENTED,
        format!("method {method} is not part of {capability}"),
        Value::Null,
    )
}

fn store_error(error: AdapterError) -> CapabilityError {
    match error {
        AdapterError::BudgetExceeded(why) => {
            CapabilityError::new(capability_codes::BUDGET_EXCEEDED, why, Value::Null)
        }
        other => CapabilityError::new(capability_codes::ERROR, other.to_string(), Value::Null),
    }
}

fn argument<'a>(call: &'a CapabilityCall, name: &str) -> Result<&'a Value, CapabilityError> {
    call.arguments
        .as_object()
        .and_then(|object| object.get(name))
        .ok_or_else(|| {
            CapabilityError::new(
                capability_codes::INVALID_ARGUMENT,
                format!("missing argument {name}"),
                Value::Null,
            )
        })
}

fn key_argument(call: &CapabilityCall) -> Result<String, CapabilityError> {
    argument(call, "key")?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| invalid_argument("key"))
}

/// One field per cell keeps the row inside the embedded field budget.
fn stored_row(key: &str, value: &Value) -> Result<Value, CapabilityError> {
    let (kind, cell) = match value {
        Value::Null => ("null", Value::Null),
        Value::Bool(flag) => ("bool", Value::Bool(*flag)),
        Value::Number(number) if number.is_i64() => ("int", Value::Number(number.clone())),
        other => (
            "json",
            Value::String(serde_json::to_string(other).map_err(|error| {
                CapabilityError::new(capability_codes::ERROR, error.to_string(), Value::Null)
            })?),
        ),
    };
    Ok(json!({ "key": key, "kind": kind, "value": cell }))
}

fn loaded_value(row: &Value) -> Value {
    let Some(object) = row.as_object() else {
        return Value::Null;
    };
    match object.get("kind").and_then(Value::as_str) {
        Some("null") | Some("bool") | Some("int") => {
            object.get("value").cloned().unwrap_or(Value::Null)
        }
        _ => object
            .get("value")
            .and_then(Value::as_str)
            .and_then(|text| serde_json::from_str(text).ok())
            .unwrap_or(Value::Null),
    }
}

fn row_key(row: &Value) -> String {
    row.get("key")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

pub fn storage(store: &mut dyn Store, call: &CapabilityCall) -> Result<Value, CapabilityError> {
    match call.method.as_str() {
        "set" => {
            let key = key_argument(call)?;
            let value = argument(call, "value")?.clone();
            let row = stored_row(&key, &value)?;
            store
                .insert(STORAGE_MODEL, &key, row)
                .map_err(store_error)?;
            Ok(Value::Bool(true))
        }
        "get" => {
            let key = key_argument(call)?;
            Ok(store
                .get(STORAGE_MODEL, &key)
                .as_ref()
                .map(loaded_value)
                .unwrap_or(Value::Null))
        }
        "remove" => {
            let key = key_argument(call)?;
            Ok(Value::Bool(store.remove(STORAGE_MODEL, &key)))
        }
        "clear" => {
            store.clear();
            Ok(Value::Bool(true))
        }
        "keys" => Ok(Value::Array(
            store
                .list(STORAGE_MODEL)
                .iter()
                .map(|row| Value::String(row_key(row)))
                .collect(),
        )),
        "getAll" => {
            let mut all = serde_json::Map::new();
            for row in store.list(STORAGE_MODEL) {
                all.insert(row_key(&row), loaded_value(&row));
            }
            Ok(Value::Object(all))
        }
        other => Err(method_not_implemented(other, "storage")),
    }
}

/// Optional host capabilities: a runtime without a device answers using
/// Flutter's no-op ladder. Reads return null, queries return false, commands
/// succeed without effect. An unrecognized method stays `unimplemented`, so an
/// unknown operation never gets a fake success.
fn no_op(capability: &str, call: &CapabilityCall) -> Result<Value, CapabilityError> {
    let known = match capability {
        "clipboard" => matches!(call.method.as_str(), "getData" | "hasStrings" | "setData"),
        "haptics" => matches!(
            call.method.as_str(),
            "vibrate"
                | "lightImpact"
                | "mediumImpact"
                | "heavyImpact"
                | "selectionClick"
                | "successNotification"
                | "warningNotification"
                | "errorNotification"
        ),
        "system_sound" => call.method == "play",
        "system_ui" => matches!(
            call.method.as_str(),
            "setPreferredOrientations" | "setEnabledSystemUIMode"
        ),
        _ => false,
    };
    if !known {
        return Err(method_not_implemented(&call.method, capability));
    }
    if capability == "clipboard" && call.method == "setData" {
        let text = argument(call, "text")?;
        if !text.is_string() {
            return Err(invalid_argument("text"));
        }
    }
    match (capability, call.method.as_str()) {
        ("clipboard", "getData") => Ok(Value::Null),
        ("clipboard", "hasStrings") => Ok(Value::Bool(false)),
        _ => Ok(Value::Null),
    }
}

fn app_info(app: &str, call: &CapabilityCall) -> Result<Value, CapabilityError> {
    match call.method.as_str() {
        "buildName" => Ok(Value::String(APP_VERSION.to_owned())),
        "buildNumber" => Ok(Value::Null),
        "get" => Ok(json!({
            "name": app,
            "buildName": APP_VERSION,
            "buildNumber": Value::Null,
        })),
        other => Err(method_not_implemented(other, "app_info")),
    }
}

pub fn dispatch(
    app: &str,
    store: &mut dyn Store,
    call: &CapabilityCall,
) -> Result<Value, CapabilityError> {
    match call.capability.as_str() {
        "storage" => storage(store, call),
        "app_info" => app_info(app, call),
        "clipboard" | "haptics" | "system_sound" | "system_ui" => no_op(&call.capability, call),
        _ => Err(CapabilityError::unimplemented(&call.capability)),
    }
}

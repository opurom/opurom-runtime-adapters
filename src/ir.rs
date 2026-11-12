//! Parsing of the capability additive `models` / `api` arrays from an Opurom IR file.
//!
//! A plain 0.1.0 IR (the IR format) has neither array; it parses to an empty
//! model set and the conformance harness falls back to structural checks.

use crate::AdapterError;
use serde_json::{json, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldKind {
    Int,
    String,
    Bool,
}

impl FieldKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FieldKind::Int => "int",
            FieldKind::String => "string",
            FieldKind::Bool => "bool",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct IrField {
    pub name: String,
    pub kind: FieldKind,
    pub primary: bool,
    pub default: Option<Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct IrModel {
    pub name: String,
    pub fields: Vec<IrField>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct IrApi {
    pub path: String,
    pub model: String,
    pub handlers: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FullstackIr {
    pub app: String,
    pub models: Vec<IrModel>,
    pub api: Vec<IrApi>,
}

impl FullstackIr {
    pub fn parse(text: &str) -> Result<Self, AdapterError> {
        let value: Value = serde_json::from_str(text)
            .map_err(|e| AdapterError::Ir(format!("cannot parse IR JSON: {e}")))?;
        Self::from_value(&value)
    }

    pub fn from_value(value: &Value) -> Result<Self, AdapterError> {
        let object = value
            .as_object()
            .ok_or_else(|| AdapterError::Ir("top level must be a JSON object".into()))?;
        match object.get("ir").and_then(Value::as_str) {
            Some("0.1.0") => {}
            Some(other) => {
                return Err(AdapterError::Ir(format!(
                    "unsupported IR version {other:?}, expected \"0.1.0\""
                )))
            }
            None => {
                return Err(AdapterError::Ir(
                    "missing \"ir\": \"0.1.0\" field (the IR format)".into(),
                ))
            }
        }

        let app = object
            .get("app")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();

        let models = match object.get("models") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) => items
                .iter()
                .map(parse_model)
                .collect::<Result<Vec<_>, _>>()?,
            Some(_) => return Err(AdapterError::Ir("\"models\" must be an array".into())),
        };

        let api = match object.get("api") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) => {
                items.iter().map(parse_api).collect::<Result<Vec<_>, _>>()?
            }
            Some(_) => return Err(AdapterError::Ir("\"api\" must be an array".into())),
        };

        for entry in &api {
            if !models.iter().any(|m| m.name == entry.model) {
                return Err(AdapterError::Ir(format!(
                    "api {} references undeclared model {:?} (the framework implementation)",
                    entry.path, entry.model
                )));
            }
        }

        Ok(Self { app, models, api })
    }

    pub fn model(&self, name: &str) -> Option<&IrModel> {
        self.models.iter().find(|m| m.name == name)
    }

    /// The canonical subset handed to the Node host during the handshake.
    pub fn to_value(&self) -> Value {
        let models: Vec<Value> = self
            .models
            .iter()
            .map(|m| {
                let fields: Vec<Value> = m
                    .fields
                    .iter()
                    .map(|f| {
                        json!({
                            "name": f.name,
                            "type": f.kind.as_str(),
                            "primary": f.primary,
                            "default": f.default.clone().unwrap_or(Value::Null),
                        })
                    })
                    .collect();
                json!({ "name": m.name, "fields": fields })
            })
            .collect();
        let api: Vec<Value> = self
            .api
            .iter()
            .map(|a| json!({ "path": a.path, "model": a.model, "handlers": a.handlers }))
            .collect();
        json!({ "app": self.app, "models": models, "api": api })
    }
}

fn parse_model(value: &Value) -> Result<IrModel, AdapterError> {
    let object = value
        .as_object()
        .ok_or_else(|| AdapterError::Ir("each model must be a JSON object".into()))?;
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| AdapterError::Ir("model is missing a string \"name\"".into()))?
        .to_owned();
    let fields = match object.get("fields") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(parse_field)
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => {
            return Err(AdapterError::Ir(format!(
                "model {name}: \"fields\" must be an array"
            )))
        }
    };
    if fields.iter().filter(|f| f.primary).count() > 1 {
        return Err(AdapterError::Ir(format!(
            "model {name}: more than one primary field"
        )));
    }
    Ok(IrModel { name, fields })
}

fn parse_field(value: &Value) -> Result<IrField, AdapterError> {
    let object = value
        .as_object()
        .ok_or_else(|| AdapterError::Ir("each field must be a JSON object".into()))?;
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| AdapterError::Ir("field is missing a string \"name\"".into()))?
        .to_owned();
    let type_name = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| AdapterError::Ir(format!("field {name}: missing string \"type\"")))?;
    let kind = match type_name {
        "int" => FieldKind::Int,
        "string" => FieldKind::String,
        "bool" => FieldKind::Bool,
        _ => {
            return Err(AdapterError::Ir(format!(
                "field {name}: unsupported type {type_name:?}, expected int|string|bool"
            )))
        }
    };
    let primary = object
        .get("primary")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let default = match object.get("default") {
        Some(Value::Null) | None => None,
        Some(v) => {
            if !matches_kind(kind, v) {
                return Err(AdapterError::Ir(format!(
                    "field {name}: default does not match its declared type"
                )));
            }
            Some(v.clone())
        }
    };
    Ok(IrField {
        name,
        kind,
        primary,
        default,
    })
}

fn parse_api(value: &Value) -> Result<IrApi, AdapterError> {
    let object = value
        .as_object()
        .ok_or_else(|| AdapterError::Ir("each api entry must be a JSON object".into()))?;
    let path = object
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| AdapterError::Ir("api entry is missing a string \"path\"".into()))?
        .to_owned();
    let model = object
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| AdapterError::Ir(format!("api {path}: missing string \"model\"")))?
        .to_owned();
    let handlers = match object.get("handlers") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|h| {
                h.as_str().map(str::to_owned).ok_or_else(|| {
                    AdapterError::Ir(format!("api {path}: handler must be a string"))
                })
            })
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => {
            return Err(AdapterError::Ir(format!(
                "api {path}: \"handlers\" must be an array"
            )))
        }
    };
    Ok(IrApi {
        path,
        model,
        handlers,
    })
}

/// Type check shared by IR validation and request validation.
pub fn matches_kind(kind: FieldKind, value: &Value) -> bool {
    match kind {
        FieldKind::Int => value.as_i64().is_some(),
        FieldKind::String => value.is_string(),
        FieldKind::Bool => value.is_boolean(),
    }
}

//! The two storage backends used by the Rust-side adapters.
//!
//! `MapStore` is the ordinary in-process table. `FixedStore` is the embedded
//! profile: every byte of persistent state lives in inline, fixed-capacity
//! arrays, and any write that would exceed those arrays is refused instead of
//! growing the heap.

use crate::AdapterError;
use serde_json::{Map, Value};
use std::collections::BTreeMap;

pub type Row = Value;

pub trait Store {
    /// All rows of a model, in insertion order.
    fn list(&self, model: &str) -> Vec<Row>;
    fn get(&self, model: &str, key: &str) -> Option<Row>;
    fn insert(&mut self, model: &str, key: &str, row: Row) -> Result<(), AdapterError>;
    /// `false` means the key was not present.
    fn update(&mut self, model: &str, key: &str, row: Row) -> Result<bool, AdapterError>;
    fn remove(&mut self, model: &str, key: &str) -> bool;
    fn clear(&mut self);
}

pub struct MapStore {
    tables: BTreeMap<String, Vec<(String, Row)>>,
}

impl MapStore {
    pub fn new() -> Self {
        Self {
            tables: BTreeMap::new(),
        }
    }
}

impl Default for MapStore {
    fn default() -> Self {
        Self::new()
    }
}

impl Store for MapStore {
    fn list(&self, model: &str) -> Vec<Row> {
        self.tables
            .get(model)
            .map(|rows| rows.iter().map(|(_, row)| row.clone()).collect())
            .unwrap_or_default()
    }

    fn get(&self, model: &str, key: &str) -> Option<Row> {
        self.tables
            .get(model)?
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, row)| row.clone())
    }

    fn insert(&mut self, model: &str, key: &str, row: Row) -> Result<(), AdapterError> {
        self.tables
            .entry(model.to_owned())
            .or_default()
            .push((key.to_owned(), row));
        Ok(())
    }

    fn update(&mut self, model: &str, key: &str, row: Row) -> Result<bool, AdapterError> {
        let Some(rows) = self.tables.get_mut(model) else {
            return Ok(false);
        };
        let Some(slot) = rows.iter_mut().find(|(k, _)| k == key) else {
            return Ok(false);
        };
        slot.1 = row;
        Ok(true)
    }

    fn remove(&mut self, model: &str, key: &str) -> bool {
        let Some(rows) = self.tables.get_mut(model) else {
            return false;
        };
        let before = rows.len();
        rows.retain(|(k, _)| k != key);
        rows.len() != before
    }

    fn clear(&mut self) {
        self.tables.clear();
    }
}

pub const MAX_MODELS: usize = 8;
pub const MAX_ROWS: usize = 32;
pub const MAX_FIELDS: usize = 8;
pub const MAX_MODEL_BYTES: usize = 24;
pub const MAX_KEY_BYTES: usize = 32;
pub const MAX_NAME_BYTES: usize = 24;
pub const MAX_VALUE_BYTES: usize = 64;

#[derive(Clone, Copy)]
enum Cell {
    Int(i64),
    Str {
        len: usize,
        bytes: [u8; MAX_VALUE_BYTES],
    },
    Bool(bool),
    Null,
}

#[derive(Clone, Copy)]
struct Field {
    name_len: usize,
    name: [u8; MAX_NAME_BYTES],
    cell: Cell,
}

impl Field {
    fn empty() -> Self {
        Self {
            name_len: 0,
            name: [0; MAX_NAME_BYTES],
            cell: Cell::Null,
        }
    }
}

#[derive(Clone, Copy)]
struct Slot {
    used: bool,
    model_len: usize,
    model: [u8; MAX_MODEL_BYTES],
    key_len: usize,
    key: [u8; MAX_KEY_BYTES],
    field_count: usize,
    fields: [Field; MAX_FIELDS],
}

impl Slot {
    fn empty() -> Self {
        Self {
            used: false,
            model_len: 0,
            model: [0; MAX_MODEL_BYTES],
            key_len: 0,
            key: [0; MAX_KEY_BYTES],
            field_count: 0,
            fields: std::array::from_fn(|_| Field::empty()),
        }
    }
}

pub struct FixedStore {
    slots: [Slot; MAX_ROWS],
}

impl FixedStore {
    pub fn new() -> Self {
        Self {
            slots: std::array::from_fn(|_| Slot::empty()),
        }
    }

    /// Resident state size. Constant by construction, independent of row count.
    pub fn resident_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
    }

    pub fn row_count(&self) -> usize {
        self.slots.iter().filter(|s| s.used).count()
    }

    fn free_slot(&self) -> Option<usize> {
        self.slots.iter().position(|s| !s.used)
    }

    fn matching_rows(&self, model: &str) -> Vec<Value> {
        self.slots
            .iter()
            .filter(|s| s.used && str_from(&s.model, s.model_len) == model)
            .map(decode)
            .collect()
    }
}

impl Default for FixedStore {
    fn default() -> Self {
        Self::new()
    }
}

fn str_from(bytes: &[u8], len: usize) -> &str {
    std::str::from_utf8(&bytes[..len]).unwrap_or("")
}

fn encode(model: &str, key: &str, row: &Value) -> Result<Slot, AdapterError> {
    let object = row
        .as_object()
        .ok_or_else(|| AdapterError::BudgetExceeded("row is not a JSON object".into()))?;
    if object.len() > MAX_FIELDS {
        return Err(AdapterError::BudgetExceeded(format!(
            "row has {} fields, limit is {MAX_FIELDS}",
            object.len()
        )));
    }
    let mut slot = Slot::empty();
    slot.used = true;
    slot.model_len = copy_bytes(model.as_bytes(), &mut slot.model, "model name")?;
    slot.key_len = copy_bytes(key.as_bytes(), &mut slot.key, "row key")?;

    for (name, value) in object {
        let field = &mut slot.fields[slot.field_count];
        field.name_len = copy_bytes(name.as_bytes(), &mut field.name, "field name")?;
        field.cell = match value {
            Value::Number(n) if n.as_i64().is_some() => Cell::Int(n.as_i64().unwrap_or(0)),
            Value::String(s) => {
                let mut bytes = [0; MAX_VALUE_BYTES];
                let len = copy_bytes(s.as_bytes(), &mut bytes, "string value")?;
                Cell::Str { len, bytes }
            }
            Value::Bool(b) => Cell::Bool(*b),
            Value::Null => Cell::Null,
            _ => {
                return Err(AdapterError::BudgetExceeded(format!(
                    "field {name}: only int|string|bool|null fit the embedded budget"
                )))
            }
        };
        slot.field_count += 1;
    }
    Ok(slot)
}

fn copy_bytes(src: &[u8], dst: &mut [u8], what: &str) -> Result<usize, AdapterError> {
    if src.len() > dst.len() {
        return Err(AdapterError::BudgetExceeded(format!(
            "{what} is {} bytes, limit is {}",
            src.len(),
            dst.len()
        )));
    }
    dst[..src.len()].copy_from_slice(src);
    Ok(src.len())
}

fn decode(slot: &Slot) -> Value {
    let mut object = Map::new();
    for field in slot.fields.iter().take(slot.field_count) {
        let name = str_from(&field.name, field.name_len).to_owned();
        let value = match field.cell {
            Cell::Int(n) => Value::from(n),
            Cell::Str { len, bytes } => Value::from(str_from(&bytes, len).to_owned()),
            Cell::Bool(b) => Value::from(b),
            Cell::Null => Value::Null,
        };
        object.insert(name, value);
    }
    Value::Object(object)
}

impl Store for FixedStore {
    fn list(&self, model: &str) -> Vec<Row> {
        self.matching_rows(model)
    }

    fn get(&self, model: &str, key: &str) -> Option<Row> {
        self.slots
            .iter()
            .find(|s| {
                s.used
                    && str_from(&s.model, s.model_len) == model
                    && str_from(&s.key, s.key_len) == key
            })
            .map(decode)
    }

    fn insert(&mut self, model: &str, key: &str, row: Row) -> Result<(), AdapterError> {
        let slot = encode(model, key, &row)?;
        let index = self
            .free_slot()
            .ok_or_else(|| AdapterError::BudgetExceeded(format!("row capacity is {MAX_ROWS}")))?;
        self.slots[index] = slot;
        Ok(())
    }

    fn update(&mut self, model: &str, key: &str, row: Row) -> Result<bool, AdapterError> {
        let slot = encode(model, key, &row)?;
        let Some(index) = self.slots.iter().position(|s| {
            s.used && str_from(&s.model, s.model_len) == model && str_from(&s.key, s.key_len) == key
        }) else {
            return Ok(false);
        };
        self.slots[index] = slot;
        Ok(true)
    }

    fn remove(&mut self, model: &str, key: &str) -> bool {
        let Some(index) = self.slots.iter().position(|s| {
            s.used && str_from(&s.model, s.model_len) == model && str_from(&s.key, s.key_len) == key
        }) else {
            return false;
        };
        // Compact the used slots so a reused slot lands at the tail and list()
        // keeps the insertion order of the other stores.
        self.slots[index..].rotate_left(1);
        self.slots[MAX_ROWS - 1] = Slot::empty();
        true
    }

    fn clear(&mut self) {
        self.slots = std::array::from_fn(|_| Slot::empty());
    }
}

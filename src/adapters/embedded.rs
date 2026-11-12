//! The embedded adapter: single-threaded, fixed capacity, no heap growth.
//!
//! Storage is a fixed array of slots (`store::FixedStore`). Every byte of
//! persistent state lives inline: at most `MAX_MODELS` tables, `MAX_ROWS` rows
//! per table, `MAX_FIELDS` fields per row and `MAX_VALUE_BYTES` per value.
//! A write that would exceed those limits is refused with
//! `AdapterError::BudgetExceeded` instead of growing the heap.
//!
//! Transient allocations (building a response value) are proportional to the
//! response size, as in any JSON runtime. The *persistent* footprint is
//! constant: `Embedded::resident_bytes()` does not change with the row count.

use crate::ir::FullstackIr;
use crate::logic;
use crate::store::{FixedStore, Store};
use crate::{AdapterError, Capabilities, Request, Response, RuntimeAdapter};

pub struct Embedded {
    ir: FullstackIr,
    store: FixedStore,
    capabilities: Capabilities,
}

impl Embedded {
    pub fn new(ir: FullstackIr) -> Self {
        Self {
            ir,
            store: FixedStore::new(),
            capabilities: Capabilities {
                async_io: false,
                threads: false,
                fs: false,
                network: false,
                max_body: crate::MAX_BODY_BYTES,
            },
        }
    }

    /// Constant resident size of the persistent state.
    pub fn resident_bytes(&self) -> usize {
        self.store.resident_bytes()
    }

    pub fn row_count(&self) -> usize {
        self.store.row_count()
    }
}

impl RuntimeAdapter for Embedded {
    fn name(&self) -> &str {
        "embedded"
    }

    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    fn invoke(&mut self, req: &Request) -> Result<Response, AdapterError> {
        logic::handle(&self.ir, &mut self.store, req, self.capabilities.max_body)
    }

    fn shutdown(&mut self) -> Result<(), AdapterError> {
        self.store.clear();
        Ok(())
    }
}

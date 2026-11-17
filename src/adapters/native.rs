//! The native in-process reference adapter.

use crate::ir::FullstackIr;
use crate::logic;
use crate::store::{MapStore, Store};
use crate::{
    capabilities, AdapterError, Capabilities, CapabilityCall, CapabilityError, LayerOptions,
    Request, Response, RuntimeAdapter, Value,
};

/// The reference runtime: the business logic compiled into this process.
///
/// It performs no IO at all. The IR is handed over in memory, never read from
/// disk, and no socket is ever opened.
pub struct NativeRust {
    ir: FullstackIr,
    store: MapStore,
    cap_store: MapStore,
    capabilities: Capabilities,
    options: LayerOptions,
}

impl NativeRust {
    pub fn new(ir: FullstackIr) -> Self {
        Self {
            ir,
            store: MapStore::new(),
            cap_store: MapStore::new(),
            capabilities: Capabilities {
                async_io: false,
                threads: false,
                fs: false,
                network: false,
                max_body: crate::MAX_BODY_BYTES,
                supported: &[
                    "app_info",
                    "clipboard",
                    "haptics",
                    "storage",
                    "system_sound",
                    "system_ui",
                ],
            },
            options: LayerOptions::default(),
        }
    }

    /// Set the operator layer options, e.g. enabling CORS with one origin.
    pub fn with_options(mut self, options: LayerOptions) -> Self {
        self.options = options;
        self
    }
}

impl RuntimeAdapter for NativeRust {
    fn name(&self) -> &str {
        "native"
    }

    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    fn invoke(&mut self, req: &Request) -> Result<Response, AdapterError> {
        logic::handle(
            &self.ir,
            &mut self.store,
            req,
            self.capabilities.max_body,
            &self.options,
        )
    }

    fn invoke_capability(&mut self, call: &CapabilityCall) -> Result<Value, CapabilityError> {
        capabilities::dispatch(&self.ir.app, &mut self.cap_store, call)
    }

    fn shutdown(&mut self) -> Result<(), AdapterError> {
        self.store.clear();
        self.cap_store.clear();
        Ok(())
    }
}

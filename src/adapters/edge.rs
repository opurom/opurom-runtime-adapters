//! The edge adapter: a pure request -> response function.
//!
//! "Pure" here means the adapter performs no IO whatsoever: no filesystem, no
//! socket, no subprocess, no clock. It is a function from a request to a
//! response, the shape a serverless handler has.
//!
//! The session table lives in the adapter, exactly as module-scoped state lives
//! in a warm serverless isolate. Nothing is persisted and **nothing is
//! deployed**: there is no cloud provider, no bundle and no deployment step in
//! this repository. The `serverless-cloud` row of the conformance report stays
//! `Planned` for that reason.

use crate::ir::FullstackIr;
use crate::logic;
use crate::store::{MapStore, Store};
use crate::{
    capabilities, AdapterError, Capabilities, CapabilityCall, CapabilityError, LayerOptions,
    Request, Response, RuntimeAdapter, Value,
};

pub struct Edge {
    ir: FullstackIr,
    store: MapStore,
    cap_store: MapStore,
    capabilities: Capabilities,
    options: LayerOptions,
}

impl Edge {
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

impl RuntimeAdapter for Edge {
    fn name(&self) -> &str {
        "edge"
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

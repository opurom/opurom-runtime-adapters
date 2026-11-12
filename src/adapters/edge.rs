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
use crate::{AdapterError, Capabilities, Request, Response, RuntimeAdapter};

pub struct Edge {
    ir: FullstackIr,
    store: MapStore,
    capabilities: Capabilities,
}

impl Edge {
    pub fn new(ir: FullstackIr) -> Self {
        Self {
            ir,
            store: MapStore::new(),
            capabilities: Capabilities {
                async_io: false,
                threads: false,
                fs: false,
                network: false,
                max_body: crate::MAX_BODY_BYTES,
            },
        }
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
        logic::handle(&self.ir, &mut self.store, req, self.capabilities.max_body)
    }

    fn shutdown(&mut self) -> Result<(), AdapterError> {
        self.store.clear();
        Ok(())
    }
}

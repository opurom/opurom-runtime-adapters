//! The Node.js runtime adapter: a real `node` subprocess.
//!
//! The host script `adapters/node-host.mjs` implements the same CRUD business
//! logic as the Rust adapters and speaks line-delimited JSON over stdin/stdout:
//!
//! ```text
//! -> {"type":"handshake","ir":{...}}
//! <- {"type":"ready","ok":true}
//! -> {"type":"invoke","request":{"method":"GET","path":"/todos","body":null,"contentType":null}}
//! <- {"type":"response","status":200,"body":"[{\"id\":1}]"}
//! -> {"type":"shutdown"}
//! <- {"type":"bye"}
//! ```
//!
//! Every wait is bounded: a host that stops answering produces
//! `AdapterError::Timeout`, never a hang and never a panic.

use crate::ir::FullstackIr;
use crate::{AdapterError, Capabilities, Request, Response, RuntimeAdapter};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::Duration;

/// Default bound for one runtime round trip.
pub const DEFAULT_TIMEOUT_MS: u64 = 1000;

/// Grace period given to the host to answer the shutdown message.
const SHUTDOWN_GRACE_MS: u64 = 200;

/// Path of the bundled Node host script.
pub fn default_host_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("adapters")
        .join("node-host.mjs")
}

#[derive(Clone, Debug)]
pub struct NodeConfig {
    /// Executable used to run the host script.
    pub node_binary: String,
    /// Path of the host script.
    pub host_script: PathBuf,
    /// Bound for one round trip, in milliseconds.
    pub timeout_ms: u64,
}

impl NodeConfig {
    pub fn new(host_script: impl Into<PathBuf>) -> Self {
        Self {
            node_binary: "node".into(),
            host_script: host_script.into(),
            timeout_ms: DEFAULT_TIMEOUT_MS,
        }
    }

    pub fn with_binary(mut self, binary: impl Into<String>) -> Self {
        self.node_binary = binary.into();
        self
    }

    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = timeout_ms;
        self
    }
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self::new(default_host_script())
    }
}

pub struct NodeJs {
    config: NodeConfig,
    ir: FullstackIr,
    capabilities: Capabilities,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    lines: Option<Receiver<Option<String>>>,
    reader: Option<JoinHandle<()>>,
}

impl NodeJs {
    pub fn new(ir: FullstackIr) -> Self {
        Self::with_config(ir, NodeConfig::default())
    }

    pub fn with_config(ir: FullstackIr, config: NodeConfig) -> Self {
        Self {
            config,
            ir,
            capabilities: Capabilities {
                async_io: true,
                threads: true,
                fs: false,
                network: false,
                max_body: crate::MAX_BODY_BYTES,
            },
            child: None,
            stdin: None,
            lines: None,
            reader: None,
        }
    }

    pub fn config(&self) -> &NodeConfig {
        &self.config
    }

    fn send(&mut self, message: &Value) -> Result<(), AdapterError> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| AdapterError::Protocol("host is not connected".into()))?;
        let mut line = serde_json::to_string(message)
            .map_err(|e| AdapterError::Protocol(format!("cannot encode host message: {e}")))?;
        line.push('\n');
        stdin
            .write_all(line.as_bytes())
            .map_err(|e| AdapterError::Io(format!("cannot write to host stdin: {e}")))?;
        stdin
            .flush()
            .map_err(|e| AdapterError::Io(format!("cannot flush host stdin: {e}")))?;
        Ok(())
    }

    /// Read the next protocol message, skipping blank lines.
    fn read_message(&mut self, timeout_ms: u64) -> Result<Option<Value>, AdapterError> {
        let Some(lines) = self.lines.as_ref() else {
            return Err(AdapterError::Protocol("host is not connected".into()));
        };
        let deadline = Duration::from_millis(timeout_ms.max(1));
        loop {
            let received = lines.recv_timeout(deadline);
            match received {
                Ok(Some(line)) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let value: Value = serde_json::from_str(trimmed).map_err(|e| {
                        AdapterError::Protocol(format!("host sent a non-JSON line: {e}"))
                    })?;
                    return Ok(Some(value));
                }
                Ok(None) => return Ok(None),
                Err(RecvTimeoutError::Timeout) => {
                    return Err(AdapterError::Timeout {
                        limit_ms: timeout_ms.max(1),
                    })
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(AdapterError::Protocol("host closed the connection".into()))
                }
            }
        }
    }

    /// Start the host process and complete the handshake.
    fn connect(&mut self) -> Result<(), AdapterError> {
        if self.child.is_some() {
            return Ok(());
        }
        let mut command = Command::new(&self.config.node_binary);
        command
            .arg(&self.config.host_script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command.spawn().map_err(|e| {
            AdapterError::HostUnavailable(format!(
                "cannot start runtime {:?} with host script {}: {e}",
                self.config.node_binary,
                self.config.host_script.display()
            ))
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AdapterError::HostUnavailable("host stdin is not piped".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AdapterError::HostUnavailable("host stdout is not piped".into()))?;
        let (sender, receiver) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut buffered = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                match buffered.read_line(&mut line) {
                    Ok(0) | Err(_) => {
                        let _ = sender.send(None);
                        break;
                    }
                    Ok(_) => {
                        if sender.send(Some(line)).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        self.child = Some(child);
        self.stdin = Some(stdin);
        self.lines = Some(receiver);
        self.reader = Some(reader);

        self.send(&json!({ "type": "handshake", "ir": self.ir.to_value() }))?;
        match self.read_message(self.config.timeout_ms)? {
            Some(message) if message.get("type").and_then(Value::as_str) == Some("ready") => Ok(()),
            Some(message) => Err(AdapterError::Protocol(format!(
                "handshake refused by host: {message}"
            ))),
            None => Err(AdapterError::Protocol(
                "host closed the connection during the handshake".into(),
            )),
        }
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.stdin = None;
        self.lines = None;
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

impl RuntimeAdapter for NodeJs {
    fn name(&self) -> &str {
        "node"
    }

    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    fn invoke(&mut self, req: &Request) -> Result<Response, AdapterError> {
        self.connect()?;
        self.send(&json!({ "type": "invoke", "request": req.to_value() }))?;
        let message = self.read_message(self.config.timeout_ms)?.ok_or_else(|| {
            AdapterError::Protocol("host closed the connection before answering".into())
        })?;
        match message.get("type").and_then(Value::as_str) {
            Some("response") => {}
            Some("error") => {
                return Err(AdapterError::Protocol(format!(
                    "host reported an error: {message}"
                )))
            }
            other => {
                return Err(AdapterError::Protocol(format!(
                    "unexpected host message type {other:?}"
                )))
            }
        }
        let status = message
            .get("status")
            .and_then(Value::as_u64)
            .ok_or_else(|| AdapterError::Protocol("host response has no numeric status".into()))?;
        let body_text = message
            .get("body")
            .and_then(Value::as_str)
            .ok_or_else(|| AdapterError::Protocol("host response has no string body".into()))?;
        let body = super::parse_canonical_body(body_text)?;
        Ok(Response::new(status as u16, body))
    }

    fn shutdown(&mut self) -> Result<(), AdapterError> {
        if self.child.is_none() {
            return Ok(());
        }
        let _ = self.send(&json!({ "type": "shutdown" }));
        let _ = self.read_message(SHUTDOWN_GRACE_MS);
        self.stop();
        Ok(())
    }
}

impl Drop for NodeJs {
    fn drop(&mut self) {
        self.stop();
    }
}

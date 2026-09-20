//! Probe-side KV executor (Phase 4.5, ADR-0010): a `#[kv_storage]` executor
//! instance deployed alongside the actuator. Remote VirtualStorage backends
//! (e.g. a Krystallizer) send op frames over the existing outbound WS
//! connection; the executor prepends its declared prefix and executes
//! against a local engine. No dedicated listener, no second protocol — KV
//! rides the same connection as tool calls.
//!
//! Shape (ADR-0010 §2/§3): the executor is the NestStorage receiver role —
//! `apply(frame) -> Option<OpResponse>`, byte-level engine, zero semantic
//! parsing. The frame codec is okm-wire's `OpFrame`/`OpResponse` (counted
//! fields, no version header). Structural isolation: handles are
//! prefix-bound at construction; namespace escape is not expressible.

use anyhow::{Context, Result};
use okm_core::fjall_backend::FjallStore;
use okm_core::nest::NestStorage;
pub use okm_core::storage::SharedVirtualStorage;
use probe_config::KvExecutorDecl;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// One executor instance: a declared prefix bound to a local engine.
/// `apply` is the single intake surface — frame in, response out. The
/// arrival path (outbound WS / realm events / in-process) is the caller's
/// business, invisible here.
pub struct KvExecutor<S: SharedVirtualStorage> {
    nest: NestStorage<S>,
}

impl<S: SharedVirtualStorage + Send + 'static> KvExecutor<S> {
    /// Create an executor bound to `prefix` over the local engine. Every
    /// key from remote senders enters as `[prefix][sender bytes]` —
    /// structural isolation, no naming-filter layer.
    pub fn new(engine: S, prefix: &[u8]) -> (Self, okm_core::nest::VirtualHandle) {
        let (nest, handle) = NestStorage::new(engine, prefix);
        (Self { nest }, handle)
    }

    /// The single execution surface: one frame = one batch = one WAL commit.
    pub fn apply(&self, frame: &[u8]) -> Option<okm_wire::OpResponse> {
        self.nest.apply(frame)
    }
}

/// The intake the wire dispatch calls: one frame in, one response out.
/// Object-safe, so a registry holds instances of different engine types
/// behind one handle. `NestStorage::apply` takes `&self` but the host is
/// not `Sync` (it owns its mpsc intake end), so instances are held behind
/// a `Mutex` — which is also the frame-level single-writer discipline:
/// one executor = one engine = frames serialized in arrival order.
pub trait KvSink: Send + Sync {
    fn apply(&self, frame: &[u8]) -> Option<okm_wire::OpResponse>;
}

impl<S: SharedVirtualStorage + Send + 'static> KvSink for Mutex<KvExecutor<S>> {
    fn apply(&self, frame: &[u8]) -> Option<okm_wire::OpResponse> {
        self.lock().expect("kv executor lock").apply(frame)
    }
}

/// The declared executor instances, addressed by name (the wire's
/// `executor` slot). Built once at startup: a declaration that cannot be
/// opened is a startup failure, never a per-frame surprise.
pub struct KvRegistry {
    executors: HashMap<String, Arc<dyn KvSink>>,
}

/// Outcome of one dispatched frame in the probe's own terms. The transport
/// maps it onto the wire; the refusal reason is for the probe's log, not
/// for the wire (see `remote.rs`).
pub enum KvReply {
    /// The executor answered — the encoded `OpResponse` payload.
    Response(Vec<u8>),
    /// No executor answered: the frame named no declared instance, or the
    /// executor could not decode it. Either way the caller is answered
    /// rather than left waiting.
    Refused(&'static str),
}

impl KvRegistry {
    /// One local engine + one executor per declaration. The keyspace name
    /// is fixed (`kv`): an instance owns its whole directory, so the name
    /// is not an operator surface.
    pub fn open(decls: &[KvExecutorDecl]) -> Result<Self> {
        let mut executors: HashMap<String, Arc<dyn KvSink>> = HashMap::new();
        for decl in decls {
            let engine = FjallStore::open(std::path::Path::new(&decl.data_dir), "kv")
                .with_context(|| {
                    format!(
                        "kv executor `{}`: open engine at {}",
                        decl.name, decl.data_dir
                    )
                })?;
            // Declared prefix, big-endian 2 bytes (`#[ok_ns(N)]` encoding).
            let prefix = [(decl.ns >> 8) as u8, (decl.ns & 0xff) as u8];
            // okm hands back the sender endpoints for in-process callers;
            // the probe's intake is the wire, so they are dropped here.
            let (exec, _handle) = KvExecutor::new(engine.shared_handle(), &prefix);
            if executors
                .insert(decl.name.clone(), Arc::new(Mutex::new(exec)))
                .is_some()
            {
                anyhow::bail!("duplicate kv executor name `{}`", decl.name);
            }
        }
        Ok(Self { executors })
    }

    /// One frame in, one reply out. The registry knows names, not contents:
    /// which instance serves which prefix is the declaration's business,
    /// and what the frame means is the sender's.
    pub fn dispatch(&self, executor: &str, frame: &[u8]) -> KvReply {
        let Some(sink) = self.executors.get(executor) else {
            return KvReply::Refused("no such declared executor");
        };
        match sink.apply(frame) {
            Some(resp) => KvReply::Response(resp.encode()),
            None => KvReply::Refused("frame not decodable"),
        }
    }
}

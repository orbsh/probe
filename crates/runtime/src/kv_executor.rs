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

use anyhow::Result;
use okm_core::nest::NestStorage;
pub use okm_core::storage::SharedVirtualStorage;

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

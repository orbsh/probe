//! Probe runtime: executes the task contract inside a container-isolated node.
//!
//! Phase 0/1. Carriers live in `carrier` (steel / python / wasmtime,
//! feature-gated). The runtime binary itself arrives with Phase 3 (outbound
//! registration); `main` stays minimal until then.

pub mod carrier;

fn main() {
    // Intentionally minimal: the runtime binary arrives with Phase 3.
}

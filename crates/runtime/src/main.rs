//! Probe runtime: executes the task contract inside a container-isolated node.
//!
//! Phase 0 skeleton. Phase 1 mounts the language carriers (koto / rune /
//! steel / wasmer / wasmtime / wasmi, feature-gated, carried over from the
//! krystallizer vm crate).

pub fn main() {
    // Intentionally minimal: the runtime binary arrives with Phase 3
    // (outbound registration) and Phase 1 (runtime carriers).
}

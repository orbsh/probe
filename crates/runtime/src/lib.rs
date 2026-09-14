//! Probe runtime: executes the task contract inside a container-isolated node.
//!
//! Carriers live in `carrier` (steel / python / wasmtime / nushell,
//! feature-gated). Executable documentation for the carrier contract is in
//! `tests/carriers.rs`.

pub mod carrier;

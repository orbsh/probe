//! Wasm carrier (Wasmtime). Third-party untrusted code: hardware isolation.
//!
//! Convention: the module exports a WASI command `_start` (args via WASI) or
//! a typed `execute` function taking a JSON string pointer and returning a
//! result pointer. This skeleton wires the `execute` convention; WASI
//! surfacing arrives with host capability pass-through.

use super::{ExecRequest, ExecResult};
use serde_json::Value;
use wasmtime::{Engine, Linker, Module, Store};

pub fn execute(req: ExecRequest) -> ExecResult {
    let engine = Engine::default();
    let module = Module::new(&engine, req.source)?;

    let mut linker: Linker<State> = Linker::new(&engine);
    // Host imports are deliberately minimal: no fs, no network unless the
    // capability surface grants it (Phase 5 wires the grant into this linker).
    wasmtime_wasi_none(&mut linker)?;

    let mut store = Store::new(&engine, State { args: req.args.clone() });
    let instance = linker.instantiate(&mut store, &module)?;

    if let Some(entry) = req.entry {
        let func = instance.get_typed_func::<(i64,), (i64,)>(&mut store, entry)?;
        // Convention: entry receives the args JSON pointer; linear memory
        // marshalling is Phase 4 `link` payload territory (MB-scale bytes,
        // hash-verified before execution).
        let (_args_ptr, result_ptr) = (0i64, func.call(&mut store, (0,))?.0);
        let _ = result_ptr;
        anyhow::bail!("wasm entry-point marshalling lands with Phase 4 link payloads")
    }

    // No entry: run `_start` if exported (WASI command convention).
    if let Ok(start) = instance.get_typed_func::<(), ()>(&mut store, "_start") {
        start.call(&mut store, ())?;
        return Ok(Value::Null);
    }
    anyhow::bail!("wasm module exports neither declared entry nor _start")
}

struct State {
    #[allow(dead_code)]
    args: Value,
}

fn wasmtime_wasi_none(linker: &mut Linker<State>) -> anyhow::Result<()> {
    // No WASI imports granted yet. Modules requiring them fail at
    // instantiation with a clear error — capability grants decide later.
    let _ = linker;
    Ok(())
}

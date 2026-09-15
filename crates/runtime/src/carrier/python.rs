//! Python carrier (PyO3, in-process CPython, zero IPC).

use super::{ExecRequest, ExecResult, HostFn};
use std::ffi::CString;
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyBool, PyCFunction, PyDict, PyFloat, PyInt, PyList, PyModule, PyString};
use serde_json::Value;

/// Compile `source` and execute it at import time with the `@on` collector
/// already bound — decorators append `(event, key)` pairs to the returned
/// registry while the module body runs. The collector is language-shape
/// (a declaration registry); deriving schema semantics from it is the
/// caller's (aura's) job.
fn load_module<'py>(py: Python<'py>, source: &str) -> PyResult<(Bound<'py, PyModule>, Bound<'py, PyList>)> {
    let module = PyModule::new(py, "operation")?;
    let registry = PyList::empty(py);
    let reg: Py<PyList> = registry.clone().unbind();
    let on = PyCFunction::new_closure(py, None, None, move |args, kw| {
        let py = unsafe { Python::assume_gil_acquired() };
        let reg = reg.bind(py);
        // Two call shapes: `on("event", key=...)` (declaration — returns an
        // identity decorator) and `decorator(fn)` (application — returns
        // the function unchanged).
        let first = args.get_item(0)?;
        if first.extract::<String>().is_ok() {
            let mut key = String::new();
            if let Some(kw) = kw {
                if let Some(k) = kw.get_item("key")? {
                    key = k.extract()?;
                }
            }
            reg.append((first.extract::<String>()?, key))?;
            let identity = PyCFunction::new_closure(py, None, None, |a: &Bound<'_, pyo3::types::PyTuple>, _kw: Option<&Bound<'_, pyo3::types::PyDict>>| {
                Ok::<_, pyo3::PyErr>(a.get_item(0)?.unbind())
            })?;
            Ok::<_, pyo3::PyErr>(identity.into_any().unbind())
        } else {
            Ok::<_, pyo3::PyErr>(first.unbind())
        }
    })?;
    module.add("on", on)?;
    // Execute the module body with the module dict as globals so the
    // decorators resolve `on` (bound above, before the body runs).
    let globals = module.dict();
    globals.set_item("__name__", "operation")?;
    py.run(CString::new(source)?.as_c_str(), Some(&globals), None)?;
    // Assemble the merged `interface_schema` AFTER the body ran. Captures:
    // the registry (decorator declarations) + the script's explicit partial
    // schema, if it declared one. Field-wise merge: derived
    // receives/wildcard_receives + whatever the explicit half adds
    // (lifecycle, ...). One `interface_schema` name on the module either
    // way — aura's call path is uniform across languages.
    let reg_handle: Py<PyList> = registry.clone().unbind();
    let explicit_fn: Option<Py<PyAny>> = module
        .getattr("interface_schema")
        .ok()
        .map(|f| f.unbind());
    let merge = PyCFunction::new_closure(py, None, None, move |_args, _kw| {
        let py = unsafe { Python::assume_gil_acquired() };
        let reg = reg_handle.bind(py);
        // Derived half from the @on registry: [(event, key), ...].
        let mut receives = serde_json::Map::new();
        let mut wildcards: Vec<Value> = Vec::new();
        for item in reg.iter() {
            let pair: (String, String) = match item.extract() {
                Ok(p) => p,
                Err(e) => return Err(pyo3::exceptions::PyRuntimeError::new_err(format!("@on registry entry: {e}"))),
            };
            if pair.0.ends_with(".*") {
                wildcards.push(Value::String(pair.0));
                continue;
            }
            let mut entry = serde_json::Map::new();
            if !pair.1.is_empty() {
                entry.insert("key".into(), Value::String(pair.1));
            }
            receives.insert(pair.0, Value::Object(entry));
        }
        let derived = Value::Object([
            ("receives".to_string(), Value::Object(receives)),
            ("wildcard_receives".to_string(), Value::Array(wildcards)),
        ].into_iter().collect());
        // Explicit half: call the script's captured declaration, if any.
        let mut merged = derived.clone();
        if let Some(f) = &explicit_fn {
            let r = f.bind(py).call1((py.None(),))
                .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(format!("interface_schema: {e}")))?;
            let explicit_v = json_from_py(py, &r)
                .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))?;
            merged = merge_schema(derived.clone(), explicit_v);
        }
        json_to_py(py, &merged).map(|v| v.unbind())
    })?;
    module.add("interface_schema", merge)?;
    Ok((module, registry))
}

pub fn execute(req: ExecRequest) -> ExecResult {
    Python::with_gil(|py| -> ExecResult {
        let (module, _registry) = load_module(py, req.source)
            .map_err(|e| anyhow::anyhow!("python load: {e}"))?;

        // Expose each host function as a module-level callable taking one
        // JSON string and returning the parsed JSON value. PyCFunction over
        // a Rust closure keeps the marshal at the boundary — no code-gen.
        if let Some(bridge) = req.host {
            for (name, f) in &bridge.functions {
                let f: HostFn = f.clone();
                let call = PyCFunction::new_closure(py, None, None, move |args, _kw| {
                    let raw: String = args
                        .get_item(0)?
                        .extract()?;
                    // We already hold the GIL (running inside a Python call);
                    // unsafe-assert it to detach the result's lifetime from
                    // the closure args.
                    let py = unsafe { Python::assume_gil_acquired() };
                    // The script passes a JSON string; decode it so the host
                    // fn sees the structured value (marshal at boundary).
                    let decoded: Value = serde_json::from_str(&raw)
                        .map_err(|e| pyo3::exceptions::PyValueError::new_err(format!("host arg not JSON: {e}")))?;
                    let out = (f)(decoded)
                        .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))?;
                    Ok::<_, pyo3::PyErr>(json_to_py(py, &out)?.unbind())
                })?;
                module.add(name.as_str(), call)?;
            }
        }

        let args_py = json_to_py(py, req.args)?;

        let result = match req.entry {
            Some(entry) => {
                let func = module.getattr(entry).map_err(|e| anyhow::anyhow!("python entry {entry}: {e}"))?;
                func.call1((args_py,)).map_err(|e| anyhow::anyhow!("python call {entry}: {e}"))?
            }
            // No entry point: the script sets a module-level `result`
            // variable during import-time execution.
            None => module
                .getattr("result")
                .map_err(|e| anyhow::anyhow!("python: no entry and no `result` binding: {e}"))?,
        };

        json_from_py(py, &result)
    })
}

/// Upload-time introspection: load the module once (load discarded after),
/// call the module's `interface_schema` — one function, one call path for
/// aura. The module assembles it at import: the implicit (decorator-derived)
/// half merges with the script's explicit partial declaration (which may
/// add lifecycle etc.). Same call contract as steel/nushell/wasm.
pub fn introspect(source: &str) -> ExecResult {
    Python::with_gil(|py| -> ExecResult {
        let (module, _registry) = load_module(py, source)
            .map_err(|e| anyhow::anyhow!("python load: {e}"))?;
        let schema_fn = module
            .getattr("interface_schema")
            .map_err(|e| anyhow::anyhow!("python: no interface_schema (carrier assembles it) — {e}"))?;
        let result = schema_fn
            .call1((py.None(),))
            .map_err(|e| anyhow::anyhow!("python interface_schema: {e}"))?;
        json_from_py(py, &result)
    })
}

/// Field-wise schema merge: the explicit (script-written) declaration
/// fills keys the derived (decorator) half does not set; derived
/// receives/wildcard_receives win on their own keys — the decorators are
/// the authoritative source for receives, the explicit half contributes
/// everything else (lifecycle, ...).
fn merge_schema(derived: Value, explicit: Value) -> Value {
    let mut out = match (derived, explicit) {
        (Value::Object(mut d), Value::Object(e)) => {
            for (k, v) in e {
                d.entry(k).or_insert(v);
            }
            Value::Object(d)
        }
        (d, Value::Object(e)) if e.is_empty() => d,
        (d, e) => e.is_null().then_some(d).unwrap_or(e),
    };
    // Deep-merge the receives maps: explicit receives entries that the
    // decorators did not declare still contribute (the script may know a
    // receive the decorators cannot express).
    out
}

fn json_to_py<'py>(py: Python<'py>, v: &Value) -> PyResult<Bound<'py, PyAny>> {
    Ok(match v {
        Value::Null => py.None().into_bound(py),
        Value::Bool(b) => PyBool::new(py, *b).to_owned().into_any(),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                PyInt::new(py, i).into_any()
            } else {
                PyFloat::new(py, n.as_f64().unwrap_or(0.0)).into_any()
            }
        }
        Value::String(s) => PyString::new(py, s).into_any(),
        Value::Array(items) => {
            let list = PyList::empty(py);
            for item in items {
                list.append(json_to_py(py, item)?)?;
            }
            list.into_any()
        }
        Value::Object(map) => {
            let dict = PyDict::new(py);
            for (k, val) in map {
                dict.set_item(k, json_to_py(py, val)?)?;
            }
            dict.into_any()
        }
    })
}

fn json_from_py(py: Python<'_>, v: &Bound<'_, PyAny>) -> ExecResult {
    if v.is_none() {
        return Ok(Value::Null);
    }
    if let Ok(b) = v.extract::<bool>() {
        return Ok(Value::Bool(b));
    }
    if let Ok(i) = v.extract::<i64>() {
        return Ok(Value::Number(i.into()));
    }
    if let Ok(f) = v.extract::<f64>() {
        return Ok(serde_json::Number::from_f64(f).map(Value::Number).unwrap_or(Value::Null));
    }
    if let Ok(s) = v.extract::<String>() {
        return Ok(Value::String(s));
    }
    if let Ok(list) = v.downcast::<PyList>() {
        let mut out = Vec::with_capacity(list.len());
        for item in list.iter() {
            out.push(json_from_py(py, &item)?);
        }
        return Ok(Value::Array(out));
    }
    if let Ok(dict) = v.downcast::<PyDict>() {
        let mut map = serde_json::Map::new();
        for (k, val) in dict.iter() {
            let key: String = k.extract()?;
            map.insert(key, json_from_py(py, &val)?);
        }
        return Ok(Value::Object(map));
    }
    anyhow::bail!("unsupported python return type: {v:?}")
}

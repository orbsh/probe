//! Python carrier (PyO3, in-process CPython, zero IPC).

use super::{ExecRequest, ExecResult, HostFn};
use std::ffi::CString;
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyBool, PyCFunction, PyDict, PyFloat, PyInt, PyList, PyModule, PyString};
use serde_json::Value;

pub fn execute(req: ExecRequest) -> ExecResult {
    Python::with_gil(|py| -> ExecResult {
        let module = PyModule::from_code(py, &CString::new(req.source)?, c"operation.py", c"operation")
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

//! Steel (Scheme) carrier. Zero-ambiguity S-expressions; the natural fit for
//! AI-generated operation code.

use super::{ExecRequest, ExecResult, HostBridge};
use serde_json::Value;
use steel::SteelVal;
use steel::steel_vm::engine::Engine;
use steel::steel_vm::register_fn::RegisterFn;

pub fn execute(req: ExecRequest) -> ExecResult {
    let mut engine = Engine::new();
    register_host(&mut engine, req.host);
    register_on_collector(&mut engine);
    engine.run(req.source.to_owned()).map_err(|e| anyhow::anyhow!("steel run: {e:?}"))?;

    if let Some(entry) = req.entry {
        // Args arrive as a single JSON string; the operation parses what it needs.
        let args_json = SteelVal::StringV(serde_json::to_string(req.args)?.into());
        let val = engine
            .call_function_by_name_with_args(entry, vec![args_json])
            .map_err(|e| anyhow::anyhow!("steel call {entry}: {e:?}"))?;
        return steel_to_json(&val);
    }
    // No entry point: the source registers its result in `*result*`.
    match engine.extract_value("*result*") {
        Ok(val) => steel_to_json(&val),
        Err(_) => Ok(Value::Null),
    }
}

/// Expose each host function as a steel builtin taking one JSON-string arg
/// and returning a native steel value — scripts get hashes/numbers/bools,
/// not JSON strings to re-parse.
fn register_host(engine: &mut Engine, host: Option<&HostBridge>) {
    let Some(bridge) = host else { return };
    for (name, f) in &bridge.functions {
        let f = f.clone();
        // Steel's register_fn requires 'static — leak the name (small,
        // bounded by the declared host functions) and clone the HostFn Arc.
        let name: &'static str = Box::leak(name.clone().into_boxed_str());
        engine.register_fn(name, move |arg: SteelVal| -> Result<SteelVal, String> {
            // Scripts pass a JSON string (the common form for host calls);
            // other steel values marshal through steel_to_json.
            let json_in = match arg {
                SteelVal::StringV(s) => s.to_string(),
                other => match steel_to_json(&other) {
                    Ok(v) => serde_json::to_string(&v).map_err(|e| e.to_string())?,
                    Err(e) => return Err(e.to_string()),
                },
            };
            let decoded: Value = serde_json::from_str(&json_in)
                .map_err(|e| format!("host arg not JSON: {e}"))?;
            let out = (f)(decoded).map_err(|e| e.to_string())?;
            json_to_steel(&out)
        });
    }
}

/// Marshal a JSON value into native steel (hashes stay hashes).
fn json_to_steel(v: &Value) -> Result<SteelVal, String> {
    Ok(match v {
        Value::Null => SteelVal::Void,
        Value::Bool(b) => SteelVal::BoolV(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                SteelVal::IntV(i as isize)
            } else {
                SteelVal::NumV(n.as_f64().unwrap_or(0.0))
            }
        }
        Value::String(s) => SteelVal::StringV(s.clone().into()),
        Value::Array(items) => SteelVal::ListV(
            items.iter().map(json_to_steel).collect::<Result<Vec<_>, String>>()?.into(),
        ),
        Value::Object(map) => {
            let mut hm = im_rc::HashMap::new();
            for (k, val) in map {
                hm.insert(SteelVal::StringV(k.clone().into()), json_to_steel(val)?);
            }
            SteelVal::HashMapV(steel::rvals::SteelHashMap::from(steel::gc::Gc::new(hm)))
        }
    })
}

fn steel_to_json(v: &SteelVal) -> ExecResult {
    let out = match v {
        SteelVal::Void => Value::Null,
        SteelVal::BoolV(b) => Value::Bool(*b),
        SteelVal::NumV(n) => serde_json::Number::from_f64(*n)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        SteelVal::IntV(i) => Value::Number(((*i) as i64).into()),
        SteelVal::StringV(s) => Value::String(s.to_string()),
        SteelVal::ListV(l) => Value::Array(
            l.iter().map(steel_to_json).collect::<anyhow::Result<Vec<_>>>()?,
        ),
        SteelVal::VectorV(v) => Value::Array(
            v.iter().map(steel_to_json).collect::<anyhow::Result<Vec<_>>>()?,
        ),
        SteelVal::HashMapV(m) => {
            let mut map = serde_json::Map::new();
            for (k, val) in m.iter() {
                let key = steel_to_json(k)?;
                let Value::String(key) = key else {
                    anyhow::bail!("steel map key is not a string: {key}");
                };
                map.insert(key, steel_to_json(val)?);
            }
            Value::Object(map)
        }
        other => anyhow::bail!("unsupported steel return type: {other:?}"),
    };
    Ok(out)
}

/// Event-declaration collector, bound before the source runs: `(on "event"
/// "key" handler)` records (event, key) into a shared list and returns the
/// handler unchanged. Language-shape only; schema semantics are aura's.
fn register_on_collector(engine: &mut Engine) {
    let declarations: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>> =
        Default::default();
    let decl = declarations.clone();
    engine.register_fn("on", move |event: String, key: String, handler: SteelVal| -> Result<SteelVal, String> {
        decl.lock().unwrap().push((event, key));
        Ok(handler)
    });
    let _ = declarations;
}

/// Upload-time introspection: run the source once in a fresh VM (load
/// discarded after) with the `on` collector bound, call the script's
/// `interface_schema` if it wrote one, and merge field-wise with the
/// collector-derived half. One name, one call — uniform with python.
pub fn introspect(source: &str) -> ExecResult {
    let mut engine = Engine::new();
    let declarations: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>> =
        Default::default();
    let decl = declarations.clone();
    engine.register_fn("on", move |event: String, key: String, handler: SteelVal| -> Result<SteelVal, String> {
        decl.lock().unwrap().push((event, key));
        Ok(handler)
    });
    engine
        .run(source.to_owned())
        .map_err(|e| anyhow::anyhow!("steel load: {e:?}"))?;

    // Derived half from the collected declarations.
    let mut receives = serde_json::Map::new();
    let mut wildcards: Vec<Value> = Vec::new();
    for (event, key) in declarations.lock().unwrap().iter() {
        if event.ends_with(".*") {
            wildcards.push(Value::String(event.clone()));
            continue;
        }
        let mut entry = serde_json::Map::new();
        if !key.is_empty() {
            entry.insert("key".into(), Value::String(key.clone()));
        }
        receives.insert(event.clone(), Value::Object(entry));
    }
    let derived = Value::Object([
        ("receives".to_string(), Value::Object(receives)),
        ("wildcard_receives".to_string(), Value::Array(wildcards)),
    ].into_iter().collect());

    // Explicit half: the script's own interface_schema, if it wrote one.
    let explicit: Option<Value> = match engine
        .call_function_by_name_with_args("interface_schema", vec![SteelVal::StringV("{}".into())])
    {
        Ok(v) => Some(steel_to_json(&v)?),
        Err(_) => None,
    };

    Ok(match explicit {
        Some(e) => merge_schema(derived, e),
        None => derived,
    })
}

/// Field-wise merge: the derived (collector) half wins on its keys;
/// the explicit half contributes everything else (lifecycle, ...).
fn merge_schema(derived: Value, explicit: Value) -> Value {
    match (derived, explicit) {
        (Value::Object(mut d), Value::Object(e)) => {
            for (k, v) in e {
                d.entry(k).or_insert(v);
            }
            Value::Object(d)
        }
        (d, _) => d,
    }
}

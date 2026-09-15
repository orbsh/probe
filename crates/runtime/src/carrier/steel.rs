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

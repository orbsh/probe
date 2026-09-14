//! Steel (Scheme) carrier. Zero-ambiguity S-expressions; the natural fit for
//! AI-generated operation code.

use super::{ExecRequest, ExecResult};
use serde_json::Value;
use steel::SteelVal;
use steel::steel_vm::engine::Engine;

pub fn execute(req: ExecRequest) -> ExecResult {
    let mut engine = Engine::new();
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

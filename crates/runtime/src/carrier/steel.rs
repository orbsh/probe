//! Steel (Scheme) carrier. Zero-ambiguity S-expressions; the natural fit for
//! AI-generated operation code.

use super::{ExecResult, HostBridge};
use serde_json::Value;
use steel::SteelVal;
use steel::steel_vm::engine::Engine;
use steel::steel_vm::register_fn::RegisterFn;

/// Resident steel session: one VM per booth instance. The source runs ONCE
/// at load (with the `on` collector bound and handlers bound under event
/// names); every later call addresses a handler by name in the SAME VM —
/// definitions, `define`s and other top-level state persist across calls.
/// Drop = VM gone.
pub struct SteelSession {
    engine: Engine,
}

unsafe impl Send for SteelSession {}

impl SteelSession {
    pub fn new(host: Option<&HostBridge>) -> Self {
        let mut engine = Engine::new();
        register_host(&mut engine, host);
        register_on_collector(&mut engine);
        Self { engine }
    }
}

impl super::session::ResidentSession for SteelSession {
    fn load(&mut self, source: &str) -> anyhow::Result<()> {
        self.engine
            .run(source.to_owned())
            .map_err(|e| anyhow::anyhow!("steel load: {e:?}"))?;
        bind_event_handlers(&mut self.engine);
        Ok(())
    }

    fn call(&mut self, handler: &str, args: &Value) -> anyhow::Result<Value> {
        // Handlers are addressed by their EVENT name (the @on collector
        // binds functions under it). No fallback: a name that does not
        // resolve is an error, never a magic-entry redirect.
        let args_val = json_to_steel(args)
            .map_err(|e| anyhow::anyhow!("steel args marshal: {e}"))?;
        let val = self
            .engine
            .call_function_by_name_with_args(handler, vec![args_val])
            .map_err(|e| anyhow::anyhow!("steel call {handler}: {e:?}"))?;
        steel_to_json(&val)
    }

    fn as_any(&mut self) -> &mut dyn std::any::Any {
        self
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
            // Arg marshal: a steel string is tried as JSON first (object
            // form `{"field": ...}`) and falls back to a bare string value
            // (the bare-field-name form `"count"`). Other steel values
            // marshal through steel_to_json.
            let decoded: Value = match &arg {
                SteelVal::StringV(s) => {
                    let raw = s.to_string();
                    serde_json::from_str(&raw)
                        .unwrap_or(Value::String(raw))
                }
                other => steel_to_json(other).map_err(|e| e.to_string())?,
            };
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

thread_local! {
    /// (event, key, handler) declarations collected by `on` during the
    /// body run. Thread-local because SteelVal carries Rc internals (not
    /// Send) and register_fn closures must be Send — the whole execution
    /// (VM + builtins) stays on one thread, so thread-local is exact.
    static DECLARATIONS: std::cell::RefCell<Vec<(String, String, SteelVal)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Event-declaration collector, bound before the source runs: `(on "event"
/// "key" handler)` records the declaration and returns the handler
/// unchanged. Language-shape only; schema semantics are aura's.
fn register_on_collector(engine: &mut Engine) {
    DECLARATIONS.with(|d| d.borrow_mut().clear());
    engine.register_fn("on", |event: String, key: String, handler: SteelVal| -> Result<SteelVal, String> {
        DECLARATIONS.with(|d| d.borrow_mut().push((event, key, handler.clone())));
        Ok(handler)
    });
}

/// Bind collected handlers under their event names (register_value —
/// dotted names are fine: env lookup is by string, not parser ident).
fn bind_event_handlers(engine: &mut Engine) {
    DECLARATIONS.with(|d| {
        for (event, _key, handler) in d.borrow().iter() {
            engine.register_value(event, handler.clone());
        }
    });
}

/// Ctx-fn stubs for the introspection throwaway engine ONLY (never the
/// resident session — there the real host bridge functions must win):
/// scripts reference `ctx_*` host fns at load time (steel resolves free
/// identifiers when the define is compiled), but the host bridge is not
/// available during introspection. The stubs exist so the script loads;
/// they error if ever invoked (introspection never calls a handler).
fn register_ctx_stubs(engine: &mut Engine) {
    const CTX_STUBS: &[&str] = &[
        "ctx_invoke", "ctx_store_emit", "ctx_interface_schema",
        "ctx_timer_register", "ctx_timer_cancel",
    ];
    for name in CTX_STUBS {
        let name: &'static str = Box::leak((*name).to_string().into_boxed_str());
        engine.register_fn(name, move |_arg: SteelVal| -> Result<SteelVal, String> {
            Err("ctx function called during introspection (no host bridge)".to_string())
        });
    }
}

fn collected() -> Vec<(String, String)> {
    DECLARATIONS.with(|d| {
        d.borrow()
            .iter()
            .map(|(e, k, _)| (e.clone(), k.clone()))
            .collect()
    })
}

/// Upload-time introspection: run the source once in a fresh VM (load
/// discarded after) with the `on` collector bound, call the script's
/// `interface_schema` if it wrote one, and merge field-wise with the
/// collector-derived half. One name, one call — uniform with python.
pub fn introspect(source: &str) -> ExecResult {
    let mut engine = Engine::new();
    register_on_collector(&mut engine);
    register_ctx_stubs(&mut engine);
    engine
        .run(source.to_owned())
        .map_err(|e| anyhow::anyhow!("steel load: {e:?}"))?;
    let declarations = collected();

    // Derived half from the collected declarations.
    let mut receives = serde_json::Map::new();
    let mut wildcards: Vec<Value> = Vec::new();
    for (event, key) in declarations.iter() {
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
/// `receives` is deep-merged key-by-key — a top-level `or_insert` would
/// let the collector's (empty) map shadow an explicit hand-written
/// receives block, silently dropping it (python's module-side merge
/// composes both sources into one map; steel merges here).
fn merge_schema(derived: Value, explicit: Value) -> Value {
    fn deep_merge_receives(d: &mut serde_json::Map<String, Value>, e: &serde_json::Map<String, Value>) {
        for k in ["receives", "wildcard_receives"] {
            match (d.get_mut(k), e.get(k)) {
                // receives: a map — explicit entries contribute keys the
                // collector did not declare (collision: collector wins).
                (Some(Value::Object(dm)), Some(Value::Object(em))) => {
                    for (ek, ev) in em {
                        dm.entry(ek.clone()).or_insert_with(|| ev.clone());
                    }
                }
                // wildcard_receives: an array of patterns — set union.
                (Some(Value::Array(da)), Some(Value::Array(ea))) => {
                    for pat in ea {
                        if !da.contains(pat) {
                            da.push(pat.clone());
                        }
                    }
                }
                // Derived side missing or differently shaped: take explicit.
                (None | Some(Value::Null), Some(v)) => {
                    d.insert(k.to_string(), v.clone());
                }
                _ => {}
            }
        }
    }
    match (derived, explicit) {
        (Value::Object(mut d), Value::Object(e)) => {
            for (k, v) in &e {
                if k == "receives" || k == "wildcard_receives" {
                    continue; // deep-merged below
                }
                d.entry(k.clone()).or_insert_with(|| v.clone());
            }
            deep_merge_receives(&mut d, &e);
            Value::Object(d)
        }
        (d, _) => d,
    }
}

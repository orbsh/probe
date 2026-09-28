//! BGI carrier fixture (ADR-0035): a native bin speaking the line
//! protocol — the contract a compiled Rust booth implements against
//! (or links a shim to; the loop lives on the child side in bgi).
//! Loop over stdin lines until EOF; one request in, one result out.
//! `ctx_round_trip` answers a plain call with a child→parent ctx
//! round trip (host frame → host_reply → result), proving the ctx seam
//! crosses the process boundary. Iterate handlers return envelopes per
//! ADR-0034 with the guard counter in this process's memory (the
//! residency IS the booth state — idle_ttl eviction kills it, matching
//! generator-mode semantics without a generator language).
//!
//! No aura dependencies: serde_json only. The protocol is the ABI.

use serde_json::Value;
use std::collections::HashMap;
use std::io::{BufRead, Write};

struct Guard {
    pulled: u64,
    total: u64,
}

fn main() {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    let mut lines = stdin.lock().lines();
    let mut streams: HashMap<String, Guard> = HashMap::new();
    while let Some(Ok(line)) = lines.next() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => {
                emit(&mut out, &serde_json::json!({"result": {"error": "malformed frame"}}));
                continue;
            }
        };
        let id = req.get("id").cloned().unwrap_or(Value::Null);
        let kind = req.get("kind").and_then(|v| v.as_str()).unwrap_or("call").to_string();
        let event = req.get("event").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let args = req.get("args").cloned().unwrap_or(Value::Null);
        let stream_id = req.get("stream_id").and_then(|v| v.as_str()).unwrap_or("").to_string();

        match (kind.as_str(), event.as_str()) {
            // Plain handler: echo the args — the invoke shape.
            ("call", "echo") => reply(&mut out, id, serde_json::json!({"echoed": args})),
            // Upload-time introspection (ADR-0035: the child declares
            // its receives in its own code — the frame protocol carries
            // the same JSON schema shape every carrier's interface_schema
            // returns). The storage block is the hand-written literal,
            // the same shape steel/nushell declare: `counters` keyed by
            // `id`, one `count` field (ADR-0026 §4).
            ("call", "interface_schema") => reply(
                &mut out,
                id,
                serde_json::json!({
                    "receives": { "echo": {}, "ctx_round_trip": {}, "store_round_trip": {} },
                    "wildcard_receives": [],
                    "storage": { "collections": { "counters": { "schema": {
                        "key_len": 8,
                        "key_fields": [{"name": "id", "ty": "U64", "width": 8, "offset": 0, "tag": 0}],
                        "layout_version": 1, "hot_width": 8, "payload_header_len": 3,
                        "hot_fields": [{"name": "count", "ty": "U64", "width": 8, "offset": 0, "tag": 0}],
                        "cold_fields": [],
                        "slots": {"primary": 0, "dynamic": 1, "dict_id": 2, "dict_name": 3,
                                  "declared_index_base": 4096, "declared_reduce_base": 8192,
                                  "junction_base": 12288}
                    }}}}
                }),
            ),
            // Plain handler: ONE host round trip, then forward the answer.
            ("call", "ctx_round_trip") => {
                emit(
                    &mut out,
                    &serde_json::json!({"host": {"op": "ctx_invoke", "args": args}}),
                );
                // The answer arrives as its own line (the parent writes
                // it between request lines — the child blocks here, the
                // same synchronous rule as every other carrier bridge).
                let answer = loop {
                    match lines.next() {
                        Some(Ok(inner)) => {
                            if let Ok(v) = serde_json::from_str::<Value>(inner.trim()) {
                                if let Some(hr) = v.get("host_reply") {
                                    break hr.get("ok").cloned().unwrap_or(Value::Null);
                                }
                            }
                        }
                        _ => break Value::String("no host_reply".into()),
                    }
                };
                reply(&mut out, id, answer);
            }
            // ctx_store_emit over the bgi seam (Phase 4.14 gate 1): the
            // child forwards its `args` as two okm instructions — put,
            // then read-back — and answers with what the host's store
            // read returns. Pure transport: the child never parses the
            // instruction, exactly the rule the wire enforces.
            ("call", "store_round_trip") => {
                let mut host_call = |v: &Value| {
                    emit(&mut out, &serde_json::json!({"host": {"op": "ctx_store_emit", "args": v}}));
                    loop {
                        match lines.next() {
                            Some(Ok(inner)) => {
                                if let Ok(m) = serde_json::from_str::<Value>(inner.trim()) {
                                    if let Some(hr) = m.get("host_reply") {
                                        return hr.get("ok").cloned().unwrap_or(Value::Null);
                                    }
                                }
                            }
                            _ => return Value::String("no host_reply".into()),
                        }
                    }
                };
                let put = args.get("put").cloned().unwrap_or(Value::Null);
                let get = args.get("get").cloned().unwrap_or(Value::Null);
                host_call(&put);
                let read_back = host_call(&get);
                reply(&mut out, id, serde_json::json!({"read_back": read_back}));
            }
            // iterate_start: register the guard; the first round carries
            // the first item (ADR-0034: Start is also the first pull).
            ("iterate_start", _) => {
                let total = args.get("total").and_then(|v| v.as_u64()).unwrap_or(0);
                if total == 0 {
                    reply(&mut out, id, serde_json::json!({"done": true}));
                } else {
                    streams.insert(stream_id.clone(), Guard { pulled: 1, total });
                    reply(&mut out, id, serde_json::json!({"item": "i0", "done": false}));
                }
            }
            // iterate_next: advance the guard; exhaustion WRITES done.
            ("iterate_next", _) => match streams.get_mut(&stream_id) {
                None => reply(
                    &mut out,
                    id,
                    serde_json::json!({"error": format!("unknown stream {stream_id}")}),
                ),
                Some(g) => {
                    g.pulled += 1;
                    if g.pulled > g.total {
                        streams.remove(&stream_id);
                        reply(&mut out, id, serde_json::json!({"done": true}));
                    } else {
                        // pulled counts rounds INCLUDING start's i0 —
                        // item index is the 0-based round.
                        let n = g.pulled - 1;
                        reply(&mut out, id, serde_json::json!({"item": format!("i{n}"), "done": false}));
                    }
                }
            },
            ("iterate_dispose", _) => {
                streams.remove(&stream_id);
                reply(&mut out, id, Value::Null);
            }
            _ => reply(&mut out, id, serde_json::json!({"error": "no such handler"})),
        }
    }
}

fn emit(w: &mut impl Write, v: &Value) {
    writeln!(w, "{v}").unwrap();
    w.flush().unwrap();
}

fn reply(w: &mut impl Write, id: Value, result: Value) {
    emit(w, &serde_json::json!({"id": id, "result": result}));
}

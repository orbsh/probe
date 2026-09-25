//! The second bridge turn (regression lock): after a call whose handler
//! used the ctx bridge, the result file appears while the nu REPL is still
//! redrawing its prompt. Returning from `call` at that instant makes the
//! NEXT call's `source` line land in a half-drawn prompt and never execute
//! — the second bridge turn hangs until timeout. `call` must hand the
//! session back only after the PTY stream goes quiet (pump_quiet).

use probe_runtime::carrier::session::Sessions;
use probe_runtime::carrier::{HostBridge, HostFn};
use serde_json::Value;
use std::sync::Arc;

// The nu handler calls ctx-invoke (the dash form the bridge generates).
#[test]
fn nushell_ctx_invoke_over_bridge() {
    let mut bridge = HostBridge::default();
    bridge.functions.insert(
        "ctx_invoke".into(),
        Arc::new(|arg: Value| {
            // Echo double: the fixture asserts the round trip, not the
            // realm dispatch (that side is locked by the engine tests).
            let n = arg.get("n").and_then(|v| v.as_u64()).unwrap_or(0);
            Ok(serde_json::json!({ "doubled": n * 2 }))
        }) as HostFn,
    );
    let src = r#"
export def --env ask [args] {
    let r = (ctx-invoke { n: $args.n })
    { doubled: $r.doubled }
}
"#;
    let sessions = Sessions::new();
    let out = sessions
        .with_session(
            "nu-bridge",
            "nushell",
            src,
            Some(&bridge),
            &probe_runtime::sandbox::SandboxPolicy::None,
            |s: &mut dyn probe_runtime::carrier::session::ResidentSession| {
                s.call("ask", &serde_json::json!({ "n": 21 }))
            },
        )
        .expect("bridge round trip");
    assert_eq!(out["doubled"], 42, "nu handler got the host fn reply: {out}");
}

#[test]
fn two_bridge_turns_on_one_resident_session() {
    let mut bridge = HostBridge::default();
    bridge.functions.insert(
        "ctx_store_emit".into(),
        // Real host fns answer from realm state mid-call; the fixture
        // keeps that shape (a value per turn) without the realm.
        Arc::new(|arg: Value| Ok(serde_json::json!({ "echo": arg }))) as HostFn,
    );
    let src = r#"
export def put-it [args] {
    ctx-store-emit { op: "put" }
    { ok: true }
}
export def get-it [args] {
    ctx-store-emit { op: "get" }
}
"#;
    let sessions = Sessions::new();
    let out1 = sessions
        .with_session(
            "twocall",
            "nushell",
            src,
            Some(&bridge),
            &probe_runtime::sandbox::SandboxPolicy::None,
            |s| s.call("put-it", &serde_json::json!({})),
        )
        .expect("first bridge turn");
    assert_eq!(out1["ok"], true);
    let out2 = sessions
        .with_session(
            "twocall",
            "nushell",
            src,
            Some(&bridge),
            &probe_runtime::sandbox::SandboxPolicy::None,
            |s| s.call("get-it", &serde_json::json!({})),
        )
        .expect("second bridge turn (regression: hung pre pump_quiet)");
    assert_eq!(out2["echo"]["op"], "get", "handler's reply rode the bridge: {out2}");
}

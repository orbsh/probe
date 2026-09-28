//! Exec carrier tests (ADR-0035 mode A): the line protocol over real
//! pipes against the `exec_loop` fixture binary — call, ctx round trip
//! over the host bridge, the iterate verbs, and the residency rules
//! (the child IS the residency: eviction closes stdin + reaps; a dead
//! child is swept from the registry so the next call cold-starts).
//!
//! The fixture is built by the workspace (`cargo build -p actor-guest
//! --example exec_loop`); a missing binary fails the test loudly — a
//! stale build is a recipe error, never a skip.

use probe_runtime::carrier::session::StreamOp;
use probe_runtime::carrier::HostBridge;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

fn exec_bin() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug/examples/exec_loop");
    assert!(
        path.exists(),
        "exec_loop missing — build it: cargo build -p actor-guest --example exec_loop"
    );
    path.to_str().unwrap().to_string()
}

fn sessions() -> probe_runtime::carrier::session::Sessions {
    probe_runtime::carrier::session::Sessions::new()
}

/// Plain call through the resident session: request frame out, result
/// frame back — the invoke shape across the process boundary.
#[test]
fn exec_call_round_trip() {
    let s = sessions();
    let argv = exec_bin();
    let out = s
        .with_session(
            "box/k1",
            "exec",
            &argv,
            None,
            &probe_runtime::sandbox::SandboxPolicy::None,
            |sess| sess.call("echo", &json!({"x": 1})),
        )
        .unwrap();
    assert_eq!(out, json!({"echoed": {"x": 1}}));

    // The SECOND call reuses the same child (residency): if each call
    // re-spawned, the stream guard state below would never work — and
    // here, the same session key must not have been evicted.
    let out2 = s
        .with_session(
            "box/k1",
            "exec",
            &argv,
            None,
            &probe_runtime::sandbox::SandboxPolicy::None,
            |sess| sess.call("echo", &json!({"y": 2})),
        )
        .unwrap();
    assert_eq!(out2, json!({"echoed": {"y": 2}}));
}

/// The ctx seam crosses the boundary: the child sends a host frame
/// (ctx_invoke) mid-call, the parent's bridge answers on stdin, the
/// child forwards the reply into its result. This is ADR-0035's §3
/// `host` / `host_reply` pair over real pipes.
#[test]
fn exec_host_call_crosses_the_seam() {
    let seen: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let bridge = {
        let seen = seen.clone();
        let mut b = HostBridge::default();
        b.functions.insert(
            "ctx_invoke".into(),
            Arc::new(move |arg: Value| {
                seen.lock().unwrap().push(arg.clone());
                Ok(json!({"from": "host", "echo": arg}))
            }) as probe_runtime::carrier::HostFn,
        );
        b
    };
    let s = sessions();
    let argv = exec_bin();
    let out = s
        .with_session(
            "box/k2",
            "exec",
            &argv,
            Some(&bridge),
            &probe_runtime::sandbox::SandboxPolicy::None,
            |sess| {
                sess.call(
                    "ctx_round_trip",
                    &json!({"type": "t", "key": "k", "handler": "h", "args": {"q": 1}}),
                )
            },
        )
        .unwrap();
    assert_eq!(seen.lock().unwrap().len(), 1, "the host fn ran once");
    assert_eq!(
        out["from"], "host",
        "the parent's answer reached the child's reply"
    );
}

/// ADR-0034 over the exec seam: start carries the first envelope,
/// next advances the guard the CHILD keeps in its own memory (the
/// residency is the stream state — mode A's envelope producer has no
/// generator, and it does not need one), done is written.
#[test]
fn exec_iterate_stream() {
    let s = sessions();
    let argv = exec_bin();
    // One with_session per pull — each call re-enters the SAME child
    // (the registry keys residency; the guard state lives in the
    // child's memory across the calls, exactly like a session VM).
    let pull = |op: StreamOp| -> Value {
        s.with_session(
            "box/k3",
            "exec",
            &argv,
            None,
            &probe_runtime::sandbox::SandboxPolicy::None,
            move |sess| sess.iterate(op.clone()),
        )
        .unwrap()
    };
    let mut envs = vec![pull(StreamOp::Start {
        stream_id: "s1".into(),
        handler: "count".into(),
        args: json!({"total": 3}),
    })];
    for _ in 0..4 {
        let e = pull(StreamOp::Next {
            stream_id: "s1".into(),
            handler: "count".into(),
            args: json!({"total": 3}),
        });
        let done = e.get("done").and_then(|d| d.as_bool()) == Some(true);
        envs.push(e);
        if done {
            break;
        }
    }
    assert_eq!(envs[0], json!({"item": "i0", "done": false}), "start carries the first item");
    assert_eq!(envs[1], json!({"item": "i1", "done": false}));
    assert_eq!(envs[2], json!({"item": "i2", "done": false}));
    assert_eq!(envs.last().unwrap(), &json!({"done": true}), "exhaustion writes done");

    // Pull-after-done: the child's guard is gone — a failed pull (an
    // error value through the stream), never a resurrection.
    let late = s
        .with_session(
            "box/k3",
            "exec",
            &argv,
            None,
            &probe_runtime::sandbox::SandboxPolicy::None,
            |sess| {
                sess.iterate(StreamOp::Next {
                    stream_id: "s1".into(),
                    handler: "count".into(),
                    args: json!({}),
                })
            },
        )
        .unwrap();
    assert!(late.get("error").is_some(), "unknown stream is an error value, got {late}");
}

/// ADR-0035 mode B (the SKILL shape): one process per call. The same
/// exec_loop binary serves — the parent closes stdin right after the
/// request (EOF is the child's cue, which is exactly how nushell reads
/// its args), the child answers and its loop ends. Two calls = two
/// processes: nothing survives between them (guard state included —
/// the residency is gone with the process, invoke-only semantics).
#[test]
fn exec_oneshot_runs_per_call() {
    let s = sessions();
    let argv = exec_bin();
    let call = |v: Value| {
        s.with_session(
            "box/b1",
            "exec-b",
            &argv,
            None,
            &probe_runtime::sandbox::SandboxPolicy::None,
            move |sess| sess.call("echo", &v),
        )
        .unwrap()
    };
    assert_eq!(call(json!({"n": 1})), json!({"echoed": {"n": 1}}));
    assert_eq!(call(json!({"n": 2})), json!({"echoed": {"n": 2}}));

    // No residency to sweep: mode B's slot parks no live child between
    // calls (is_alive is true by contract — an absent child is normal).
    assert!(s.sweep_dead().is_empty(), "B's parked slots are never 'dead'");
}

/// Mode B's ctx rule as an ASSERTION, not a surprise: a one-shot child
/// that sends a host frame hits the contract violation (B is ctx-free —
/// the established invoke-only downgrade; the error value names the
/// design, the fix is mode A).
#[test]
fn exec_oneshot_rejects_host_frames() {
    let s = sessions();
    let argv = exec_bin();
    let mut bridge = HostBridge::default();
    bridge.functions.insert(
        "ctx_invoke".into(),
        Arc::new(|arg: Value| Ok(json!({"answered": arg}))) as probe_runtime::carrier::HostFn,
    );
    let r = s.with_session(
        "box/b2",
        "exec-b",
        &argv,
        Some(&bridge),
        &probe_runtime::sandbox::SandboxPolicy::None,
        |sess| {
            sess.call(
                "ctx_round_trip",
                &json!({"type": "t", "key": "k", "handler": "h", "args": {}}),
            )
        },
    );
    let err = r.unwrap_err().to_string();
    assert!(err.contains("ctx-free"), "mode B names the contract violation: {err}");
}

/// Mode A residency rule: eviction closes stdin (the loop's EOF) and
/// reaps; a child that died on its own is swept by sweep_dead so the
/// next call cold-starts a fresh spawn.
#[test]
fn eviction_ends_the_residency_and_sweep_clears_the_dead() {
    let s = sessions();
    let argv = exec_bin();
    // Spawn + one call.
    s.with_session("box/k4", "exec", &argv, None, &probe_runtime::sandbox::SandboxPolicy::None, |sess| {
        sess.call("echo", &json!(1))
    })
    .unwrap();

    // Kill the child behind the registry's back (simulates a crash):
    // sweep_dead finds it and drops the slot (which reaps + releases).
    let pid = s
        .with_session(
            "box/k4",
            "exec",
            &argv,
            None,
            &probe_runtime::sandbox::SandboxPolicy::None,
            |sess| {
                sess.as_any()
                    .downcast_mut::<probe_runtime::carrier::exec::ExecSession>()
                    .map(|e| serde_json::json!({ "pid": e.child_pid() }))
                    .ok_or_else(|| anyhow::anyhow!("not an exec session"))
            },
        )
        .unwrap()["pid"]
            .as_u64()
            .unwrap() as i32;
    // SIGKILL from the outside; delivery is asynchronous — poll until
    // the kernel finishes (WNOHANG reads "alive" inside the delivery
    // window, so a single shot races the signal).
    unsafe { libc::kill(pid, libc::SIGKILL) };
    let mut swept = Vec::new();
    for _ in 0..200 {
        swept = s.sweep_dead();
        if swept.contains(&"box/k4".to_string()) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(swept.contains(&"box/k4".to_string()), "the dead slot is swept: {swept:?}");
    let out = s
        .with_session("box/k4", "exec", &argv, None, &probe_runtime::sandbox::SandboxPolicy::None, |sess| {
            sess.call("echo", &json!("after respawn"))
        })
        .unwrap();
    assert_eq!(out, json!({"echoed": "after respawn"}));
}

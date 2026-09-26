//! Carrier contract, as executable documentation.
//!
//! Every carrier honors the same contract: JSON args in, JSON result out,
//! handlers addressed by name, failures as error values (never panics).
//! Execution is RESIDENT — one session per booth instance, loaded once,
//! called per event. These tests double as the reference for how a booth
//! looks in each carried language.

use probe_runtime::carrier::session::Sessions;
use probe_runtime::carrier::{execute, HostBridge};

fn run(language: &str, src: &str, handler: &str, args: &serde_json::Value) -> anyhow::Result<serde_json::Value> {
    let sessions = Sessions::new();
    sessions.with_session("t1", language, src, None::<&HostBridge>, &probe_runtime::sandbox::SandboxPolicy::None, |s: &mut dyn probe_runtime::carrier::session::ResidentSession| {
        s.call(handler, args)
    })
}

// ---------------------------------------------------------------- python --
// Booth shape: handlers defined and bound (via @on or plain def) at load;
// each event call invokes the handler by name with parsed args.
#[cfg(feature = "python")]
#[test]
fn py_entry_with_args() {
    let src = "def doubled(args):\n    return {\"doubled\": args[\"x\"] * 2}\n";
    let out = run("python", src, "doubled", &serde_json::json!({ "x": 21 })).unwrap();
    assert_eq!(out, serde_json::json!({"doubled": 42}));
}

// Module-level state persists across calls in the same session.
#[cfg(feature = "python")]
#[test]
fn py_state_persists() {
    let src = "acc = []\ndef push(args):\n    acc.append(args[\"item\"])\n    return acc\n";
    let sessions = Sessions::new();
    let host: Option<&HostBridge> = None;
    let call = |s: &mut dyn probe_runtime::carrier::session::ResidentSession| {
        s.call("push", &serde_json::json!({"item": "book"}))
    };
    sessions.with_session("i", "python", src, host, &probe_runtime::sandbox::SandboxPolicy::None, call).unwrap();
    let out = sessions.with_session("i", "python", src, host, &probe_runtime::sandbox::SandboxPolicy::None, call).unwrap();
    assert_eq!(out, serde_json::json!(["book", "book"]));
}

// A missing handler is an error value, never a panic.
#[cfg(feature = "python")]
#[test]
fn py_missing_entry_is_error_value() {
    let err = run("python", "x = 1\n", "nope", &serde_json::json!(null)).unwrap_err();
    assert!(err.to_string().contains("nope"));
}

// --------------------------------------------------------------- nushell --
// Booth shape: a module exporting named functions (`def --env` for handlers
// that write $env state). Args arrive as one parsed value; results are
// structured and travel via files (the PTY stream is discarded).
#[cfg(feature = "nushell")]
#[test]
fn nu_entry_with_args() {
    let src = r#"
export def double [args] {
    { doubled: ($args.x * 2) }
}
"#;
    let out = run("nushell", src, "double", &serde_json::json!({ "x": 21 })).unwrap();
    assert_eq!(out, serde_json::json!({"doubled": 42}));
}

// Pipelines work naturally: structured data flows through nu operations.
#[cfg(feature = "nushell")]
#[test]
fn nu_pipeline_result() {
    let src = r#"
export def sort_items [args] {
    $args.items | sort
}
"#;
    let out = run("nushell", src, "sort_items", &serde_json::json!({ "items": [3, 1, 2] })).unwrap();
    assert_eq!(out, serde_json::json!([1, 2, 3]));
}

// ----------------------------------------------------------------- steel --
// Booth shape: definitions plus handler lambdas; args arrive as a native
// steel value (the carrier marshals at the boundary).
#[cfg(feature = "steel")]
#[test]
fn steel_entry_with_args() {
    let src = r#"
(define (greet args)
  (string-append "x=" (number->string (hash-ref args "x"))))
"#;
    let out = run("steel", src, "greet", &serde_json::json!({ "x": 2 })).unwrap();
    assert_eq!(out, serde_json::json!("x=2"));
}

// A language the node does not carry is an error value: the control plane
// declares, the Probe only validates. (Surfaces at session spawn.)
#[cfg(feature = "steel")]
#[test]
fn unknown_language_is_error_value() {
    let sessions = Sessions::new();
    let err = sessions
        .with_session("t1", "koto", "(+ 1 2)", None::<&HostBridge>, &probe_runtime::sandbox::SandboxPolicy::None, |s: &mut dyn probe_runtime::carrier::session::ResidentSession| {
            s.call("f", &serde_json::json!(null))
        })
        .unwrap_err();
    assert!(err.to_string().contains("koto"));
}

// The retired one-shot execute() refuses resident languages explicitly.
#[test]
fn one_shot_execute_is_gone() {
    let err = execute(
        "steel",
        probe_runtime::carrier::ExecRequest {
            source: "",
            entry: None,
            args: &serde_json::json!(null),
            host: None,
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("resident-only"));
}

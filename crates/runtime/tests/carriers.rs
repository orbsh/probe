//! Carrier contract, as executable documentation.
//!
//! Every carrier — in-process or subprocess — honors the same contract:
//! JSON args in, JSON result out, entry function declared by the operation,
//! failures as error values (never panics). These tests double as the
//! reference for how an operation looks in each carried language.

use probe_runtime::carrier::{execute, ExecRequest};

// ---------------------------------------------------------------- python --
// Operation shape: a module with an exported entry function taking the
// parsed args (dict/list/...) and returning a JSON-serializable value.
#[cfg(feature = "python")]
#[test]
fn py_entry_with_args() {
    let args = serde_json::json!({ "x": 21 });
    let src = "def execute(args):\n    return {\"doubled\": args[\"x\"] * 2}\n";
    let out = execute(
        "python",
        ExecRequest { source: src, entry: Some("execute"), args: &args, host: None },
    )
    .unwrap();
    assert_eq!(out, serde_json::json!({"doubled": 42}));
}

// No entry declared: the script sets a module-level `result` variable.
#[cfg(feature = "python")]
#[test]
fn py_result_binding() {
    let args = serde_json::json!(null);
    let src = "result = [1, 2, 3]\n";
    let out = execute(
        "python",
        ExecRequest { source: src, entry: None, args: &args, host: None },
    )
    .unwrap();
    assert_eq!(out, serde_json::json!([1, 2, 3]));
}

// A missing entry is an error value, never a panic.
#[cfg(feature = "python")]
#[test]
fn py_missing_entry_is_error_value() {
    let args = serde_json::json!(null);
    let err = execute(
        "python",
        ExecRequest { source: "x = 1\n", entry: Some("nope"), args: &args, host: None },
    )
    .unwrap_err();
    assert!(err.to_string().contains("nope"));
}

// --------------------------------------------------------------- nushell --
// Operation shape: a module exporting a named function. Args arrive as one
// parsed value (record/list/...); the return value is structured and
// serialized by the wrapper.
#[cfg(feature = "nushell")]
#[test]
fn nu_entry_with_args() {
    let args = serde_json::json!({ "x": 21 });
    let src = r#"
export def execute [args] {
    { doubled: ($args.x * 2) }
}
"#;
    let out = execute(
        "nushell",
        ExecRequest { source: src, entry: Some("execute"), args: &args, host: None },
    )
    .unwrap();
    assert_eq!(out, serde_json::json!({"doubled": 42}));
}

// Pipelines work naturally: structured data flows through nu operations.
#[cfg(feature = "nushell")]
#[test]
fn nu_pipeline_result() {
    let args = serde_json::json!({ "items": [3, 1, 2] });
    let src = r#"
export def execute [args] {
    $args.items | sort
}
"#;
    let out = execute(
        "nushell",
        ExecRequest { source: src, entry: Some("execute"), args: &args, host: None },
    )
    .unwrap();
    assert_eq!(out, serde_json::json!([1, 2, 3]));
}

// nu module import cannot address a bare `main`; operations must export a
// named function. Declaring `main` is rejected up front.
#[cfg(feature = "nushell")]
#[test]
fn nu_rejects_main_entry() {
    let args = serde_json::json!(null);
    let err = execute(
        "nushell",
        ExecRequest { source: "export def main [] {}", entry: Some("main"), args: &args, host: None },
    )
    .unwrap_err();
    assert!(err.to_string().contains("named function"));
}

// ----------------------------------------------------------------- steel --
// Operation shape: definitions plus an entry function; args arrive as a
// JSON string (the operation parses what it needs).
#[cfg(feature = "steel")]
#[test]
fn steel_entry_with_args() {
    let args = serde_json::json!({ "x": 2 });
    let src = r#"
(define (execute args)
  (string-append "x=" (number->string 2)))
"#;
    let out = execute(
        "steel",
        ExecRequest { source: src, entry: Some("execute"), args: &args, host: None },
    )
    .unwrap();
    assert_eq!(out, serde_json::json!("x=2"));
}

// No entry: the source registers its result in `*result*`.
#[cfg(feature = "steel")]
#[test]
fn steel_result_binding() {
    let args = serde_json::json!(null);
    let out = execute(
        "steel",
        ExecRequest {
            source: "(define *result* (* 6 7))",
            entry: None,
            args: &args,
            host: None,
        },
    )
    .unwrap();
    assert_eq!(out, serde_json::json!(42));
}

// A language the node does not carry is an error value: the control plane
// declares, the Probe only validates.
#[cfg(feature = "steel")]
#[test]
fn unknown_language_is_error_value() {
    let args = serde_json::json!(null);
    let err = execute(
        "koto",
        ExecRequest { source: "(+ 1 2)", entry: None, args: &args, host: None },
    )
    .unwrap_err();
    assert!(err.to_string().contains("koto"));
}

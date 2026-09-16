use probe_runtime::carrier::{python, steel, ExecRequest, HostBridge};

#[test]
fn python_event_name_addressing() {
    // @on handlers are bound under the EVENT name — delivery addresses
    // handler="add_to_cart" resolves directly, no execute fallback.
    let src = r##"
@on("add_to_cart", key="user_id")
def add(args):
    return {"added": args["item"]}

@on("remove_from_cart")
def remove(args):
    return {"removed": True}
"##;
    let out = python::execute(ExecRequest {
        source: src,
        entry: Some("add_to_cart"),
        args: &serde_json::json!({"item": "book", "user_id": "u1"}),
        host: None,
    }).unwrap();
    println!("PY: {out}");
    assert_eq!(out["added"], "book");
}

#[test]
fn steel_event_name_addressing() {
    // (on ...) binds the handler under the event name in the VM.
    let src = r##"
(on "add_to_cart" "user_id" (lambda (args) (hash "added" (hash-ref args "item"))))
"##;
    let out = steel::execute(ExecRequest {
        source: src,
        entry: Some("add_to_cart"),
        args: &serde_json::json!({"item": "book"}),
        host: None,
    }).unwrap();
    println!("STEEL: {out}");
    assert_eq!(out["added"], "book");
}

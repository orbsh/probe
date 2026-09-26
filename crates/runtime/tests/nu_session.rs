use probe_runtime::carrier::nushell_session::NushellSession;

#[test]
fn pty_session_state_and_multi_entry() {
    let src = r##"
export def --env add_to_cart [args] {
    $env.count = (($env.count? | default 0) + 1)
    { item: $args.item, count: $env.count }
}
export def --env remove_from_cart [args] {
    $env.count = (($env.count? | default 0) - 1)
    { item: $args.item, count: $env.count }
}
"##;
    let dir = std::env::temp_dir().join(format!("probe-nu-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let op = dir.join("operation.nu");
    std::fs::write(&op, src).unwrap();

    let mut session = NushellSession::spawn(&probe_runtime::sandbox::SandboxPolicy::None).unwrap();
    session.load(op.to_str().unwrap()).expect("nu session load");

    let args = serde_json::json!({ "item": "book" });

    // Same handler twice: env state accumulates across calls.
    let r1 = session.call("add_to_cart", &args).unwrap();
    assert_eq!(r1["count"], 1, "first add: {r1}");
    let r2 = session.call("add_to_cart", &args).unwrap();
    assert_eq!(r2["count"], 2, "second add (state persisted): {r2}");

    // Different handler in the same resident session (multi-entry).
    let r3 = session.call("remove_from_cart", &args).unwrap();
    assert_eq!(r3["count"], 1, "remove after two adds: {r3}");

    println!("R1={r1} R2={r2} R3={r3}");
}

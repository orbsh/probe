use probe_protocol::{CodePayload, ToolCall};

#[test]
fn roundtrip_inline_call() {
    let call = ToolCall {
        call_id: "c1".into(),
        tool: "probe:home-pc:read_file".into(),
        language: "nushell".into(),
        args: serde_json::json!({ "path": "~/notes.md" }),
        code: CodePayload::Inline { bytes: b"print(1)".to_vec() },
    };
    let s = serde_json::to_string(&call).unwrap();
    let back: ToolCall = serde_json::from_str(&s).unwrap();
    assert_eq!(back.call_id, "c1");
    assert_eq!(back.language, "nushell");
    assert!(matches!(back.code, CodePayload::Inline { .. }));
}

#[test]
fn link_payload_has_version_and_hash() {
    let p = CodePayload::Link {
        url: "https://cdn.example/op.wasm".into(),
        version: "sha256:abc".into(),
        expected_sha256: "abc".into(),
    };
    let s = serde_json::to_string(&p).unwrap();
    assert!(s.contains("\"type\":\"link\""));
}

#[test]
fn capability_surface_defaults_deny() {
    let c = probe_config::CapabilitySurface::default();
    assert!(c.fs_scope.is_empty());
    assert!(!c.command_exec);
    assert!(matches!(c.network, probe_config::NetworkPolicy::None));
}

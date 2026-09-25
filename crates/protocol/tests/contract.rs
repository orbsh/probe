use probe_protocol::{CodeRef, ToolCall};

#[test]
fn roundtrip_call_carries_a_code_reference() {
    let call = ToolCall {
        call_id: "c1".into(),
        session: "notes/7".into(),
        entry: "read_file".into(),
        language: "nushell".into(),
        args: serde_json::json!({ "path": "~/notes.md" }),
        code: CodeRef {
            url: "http://code.local/ab12".into(),
            sha256: "ab12".into(),
        },
    };
    let s = serde_json::to_string(&call).unwrap();
    let back: ToolCall = serde_json::from_str(&s).unwrap();
    assert_eq!(back.call_id, "c1");
    assert_eq!(back.session, "notes/7", "residency identity survives the wire");
    assert_eq!(back.entry, "read_file", "entry name survives the wire");
    assert_eq!(back.language, "nushell");
    assert_eq!(back.code.sha256, "ab12", "the frame asserts the hash");
    assert_eq!(back.code.url, "http://code.local/ab12");
}

#[test]
fn code_ref_has_no_version_field() {
    // ADR-0027: the content hash IS the version identity — the retired
    // Link arm's `version` string must not resurface in the wire shape.
    let p = CodeRef {
        url: "https://cdn.example/abc".into(),
        sha256: "abc".into(),
    };
    let s = serde_json::to_string(&p).unwrap();
    assert!(!s.contains("version"), "no version token: {s}");
    assert!(s.contains("\"url\"") && s.contains("\"sha256\""));
}

#[test]
fn capability_surface_defaults_deny() {
    let c = probe_config::CapabilitySurface::default();
    assert!(c.fs_scope.is_empty());
    assert!(!c.command_exec);
    assert!(matches!(c.network, probe_config::NetworkPolicy::None));
}

//! Phase 3 acceptance: a fake control plane over a real WS endpoint.
//! Register -> Registered -> push one ToolCall -> assert the ToolResult.

use probe_config::{CapabilitySurface, NetworkPolicy, ProbeConfig};
use probe_protocol::{CodePayload, Frame, ToolCall, ToolResult};
use probe_runtime::remote;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;
use futures_util::{SinkExt, StreamExt};

async fn fake_control_plane(listener: TcpListener) {
    let (stream, _) = listener.accept().await.unwrap();
    let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
    let (mut sink, mut stream) = ws.split();

    // Registration: expect it, reply Registered.
    let Message::Text(text) = stream.next().await.unwrap().unwrap() else {
        panic!("expected text frame");
    };
    match serde_json::from_str::<Frame>(&text).unwrap() {
        Frame::Register { node_alias, carriers, .. } => {
            assert_eq!(node_alias, "test-node");
            assert!(carriers.contains(&"steel".to_string()));
        }
        other => panic!("expected Register, got {other:?}"),
    }
    sink.send(Message::Text(
        serde_json::to_string(&Frame::Registered).unwrap(),
    ))
    .await
    .unwrap();

    // Push one call: a steel counter handler.
    let call = ToolCall {
        call_id: "c-1".into(),
        tool: "counter".into(),
        language: "steel".into(),
        args: serde_json::json!({"n": 3}),
        code: CodePayload::Inline {
            bytes: br#"
(define (counter args)
  (hash "doubled" (* 2 (hash-ref args "n"))))
"#
            .to_vec(),
        },
    };
    sink.send(Message::Text(
        serde_json::to_string(&Frame::Call(call)).unwrap(),
    ))
    .await
    .unwrap();

    // Expect the result.
    let Message::Text(text) = stream.next().await.unwrap().unwrap() else {
        panic!("expected text frame");
    };
    match serde_json::from_str::<Frame>(&text).unwrap() {
        Frame::Result(ToolResult { call_id, outcome }) => {
            assert_eq!(call_id, "c-1");
            assert_eq!(outcome.unwrap(), serde_json::json!({"doubled": 6}));
        }
        other => panic!("expected Result, got {other:?}"),
    }
}

#[tokio::test]
async fn outbound_registration_and_task_downlink() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(fake_control_plane(listener));

    let config = ProbeConfig {
        control_plane_url: format!("ws://127.0.0.1:{port}"),
        credential_env: "PROBE_TEST_CREDENTIAL".into(),
        capabilities: CapabilitySurface {
            carriers: vec!["steel".into()],
            node_alias: "test-node".into(),
            network: NetworkPolicy::None,
            ..Default::default()
        },
    };
    std::env::set_var("PROBE_TEST_CREDENTIAL", "secret-token");

    // The probe serves one connection; the fake plane drops it after
    // asserting, then run() reconnects forever — bound the probe with a
    // timeout (the server task holds the real assertions).
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), remote::run(config)).await;
    server.await.unwrap();
}

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

// ---------------------------------------------- Phase 4: Link payloads ----

/// Serve code bytes over plain HTTP once; returns (port, sha256 hex).
fn serve_code(bytes: Vec<u8>) -> (u16, String) {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(&bytes);
    let sha = hex::encode(hasher.finalize());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let body = bytes.clone();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.write_all(&body);
        }
    });
    (port, sha)
}

use std::io::{Read as _, Write as _};

#[tokio::test]
async fn link_payload_fetch_verify_and_mismatch_rejection() {
    let code = br#"
(define (triple args)
  (hash "tripled" (* 3 (hash-ref args "n"))))
"#.to_vec();
    let (http_port, sha) = serve_code(code);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        let (mut sink, mut stream) = ws.split();

        let msg = stream.next().await.unwrap().unwrap();
        let f: Frame = serde_json::from_str(msg.to_text().unwrap()).unwrap();
        assert!(matches!(f, Frame::Register { .. }));
        sink.send(Message::Text(serde_json::to_string(&Frame::Registered).unwrap()))
            .await
            .unwrap();

        // Link call with the CORRECT hash: fetch, verify, execute.
        let call = ToolCall {
            call_id: "c-link".into(),
            tool: "triple".into(),
            language: "steel".into(),
            args: serde_json::json!({"n": 5}),
            code: CodePayload::Link {
                url: format!("http://127.0.0.1:{http_port}/code"),
                version: "v1".into(),
                expected_sha256: sha,
            },
        };
        sink.send(Message::Text(serde_json::to_string(&Frame::Call(call)).unwrap()))
            .await
            .unwrap();
        let msg = stream.next().await.unwrap().unwrap();
        match serde_json::from_str::<Frame>(msg.to_text().unwrap()).unwrap() {
            Frame::Result(r) => {
                assert_eq!(r.call_id, "c-link");
                assert_eq!(r.outcome.unwrap(), serde_json::json!({"tripled": 15}));
            }
            other => panic!("expected Result, got {other:?}"),
        }

        // Link call with a WRONG hash: error value, never a silent accept.
        let call = ToolCall {
            call_id: "c-bad".into(),
            tool: "triple".into(),
            language: "steel".into(),
            args: serde_json::json!({"n": 5}),
            code: CodePayload::Link {
                url: format!("http://127.0.0.1:{http_port}/code"),
                version: "v1".into(),
                expected_sha256: "deadbeef".into(),
            },
        };
        sink.send(Message::Text(serde_json::to_string(&Frame::Call(call)).unwrap()))
            .await
            .unwrap();
        let msg = stream.next().await.unwrap().unwrap();
        match serde_json::from_str::<Frame>(msg.to_text().unwrap()).unwrap() {
            Frame::Result(r) => {
                assert_eq!(r.call_id, "c-bad");
                assert!(r.outcome.is_err(), "hash mismatch must be an error value");
            }
            other => panic!("expected Result, got {other:?}"),
        }
    });

    let config = ProbeConfig {
        control_plane_url: format!("ws://127.0.0.1:{ws_port}"),
        credential_env: "PROBE_LINK_CREDENTIAL".into(),
        capabilities: CapabilitySurface {
            carriers: vec!["steel".into()],
            node_alias: "link-node".into(),
            network: NetworkPolicy::Open,
            ..Default::default()
        },
    };
    std::env::set_var("PROBE_LINK_CREDENTIAL", "tok");
    let _ = tokio::time::timeout(std::time::Duration::from_secs(10), remote::run(config)).await;
    server.await.unwrap();
}

//! Phase 4.5 acceptance: KV op frames ride the SAME outbound WS connection
//! as tool calls (ADR-0010 — no dedicated listener, no second protocol).
//! A fake control plane declares two executors, pushes write/read frames,
//! and asserts the answers, the prefix isolation (one key name, two
//! executors), and the refusal path.

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use okm_wire::{OpFrame, OpResponse};
use probe_config::{CapabilitySurface, KvExecutorDecl, ProbeConfig};
use probe_protocol::{Frame, KvFrame};
use probe_runtime::remote;
use std::path::PathBuf;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

// Op tags (okm-wire): the low 2 bits of the tag byte.
const OP_PUT: u8 = 0;
const OP_GET: u8 = 2;

type Sink = SplitSink<WebSocketStream<TcpStream>, Message>;
type Stream = SplitStream<WebSocketStream<TcpStream>>;

/// The control-plane end of the connection: send a frame, await one back.
struct Plane {
    sink: Sink,
    stream: Stream,
}

impl Plane {
    async fn send(&mut self, frame: &Frame) {
        self.sink
            .send(Message::Text(serde_json::to_string(frame).unwrap()))
            .await
            .unwrap();
    }

    async fn next(&mut self) -> Frame {
        let Message::Text(text) = self.stream.next().await.unwrap().unwrap() else {
            panic!("expected text frame");
        };
        serde_json::from_str(&text).unwrap()
    }

    /// One KV round trip: push the op frame, return the payload the probe
    /// answered with. A refusal is NOT an answer here — it arrives as its
    /// own frame type and would panic this helper (`kv_refused`).
    async fn kv(&mut self, executor: &str, kv_id: &str, frame: &OpFrame) -> Vec<u8> {
        self.send(&Frame::Kv(KvFrame {
            executor: executor.into(),
            kv_id: kv_id.into(),
            frame: frame.encode(),
        }))
        .await;
        match self.next().await {
            Frame::Kv(kv) => {
                assert_eq!(kv.executor, executor, "answer names the addressed executor");
                assert_eq!(kv.kv_id, kv_id, "answer correlates by kv_id");
                kv.frame
            }
            other => panic!("expected Kv frame, got {other:?}"),
        }
    }

    /// One refused KV round trip: push raw op-frame bytes (an encoded frame
    /// or deliberate garbage), require a dedicated `KvRefused` carrying the
    /// same executor + kv_id and a non-empty reason — the sender is told
    /// WHY, on the wire, instead of reading it out of the op codec's value
    /// space. Returns the reason.
    async fn kv_refused(&mut self, executor: &str, kv_id: &str, frame: Vec<u8>) -> String {
        self.send(&Frame::Kv(KvFrame {
            executor: executor.into(),
            kv_id: kv_id.into(),
            frame,
        }))
        .await;
        match self.next().await {
            Frame::KvRefused {
                executor: got,
                kv_id: got_id,
                reason,
            } => {
                assert_eq!(got, executor, "refusal names the addressed executor");
                assert_eq!(got_id, kv_id, "refusal correlates by kv_id");
                assert!(!reason.is_empty(), "a refusal states its reason");
                reason
            }
            other => panic!("expected KvRefused, got {other:?}"),
        }
    }
}

async fn fake_control_plane(listener: TcpListener) {
    let (stream, _) = listener.accept().await.unwrap();
    let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
    let (sink, stream) = ws.split();
    let mut plane = Plane { sink, stream };

    // Registration handshake first — KV rides the same connection.
    match plane.next().await {
        Frame::Register { node_alias, .. } => assert_eq!(node_alias, "kv-node"),
        other => panic!("expected Register, got {other:?}"),
    }
    plane.send(&Frame::Registered).await;

    // Two executors, one key name: writes land under different declared
    // prefixes, so neither can read or overwrite the other's row.
    let put_a = OpFrame::one(OP_PUT, b"key".to_vec(), b"value-A".to_vec());
    let put_b = OpFrame::one(OP_PUT, b"key".to_vec(), b"value-B".to_vec());
    let acked = plane.kv("app-a", "kv-1", &put_a).await;
    assert_eq!(
        OpResponse::decode(&acked),
        Some(OpResponse::default()),
        "a put is answered with the empty (default) response"
    );
    let _ = plane.kv("app-b", "kv-2", &put_b).await;

    let get = OpFrame::one(OP_GET, b"key".to_vec(), Vec::new());
    let read_a =
        OpResponse::decode(&plane.kv("app-a", "kv-3", &get).await).expect("response decodes");
    let read_b =
        OpResponse::decode(&plane.kv("app-b", "kv-4", &get).await).expect("response decodes");
    assert_eq!(read_a.value.as_deref(), Some(&b"value-A"[..]));
    assert_eq!(read_b.value.as_deref(), Some(&b"value-B"[..]));

    // Undeclared name: refused by name, with a reason — the caller is
    // answered on the same connection instead of waiting. The reason is the
    // registry's own vocabulary (the frame already carries the executor, so
    // the reason states the cause, not the address).
    let why = plane.kv_refused("app-c", "kv-5", get.encode()).await;
    assert_eq!(why, "no such declared executor");

    // A frame the executor cannot decode is refused the same way, still
    // correlated to its request, and states that other cause.
    let why = plane
        .kv_refused("app-a", "kv-6", vec![0xff, 0xff, 0xff])
        .await;
    assert_eq!(why, "frame not decodable");
}

#[tokio::test]
async fn kv_frames_ride_the_call_connection() {
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(fake_control_plane(listener));

    let decl = |name: &str, ns: u16, dir: &PathBuf| KvExecutorDecl {
        name: name.into(),
        ns,
        data_dir: dir.display().to_string(),
    };
    let config = ProbeConfig {
        control_plane_url: format!("ws://127.0.0.1:{port}"),
        sandbox: true,
        credential_env: "PROBE_TEST_CREDENTIAL".into(),
        capabilities: CapabilitySurface {
            node_alias: "kv-node".into(),
            ..Default::default()
        },
        kv_executors: vec![
            decl("app-a", 1, &dir_a.path().to_path_buf()),
            decl("app-b", 2, &dir_b.path().to_path_buf()),
        ],
    };
    std::env::set_var("PROBE_TEST_CREDENTIAL", "secret-token");

    // The probe serves one connection; the fake plane drops it after
    // asserting, then run() reconnects forever — bound the probe with a
    // timeout (the server task holds the real assertions).
    let _ = tokio::time::timeout(std::time::Duration::from_secs(3), remote::run(config)).await;
    server.await.unwrap();
}

use okm_core::fjall_backend::FjallStore;
use okm_wire::{OpFrame, OpResponse};
use probe_runtime::kv_executor::KvExecutor;
use probe_runtime::kv_executor::SharedVirtualStorage;

// Op tags (okm-wire): the low 2 bits of the tag byte.
const OP_PUT: u8 = 0;
const OP_DELETE: u8 = 1;
const OP_GET: u8 = 2;
const OP_SCAN: u8 = 3;

#[test]
fn kv_executor_roundtrip_with_prefix_isolation() {
    let dir = tempfile::tempdir().unwrap();
    let engine = FjallStore::open(dir.path(), "kv").unwrap();

    // Two executors on ONE engine, distinct prefixes — structural isolation.
    let (exec_a, _handle_a) = KvExecutor::new(engine.shared_handle(), b"app-a");
    let (exec_b, _handle_b) = KvExecutor::new(engine.shared_handle(), b"app-b");

    // Sender A: put two keys via an encoded frame (exactly what a remote
    // VirtualStorage backend puts on the wire).
    let frame = OpFrame::new(vec![
        (OP_PUT, b"key1".to_vec(), b"value-A1".to_vec()),
        (OP_PUT, b"key2".to_vec(), b"value-A2".to_vec()),
    ]);
    let resp: OpResponse = exec_a.apply(&frame.encode()).expect("apply succeeds");
    assert_eq!(resp, OpResponse::default(), "puts answer empty");

    // Sender B writes the SAME key name — isolated by the executor prefix.
    let frame_b = OpFrame::new(vec![(OP_PUT, b"key1".to_vec(), b"value-B1".to_vec())]);
    let _ = exec_b.apply(&frame_b.encode()).expect("apply succeeds");

    // A's key1 still holds A's value (B's write landed under app-b/).
    let resp_a = exec_a
        .apply(&OpFrame::new(vec![(OP_GET, b"key1".to_vec(), vec![])]).encode())
        .unwrap();
    assert_eq!(resp_a.value.as_deref(), Some(&b"value-A1"[..]));

    // Scan returns key suffixes relative to the requested prefix.
    let resp_scan = exec_a
        .apply(&OpFrame::new(vec![(OP_SCAN, b"key".to_vec(), vec![])]).encode())
        .unwrap();
    assert_eq!(resp_scan.suffixes.len(), 2, "two keys under 'key'");

    // Delete removes.
    let _ = exec_a
        .apply(&OpFrame::new(vec![(OP_DELETE, b"key1".to_vec(), vec![])]).encode())
        .unwrap();
    let resp_gone = exec_a
        .apply(&OpFrame::new(vec![(OP_GET, b"key1".to_vec(), vec![])]).encode())
        .unwrap();
    assert_eq!(resp_gone.value, None);
}

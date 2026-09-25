//! The test fixture module: a real rustc-compiled wasm actor exercising
//! the actor-guest kit — static derives + in-module Collection over the
//! emit bridge. Compiled to wasm32 by `tests/wasm_guest_build.rs` via
//! `cargo build -p actor-guest --example counter_actor --target
//! wasm32-unknown-unknown`; the carrier tests load the artifact.

use actor_guest::{schema::CollectionDecl, EmitStore};
use okm_core::model::document::Collection;
use okm_core::model::obj_dynamic::DynamicValue;
use okm_core::{DocumentEncode, KeyEncode};
use std::collections::BTreeMap;

// ---- Declared collection: schema IS code (static mode, ADR-0026 §4) ----

#[derive(KeyEncode, Clone, PartialEq, Debug)]
#[ok_ns(700)]
pub struct CounterKey {
    pub user_id: u64,
}

#[derive(DocumentEncode, Clone, Debug)]
#[ok_ref(CounterKey)]
pub struct CounterDoc {
    pub count: u64,
}

fn counters() -> Collection<EmitStore, CounterKey, CounterDoc> {
    Collection::new(EmitStore)
}

fn read_count(key: &CounterKey) -> u64 {
    counters()
        .get_document(key)
        .and_then(|m| m.get("count").cloned())
        .and_then(|v| match v {
            DynamicValue::UInt(n) => Some(n),
            _ => None,
        })
        .unwrap_or(0)
}

// ---- Handlers (multi-entry: export name = event name) ----

/// RMW: read → bump → write through the in-module Collection (each op is
/// one emit round trip across the host bridge).
#[no_mangle]
pub extern "C" fn bump(_ptr: i32, _len: i32) -> i64 {
    let key = CounterKey { user_id: 1 };
    let next = read_count(&key) + 1;
    let mut doc = BTreeMap::new();
    doc.insert("count".to_string(), DynamicValue::UInt(next));
    counters().put_document(&key, &doc);
    reply_uint(next)
}

/// Reader: the current count (0 when absent).
#[no_mangle]
pub extern "C" fn peek(_ptr: i32, _len: i32) -> i64 {
    reply_uint(read_count(&CounterKey { user_id: 1 }))
}

/// The upload-time schema declaration (4.5b lifecycle): the author's
/// `interface_schema` export composes the storage block from the
/// compiled schema; the carrier merges it with the export-list
/// receives half (explicit export wins per the carrier contract).
#[no_mangle]
pub extern "C" fn interface_schema(_ptr: i32, _len: i32) -> i64 {
    let storage = actor_guest::schema::collection_entry::<CounterKey, CounterDoc>(
        "counters",
        &CollectionDecl { name: "counters", indexes: &[], reduces: &[] },
    );
    let schema = serde_json::json!({
        "receives": { "bump": {}, "peek": {} },
        "wildcard_receives": [],
        "storage": { "collections": storage },
    });
    // The carrier CBOR-decodes handler replies (the session's value
    // seam). The schema value crosses as CBOR-encoded JSON.
    let mut cbor = Vec::new();
    ciborium::into_writer(&schema, &mut cbor).expect("schema cbor");
    reply_bytes(&cbor)
}

// ---- reply plumbing: the same packed ABI the carrier expects ----

/// The guest allocator export the carrier REQUIRES (its host imports
/// write replies through it). Exported by name; the actor-guest crate's
/// `guest_alloc` extern resolves against this symbol in the same link.
#[no_mangle]
pub extern "C" fn aura_alloc(len: i32) -> i32 {
    bump_heap(len)
}

static mut HEAP: usize = 1024;

fn bump_heap(len: i32) -> i32 {
    unsafe {
        let p = HEAP;
        HEAP += len as usize;
        p as i32
    }
}

fn pack(ptr: u32, len: u32) -> i64 {
    ((ptr as i64) << 32) | (len as i64)
}

fn reply_bytes(bytes: &[u8]) -> i64 {
    unsafe {
        let ptr = aura_alloc(bytes.len() as i32);
        let dst = std::slice::from_raw_parts_mut(ptr as *mut u8, bytes.len());
        dst.copy_from_slice(bytes);
        pack(ptr as u32, bytes.len() as u32)
    }
}

fn reply_uint(n: u64) -> i64 {
    // Minimal CBOR uint (major type 0): < 24 self-encoded, ≤ u8 the
    // 0x18 prefix form, ≤ u16 the 0x19 form.
    let mut buf = Vec::new();
    if n < 24 {
        buf.push(n as u8);
    } else if n <= u8::MAX as u64 {
        buf.push(0x18);
        buf.push(n as u8);
    } else {
        buf.push(0x19);
        buf.extend_from_slice(&(n as u16).to_be_bytes());
    }
    reply_bytes(&buf)
}

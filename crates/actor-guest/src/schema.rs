//! Upload-time schema export: serialize the module's compiled
//! `CollectionSchema`s into the `storage.collections` block of
//! `interface_schema` — the object aura's `StorePlan::from_schema`
//! parses. This is the 4.5b lifecycle's "read the schema out of the
//! artifact" step for wasm: the schema IS the derive (code in the
//! artifact), the export only surfaces it as data at upload.
//!
//! The module author composes their own `interface_schema` export and
//! calls [`collection_entry`] per declared collection (the receives
//! half derives from the export list in the carrier, same as today);
//! this crate provides only the schema serialization.

use okm_core::schema::CollectionSchema;
use okm_core::{Document, KeyEncode};

/// One collection's access-method declarations — the parts of the
/// storage block that live OUTSIDE the schema data (the derive records
/// the fields, the author names the access methods).
pub struct CollectionDecl<'a> {
    pub name: &'a str,
    /// `(index name, slot, fields)` — serde form matching
    /// `StorePlan::from_schema`'s `indexes` list.
    pub indexes: &'a [(String, u16, Vec<String>)],
    /// `(reduce name, slot, group fields, kind JSON)`.
    pub reduces: &'a [(String, u16, Vec<String>, serde_json::Value)],
}

/// Serialize `<K, R>`'s compiled schema + the author's access-method
/// declarations into the `storage.collections` entry for one collection.
/// The JSON shape is EXACTLY what `StorePlan::from_schema` consumes:
/// `{ "<name>": { "schema": <CollectionSchema serde>, "indexes": [...],
/// "reduces": [...] } }`.
pub fn collection_entry<K: KeyEncode, R: Document<Key = K>>(
    name: &str,
    decl: &CollectionDecl<'_>,
) -> serde_json::Value {
    let schema = CollectionSchema::of::<K, R>();
    let mut entry = serde_json::json!({ "schema": schema });
    if !decl.indexes.is_empty() {
        let list: Vec<serde_json::Value> = decl
            .indexes
            .iter()
            .map(|(n, slot, fields)| {
                serde_json::json!({ "name": n, "slot": slot, "fields": fields, "kind": "plain" })
            })
            .collect();
        entry["indexes"] = serde_json::Value::Array(list);
    }
    if !decl.reduces.is_empty() {
        let list: Vec<serde_json::Value> = decl
            .reduces
            .iter()
            .map(|(n, slot, group, kind)| {
                serde_json::json!({ "name": n, "slot": slot, "group": group, "kind": kind })
            })
            .collect();
        entry["reduces"] = serde_json::Value::Array(list);
    }
    serde_json::json!({ name: entry })
}

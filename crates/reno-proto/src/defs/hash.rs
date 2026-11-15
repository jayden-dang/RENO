//! Canonical JSON and the content hash that identifies a version.

use serde_json::Value;
use sha2::{Digest, Sha256};

/// `value` as JSON text with every object's keys in sorted order and no whitespace.
///
/// Keys are sorted by their UTF-8 bytes, which is code point order. The order is spelled out
/// here and not left to `serde_json`, whose map type is sorted or insertion-ordered depending on
/// a feature that any dependency may turn on.
pub fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write(value, &mut out);
    out
}

fn write(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            out.push('{');
            for (i, (key, child)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                write(child, out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write(item, out);
            }
            out.push(']');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

/// The SHA-256 of [`canonical_json`], as 64 lowercase hex digits.
pub fn content_hash(value: &Value) -> String {
    let digest = Sha256::digest(canonical_json(value).as_bytes());
    digest
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            for nibble in [byte >> 4, byte & 0xF] {
                hex.push(char::from_digit(u32::from(nibble), 16).unwrap_or('0'));
            }
            hex
        })
}

use std::fmt;

use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;

/// SHA-256 of the canonical JSON form of a workflow document.
///
/// The hash identifies a definition. Two documents that parse to the same
/// JSON value (YAML or JSON, any key order, insignificant whitespace) share
/// a hash. A stored definition with this hash is not modified in place.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ContentHash([u8; 32]);

impl ContentHash {
    pub(crate) fn of(document: &Value) -> Self {
        let canonical = canonical_json(document);
        let digest = Sha256::digest(canonical.as_bytes());
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&digest);
        Self(bytes)
    }

    /// Raw digest bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Lowercase hexadecimal digest.
    pub fn to_hex(&self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut out = String::with_capacity(64);
        for byte in self.0 {
            out.push(HEX[(byte >> 4) as usize] as char);
            out.push(HEX[(byte & 0xf) as usize] as char);
        }
        out
    }
}

impl fmt::Debug for ContentHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "ContentHash({})", self.to_hex())
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.to_hex())
    }
}

/// Canonical JSON used as the hash input.
///
/// Object keys are sorted by raw UTF-8 bytes. Arrays keep their order.
/// There is no insignificant whitespace. Strings use JSON escaping.
pub fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(&mut out, value);
    out
}

fn write_canonical(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => out.push_str(&number.to_string()),
        Value::String(text) => {
            out.push_str(&serde_json::to_string(text).expect("string serialization"));
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical(out, item);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(key).expect("key serialization"));
                out.push(':');
                write_canonical(out, &map[*key]);
            }
            out.push('}');
        }
    }
}

#[cfg(test)]
mod tests {
    use sha2::Digest;
    use sha2::Sha256;

    use super::ContentHash;
    use super::canonical_json;

    #[test]
    fn sorts_object_keys_and_keeps_array_order() {
        let value = serde_json::json!({"b": 1, "a": [true, null, "x"], "c": {"z": 1, "y": false}});
        assert_eq!(
            canonical_json(&value),
            r#"{"a":[true,null,"x"],"b":1,"c":{"y":false,"z":1}}"#
        );
    }

    #[test]
    fn content_hash_is_sha256_of_canonical_json() {
        let value = serde_json::json!({"b": 1, "a": 2});
        let hash = ContentHash::of(&value);
        let digest = Sha256::digest(canonical_json(&value).as_bytes());
        assert_eq!(hash.as_bytes().as_slice(), digest.as_slice());
    }
}

use crate::causal_tree::branch::SchemaFingerprint;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::{self, Write};

#[derive(Default)]
struct ByteCount(usize);

impl Write for ByteCount {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct HashWriter<'a>(&'a mut Sha256);

impl Write for HashWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The same length-prefixed per-spec digest as `CompiledToolContext`.
pub(crate) fn tool_spec_fingerprint(name: &str, description: &str, parameters: &Value) -> SchemaFingerprint {
    let mut prefix = Sha256::new();
    for bytes in [name.as_bytes(), description.as_bytes()] {
        prefix.update(bytes.len().to_be_bytes());
        prefix.update(bytes);
    }
    let mut count = ByteCount::default();
    if serde_json::to_writer(&mut count, parameters).is_ok() {
        let mut hasher = prefix.clone();
        hasher.update(count.0.to_be_bytes());
        if serde_json::to_writer(HashWriter(&mut hasher), parameters).is_ok() {
            return SchemaFingerprint(hasher.finalize().into());
        }
    }
    // Match the prior `to_vec(...).unwrap_or_default()` fallback.
    prefix.update(0_usize.to_be_bytes());
    SchemaFingerprint(prefix.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaming_fingerprint_matches_owned_serialization() {
        for parameters in [
            serde_json::json!(null),
            serde_json::json!({"type":"object","properties":{"path":{"type":"string"}}}),
            serde_json::json!({"oneOf":[{"const":"a"},{"const":"b"}],"nested":{"items":[1,true,null]}}),
            serde_json::json!({"escaped":"quote \" slash \\ and \u{00E9}"}),
        ] {
            let bytes = serde_json::to_vec(&parameters).expect("JSON value serializes");
            let mut old = Sha256::new();
            for part in [b"file_read".as_slice(), b"PRX test schema".as_slice(), bytes.as_slice()] {
                old.update(part.len().to_be_bytes());
                old.update(part);
            }
            assert_eq!(
                tool_spec_fingerprint("file_read", "PRX test schema", &parameters),
                SchemaFingerprint(old.finalize().into())
            );
        }
    }
}

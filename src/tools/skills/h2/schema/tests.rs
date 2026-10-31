//! Lock the complete pre-builder schema contract, not just a few field names.
use super::*;
use sha2::{Digest, Sha256};
fn canonical(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Object(map) => {
            out.push(b'{');
            let mut fields: Vec<_> = map.iter().collect();
            fields.sort_unstable_by(|left, right| left.0.cmp(right.0));
            for (i, (key, value)) in fields.into_iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                serde_json::to_writer(&mut *out, key).unwrap();
                out.push(b':');
                canonical(value, out);
            }
            out.push(b'}');
        }
        Value::Array(values) => {
            out.push(b'[');
            for (i, value) in values.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                canonical(value, out);
            }
            out.push(b']');
        }
        _ => serde_json::to_writer(out, value).unwrap(),
    }
}
#[test]
fn complete_schema_matches_frozen_thirteen_action_contract_after_builder_split() {
    let schema = parameters_schema();
    let mut bytes = Vec::new();
    canonical(&schema, &mut bytes);
    assert_eq!(
        hex::encode(Sha256::digest(&bytes)),
        "a3a3d6382ebc7b984f99fa2a9240af803b9aa64b473a21598160a420b2934db3"
    );
    let definitions = schema.get("$defs").unwrap().as_object().unwrap();
    assert_eq!(definitions.len(), 37);
    assert_eq!(
        schema
            .pointer("/properties/action/enum")
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        13
    );
    assert_eq!(schema.get("allOf").unwrap().as_array().unwrap().len(), 13);
}

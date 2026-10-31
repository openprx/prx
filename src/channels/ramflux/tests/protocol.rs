use super::super::authorization::{self, EffectiveAutonomy};
use super::super::protocol::{self, Envelope, RpcId, args_hash, canonical_json, parse_envelope};
use crate::config::RamfluxAuthorizationMode;
use serde_json::json;

#[test]
fn request_round_trips() {
    let params = json!({"conversation_id": "c1", "text": "hello"});
    let bytes = protocol::encode_bridge_call(&RpcId::Number(7), "inbound.message", &params).expect("test: encode");
    let envelope = parse_envelope(&bytes).expect("test: parse");
    match envelope {
        Envelope::Request {
            id,
            method,
            params: parsed,
        } => {
            assert_eq!(id, RpcId::Number(7));
            assert_eq!(method, "inbound.message");
            assert_eq!(parsed["text"], "hello");
        }
        other => panic!("test: unexpected envelope {other:?}"),
    }
}

#[test]
fn result_and_error_round_trip() {
    let ok = protocol::encode_result(&RpcId::String("prx-1".into()), &json!({"ok": true})).expect("test: result");
    match parse_envelope(&ok).expect("test: parse result") {
        Envelope::Response { id, body } => {
            assert_eq!(id, RpcId::String("prx-1".into()));
            assert_eq!(body.expect("test: ok")["ok"], true);
        }
        other => panic!("test: unexpected {other:?}"),
    }
    let err = protocol::encode_error(
        &RpcId::Number(3),
        &protocol::RpcErrorBody::new(-32602, "invalid bridge params"),
    )
    .expect("test: error");
    match parse_envelope(&err).expect("test: parse error") {
        Envelope::Response { body, .. } => {
            let error = body.expect_err("test: remote error");
            assert_eq!(error.code, -32602);
        }
        other => panic!("test: unexpected {other:?}"),
    }
}

#[test]
fn unknown_field_is_rejected() {
    let bytes = br#"{"jsonrpc":"2.0","id":1,"method":"bridge.ping","params":{},"extra":1}"#;
    let error = parse_envelope(bytes).expect_err("test: extra field");
    assert!(error.to_string().contains("unknown"), "{error}");
}

#[test]
fn canonical_json_sorts_object_keys() {
    let value = json!({"b": 1, "a": {"d": true, "c": "x"}});
    let encoded = canonical_json(&value).expect("test: canonical");
    assert_eq!(encoded, br#"{"a":{"c":"x","d":true},"b":1}"#);
}

#[test]
fn args_hash_is_stable_sha256() {
    let first = args_hash(&json!({"z": 1, "a": "secret"})).expect("test: hash");
    let second = args_hash(&json!({"a": "secret", "z": 1})).expect("test: hash");
    assert_eq!(first, second);
    assert_eq!(first.len(), 64);
}

#[test]
fn hello_payload_rejects_unknown_fields() {
    let value = json!({
        "protocol_version": "1",
        "agent_account_id": "agent-a",
        "authorization": "supervised",
        "token": "nope"
    });
    let error = serde_json::from_value::<protocol::BridgeHello>(value).expect_err("test: unknown");
    assert!(error.to_string().contains("token"), "{error}");
}

#[test]
fn authorization_matrix_never_promotes_supervised() {
    assert_eq!(
        authorization::resolve(RamfluxAuthorizationMode::FollowRf, None),
        EffectiveAutonomy::Supervised
    );
    assert_eq!(
        authorization::resolve(RamfluxAuthorizationMode::Full, None),
        EffectiveAutonomy::Supervised
    );
    assert_eq!(
        authorization::resolve(
            RamfluxAuthorizationMode::FollowRf,
            Some(protocol::HelloAuthorization::Supervised)
        ),
        EffectiveAutonomy::Supervised
    );
    assert_eq!(
        authorization::resolve(
            RamfluxAuthorizationMode::Full,
            Some(protocol::HelloAuthorization::Supervised)
        ),
        EffectiveAutonomy::Supervised
    );
    assert_eq!(
        authorization::resolve(
            RamfluxAuthorizationMode::Supervised,
            Some(protocol::HelloAuthorization::Full)
        ),
        EffectiveAutonomy::Supervised
    );
    assert_eq!(
        authorization::resolve(
            RamfluxAuthorizationMode::FollowRf,
            Some(protocol::HelloAuthorization::Full)
        ),
        EffectiveAutonomy::Full
    );
    assert_eq!(
        authorization::resolve(RamfluxAuthorizationMode::Full, Some(protocol::HelloAuthorization::Full)),
        EffectiveAutonomy::Full
    );
    assert_eq!(
        authorization::doctor_label(
            RamfluxAuthorizationMode::FollowRf,
            Some(protocol::HelloAuthorization::Full)
        ),
        "follow_rf→full"
    );
}

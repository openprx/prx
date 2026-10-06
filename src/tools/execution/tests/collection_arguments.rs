#![cfg(test)]
use super::*;

fn descriptor() -> ToolDescriptor {
    tool_descriptor(
        "prx_collection_fixture",
        ToolSpec {
            name: "prx_collection_fixture".to_owned(),
            description: "Collection argument contract fixture".to_owned(),
            parameters: serde_json::json!({
                "type":"object",
                "properties": {"path":{"type":"string"},"repeat":{"type":"integer","minimum":1}},
                "required":["path","repeat"],
                "additionalProperties":false
            }),
        },
        ToolTier::Standard,
        Vec::new(),
        |_| ToolAdapterKind::Native,
        CapabilityAvailability::ready("isolated schema fixture"),
    )
}

#[test]
fn collection_preserves_the_effect_contract_without_injecting_authority() -> anyhow::Result<()> {
    let descriptor = descriptor();
    let input = serde_json::json!({"path":"project.txt","repeat":2.0});
    let original = input.clone();
    let normalized =
        normalize_collection_arguments(&input, &descriptor, ToolSchemaDialect::Raw).map_err(anyhow::Error::msg)?;
    assert_eq!(input, original);
    assert_eq!(normalized.get("repeat").and_then(serde_json::Value::as_u64), Some(2));
    let object = normalized
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("normalized object missing"))?;
    assert_eq!(object.len(), 2);
    assert!(object.keys().all(|key| !is_runtime_only_arg(key)));
    for invalid in [
        serde_json::json!(null),
        serde_json::json!(["project.txt"]),
        serde_json::json!({"path":"project.txt"}),
        serde_json::json!({"path":"project.txt","repeat":0}),
        serde_json::json!({"path":"project.txt","repeat":1,"extra":true}),
    ] {
        assert!(normalize_collection_arguments(&invalid, &descriptor, ToolSchemaDialect::Raw).is_err());
    }
    Ok(())
}

#[test]
fn collection_rejects_reserved_scope_and_approval_fields_before_persistence() -> anyhow::Result<()> {
    let descriptor = descriptor();
    for key in [
        "_zc_scope",
        "_zc_scope_trusted",
        "_prx_scope_trusted",
        RUNTIME_APPROVAL_GRANTED_ARG,
        RUNTIME_APPROVAL_GRANT_ARG,
        "_zc_future_authority",
        "_prx_future_authority",
    ] {
        let mut input = serde_json::json!({"path":"project.txt","repeat":1});
        input
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("fixture object missing"))?
            .insert(key.to_owned(), serde_json::json!(true));
        assert_eq!(
            normalize_collection_arguments(&input, &descriptor, ToolSchemaDialect::Raw)
                .err()
                .as_deref(),
            Some("collection arguments contain reserved runtime fields")
        );
    }
    Ok(())
}

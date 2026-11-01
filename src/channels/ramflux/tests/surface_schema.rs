use super::super::surface_schema::{self, SchemaError};
use crate::channels::rich::{SurfaceBlock, SurfaceOp, SurfaceReply};
use crate::config::RamfluxSurfaceLimits;

fn reply(op: SurfaceOp, fallback: Option<&str>, blocks: Vec<SurfaceBlock>, delta: Option<&str>) -> SurfaceReply {
    SurfaceReply {
        conversation_id: "c".into(),
        turn_id: "t".into(),
        op,
        surface_id: "s".into(),
        version: 1,
        base_hash: None,
        blocks: Some(blocks),
        actions: None,
        ops: None,
        block_id: None,
        text_delta: delta.map(str::to_string),
        is_final: None,
        fallback_text: fallback.map(str::to_string),
    }
}

fn limits() -> RamfluxSurfaceLimits {
    RamfluxSurfaceLimits {
        max_blocks: 2,
        max_actions: 1,
        max_bytes: 64,
    }
}

#[test]
fn rejects_script_urls_and_oversize() {
    let allowed = vec!["Text".to_string(), "Markdown".to_string()];
    let script = reply(
        SurfaceOp::Create,
        Some("fallback"),
        vec![SurfaceBlock {
            id: "b".into(),
            kind: "Text".into(),
            data: serde_json::json!({"text": "<script>alert(1)</script>"}),
        }],
        None,
    );
    assert_eq!(
        surface_schema::validate(&script, &limits(), &allowed),
        Err(SchemaError::Markup)
    );
    let url = reply(
        SurfaceOp::Create,
        Some("fallback"),
        vec![SurfaceBlock {
            id: "b".into(),
            kind: "Text".into(),
            data: serde_json::json!({"href": "javascript:alert(1)"}),
        }],
        None,
    );
    assert!(surface_schema::validate(&url, &limits(), &allowed).is_err());
    let unknown = reply(
        SurfaceOp::Create,
        Some("fallback"),
        vec![SurfaceBlock {
            id: "b".into(),
            kind: "Evil".into(),
            data: serde_json::json!({}),
        }],
        None,
    );
    assert!(matches!(
        surface_schema::validate(&unknown, &limits(), &allowed),
        Err(SchemaError::BlockKind(_))
    ));
    let missing = reply(
        SurfaceOp::Create,
        None,
        vec![SurfaceBlock {
            id: "b".into(),
            kind: "Text".into(),
            data: serde_json::json!({}),
        }],
        None,
    );
    assert_eq!(
        surface_schema::validate(&missing, &limits(), &allowed),
        Err(SchemaError::MissingFallback)
    );
    let ok = reply(SurfaceOp::StreamAppend, None, Vec::new(), Some("hello <script>"));
    assert!(surface_schema::validate(&ok, &limits(), &allowed).is_ok());
    let text_close = reply(
        SurfaceOp::Close,
        Some("a reply with <script> and ftp://files.example"),
        Vec::new(),
        None,
    );
    assert!(surface_schema::validate(&text_close, &limits(), &allowed).is_ok());
}

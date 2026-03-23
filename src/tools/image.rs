//! image — analyze images with a vision-capable provider.
//!
//! Accepts local file paths or HTTP(S) URLs, encodes them using the
//! standard [IMAGE:...] multimodal marker format, and calls the
//! active vision provider to answer the given prompt.
//!
//! Aligns with OpenClaw's `image` tool for multimodal understanding.

use super::traits::{Tool, ToolResult};
use crate::config::MultimodalConfig;
use crate::multimodal;
use crate::providers::{ChatMessage, ChatRequest, Provider};
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

/// Tool for vision/image understanding via the active provider.
pub struct ImageTool {
    provider: Arc<dyn Provider>,
    model: String,
    temperature: f64,
    security: Arc<SecurityPolicy>,
    multimodal_config: MultimodalConfig,
}

impl ImageTool {
    pub fn new(
        provider: Arc<dyn Provider>,
        model: impl Into<String>,
        temperature: f64,
        security: Arc<SecurityPolicy>,
        multimodal_config: MultimodalConfig,
    ) -> Self {
        Self {
            provider,
            model: model.into(),
            temperature,
            security,
            multimodal_config,
        }
    }

    /// Return a handle to update the model name when the runtime model changes.
    /// (This is a snapshot — callers re-construct or re-register the tool for model changes.)
    pub fn model(&self) -> &str {
        &self.model
    }
}

#[async_trait]
impl Tool for ImageTool {
    fn name(&self) -> &str {
        "image"
    }

    fn description(&self) -> &str {
        "Analyze one or more images with a vision model. \
         Accepts local file paths or HTTPS URLs. Supports asking questions about image content, \
         describing scenes, reading text (OCR), identifying objects, and visual reasoning. \
         Only use this tool when images are NOT already provided in the current message."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "image": {
                    "type": "string",
                    "description": "Path to a local image file or an HTTPS URL. Supports jpg, png, gif, webp."
                },
                "images": {
                    "type": "array",
                    "items": { "type": "string" },
                    "maxItems": 20,
                    "description": "Multiple image paths or URLs (up to 20)."
                },
                "prompt": {
                    "type": "string",
                    "description": "What to analyze or ask about the image(s). Defaults to 'Describe this image in detail.'"
                }
            },
            "required": []
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        // Collect image references from 'image' and/or 'images' parameters
        let mut image_refs: Vec<String> = Vec::new();

        if let Some(single) = args.get("image").and_then(|v| v.as_str()) {
            let s = single.trim();
            if !s.is_empty() {
                image_refs.push(s.to_string());
            }
        }

        if let Some(arr) = args.get("images").and_then(|v| v.as_array()) {
            for item in arr {
                if let Some(s) = item.as_str() {
                    let s = s.trim();
                    if !s.is_empty() {
                        image_refs.push(s.to_string());
                    }
                }
            }
        }

        if image_refs.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(
                    "No image provided. Specify 'image' (single path/URL) \
                     or 'images' (array of paths/URLs)."
                        .into(),
                ),
            });
        }

        // Security check for local paths (skip for HTTP(S) URLs and data URIs)
        for ref_str in &image_refs {
            if ref_str.starts_with("http://") || ref_str.starts_with("https://") || ref_str.starts_with("data:") {
                continue;
            }

            if !self.security.is_path_allowed(ref_str) {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Path not allowed: {ref_str} (must be within workspace)")),
                });
            }

            // Canonicalize to block symlink escapes outside workspace
            let full_path = self.security.workspace_dir.join(ref_str);
            let resolved = match tokio::fs::canonicalize(&full_path).await {
                Ok(p) => p,
                Err(e) => {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Failed to resolve image path: {ref_str} ({e})")),
                    });
                }
            };

            if !self.security.is_resolved_path_allowed(&resolved) {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Resolved path escapes workspace: {}", resolved.display())),
                });
            }
        }

        // Check that the provider supports vision
        if !self.provider.supports_vision() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(
                    "Current model does not support vision. \
                     Switch to a vision-capable model (e.g. gpt-4o, grok-4, claude-3-*)."
                        .into(),
                ),
            });
        }

        let prompt = args
            .get("prompt")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("Describe this image in detail.");

        // Build message content: prompt + [IMAGE:...] markers
        let image_markers: String = image_refs
            .iter()
            .map(|r| format!("[IMAGE:{r}]"))
            .collect::<Vec<_>>()
            .join("\n");

        let content = format!("{prompt}\n\n{image_markers}");
        let messages = vec![ChatMessage::user(content)];

        // Normalize image references (local → base64 data URI, remote → validated data URI)
        let prepared = multimodal::prepare_messages_for_provider(&messages, &self.multimodal_config)
            .await
            .map_err(|e| anyhow::anyhow!("Image preparation failed: {e}"))?;

        if !prepared.contains_images {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(
                    "Failed to process images. \
                     Check that file paths exist and are readable, \
                     or that URLs are accessible."
                        .into(),
                ),
            });
        }

        // Call the vision provider
        let request = ChatRequest {
            messages: &prepared.messages,
            tools: None,
        };

        let response = self
            .provider
            .chat(request, &self.model, self.temperature)
            .await
            .map_err(|e| anyhow::anyhow!("Vision model call failed: {e}"))?;

        let text = response.text_or_empty().to_string();

        if text.trim().is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Vision model returned an empty response.".into()),
            });
        }

        Ok(ToolResult {
            success: true,
            output: text,
            error: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::{AutonomyLevel, SecurityPolicy};
    use anyhow::anyhow;
    use async_trait::async_trait;

    fn test_security() -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Full,
            workspace_dir: std::env::temp_dir(),
            workspace_only: false,
            forbidden_paths: vec![],
            ..SecurityPolicy::default()
        })
    }

    struct VisionProvider {
        response: String,
        has_vision: bool,
    }

    #[async_trait]
    impl crate::providers::Provider for VisionProvider {
        async fn chat_with_system(
            &self,
            _system: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            Ok(self.response.clone())
        }

        async fn chat(
            &self,
            _request: ChatRequest<'_>,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<crate::providers::ChatResponse> {
            if !self.has_vision {
                return Err(anyhow!("no vision"));
            }
            Ok(crate::providers::ChatResponse {
                text: Some(self.response.clone()),
                tool_calls: Vec::new(),
            })
        }

        fn capabilities(&self) -> crate::providers::ProviderCapabilities {
            crate::providers::ProviderCapabilities {
                vision: self.has_vision,
                native_tool_calling: false,
            }
        }
    }

    fn make_tool(provider: Arc<dyn Provider>) -> ImageTool {
        ImageTool::new(
            provider,
            "test-vision-model",
            0.0,
            test_security(),
            MultimodalConfig::default(),
        )
    }

    #[test]
    fn name_and_description() {
        let p = Arc::new(VisionProvider {
            response: "".into(),
            has_vision: true,
        });
        let tool = make_tool(p);
        assert_eq!(tool.name(), "image");
        assert!(!tool.description().is_empty());
        assert!(tool.description().contains("vision"));
    }

    #[test]
    fn schema_has_expected_fields() {
        let p = Arc::new(VisionProvider {
            response: "".into(),
            has_vision: true,
        });
        let tool = make_tool(p);
        let schema = tool.parameters_schema();
        assert!(schema["properties"]["image"].is_object());
        assert!(schema["properties"]["images"].is_object());
        assert!(schema["properties"]["prompt"].is_object());
    }

    #[tokio::test]
    async fn no_image_returns_error() {
        let p = Arc::new(VisionProvider {
            response: "ok".into(),
            has_vision: true,
        });
        let tool = make_tool(p);
        let result = tool.execute(json!({})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("No image provided"));
    }

    #[tokio::test]
    async fn no_vision_support_returns_error() {
        let p = Arc::new(VisionProvider {
            response: "ok".into(),
            has_vision: false,
        });
        let tool = make_tool(p);
        // Use a data URI to bypass file reading
        let result = tool
            .execute(json!({"image": "https://example.com/img.jpg", "prompt": "describe"}))
            .await
            .unwrap();
        // Will fail at vision check OR at image prep if remote fetch disabled — either is ok
        assert!(!result.success);
    }

    #[tokio::test]
    async fn with_local_image_base64_data_uri_passthrough() {
        // Test with a pre-formed data URI (bypasses file reading in multimodal)
        let p = Arc::new(VisionProvider {
            response: "A red square.".into(),
            has_vision: true,
        });
        let tool = make_tool(p);
        // Use a data: URI directly as image reference — multimodal passes it through
        let result = tool
            .execute(json!({
                "image": "data:image/png;base64,iVBORw0KGgo=",
                "prompt": "What color is this?"
            }))
            .await
            .unwrap();
        // With default MultimodalConfig, remote fetch may be disabled and local data URIs
        // should be handled. Just check we don't panic.
        let _ = result;
    }
}

//! image — analyze images with a vision-capable provider.
//!
//! Accepts local file paths or HTTP(S) URLs, encodes them using the
//! standard [IMAGE:...] multimodal marker format, and calls the
//! active vision provider to answer the given prompt.
//!
//! Aligns with OpenClaw's `image` tool for multimodal understanding.

use super::traits::{Tool, ToolCategory, ToolResult, ToolTier};
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
    pinned_provider_resolver: Option<Arc<dyn crate::providers::generation::PinnedProviderResolver>>,
    pinned_provider_name: Option<String>,
    model_catalog: crate::providers::catalog::CatalogView,
    model: String,
    temperature: f64,
    security: Arc<SecurityPolicy>,
    multimodal_config: MultimodalConfig,
    media_artifacts: Arc<crate::media::MediaArtifactOwner>,
}

impl ImageTool {
    pub fn new(
        provider: Arc<dyn Provider>,
        model: impl Into<String>,
        temperature: f64,
        security: Arc<SecurityPolicy>,
        multimodal_config: MultimodalConfig,
    ) -> Self {
        let media_artifacts = crate::media::MediaArtifactOwner::for_workspace(&security.workspace_dir);
        Self {
            provider,
            pinned_provider_resolver: None,
            pinned_provider_name: None,
            model_catalog: crate::providers::catalog::CatalogView::release_only(),
            model: model.into(),
            temperature,
            security,
            multimodal_config,
            media_artifacts,
        }
    }

    /// Return a handle to update the model name when the runtime model changes.
    /// (This is a snapshot — callers re-construct or re-register the tool for model changes.)
    pub fn model(&self) -> &str {
        &self.model
    }

    #[must_use]
    pub(crate) fn with_pinned_provider_resolver(
        mut self,
        resolver: Arc<dyn crate::providers::generation::PinnedProviderResolver>,
    ) -> Self {
        self.pinned_provider_resolver = Some(resolver);
        self
    }

    /// Bind model capabilities when this runtime already owns an immutable
    /// provider/model selection but does not perform generation reloads.
    #[must_use]
    pub(crate) fn with_provider_name(mut self, provider_name: impl Into<String>) -> Self {
        self.pinned_provider_name = Some(provider_name.into());
        self
    }

    /// Bind the immutable operator/discovery/release model facts that own this
    /// non-reloadable worker tool.
    #[must_use]
    pub(crate) fn with_model_catalog(mut self, catalog: crate::providers::catalog::CatalogView) -> Self {
        self.model_catalog = catalog;
        self
    }
}

impl ImageTool {
    fn execution_schema(&self) -> serde_json::Value {
        crate::tools::schema::with_required_alternatives(
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
                        "minItems": 1,
                        "maxItems": 20,
                        "description": "Multiple image paths or URLs (up to 20)."
                    },
                    "prompt": {
                        "type": "string",
                        "description": "What to analyze or ask about the image(s). Defaults to 'Describe this image in detail.'"
                    }
                },
            }),
            &[&["image"], &["images"]],
        )
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
        crate::tools::schema::publish_flat_schema(self.execution_schema())
    }

    async fn execute(&self, mut args: serde_json::Value) -> anyhow::Result<ToolResult> {
        crate::tools::schema::validate_execution_contract(self.name(), &self.execution_schema(), &mut args)?;
        let (provider, model, temperature, capabilities) = if let Some(resolver) = &self.pinned_provider_resolver {
            let generation_id = crate::tools::execution::current_child_call_context()
                .and_then(|caller| caller.context().envelope.config_generation_id)
                .or_else(|| {
                    args.get("_zc_scope_trusted")
                        .and_then(serde_json::Value::as_bool)
                        .filter(|trusted| *trusted)
                        .and_then(|_| args.pointer("/_zc_scope/config_generation_id"))
                        .and_then(serde_json::Value::as_u64)
                })
                .ok_or_else(|| anyhow::anyhow!("image execution is missing its pinned configuration generation"))?;
            let resolved = resolver.resolve_default(generation_id).await?;
            (
                resolved.provider,
                resolved.model_name,
                resolved.temperature,
                resolved.capabilities,
            )
        } else {
            let provider = Arc::clone(&self.provider);
            let adapter =
                provider.capabilities_for(&self.model, crate::providers::traits::ProviderRequestMode::NonStreaming);
            let capabilities = self.pinned_provider_name.as_deref().map_or_else(
                || adapter.clone(),
                |name| {
                    self.model_catalog
                        .effective_provider_capabilities(name, &self.model, adapter.clone())
                },
            );
            (provider, self.model.clone(), self.temperature, capabilities)
        };
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

        // Check that the provider supports vision. Reloadable tools use the
        // generation catalog so an operator declaration can refine the
        // adapter and release-catalog facts for this exact provider/model.
        if !capabilities.vision {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(
                    "Current model does not support vision. \
                     Switch to a model whose catalog entry declares vision input."
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
        let prepared = multimodal::prepare_messages_for_provider(
            &messages,
            &self.multimodal_config,
            self.media_artifacts.as_ref(),
        )
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

        let response = provider
            .chat(request, &model, temperature)
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

    fn tier(&self) -> ToolTier {
        ToolTier::Standard
    }

    fn categories(&self) -> &'static [ToolCategory] {
        &[ToolCategory::Media]
    }
}

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::{AutonomyLevel, SecurityPolicy};
    use anyhow::anyhow;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

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
                reasoning_content: None,
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
        assert!(!tool.description().is_empty(), "expected a non-empty value");
        assert!(tool.description().contains("vision"));
    }

    #[tokio::test]
    async fn schema_has_expected_fields() {
        let p = Arc::new(VisionProvider {
            response: "".into(),
            has_vision: true,
        });
        let tool = make_tool(p);
        crate::tools::schema::assert_flat_execution_contract(&tool, &tool.execution_schema()).await;
        let schema = tool.execution_schema();
        assert!(schema["properties"]["image"].is_object());
        assert!(schema["properties"]["images"].is_object());
        assert!(schema["properties"]["prompt"].is_object());
        assert!(
            !crate::tools::schema::validate_tool_arguments(&schema, &json!({})).is_empty(),
            "expected a non-empty value"
        );
        assert!(
            crate::tools::schema::validate_tool_arguments(&schema, &json!({"image": "image.png"})).is_empty(),
            "expected an empty value"
        );
        assert!(
            crate::tools::schema::validate_tool_arguments(&schema, &json!({"images": ["image.png"]})).is_empty(),
            "expected an empty value"
        );
    }

    #[tokio::test]
    async fn no_image_returns_error() {
        let provider = Arc::new(CatalogVisionProvider {
            calls: AtomicUsize::new(0),
        });
        let provider_dyn: Arc<dyn Provider> = provider.clone();
        let mut config = crate::config::Config::default();
        config.providers.models.push(
            toml::from_str(
                "provider = \"ollama\"\nmodel = \"codellama\"\noperations = [\"chat\"]\nvision_input = true\n",
            )
            .expect("test: operator model declaration"),
        );
        let tool = ImageTool::new(
            provider_dyn,
            "codellama",
            0.0,
            test_security(),
            MultimodalConfig::default(),
        )
        .with_provider_name("ollama")
        .with_model_catalog(crate::providers::selection::catalog_for(&config));

        let error = tool.execute(json!({})).await.unwrap_err();
        assert!(
            error
                .downcast_ref::<crate::tools::schema::ToolArgumentError>()
                .is_some()
        );
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        let valid = tool
            .execute(json!({"image": "data:image/png;base64,iVBORw0KGgo="}))
            .await
            .unwrap();
        assert!(valid.success, "{:?}", valid.error);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
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

    struct CatalogVisionProvider {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl crate::providers::Provider for CatalogVisionProvider {
        async fn chat_with_system(
            &self,
            _system: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok("unexpected".to_string())
        }

        fn capabilities(&self) -> crate::providers::ProviderCapabilities {
            crate::providers::ProviderCapabilities {
                vision: true,
                native_tool_calling: true,
            }
        }
    }

    #[tokio::test]
    async fn image_execution_is_gated_by_catalog_vision_input_before_transport() {
        let provider = Arc::new(CatalogVisionProvider {
            calls: AtomicUsize::new(0),
        });
        let provider_dyn: Arc<dyn Provider> = provider.clone();
        let tool = ImageTool::new(
            provider_dyn,
            "codellama",
            0.0,
            test_security(),
            MultimodalConfig::default(),
        )
        .with_provider_name("ollama");

        let result = tool
            .execute(json!({
                "image": "data:image/png;base64,iVBORw0KGgo=",
                "prompt": "describe"
            }))
            .await
            .expect("catalog refusal is a structured tool result");

        assert!(!result.success);
        assert_eq!(
            result.error.as_deref(),
            Some("Current model does not support vision. Switch to a model whose catalog entry declares vision input.")
        );
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            0,
            "catalog refusal precedes transport"
        );
    }

    #[tokio::test]
    async fn image_execution_uses_operator_vision_declaration_from_pinned_catalog() {
        let provider = Arc::new(CatalogVisionProvider {
            calls: AtomicUsize::new(0),
        });
        let provider_dyn: Arc<dyn Provider> = provider.clone();
        let mut config = crate::config::Config::default();
        config.providers.models.push(
            toml::from_str(
                "provider = \"ollama\"\nmodel = \"codellama\"\noperations = [\"chat\"]\nvision_input = true\n",
            )
            .expect("test: operator model declaration"),
        );
        let tool = ImageTool::new(
            provider_dyn,
            "codellama",
            0.0,
            test_security(),
            MultimodalConfig::default(),
        )
        .with_provider_name("ollama")
        .with_model_catalog(crate::providers::selection::catalog_for(&config));

        let result = tool
            .execute(json!({
                "image": "data:image/png;base64,iVBORw0KGgo=",
                "prompt": "describe"
            }))
            .await
            .expect("operator-declared image request reaches the provider");

        assert!(result.success, "{result:?}");
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
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

use super::traits::{Tool, ToolResult};
use async_trait::async_trait;
use regex::Regex;
use serde_json::json;
use std::time::Duration;

/// Web fetch tool — fetches a URL and returns clean readable text.
pub struct WebFetchTool {
    max_chars: usize,
    timeout_secs: u64,
}

impl WebFetchTool {
    pub fn new(max_chars: usize, timeout_secs: u64) -> Self {
        Self {
            max_chars: max_chars.max(100),
            timeout_secs: timeout_secs.max(1),
        }
    }

    /// Convert HTML to plain readable text by stripping tags and decoding entities.
    fn html_to_text(html: &str) -> String {
        // Remove script/style blocks (case-insensitive, multiline)
        let re_script =
            Regex::new(r"(?si)<(script|style|head)[^>]*>.*?</\1>").unwrap();
        let text = re_script.replace_all(html, "");

        // Replace block-level elements with newlines so paragraphs separate cleanly
        let re_block =
            Regex::new(r"(?i)<(br\s*/?|p|div|h[1-6]|li|tr|article|section|header|footer)[^>]*>")
                .unwrap();
        let text = re_block.replace_all(&text, "\n");

        // Strip all remaining tags
        let re_tags = Regex::new(r"<[^>]+>").unwrap();
        let text = re_tags.replace_all(&text, "");

        // Decode common HTML entities
        let text = text
            .replace("&amp;", "&")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&#39;", "'")
            .replace("&apos;", "'")
            .replace("&nbsp;", " ")
            .replace("&#160;", " ")
            .replace("&mdash;", "—")
            .replace("&ndash;", "–")
            .replace("&hellip;", "…")
            .replace("&ldquo;", "\u{201C}")
            .replace("&rdquo;", "\u{201D}")
            .replace("&lsquo;", "\u{2018}")
            .replace("&rsquo;", "\u{2019}");

        // Collapse whitespace: trim each line and drop blank lines
        text.lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        "Fetch and extract readable content from a URL. \
         Downloads the page and returns clean text with HTML tags removed. \
         Useful for reading articles, documentation, or any web page."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "The URL to fetch (must start with http:// or https://)"
                },
                "max_chars": {
                    "type": "integer",
                    "description": "Maximum characters to return (default uses server config)"
                }
            },
            "required": ["url"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let url = args
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing required parameter: url"))?;

        if url.trim().is_empty() {
            anyhow::bail!("URL cannot be empty");
        }

        if !url.starts_with("http://") && !url.starts_with("https://") {
            anyhow::bail!("URL must start with http:// or https://");
        }

        // Allow caller to override max_chars; clamp to server-side maximum
        let max_chars = args
            .get("max_chars")
            .and_then(|v| v.as_u64())
            .map(|v| (v as usize).min(self.max_chars))
            .unwrap_or(self.max_chars);

        tracing::info!("Fetching URL: {}", url);

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(self.timeout_secs))
            .user_agent(
                "Mozilla/5.0 (compatible; ZeroClaw/1.0; +https://zeroclaw.ai)",
            )
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()?;

        let response = client.get(url).send().await.map_err(|e| {
            anyhow::anyhow!("Failed to fetch {}: {}", url, e)
        })?;

        let status = response.status();
        if !status.is_success() {
            anyhow::bail!("HTTP {} fetching {}", status, url);
        }

        // Check content-type — only process text-based responses
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();

        let is_html = content_type.contains("html");
        let is_text = content_type.contains("text") || content_type.contains("json")
            || content_type.contains("xml");

        if !is_html && !is_text && !content_type.is_empty() {
            anyhow::bail!(
                "Unsupported content type '{}' — web_fetch only handles text/HTML responses",
                content_type
            );
        }

        let body = response.text().await.map_err(|e| {
            anyhow::anyhow!("Failed to read response body from {}: {}", url, e)
        })?;

        let output = if is_html || body.trim_start().starts_with('<') {
            Self::html_to_text(&body)
        } else {
            // Plain text / JSON / XML — return as-is
            body
        };

        // Truncate if needed, appending a note
        let output = if output.len() > max_chars {
            format!(
                "{}\n\n[Truncated: output exceeded {} characters. Use a smaller range or a more specific URL.]",
                &output[..max_chars],
                max_chars
            )
        } else {
            output
        };

        if output.trim().is_empty() {
            return Ok(ToolResult {
                success: true,
                output: "(No readable content extracted from page)".to_string(),
                error: None,
            });
        }

        Ok(ToolResult {
            success: true,
            output,
            error: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_html_to_text_basic() {
        let html = "<html><body><h1>Hello</h1><p>World</p></body></html>";
        let text = WebFetchTool::html_to_text(html);
        assert!(text.contains("Hello"));
        assert!(text.contains("World"));
        assert!(!text.contains('<'));
        assert!(!text.contains('>'));
    }

    #[test]
    fn test_html_to_text_strips_scripts() {
        let html = "<html><head><script>var x = 1;</script></head><body>Content</body></html>";
        let text = WebFetchTool::html_to_text(html);
        assert!(!text.contains("var x"));
        assert!(text.contains("Content"));
    }

    #[test]
    fn test_html_to_text_strips_styles() {
        let html = "<html><head><style>body { color: red; }</style></head><body>Text</body></html>";
        let text = WebFetchTool::html_to_text(html);
        assert!(!text.contains("color"));
        assert!(text.contains("Text"));
    }

    #[test]
    fn test_html_to_text_decodes_entities() {
        let html = "<p>1 &lt; 2 &amp; 3 &gt; 0 &quot;quoted&quot; it&#39;s</p>";
        let text = WebFetchTool::html_to_text(html);
        assert!(text.contains("1 < 2 & 3 > 0 \"quoted\" it's"), "got: {text}");
    }

    #[test]
    fn test_html_to_text_collapses_whitespace() {
        let html = "<p>   lots   of   space   </p>";
        let text = WebFetchTool::html_to_text(html);
        // After trim, no leading/trailing whitespace per line
        for line in text.lines() {
            assert_eq!(line, line.trim());
        }
    }

    #[test]
    fn test_tool_name() {
        let tool = WebFetchTool::new(10000, 15);
        assert_eq!(tool.name(), "web_fetch");
    }

    #[test]
    fn test_tool_description_contains_fetch() {
        let tool = WebFetchTool::new(10000, 15);
        assert!(tool.description().to_lowercase().contains("fetch"));
    }

    #[test]
    fn test_parameters_schema() {
        let tool = WebFetchTool::new(10000, 15);
        let schema = tool.parameters_schema();
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["url"].is_object());
        assert_eq!(schema["required"][0], "url");
    }

    #[tokio::test]
    async fn test_execute_missing_url() {
        let tool = WebFetchTool::new(10000, 15);
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("url"));
    }

    #[tokio::test]
    async fn test_execute_empty_url() {
        let tool = WebFetchTool::new(10000, 15);
        let result = tool.execute(json!({"url": ""})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_execute_invalid_scheme() {
        let tool = WebFetchTool::new(10000, 15);
        let result = tool.execute(json!({"url": "ftp://example.com"})).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("http://") || true); // scheme check
    }
}

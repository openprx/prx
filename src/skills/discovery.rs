//! Scout — skill discovery from external sources.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::debug;

/// Production GitHub endpoint; request parameters cannot override it.
pub(crate) const GITHUB_API_BASE: &str = "https://api.github.com";

/// Safe discovery errors suitable for the gateway response.
#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    #[error("Unsupported skill discovery source '{0}'")]
    UnsupportedSource(String),
    #[error("GitHub search rate limit reached; try again later")]
    RateLimited,
    #[error("GitHub search failed: {0}")]
    Upstream(UpstreamFailure),
}

/// Fixed upstream failure reasons never include request or response secrets.
#[derive(Debug, thiserror::Error)]
pub enum UpstreamFailure {
    #[error("HTTP {0}")]
    Http(u16),
    #[error("request failed")]
    Request,
    #[error("invalid response")]
    InvalidResponse,
}

// ---------------------------------------------------------------------------
// ScoutSource
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScoutSource {
    GitHub,
    ClawHub,
    HuggingFace,
}

impl std::str::FromStr for ScoutSource {
    type Err = DiscoveryError;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Ok(match s.to_lowercase().as_str() {
            "github" => Self::GitHub,
            "clawhub" => Self::ClawHub,
            "huggingface" | "hf" => Self::HuggingFace,
            _ => return Err(DiscoveryError::UnsupportedSource(s.to_string())),
        })
    }
}

// ---------------------------------------------------------------------------
// ScoutResult
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoutResult {
    pub name: String,
    pub url: String,
    pub description: String,
    pub stars: u64,
    pub language: Option<String>,
    pub updated_at: Option<DateTime<Utc>>,
    pub source: ScoutSource,
    /// Owner / org extracted from the URL or API response.
    pub owner: String,
    /// Whether the repo has a license file.
    pub has_license: bool,
}

// ---------------------------------------------------------------------------
// Scout trait
// ---------------------------------------------------------------------------

#[async_trait]
pub trait Scout: Send + Sync {
    /// Discover candidate skills from the source.
    async fn discover(&self) -> Result<Vec<ScoutResult>, DiscoveryError>;
}

// ---------------------------------------------------------------------------
// GitHubScout
// ---------------------------------------------------------------------------

/// Searches GitHub for repos matching skill-related queries.
pub struct GitHubScout {
    client: reqwest::Client,
    queries: Vec<String>,
    api_base: String,
}

impl GitHubScout {
    pub fn new(token: Option<String>) -> anyhow::Result<Self> {
        Self::with_api_base(token, GITHUB_API_BASE)
    }

    pub(crate) fn with_api_base(token: Option<String>, base: &str) -> anyhow::Result<Self> {
        use std::time::Duration;

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::ACCEPT,
            "application/vnd.github+json"
                .parse()
                .map_err(|e| anyhow::anyhow!("failed to parse Accept header: {e}"))?,
        );
        headers.insert(
            reqwest::header::USER_AGENT,
            "OpenPRX-SkillForge/0.1"
                .parse()
                .map_err(|e| anyhow::anyhow!("failed to parse User-Agent header: {e}"))?,
        );
        if let Some(ref t) = token {
            if let Ok(val) = format!("Bearer {t}").parse() {
                headers.insert(reqwest::header::AUTHORIZATION, val);
            }
        }

        let client = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| anyhow::anyhow!("failed to build reqwest client: {e}"))?;

        Ok(Self {
            client,
            api_base: base.to_string(),
            queries: vec!["openprx skill".into(), "ai agent skill".into()],
        })
    }

    /// Override the default search queries.
    pub fn set_queries(&mut self, queries: Vec<String>) {
        self.queries = queries;
    }

    /// Parse the GitHub search/repositories JSON response.
    fn parse_items(body: &serde_json::Value) -> Vec<ScoutResult> {
        let items = match body.get("items").and_then(|v| v.as_array()) {
            Some(arr) => arr,
            None => return vec![],
        };

        items
            .iter()
            .filter_map(|item| {
                let name = item.get("name")?.as_str()?.to_string();
                let url = item.get("html_url")?.as_str()?.to_string();
                let description = item
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let stars = item.get("stargazers_count").and_then(|v| v.as_u64()).unwrap_or(0);
                let language = item.get("language").and_then(|v| v.as_str()).map(String::from);
                let updated_at = item
                    .get("updated_at")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<DateTime<Utc>>().ok());
                let owner = item
                    .get("owner")
                    .and_then(|o| o.get("login"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                let has_license = item.get("license").map(|v| !v.is_null()).unwrap_or(false);

                Some(ScoutResult {
                    name,
                    url,
                    description,
                    stars,
                    language,
                    updated_at,
                    source: ScoutSource::GitHub,
                    owner,
                    has_license,
                })
            })
            .collect()
    }
}

#[async_trait]
impl Scout for GitHubScout {
    async fn discover(&self) -> Result<Vec<ScoutResult>, DiscoveryError> {
        let mut all: Vec<ScoutResult> = Vec::new();

        for query in &self.queries {
            let url = format!(
                "{}/search/repositories?q={}&sort=stars&order=desc&per_page=30",
                self.api_base,
                urlencoding(query)
            );
            debug!(query = query.as_str(), "Searching GitHub");
            let resp = self
                .client
                .get(&url)
                .send()
                .await
                .map_err(|_| DiscoveryError::Upstream(UpstreamFailure::Request))?;
            let status = resp.status();
            if status.as_u16() == 429
                || (status.as_u16() == 403
                    && (resp
                        .headers()
                        .get("x-ratelimit-remaining")
                        .is_some_and(|value| value == "0")
                        || resp.headers().contains_key("retry-after")))
            {
                return Err(DiscoveryError::RateLimited);
            }
            if !status.is_success() {
                return Err(DiscoveryError::Upstream(UpstreamFailure::Http(status.as_u16())));
            }
            let body: serde_json::Value = resp
                .json()
                .await
                .map_err(|_| DiscoveryError::Upstream(UpstreamFailure::InvalidResponse))?;
            if !body.get("items").is_some_and(serde_json::Value::is_array) {
                return Err(DiscoveryError::Upstream(UpstreamFailure::InvalidResponse));
            }

            let mut items = Self::parse_items(&body);
            debug!(count = items.len(), query = query.as_str(), "Parsed items");
            all.append(&mut items);
        }

        dedup(&mut all);
        Ok(all)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Minimal percent-encoding for query strings (space → +).
fn urlencoding(s: &str) -> String {
    s.replace(' ', "+").replace('&', "%26").replace('#', "%23")
}

/// Deduplicate scout results by URL (keeps first occurrence).
pub fn dedup(results: &mut Vec<ScoutResult>) {
    let mut seen = std::collections::HashSet::new();
    results.retain(|r| seen.insert(r.url.clone()));
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scout_source_from_str() {
        assert_eq!("github".parse::<ScoutSource>().unwrap(), ScoutSource::GitHub);
        assert_eq!("GitHub".parse::<ScoutSource>().unwrap(), ScoutSource::GitHub);
        assert_eq!("clawhub".parse::<ScoutSource>().unwrap(), ScoutSource::ClawHub);
        assert_eq!("huggingface".parse::<ScoutSource>().unwrap(), ScoutSource::HuggingFace);
        assert_eq!("hf".parse::<ScoutSource>().unwrap(), ScoutSource::HuggingFace);
        assert!("unknown".parse::<ScoutSource>().is_err());
    }

    #[test]
    fn dedup_removes_duplicates() {
        let mut results = vec![
            ScoutResult {
                name: "a".into(),
                url: "https://github.com/x/a".into(),
                description: String::new(),
                stars: 10,
                language: None,
                updated_at: None,
                source: ScoutSource::GitHub,
                owner: "x".into(),
                has_license: true,
            },
            ScoutResult {
                name: "a-dup".into(),
                url: "https://github.com/x/a".into(),
                description: String::new(),
                stars: 10,
                language: None,
                updated_at: None,
                source: ScoutSource::GitHub,
                owner: "x".into(),
                has_license: true,
            },
            ScoutResult {
                name: "b".into(),
                url: "https://github.com/x/b".into(),
                description: String::new(),
                stars: 5,
                language: None,
                updated_at: None,
                source: ScoutSource::GitHub,
                owner: "x".into(),
                has_license: false,
            },
        ];
        dedup(&mut results);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].name, "a");
        assert_eq!(results[1].name, "b");
    }

    #[test]
    fn parse_github_items() {
        let json = serde_json::json!({
            "total_count": 1,
            "items": [
                {
                    "name": "cool-skill",
                    "html_url": "https://github.com/user/cool-skill",
                    "description": "A cool skill",
                    "stargazers_count": 42,
                    "language": "Rust",
                    "updated_at": "2026-01-15T10:00:00Z",
                    "owner": { "login": "user" },
                    "license": { "spdx_id": "MIT" }
                }
            ]
        });
        let items = GitHubScout::parse_items(&json);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "cool-skill");
        assert_eq!(items[0].stars, 42);
        assert!(items[0].has_license);
        assert_eq!(items[0].owner, "user");
    }

    #[test]
    fn r1d_production_api_base() {
        assert_eq!(GITHUB_API_BASE, "https://api.github.com");
    }

    #[test]
    fn urlencoding_works() {
        assert_eq!(urlencoding("hello world"), "hello+world");
        assert_eq!(urlencoding("a&b#c"), "a%26b%23c");
    }
}

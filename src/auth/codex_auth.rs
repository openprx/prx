use crate::auth::openai_oauth::extract_expiry_from_jwt;
use crate::auth::profiles::TokenSet;
use anyhow::{Context, Result};
use directories::UserDirs;
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

const DEFAULT_CODEX_AUTH_JSON: &str = ".codex/auth.json";

#[derive(Debug, Clone)]
pub struct ImportedOpenAiCodexProfile {
    pub token_set: TokenSet,
    pub account_id: String,
}

#[derive(Debug, Deserialize)]
struct CodexAuthFile {
    auth_mode: String,
    tokens: CodexAuthTokens,
}

#[derive(Debug, Deserialize)]
struct CodexAuthTokens {
    id_token: String,
    access_token: String,
    refresh_token: String,
    account_id: String,
}

pub fn default_codex_auth_json_path() -> PathBuf {
    UserDirs::new().map_or_else(
        || PathBuf::from(DEFAULT_CODEX_AUTH_JSON),
        |dirs| dirs.home_dir().join(DEFAULT_CODEX_AUTH_JSON),
    )
}

pub fn expand_codex_auth_json_path(path: &Path) -> PathBuf {
    PathBuf::from(shellexpand::tilde(&path.to_string_lossy()).into_owned())
}

pub fn load_openai_codex_profile_from_auth_json(path: &Path) -> Result<ImportedOpenAiCodexProfile> {
    let expanded = expand_codex_auth_json_path(path);
    let raw = fs::read_to_string(&expanded)
        .with_context(|| format!("Failed to read Codex auth.json from {}", expanded.display()))?;
    let parsed: CodexAuthFile =
        serde_json::from_str(&raw).context("Failed to parse Codex auth.json")?;

    if parsed.auth_mode != "chatgpt" {
        anyhow::bail!(
            "Unsupported Codex auth_mode {:?}; expected \"chatgpt\"",
            parsed.auth_mode
        );
    }

    let tokens = parsed.tokens;
    if tokens.id_token.trim().is_empty() {
        anyhow::bail!("Codex auth.json missing tokens.id_token");
    }
    if tokens.access_token.trim().is_empty() {
        anyhow::bail!("Codex auth.json missing tokens.access_token");
    }
    if tokens.refresh_token.trim().is_empty() {
        anyhow::bail!("Codex auth.json missing tokens.refresh_token");
    }
    if tokens.account_id.trim().is_empty() {
        anyhow::bail!("Codex auth.json missing tokens.account_id");
    }

    let expires_at = extract_expiry_from_jwt(&tokens.access_token)
        .ok_or_else(|| anyhow::anyhow!("Codex access_token missing valid exp claim"))?;

    Ok(ImportedOpenAiCodexProfile {
        token_set: TokenSet {
            access_token: tokens.access_token,
            refresh_token: Some(tokens.refresh_token),
            id_token: Some(tokens.id_token),
            expires_at: Some(expires_at),
            token_type: Some("Bearer".into()),
            scope: None,
        },
        account_id: tokens.account_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use tempfile::TempDir;

    fn write_auth_json(dir: &TempDir, body: &str) -> PathBuf {
        let path = dir.path().join("auth.json");
        std::fs::write(&path, body).unwrap();
        path
    }

    fn jwt_with_exp(exp: i64) -> String {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"alg":"none"}"#);
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(format!(r#"{{"exp":{exp},"account_id":"acct_from_jwt"}}"#));
        format!("{header}.{payload}.sig")
    }

    #[test]
    fn reads_codex_auth_json_successfully() {
        let tmp = TempDir::new().unwrap();
        let access_token = jwt_with_exp(1_900_000_000);
        let path = write_auth_json(
            &tmp,
            &format!(
                r#"{{
  "auth_mode": "chatgpt",
  "tokens": {{
    "id_token": "id-token",
    "access_token": "{access_token}",
    "refresh_token": "refresh-token",
    "account_id": "acct_123"
  }},
  "last_refresh": "2026-03-08T10:33:50.848Z"
}}"#
            ),
        );

        let imported = load_openai_codex_profile_from_auth_json(&path).unwrap();
        assert_eq!(imported.token_set.id_token.as_deref(), Some("id-token"));
        assert_eq!(
            imported.token_set.refresh_token.as_deref(),
            Some("refresh-token")
        );
    }

    #[test]
    fn missing_required_fields_fail() {
        let tmp = TempDir::new().unwrap();
        let access_token = jwt_with_exp(1_900_000_000);
        let path = write_auth_json(
            &tmp,
            &format!(
                r#"{{
  "auth_mode": "chatgpt",
  "tokens": {{
    "id_token": "id-token",
    "access_token": "{access_token}",
    "account_id": "acct_123"
  }}
}}"#
            ),
        );

        let err = load_openai_codex_profile_from_auth_json(&path).unwrap_err();
        assert!(format!("{err:#}").contains("refresh_token"));
    }

    #[test]
    fn account_id_is_passed_through() {
        let tmp = TempDir::new().unwrap();
        let path = write_auth_json(
            &tmp,
            &format!(
                r#"{{
  "auth_mode": "chatgpt",
  "tokens": {{
    "id_token": "id-token",
    "access_token": "{}",
    "refresh_token": "refresh-token",
    "account_id": "acct_passthrough"
  }}
}}"#,
                jwt_with_exp(1_900_000_000)
            ),
        );

        let imported = load_openai_codex_profile_from_auth_json(&path).unwrap();
        assert_eq!(imported.account_id, "acct_passthrough");
    }

    #[test]
    fn expires_at_is_parsed_from_access_token_exp_claim() {
        let tmp = TempDir::new().unwrap();
        let path = write_auth_json(
            &tmp,
            &format!(
                r#"{{
  "auth_mode": "chatgpt",
  "tokens": {{
    "id_token": "id-token",
    "access_token": "{}",
    "refresh_token": "refresh-token",
    "account_id": "acct_123"
  }}
}}"#,
                jwt_with_exp(1_900_000_123)
            ),
        );

        let imported = load_openai_codex_profile_from_auth_json(&path).unwrap();
        assert_eq!(
            imported.token_set.expires_at.map(|ts| ts.timestamp()),
            Some(1_900_000_123)
        );
    }
}

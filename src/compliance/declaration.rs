use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::schema::{DeclarationOfConformityConfig, EuAiActRiskClassification};

pub const REGULATION_SOURCE_URL: &str = "https://eur-lex.europa.eu/eli/reg/2024/1689/oj?locale=en";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeclarationPayload {
    pub artifact_version: String,
    pub framework_reference: String,
    pub system_name: String,
    pub system_type: String,
    pub system_reference: String,
    pub provider_name: String,
    pub provider_address: String,
    pub sole_responsibility_statement: String,
    pub conformity_statement: String,
    pub applicable_union_law: Vec<String>,
    pub processes_personal_data: bool,
    pub personal_data_compliance_statement: Option<String>,
    pub harmonised_standards: Vec<String>,
    pub standards_not_applicable_reason: Option<String>,
    pub conformity_assessment_procedure: String,
    pub notified_body_name: Option<String>,
    pub notified_body_identification_number: Option<String>,
    pub notified_body_certificate: Option<String>,
    pub issue_place: String,
    pub issue_date: String,
    pub signer_name: String,
    pub signer_function: String,
    pub signer_on_behalf_of: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeclarationSignature {
    pub status: String,
    pub reference: String,
    pub submitted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeclarationArtifact {
    pub artifact_kind: String,
    pub generated_at: String,
    pub regulation_source_url: String,
    pub payload_sha256: String,
    pub payload: DeclarationPayload,
    pub signature: DeclarationSignature,
}

fn required(value: &Option<String>, field: &str) -> Result<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .with_context(|| format!("declaration input '{field}' is required and is never fabricated"))
}

pub fn build_declaration_artifact(
    classification: EuAiActRiskClassification,
    config: &DeclarationOfConformityConfig,
    signature_reference: &str,
) -> Result<DeclarationArtifact> {
    anyhow::ensure!(
        classification == EuAiActRiskClassification::HighRisk,
        "EU declaration generation requires an operator-owned high_risk classification"
    );
    let signature_reference = signature_reference.trim();
    anyhow::ensure!(
        !signature_reference.is_empty(),
        "--signature-reference is required; PRX does not sign or invent a signature"
    );
    anyhow::ensure!(
        config.applicable_union_law.iter().any(|value| !value.trim().is_empty()),
        "declaration input 'applicable_union_law' must contain operator-reviewed legislation"
    );
    anyhow::ensure!(
        config.harmonised_standards.iter().any(|value| !value.trim().is_empty())
            || config
                .standards_not_applicable_reason
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty()),
        "declaration requires harmonised_standards or an explicit standards_not_applicable_reason"
    );
    if config.processes_personal_data {
        required(
            &config.personal_data_compliance_statement,
            "personal_data_compliance_statement",
        )?;
    }
    let notified_body_fields = [
        config.notified_body_name.as_ref(),
        config.notified_body_identification_number.as_ref(),
        config.notified_body_certificate.as_ref(),
    ];
    let supplied_notified_body_fields = notified_body_fields.iter().filter(|value| value.is_some()).count();
    anyhow::ensure!(
        supplied_notified_body_fields == 0 || supplied_notified_body_fields == notified_body_fields.len(),
        "notified body name, identification number, and certificate must be supplied together"
    );
    if supplied_notified_body_fields != 0 {
        required(&config.notified_body_name, "notified_body_name")?;
        required(
            &config.notified_body_identification_number,
            "notified_body_identification_number",
        )?;
        required(&config.notified_body_certificate, "notified_body_certificate")?;
    }
    let issue_date = required(&config.issue_date, "issue_date")?;
    chrono::NaiveDate::parse_from_str(&issue_date, "%Y-%m-%d").context("declaration issue_date must use YYYY-MM-DD")?;

    let payload = DeclarationPayload {
        artifact_version: required(&config.artifact_version, "artifact_version")?,
        framework_reference: "Regulation (EU) 2024/1689, Article 47 and Annex V".to_string(),
        system_name: required(&config.system_name, "system_name")?,
        system_type: required(&config.system_type, "system_type")?,
        system_reference: required(&config.system_reference, "system_reference")?,
        provider_name: required(&config.provider_name, "provider_name")?,
        provider_address: required(&config.provider_address, "provider_address")?,
        sole_responsibility_statement: required(
            &config.sole_responsibility_statement,
            "sole_responsibility_statement",
        )?,
        conformity_statement: required(&config.conformity_statement, "conformity_statement")?,
        applicable_union_law: config
            .applicable_union_law
            .iter()
            .map(|value| value.trim().to_string())
            .collect(),
        processes_personal_data: config.processes_personal_data,
        personal_data_compliance_statement: config
            .personal_data_compliance_statement
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        harmonised_standards: config
            .harmonised_standards
            .iter()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .collect(),
        standards_not_applicable_reason: config
            .standards_not_applicable_reason
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        conformity_assessment_procedure: required(
            &config.conformity_assessment_procedure,
            "conformity_assessment_procedure",
        )?,
        notified_body_name: config.notified_body_name.clone(),
        notified_body_identification_number: config.notified_body_identification_number.clone(),
        notified_body_certificate: config.notified_body_certificate.clone(),
        issue_place: required(&config.issue_place, "issue_place")?,
        issue_date,
        signer_name: required(&config.signer_name, "signer_name")?,
        signer_function: required(&config.signer_function, "signer_function")?,
        signer_on_behalf_of: required(&config.signer_on_behalf_of, "signer_on_behalf_of")?,
    };
    let payload_bytes = serde_json::to_vec(&payload)?;
    let payload_sha256 = format!("sha256:{:x}", Sha256::digest(&payload_bytes));
    Ok(DeclarationArtifact {
        artifact_kind: "eu_declaration_of_conformity".to_string(),
        generated_at: chrono::Utc::now().to_rfc3339(),
        regulation_source_url: REGULATION_SOURCE_URL.to_string(),
        payload_sha256,
        payload,
        signature: DeclarationSignature {
            status: "operator_supplied_reference".to_string(),
            reference: signature_reference.to_string(),
            submitted: false,
        },
    })
}

pub fn write_declaration_artifact(path: &Path, artifact: &DeclarationArtifact) -> Result<()> {
    let parent = path.parent().context("declaration output path must have a parent")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create declaration directory {}", parent.display()))?;
    let bytes = serde_json::to_vec_pretty(artifact)?;
    let temp_path: PathBuf = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().and_then(|name| name.to_str()).unwrap_or("declaration"),
        uuid::Uuid::new_v4()
    ));
    fs::write(&temp_path, bytes)
        .with_context(|| format!("failed to write temporary declaration artifact {}", temp_path.display()))?;
    fs::rename(&temp_path, path)
        .with_context(|| format!("failed to install declaration artifact {}", path.display()))?;
    Ok(())
}

pub fn verify_declaration_artifact(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("failed to read declaration artifact {}", path.display()))?;
    let artifact: DeclarationArtifact = serde_json::from_slice(&bytes).context("invalid declaration artifact JSON")?;
    anyhow::ensure!(
        artifact.artifact_kind == "eu_declaration_of_conformity",
        "unexpected declaration artifact kind"
    );
    anyhow::ensure!(
        artifact.regulation_source_url == REGULATION_SOURCE_URL,
        "declaration regulation source does not match the pinned evaluator"
    );
    anyhow::ensure!(
        !artifact.signature.reference.trim().is_empty(),
        "signature reference is missing"
    );
    anyhow::ensure!(
        !artifact.signature.submitted,
        "PRX artifacts must not claim automatic regulatory submission"
    );
    let expected = format!("sha256:{:x}", Sha256::digest(serde_json::to_vec(&artifact.payload)?));
    anyhow::ensure!(artifact.payload_sha256 == expected, "declaration payload hash mismatch");
    Ok(format!("artifact:{}:{}", artifact.payload.artifact_version, expected))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn complete_config() -> DeclarationOfConformityConfig {
        DeclarationOfConformityConfig {
            artifact_path: None,
            artifact_version: Some("1".to_string()),
            system_name: Some("PRX".to_string()),
            system_type: Some("AI orchestration runtime".to_string()),
            system_reference: Some("prx-release-1".to_string()),
            provider_name: Some("Example Provider".to_string()),
            provider_address: Some("Example Address".to_string()),
            sole_responsibility_statement: Some("Operator supplied responsibility statement".to_string()),
            conformity_statement: Some("Operator supplied conformity statement".to_string()),
            applicable_union_law: vec!["Regulation (EU) 2024/1689".to_string()],
            processes_personal_data: false,
            personal_data_compliance_statement: None,
            harmonised_standards: vec!["Operator reviewed standard".to_string()],
            standards_not_applicable_reason: None,
            conformity_assessment_procedure: Some("Annex VI internal control".to_string()),
            notified_body_name: None,
            notified_body_identification_number: None,
            notified_body_certificate: None,
            issue_place: Some("Example City".to_string()),
            issue_date: Some("2026-07-18".to_string()),
            signer_name: Some("Named Signer".to_string()),
            signer_function: Some("Responsible Officer".to_string()),
            signer_on_behalf_of: Some("Example Provider".to_string()),
        }
    }

    #[test]
    fn rejects_incomplete_or_non_applicable_inputs() {
        let mut config = complete_config();
        config.provider_name = None;
        assert!(build_declaration_artifact(EuAiActRiskClassification::HighRisk, &config, "sig:external").is_err());
        assert!(
            build_declaration_artifact(
                EuAiActRiskClassification::NotHighRisk,
                &complete_config(),
                "sig:external"
            )
            .is_err()
        );
        assert!(build_declaration_artifact(EuAiActRiskClassification::HighRisk, &complete_config(), "").is_err());
    }

    #[test]
    fn artifact_is_versioned_hashed_and_never_claims_submission() {
        let artifact = build_declaration_artifact(
            EuAiActRiskClassification::HighRisk,
            &complete_config(),
            "external-signature:receipt-1",
        )
        .unwrap();
        assert_eq!(artifact.payload.artifact_version, "1");
        assert!(artifact.payload_sha256.starts_with("sha256:"));
        assert!(!artifact.signature.submitted);

        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("declaration.json");
        write_declaration_artifact(&path, &artifact).unwrap();
        let evidence = verify_declaration_artifact(&path).unwrap();
        assert!(evidence.contains(&artifact.payload_sha256));
    }
}

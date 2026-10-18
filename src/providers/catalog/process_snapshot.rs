//! Transfer the exact local PMM evidence pinned by an admitted parent.
use super::{
    Arc, CatalogView, CuratedModel, DescriptorSource, Dialects, FeatureSet, ModelDescriptor, ModelOperation,
    ProviderId, RouteTarget, Support,
};
use crate::causal_tree::ids::DecisionId;
use crate::decision::key::{DecisionKey, KeyDomain};
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessProviderCatalog {
    payload: String,
    seal: [u8; 32],
}
impl std::fmt::Debug for ProcessProviderCatalog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcessProviderCatalog").finish_non_exhaustive()
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Curated {
    provider: String,
    model: Option<String>,
    operations: Option<Vec<ModelOperation>>,
    native_tools: Option<bool>,
    vision_input: Option<bool>,
    streaming: Option<bool>,
    structured_output: Option<bool>,
    tool_schema: Option<crate::tools::ToolSchemaDialect>,
    tool_call: Option<crate::providers::ToolCallDialect>,
    context_window: Option<u32>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Discovered {
    provider: String,
    model: String,
    operations: std::collections::BTreeMap<ModelOperation, Support>,
    features: FeatureSet,
    dialects: Dialects,
    context_window: Option<u32>,
    source: DescriptorSource,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    #[serde(default)]
    release_revision: Option<String>,
    #[serde(default)]
    release_content_sha256: Option<String>,
    curated: Vec<Curated>,
    discovered: Vec<Discovered>,
    routes: Vec<RouteTarget>,
}
fn provider(raw: &str) -> anyhow::Result<ProviderId> {
    ProviderId::from_authenticated_catalog_key(raw)
        .ok_or_else(|| anyhow::anyhow!("process provider identity is not canonical"))
}
impl CatalogView {
    pub(crate) fn bind_process(&self, key: &DecisionKey, parent: DecisionId) -> anyhow::Result<ProcessProviderCatalog> {
        let payload = self.process_payload()?;
        let seal = key.hmac(
            KeyDomain::StateFingerprint,
            &[
                b"process-provider-catalog-v1",
                parent.to_string().as_bytes(),
                payload.as_bytes(),
            ],
        );
        Ok(ProcessProviderCatalog { payload, seal })
    }
    pub(crate) fn restore_process(
        snapshot: &ProcessProviderCatalog,
        key: &DecisionKey,
        parent: DecisionId,
    ) -> anyhow::Result<Arc<Self>> {
        anyhow::ensure!(
            key.verify_hmac(
                KeyDomain::StateFingerprint,
                &[
                    b"process-provider-catalog-v1",
                    parent.to_string().as_bytes(),
                    snapshot.payload.as_bytes()
                ],
                &snapshot.seal
            ),
            "process provider catalog signature is invalid"
        );
        Self::decode_process_payload(&snapshot.payload)
    }
    fn process_payload(&self) -> anyhow::Result<String> {
        let snapshot = Snapshot {
            release_revision: self.release_revision().map(str::to_owned),
            release_content_sha256: self
                .release_catalog()
                .ok()
                .map(|catalog| catalog.content_sha256().to_owned()),
            curated: self
                .curated
                .iter()
                .map(|item| Curated {
                    provider: item.provider.as_str().to_owned(),
                    model: item.model.as_deref().map(str::to_owned),
                    operations: item.operations.clone(),
                    native_tools: item.native_tools,
                    vision_input: item.vision_input,
                    streaming: item.streaming,
                    structured_output: item.structured_output,
                    tool_schema: item.tool_schema,
                    tool_call: item.tool_call,
                    context_window: item.context_window,
                })
                .collect(),
            discovered: self
                .discovered
                .iter()
                .map(|item| Discovered {
                    provider: item.provider.as_str().to_owned(),
                    model: item.model_id.to_string(),
                    operations: item.operations.clone(),
                    features: item.features.clone(),
                    dialects: item.dialects,
                    context_window: item.context_window,
                    source: item.source.clone(),
                })
                .collect(),
            routes: self.routes.clone(),
        };
        Ok(serde_json::to_string(&snapshot)?)
    }

    fn decode_process_payload(payload: &str) -> anyhow::Result<Arc<Self>> {
        let snapshot: Snapshot = serde_json::from_str(payload)?;
        let (release, release_error) = if let Some(parent_revision) = snapshot.release_revision.as_deref() {
            let embedded = super::ReleaseCatalog::embedded()
                .map_err(|error| anyhow::anyhow!("embedded model catalog is invalid: {error}"))?;
            anyhow::ensure!(
                embedded.revision == parent_revision,
                "child model catalog revision does not match its parent"
            );
            anyhow::ensure!(
                snapshot.release_content_sha256.as_deref() == Some(embedded.content_sha256()),
                "child model catalog content does not match its parent"
            );
            (Some(embedded), None)
        } else {
            (None, None)
        };
        let curated = snapshot
            .curated
            .into_iter()
            .map(|item| {
                Ok(CuratedModel {
                    provider: provider(&item.provider)?,
                    model: item.model.map(Arc::from),
                    operations: item.operations,
                    native_tools: item.native_tools,
                    vision_input: item.vision_input,
                    streaming: item.streaming,
                    structured_output: item.structured_output,
                    tool_schema: item.tool_schema,
                    tool_call: item.tool_call,
                    context_window: item.context_window,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let discovered = snapshot
            .discovered
            .into_iter()
            .map(|item| {
                Ok(ModelDescriptor {
                    provider: provider(&item.provider)?,
                    model_id: Arc::from(item.model),
                    operations: item.operations,
                    features: item.features,
                    dialects: item.dialects,
                    context_window: item.context_window,
                    source: item.source,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(Arc::new(Self {
            curated,
            discovered,
            routes: snapshot.routes,
            release,
            release_error,
        }))
    }

    pub(crate) fn bind_operator_process(
        &self,
        key: &DecisionKey,
        publication: uuid::Uuid,
    ) -> anyhow::Result<ProcessProviderCatalog> {
        let payload = self.process_payload()?;
        let seal = key.hmac(
            KeyDomain::StateFingerprint,
            &[
                b"operator-process-provider-catalog-v1",
                publication.to_string().as_bytes(),
                payload.as_bytes(),
            ],
        );
        Ok(ProcessProviderCatalog { payload, seal })
    }

    pub(crate) fn restore_operator_process(
        snapshot: &ProcessProviderCatalog,
        key: &DecisionKey,
        publication: uuid::Uuid,
    ) -> anyhow::Result<Arc<Self>> {
        anyhow::ensure!(
            key.verify_hmac(
                KeyDomain::StateFingerprint,
                &[
                    b"operator-process-provider-catalog-v1",
                    publication.to_string().as_bytes(),
                    snapshot.payload.as_bytes()
                ],
                &snapshot.seal
            ),
            "operator provider catalog signature is invalid"
        );
        Self::decode_process_payload(&snapshot.payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> CatalogView {
        let identities = [
            "ollama",
            "claude-code",
            "qwen-us",
            "custom:http://EXAMPLE.invalid:8080/v1/",
            "anthropic-custom:https://example.invalid/v1",
            "No-Such-Provider",
            "catalog:ollama",
            "qwen@https://example.invalid/v1",
            "typesafe",
            "wasm:PRXPlugin",
        ];
        CatalogView {
            curated: identities
                .iter()
                .map(|name| CuratedModel::new(ProviderId::of(name), Some("model")))
                .collect(),
            discovered: identities
                .iter()
                .map(|name| ModelDescriptor::id_only(ProviderId::of(name), "model"))
                .collect(),
            routes: Vec::new(),
            release: None,
            release_error: None,
        }
    }

    #[test]
    fn authenticated_catalog_roundtrips_provider_namespaces_in_both_process_domains() {
        let key = DecisionKey::ephemeral();
        let parent = DecisionId::new_runtime();
        let publication = uuid::Uuid::new_v4();
        let original = catalog();
        let child = original.bind_process(&key, parent).expect("child snapshot");
        let operator = original
            .bind_operator_process(&key, publication)
            .expect("operator snapshot");
        for restored in [
            CatalogView::restore_process(&child, &key, parent).expect("child restore"),
            CatalogView::restore_operator_process(&operator, &key, publication).expect("operator restore"),
        ] {
            assert_eq!(original.curated, restored.curated);
            assert_eq!(original.discovered, restored.discovered);
            assert_eq!(
                original.process_payload().expect("payload"),
                restored.process_payload().expect("restored payload")
            );
        }
        assert!(CatalogView::restore_operator_process(&child, &key, publication).is_err());
        assert!(CatalogView::restore_process(&operator, &key, parent).is_err());
        assert!(CatalogView::restore_process(&child, &key, DecisionId::new_runtime()).is_err());
        assert!(CatalogView::restore_operator_process(&operator, &key, uuid::Uuid::new_v4()).is_err());
        let mut tampered = child;
        tampered.payload = "not json".to_owned();
        let error = CatalogView::restore_process(&tampered, &key, parent).expect_err("reject tamper");
        assert_eq!(error.to_string(), "process provider catalog signature is invalid");
    }

    #[test]
    fn canonical_snapshot_keys_do_not_change_config_label_authority() {
        for raw in [
            "ollama",
            "catalog:claude-code",
            "catalog:unknown",
            "catalog:OLLAMA",
            "catalog:ollama@not-a-url",
            "unknown:ollama",
            "decision:unknown",
            "custom:not-a-url",
        ] {
            assert!(provider(raw).is_err(), "noncanonical snapshot key accepted");
        }
        let canonical = ProviderId::of("ollama");
        let configured = ProviderId::of(canonical.as_str());
        assert_ne!(canonical, configured);
        let connection = crate::providers::identity::configured_connection_for(
            canonical.as_str(),
            "ollama",
            Some("fixture-key"),
            None,
        );
        assert!(!connection.released && connection.api_key.is_none());
        assert_eq!(configured.namespace(), crate::providers::ProviderNamespace::Unknown);
        assert_eq!(
            provider(configured.as_str()).expect("unknown identity stays unknown"),
            configured
        );
    }

    #[test]
    fn child_refuses_a_parent_catalog_revision_from_another_binary() {
        let embedded = super::super::ReleaseCatalog::embedded().expect("test: embedded catalog validates");
        let mut parent_release = (*embedded).clone();
        parent_release.revision.push_str("-different-binary");
        let parent_catalog = CatalogView::default().with_release_catalog(parent_release);
        let key = DecisionKey::ephemeral();
        let parent = DecisionId::new_runtime();
        let snapshot = parent_catalog
            .bind_process(&key, parent)
            .expect("test: parent snapshot binds");

        let error = CatalogView::restore_process(&snapshot, &key, parent)
            .expect_err("a child running another embedded revision must refuse the parent snapshot");
        assert_eq!(
            error.to_string(),
            "child model catalog revision does not match its parent"
        );
    }

    #[test]
    fn child_refuses_changed_catalog_content_under_the_same_revision() {
        let embedded = super::super::ReleaseCatalog::embedded().expect("test: embedded catalog validates");
        let mut changed = serde_json::to_value(embedded.as_ref()).expect("test: catalog serializes");
        *changed
            .pointer_mut("/models/0/display_name")
            .expect("test: first model display name exists") =
            serde_json::Value::String("Changed fixture label".to_string());
        let changed = super::super::ReleaseCatalog::from_json(&changed.to_string())
            .expect("test: changed catalog remains semantically valid");
        assert_eq!(changed.revision, embedded.revision);
        assert_ne!(changed.content_sha256(), embedded.content_sha256());

        let parent_catalog = CatalogView::default().with_release_catalog(changed);
        let key = DecisionKey::ephemeral();
        let parent = DecisionId::new_runtime();
        let snapshot = parent_catalog
            .bind_process(&key, parent)
            .expect("test: parent snapshot binds");

        let error = CatalogView::restore_process(&snapshot, &key, parent)
            .expect_err("a child must reject changed content under the same revision");
        assert_eq!(
            error.to_string(),
            "child model catalog content does not match its parent"
        );
    }
}

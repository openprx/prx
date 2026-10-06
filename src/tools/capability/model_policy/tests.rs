#![cfg(test)]
use super::*;
use crate::tools::capability::catalog::{CapabilityCatalog, CatalogScope, CatalogSources, SurfacePolicy};
use crate::tools::traits::{ToolResult, ToolSpec};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};

struct AliasTool {
    root: String,
    enabled: Arc<AtomicBool>,
    root_spec: bool,
}
#[async_trait::async_trait]
impl Tool for AliasTool {
    fn name(&self) -> &str {
        &self.root
    }
    fn description(&self) -> &str {
        "PRX policy fixture"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{}})
    }
    fn supports_name(&self, name: &str) -> bool {
        name == self.root
            || name == format!("{}_alias", self.root)
            || name == "prx_shared" && self.enabled.load(Ordering::SeqCst)
    }
    fn specs(&self) -> Vec<ToolSpec> {
        [
            self.root_spec.then(|| self.root.clone()),
            Some(format!("{}_alias", self.root)),
        ]
        .into_iter()
        .flatten()
        .map(|name| ToolSpec {
            name,
            description: self.description().into(),
            parameters: self.parameters_schema(),
        })
        .collect()
    }
    async fn execute(&self, _: serde_json::Value) -> anyhow::Result<ToolResult> {
        anyhow::bail!("policy fixture must not dispatch")
    }
}

// Independent old expansion, including visible public names and later root lookup.
fn old_denied(registry: &[Box<dyn Tool>], view: &CatalogView<'_>, allow: &[String]) -> BTreeSet<String> {
    let roots: BTreeSet<_> = registry
        .iter()
        .filter(|tool| allow.iter().any(|name| tool.supports_name(name)))
        .map(|tool| tool.name())
        .collect();
    view.iter_entries()
        .filter(|entry| !roots.contains(entry.root.as_str()))
        .map(|entry| entry.public_name.clone())
        .collect()
}

#[test]
fn fresh_bitmap_matches_old_alias_union_view_and_dynamic_support() -> anyhow::Result<()> {
    let flag = Arc::new(AtomicBool::new(false));
    let mut registry: Vec<Box<dyn Tool>> = (0..65)
        .map(|index| {
            Box::new(AliasTool {
                root: format!("prx_root_{index}"),
                enabled: Arc::clone(&flag),
                root_spec: true,
            }) as Box<dyn Tool>
        })
        .collect();
    let catalog = CapabilityCatalog::build(&registry, &CatalogSources::default(), &SurfacePolicy::default())?;
    let scope = CatalogScope::default();
    let hidden = SurfacePolicy {
        allowed_tools: Some(BTreeSet::from(["prx_root_0_alias".into()])),
        ..SurfacePolicy::default()
    };
    let key = crate::causal_tree::state::fixtures::key();
    for allow in [Vec::new(), vec!["prx_shared".into()], vec!["prx_root_0_alias".into()]] {
        for visible_alias_only in [false, true] {
            let base = catalog.view(&scope);
            let view = if visible_alias_only {
                base.with_surface(&hidden)
            } else {
                base
            };
            for enabled in [false, true] {
                flag.store(enabled, Ordering::SeqCst);
                let expected = old_denied(&registry, &view, &allow);
                let current = ModelAdmission::fresh(&registry, &view, "prx-model", &allow, "prx-policy", &key);
                for entry in view.all_entries {
                    assert_eq!(
                        current.excludes(&view, entry),
                        expected.contains(&entry.public_name) || expected.contains(&entry.root)
                    );
                }
            }
        }
    }
    let view = catalog.view(&scope);
    let allow = vec!["prx_shared".into()];
    flag.store(false, Ordering::SeqCst);
    let denied = ModelAdmission::fresh(&registry, &view, "prx-model", &allow, "prx-policy", &key);
    flag.store(true, Ordering::SeqCst);
    let allowed = ModelAdmission::fresh(&registry, &view, "prx-model", &allow, "prx-policy", &key);
    assert_ne!(denied.digest(), allowed.digest());
    flag.store(false, Ordering::SeqCst);
    // Repeated root identity must union support; the first false root cannot win.
    registry.push(Box::new(AliasTool {
        root: "prx_root_0".into(),
        enabled: Arc::new(AtomicBool::new(true)),
        root_spec: true,
    }));
    let union = ModelAdmission::fresh(&registry, &view, "prx-model", &allow, "prx-policy", &key);
    let expected = old_denied(&registry, &view, &allow);
    for entry in view.all_entries {
        assert_eq!(
            union.excludes(&view, entry),
            expected.contains(&entry.public_name) || expected.contains(&entry.root)
        );
    }
    let changed_model = ModelAdmission::fresh(&registry, &view, "prx-other", &allow, "prx-policy", &key);
    let changed_policy = ModelAdmission::fresh(&registry, &view, "prx-model", &allow, "prx-other", &key);
    assert_ne!(union.digest(), changed_model.digest());
    assert_ne!(union.digest(), changed_policy.digest());
    let alias_only: Vec<Box<dyn Tool>> = vec![Box::new(AliasTool {
        root: "prx_absent_root".into(),
        enabled: Arc::clone(&flag),
        root_spec: false,
    })];
    let alias_catalog = CapabilityCatalog::build(&alias_only, &CatalogSources::default(), &SurfacePolicy::default())?;
    assert!(alias_catalog.entry_for_public_name("prx_absent_root").is_none());
    let alias_view = alias_catalog.view(&scope);
    for allow in [vec![], vec!["prx_absent_root_alias".into()]] {
        let expected = old_denied(&alias_only, &alias_view, &allow);
        let current = ModelAdmission::fresh(&alias_only, &alias_view, "prx-model", &allow, "prx-policy", &key);
        for entry in alias_view.all_entries {
            assert_eq!(
                current.excludes(&alias_view, entry),
                expected.contains(&entry.public_name) || expected.contains(&entry.root)
            );
        }
    }
    Ok(())
}

#[test]
fn model_policy_digest_binds_presence_across_catalog_sizes() -> anyhow::Result<()> {
    let key = crate::causal_tree::state::fixtures::key();
    let mut lengths = Vec::new();
    for count in [1, 17, 65] {
        let registry: Vec<Box<dyn Tool>> = (0..count)
            .map(|index| {
                Box::new(AliasTool {
                    root: format!("prx_root_{index}"),
                    enabled: Arc::new(AtomicBool::new(true)),
                    root_spec: true,
                }) as Box<dyn Tool>
            })
            .collect();
        let catalog = CapabilityCatalog::build(&registry, &CatalogSources::default(), &SurfacePolicy::default())?;
        let view = catalog.view(&CatalogScope::default());
        let mut state = crate::causal_tree::state::fixtures::state(&key, "prx input");
        let before = state.material_fingerprint(&key);
        let mut trusted = state.trusted.parts().clone();
        trusted.model_admission = Some(ModelAdmission::fresh(
            &registry,
            &view,
            "prx-model",
            &[],
            "prx-policy",
            &key,
        ));
        state.trusted = crate::causal_tree::state::TrustedState::from_runtime(trusted);
        assert_ne!(state.material_fingerprint(&key), before);
        let policy = state
            .trusted
            .parts()
            .model_admission
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("issued policy missing"))?;
        lengths.push(policy.digest().len());
        let all_allowed = ModelAdmission::fresh(
            &registry,
            &view,
            "prx-model",
            &["prx_shared".into()],
            "prx-policy",
            &key,
        );
        assert_ne!(policy.digest(), all_allowed.digest());
        let mut trusted = state.trusted.parts().clone();
        trusted.model_admission = None;
        state.trusted = crate::causal_tree::state::TrustedState::from_runtime(trusted);
        assert_eq!(state.material_fingerprint(&key), before);
    }
    assert_eq!(lengths, [32, 32, 32]);
    Ok(())
}

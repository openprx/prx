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

#[test]
fn model_admission_dyn_equality_compares_each_original_field() -> anyhow::Result<()> {
    use crate::causal_tree::state::model_admission::ModelAdmissionView;
    let registry: Vec<Box<dyn Tool>> = vec![Box::new(AliasTool {
        root: "prx_eq_root".into(),
        enabled: Arc::new(AtomicBool::new(true)),
        root_spec: true,
    })];
    let catalog = CapabilityCatalog::build(&registry, &CatalogSources::default(), &SurfacePolicy::default())?;
    let view = catalog.view(&CatalogScope::default());
    let key = crate::causal_tree::state::fixtures::key();
    let issued = ModelAdmission::fresh(&registry, &view, "prx-model", &[], "prx-policy", &key);
    let same = ModelAdmission::fresh(&registry, &view, "prx-model", &[], "prx-policy", &key);
    let left: &dyn ModelAdmissionView = issued.as_ref();
    let equal: &dyn ModelAdmissionView = same.as_ref();
    assert!(left.eq(equal));
    assert!(equal.eq(left));
    assert!(left.eq(left));
    // Constructed negative values remain private to this test module. They are
    // never published as runtime authority: mutate exactly one original field
    // while holding the other three fixed, including the keyed digest.
    for field in 0..4 {
        let mut changed = ModelAdmission {
            denied: issued.denied.clone(),
            generation: issued.generation,
            layout: issued.layout,
            digest: issued.digest,
        };
        match field {
            0 => {
                *changed
                    .denied
                    .first_mut()
                    .ok_or_else(|| anyhow::anyhow!("real nonempty catalog bitmap"))? ^= 1
            }
            1 => {
                *changed
                    .generation
                    .0
                    .first_mut()
                    .ok_or_else(|| anyhow::anyhow!("fixed digest byte missing"))? ^= 1
            }
            2 => {
                *changed
                    .layout
                    .first_mut()
                    .ok_or_else(|| anyhow::anyhow!("fixed digest byte missing"))? ^= 1
            }
            3 => {
                *changed
                    .digest
                    .first_mut()
                    .ok_or_else(|| anyhow::anyhow!("fixed digest byte missing"))? ^= 1
            }
            _ => anyhow::bail!("unexpected equality field"),
        }
        let differences = [
            changed.denied != issued.denied,
            changed.generation != issued.generation,
            changed.layout != issued.layout,
            changed.digest != issued.digest,
        ];
        assert_eq!(differences.iter().filter(|different| **different).count(), 1);
        assert_eq!(differences.get(field).copied(), Some(true));
        let right: &dyn ModelAdmissionView = &changed;
        assert!(!left.eq(right), "field {field} must participate in dyn equality");
        assert!(!right.eq(left), "field {field} inequality must be symmetric");
        assert!(!issued.as_ref().eq(&changed), "original concrete equality agrees");
    }
    Ok(())
}

#[test]
fn model_admission_absent_slots_and_invalid_present_slots_remain_distinct() -> anyhow::Result<()> {
    use crate::causal_tree::state::model_admission::{ModelAdmissionEntry, ModelAdmissionView};
    let registry: Vec<Box<dyn Tool>> = (0..33)
        .map(|index| {
            Box::new(AliasTool {
                root: format!("prx_slot_{index}"),
                enabled: Arc::new(AtomicBool::new(true)),
                root_spec: true,
            }) as Box<dyn Tool>
        })
        .collect();
    let catalog = CapabilityCatalog::build(&registry, &CatalogSources::default(), &SurfacePolicy::default())?;
    let view = catalog.view(&CatalogScope::default());
    let key = crate::causal_tree::state::fixtures::key();
    let allowed = ModelAdmission::fresh(
        &registry,
        &view,
        "prx-model",
        &["prx_shared".into()],
        "prx-policy",
        &key,
    );
    let denied = ModelAdmission::fresh(&registry, &view, "prx-model", &[], "prx-policy", &key);
    let port: &dyn ModelAdmissionView = allowed.as_ref();
    let mut crosses_word_boundary = false;
    for entry in view.iter_entries() {
        let actual = view.model_admission_entry(entry);
        crosses_word_boundary |= actual.public_slot.is_some_and(|slot| slot >= 64);
        assert!(!port.excludes(actual), "actual published allowed entry");
        assert!(
            ModelAdmissionView::excludes(denied.as_ref(), actual),
            "actual published denied entry"
        );
    }
    assert!(crosses_word_boundary, "fixture exercises a real second bitmap word");
    let actual = view.model_admission_entry(
        view.iter_entries()
            .next()
            .ok_or_else(|| anyhow::anyhow!("real catalog entry missing"))?,
    );
    assert!(actual.public_slot.is_some());
    assert!(actual.root_slot.is_some());
    // These projections are constructed negative facts, never published or
    // used to execute tools. The concrete policy above is genuinely fresh.
    let absent = ModelAdmissionEntry {
        public_slot: None,
        root_slot: None,
        ..actual
    };
    assert!(!port.excludes(absent));
    assert!(!port.excludes(ModelAdmissionEntry {
        public_slot: None,
        ..actual
    }));
    assert!(!port.excludes(ModelAdmissionEntry {
        root_slot: None,
        ..actual
    }));
    let first_missing_word = allowed.denied.len() * 64;
    for invalid in [first_missing_word, usize::MAX] {
        assert!(port.excludes(ModelAdmissionEntry {
            public_slot: Some(invalid),
            ..actual
        }));
        assert!(port.excludes(ModelAdmissionEntry {
            root_slot: Some(invalid),
            ..actual
        }));
        assert!(port.excludes(ModelAdmissionEntry {
            public_slot: Some(invalid),
            ..absent
        }));
        assert!(port.excludes(ModelAdmissionEntry {
            root_slot: Some(invalid),
            ..absent
        }));
    }
    let mut wrong_generation = absent.generation;
    *wrong_generation
        .first_mut()
        .ok_or_else(|| anyhow::anyhow!("fixed digest byte missing"))? ^= 1;
    assert!(port.excludes(ModelAdmissionEntry {
        generation: wrong_generation,
        ..absent
    }));
    let mut wrong_layout = *absent.layout;
    *wrong_layout
        .first_mut()
        .ok_or_else(|| anyhow::anyhow!("fixed digest byte missing"))? ^= 1;
    assert!(port.excludes(ModelAdmissionEntry {
        layout: &wrong_layout,
        ..absent
    }));
    Ok(())
}

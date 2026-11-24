#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::capability::search::CatalogSearch;
    use crate::tools::traits::ToolResult;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct ProbeTool {
        name: &'static str,
        description: &'static str,
    }

    #[async_trait]
    impl Tool for ProbeTool {
        fn name(&self) -> &str {
            self.name
        }

        fn description(&self) -> &str {
            self.description
        }

        fn parameters_schema(&self) -> Value {
            json!({"type":"object","properties":{"query":{"type":"string"}}})
        }

        async fn execute(&self, _args: Value) -> anyhow::Result<ToolResult> {
            Ok(ToolResult {
                success: true,
                output: String::new(),
                error: None,
            })
        }
    }

    struct DocumentCategoryProbe(&'static str);
    #[async_trait]
    impl Tool for DocumentCategoryProbe {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "Native document index operation"
        }
        fn parameters_schema(&self) -> Value {
            json!({"type":"object","properties":{}})
        }
        fn categories(&self) -> &'static [ToolCategory] {
            &[ToolCategory::Memory, ToolCategory::FileSystem]
        }
        async fn execute(&self, _: Value) -> anyhow::Result<ToolResult> {
            anyhow::bail!("metadata fixture never executes")
        }
    }

    #[test]
    fn document_index_tools_keep_legacy_ids_without_entering_filesystem_discovery() {
        let registry: Vec<Box<dyn Tool>> = vec![
            Box::new(DocumentCategoryProbe("document_ingest")),
            Box::new(DocumentCategoryProbe("document_sync")),
        ];
        let catalog =
            CapabilityCatalog::build(&registry, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
        for name in ["document_ingest", "document_sync"] {
            let entry = catalog.entry_for_public_name(name).unwrap();
            assert_eq!(entry.family, CapabilityTarget::Document);
            assert_eq!(entry.id.as_str(), format!("filesystem:native:{name}"));
        }
        let view = catalog.view(&CatalogScope {
            families: BTreeSet::from([CapabilityTarget::Filesystem]),
            ..CatalogScope::default()
        });
        assert!(view.iter_entries().next().is_none());
    }

    fn registry() -> Vec<Box<dyn Tool>> {
        vec![
            Box::new(ProbeTool {
                name: "prx_search",
                description: "Search local evidence",
            }),
            Box::new(ProbeTool {
                name: "prx_read",
                description: "Read local evidence",
            }),
        ]
    }

    // Public aliases need not export their backend identity as another capability.
    struct AliasOnlyParityTool {
        backend: &'static str,
        state: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Tool for AliasOnlyParityTool {
        fn name(&self) -> &str {
            self.backend
        }

        fn description(&self) -> &str {
            "PRX alias parity fixture"
        }

        fn parameters_schema(&self) -> Value {
            if self.state.load(Ordering::SeqCst) == 1 {
                json!({"type":"object","properties":{"value":{"type":"string"}}})
            } else {
                json!({"type":"object","properties":{}})
            }
        }

        fn specs(&self) -> Vec<ToolSpec> {
            if self.state.load(Ordering::SeqCst) == 2 {
                return Vec::new();
            }
            vec![ToolSpec {
                name: "prx_exact_alias".into(),
                description: self.description().into(),
                parameters: self.parameters_schema(),
            }]
        }

        fn supports_name(&self, name: &str) -> bool {
            name == "prx_exact_alias" && self.state.load(Ordering::SeqCst) != 2
        }

        fn availability(&self) -> CapabilityAvailability {
            if self.state.load(Ordering::SeqCst) == 3 {
                CapabilityAvailability {
                    level: CapabilityAvailabilityLevel::Declared,
                    reason: "PRX fixture is no longer configured".into(),
                }
            } else {
                CapabilityAvailability::ready("PRX fixture configured")
            }
        }

        async fn execute(&self, _args: Value) -> anyhow::Result<ToolResult> {
            anyhow::bail!("parity fixture does not execute effects")
        }
    }

    #[test]
    fn alias_only_tool_parity_preserves_exact_identity_and_live_contract() {
        let state = Arc::new(AtomicUsize::new(0));
        let registry: Vec<Box<dyn Tool>> = vec![Box::new(AliasOnlyParityTool {
            backend: "prx_alias_backend",
            state: Arc::clone(&state),
        })];
        let catalog =
            CapabilityCatalog::build(&registry, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
        let entry = catalog.entry_for_public_name("prx_exact_alias").unwrap();
        let reference = CapabilityRef {
            id: entry.id.clone(),
            generation: entry.generation,
            fingerprint: entry.fingerprint,
        };
        let allowed = BTreeSet::from([reference.clone()]);
        let service =
            ToolExecutionService::with_security_policy(registry, Arc::new(crate::security::SecurityPolicy::default()));
        assert!(catalog.entry_for_public_name("prx_alias_backend").is_none());
        assert!(service.current_descriptor("prx_alias_backend").is_none());
        assert!(catalog.check_parity(&service).is_ok());
        assert!(catalog.check_published_parity(&service, &allowed).is_ok());

        let wrong_backend = ToolExecutionService::with_security_policy(
            vec![Box::new(AliasOnlyParityTool {
                backend: "prx_other_backend",
                state: Arc::clone(&state),
            })],
            Arc::new(crate::security::SecurityPolicy::default()),
        );
        assert!(catalog.check_parity(&wrong_backend).is_err());
        assert!(catalog.check_published_parity(&wrong_backend, &allowed).is_err());
        let mut stale = reference.clone();
        stale.generation = CatalogGeneration([0; 32]);
        assert_ne!(stale.generation, reference.generation);
        assert!(
            catalog
                .check_published_parity(&service, &BTreeSet::from([stale]))
                .is_err()
        );
        let mut changed = reference;
        changed.fingerprint = SchemaFingerprint([0; 32]);
        assert_ne!(changed.fingerprint, entry.fingerprint);
        assert!(
            catalog
                .check_published_parity(&service, &BTreeSet::from([changed]))
                .is_err()
        );
        // Schema drift, disappearance and loss of configured availability each
        // independently invalidate the original immutable publication.
        for mode in [1, 2, 3] {
            state.store(mode, Ordering::SeqCst);
            assert!(catalog.check_parity(&service).is_err());
            assert!(catalog.check_published_parity(&service, &allowed).is_err());
        }
    }

    struct BlankReasonAvailabilityTool;

    #[async_trait]
    impl Tool for BlankReasonAvailabilityTool {
        fn name(&self) -> &str {
            "prx_blank_reason"
        }

        fn description(&self) -> &str {
            "PRX availability compatibility fixture"
        }

        fn parameters_schema(&self) -> Value {
            json!({"type":"object","properties":{}})
        }

        fn availability(&self) -> CapabilityAvailability {
            CapabilityAvailability {
                level: CapabilityAvailabilityLevel::Declared,
                reason: String::new(),
            }
        }

        async fn execute(&self, _args: Value) -> anyhow::Result<ToolResult> {
            Ok(ToolResult {
                success: true,
                output: String::new(),
                error: None,
            })
        }
    }

    #[test]
    fn parity_preserves_legacy_empty_availability_reason_promotion() {
        let registry: Vec<Box<dyn Tool>> = vec![Box::new(BlankReasonAvailabilityTool)];
        let catalog =
            CapabilityCatalog::build(&registry, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
        let service =
            ToolExecutionService::with_security_policy(registry, Arc::new(crate::security::SecurityPolicy::default()));
        assert_eq!(
            service
                .current_descriptor("prx_blank_reason")
                .unwrap()
                .availability
                .level,
            CapabilityAvailabilityLevel::Ready
        );
        assert!(catalog.check_parity(&service).is_ok());
    }

    struct BorrowedSpecTool {
        spec: ToolSpec,
        owned_calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Tool for BorrowedSpecTool {
        fn name(&self) -> &str {
            &self.spec.name
        }

        fn description(&self) -> &str {
            &self.spec.description
        }

        fn parameters_schema(&self) -> Value {
            self.spec.parameters.clone()
        }

        fn catalog_specs(&self) -> Vec<ToolSpec> {
            self.owned_calls.fetch_add(1, Ordering::SeqCst);
            vec![self.spec.clone()]
        }

        fn immutable_catalog_specs(&self) -> Option<&[ToolSpec]> {
            Some(std::slice::from_ref(&self.spec))
        }

        async fn execute(&self, _args: Value) -> anyhow::Result<ToolResult> {
            anyhow::bail!("borrowed-spec fixture must not execute")
        }
    }

    #[test]
    fn parity_borrows_immutable_specs_without_rebuilding_owned_inventory() {
        let owned_calls = Arc::new(AtomicUsize::new(0));
        let registry: Vec<Box<dyn Tool>> = vec![Box::new(BorrowedSpecTool {
            spec: ToolSpec {
                name: "prx_borrowed".to_string(),
                description: "PRX borrowed spec fixture".to_string(),
                parameters: json!({"type":"object","properties":{}}),
            },
            owned_calls: Arc::clone(&owned_calls),
        })];
        let catalog =
            CapabilityCatalog::build(&registry, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
        let service =
            ToolExecutionService::with_security_policy(registry, Arc::new(crate::security::SecurityPolicy::default()));
        let before = owned_calls.load(Ordering::SeqCst);
        assert!(catalog.check_parity(&service).is_ok());
        assert_eq!(owned_calls.load(Ordering::SeqCst), before);
    }

    #[test]
    fn catalog_fingerprint_equals_compiled_tool_context_per_spec_sha() {
        let tools = registry();
        let catalog = CapabilityCatalog::build(&tools, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
        let compiled =
            crate::agent::turn_context::CompiledToolContext::new(true, tools.iter().map(|tool| tool.spec()).collect());
        for entry in catalog.view(&CatalogScope::default()).entries() {
            let compiled_spec = compiled
                .fingerprint()
                .tools
                .iter()
                .find(|spec| spec.name == entry.public_name)
                .unwrap();
            assert_eq!(hex::encode(entry.fingerprint.0), compiled_spec.sha256);
        }
    }

    /// The canonical ids that `required-tools` takes are the ones the catalog
    /// builds: Shell retains its historical file-system ID, and a discovered MCP
    /// tool is filed under its server with its alias as the public name.
    #[test]
    fn canonical_ids_of_the_shell_and_a_discovered_mcp_tool_are_the_documented_ones() {
        use crate::skills::lifecycle::{Bundle, BundleEntry, validate_manifest_strict};
        let workspace = tempfile::tempdir().unwrap();
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(crate::tools::shell::ShellTool::new(
            Arc::new(crate::runtime::NativeRuntime::new()),
            workspace.path().to_path_buf(),
        ))];
        let mut sources = CatalogSources::default();
        sources.mcp.push(McpCapabilitySpec {
            server: "notes".into(),
            tool: "search".into(),
            state_digest: [0; 32],
            full_only: false,
            spec: ToolSpec {
                name: "mcp__notes__search".into(),
                description: "search notes".into(),
                parameters: json!({"type":"object","properties":{}}),
            },
        });
        let catalog = CapabilityCatalog::build(&tools, &sources, &SurfacePolicy::default()).unwrap();
        let shell = catalog.entry_for_public_name("shell").unwrap().id.as_str().to_owned();
        let mcp = catalog
            .entry_for_public_name("mcp__notes__search")
            .unwrap()
            .id
            .as_str()
            .to_owned();
        assert_eq!(shell, "filesystem:native:shell");
        let shell_entry = catalog.entry_for_public_name("shell").unwrap();
        assert!(shell_entry.categories.contains(&ToolCategory::FileSystem));
        assert_eq!(shell_entry.family, CapabilityTarget::Shell);
        assert_eq!(shell_entry.effect, SideEffectClass::ProcessExec);
        let filesystem = catalog.view(&CatalogScope {
            families: BTreeSet::from([CapabilityTarget::Filesystem]),
            ..CatalogScope::default()
        });
        assert!(filesystem.iter_entries().all(|entry| entry.public_name != "shell"));
        let mut legacy_entries = catalog.entries.clone();
        legacy_entries
            .iter_mut()
            .find(|entry| entry.public_name == "shell")
            .unwrap()
            .family = CapabilityTarget::Filesystem;
        assert_ne!(
            generation_of(&sources, &SurfacePolicy::default(), &legacy_entries),
            *catalog.generation(),
            "semantic family drift must invalidate approvals even with stable capability IDs"
        );
        assert_eq!(mcp, "mcp:notes:mcp__notes__search");
        // Exactly these strings are accepted by the lifecycle manifest check.
        let skill = format!("---\nname: ids\ndescription: ids\nrequired-tools: [{shell}, {mcp}]\n---\n\nBody.\n");
        let bundle = Bundle::from_entries(vec![BundleEntry {
            path: std::path::PathBuf::from("SKILL.md"),
            bytes: skill.into_bytes(),
            executable: false,
        }])
        .unwrap();
        let info = validate_manifest_strict(&bundle).unwrap();
        assert_eq!(info.required_tools, vec![shell, mcp]);
    }

    /// Every id the real catalog builds, for the built-in tools and for both MCP
    /// forms, passes the strict write-time shape check of `required-tools`.
    #[test]
    fn every_id_the_catalog_builds_passes_the_required_tools_shape_check() {
        use crate::skills::lifecycle::{Bundle, BundleEntry, validate_manifest_strict};
        let workspace = tempfile::tempdir().unwrap();
        let runtime: Arc<dyn crate::runtime::RuntimeAdapter> = Arc::new(crate::runtime::NativeRuntime::new());
        let security = Arc::new(crate::security::SecurityPolicy::default());
        let tools: Vec<Box<dyn Tool>> = vec![
            Box::new(crate::tools::shell::ShellTool::new(
                runtime,
                workspace.path().to_path_buf(),
            )),
            Box::new(crate::tools::file_read::FileReadTool::new(security, false)),
        ];
        let mut sources = CatalogSources::default();
        for server in ["notes", "my__srv"] {
            sources.mcp.push(McpCapabilitySpec {
                server: server.into(),
                tool: "search".into(),
                state_digest: [0; 32],
                full_only: false,
                spec: ToolSpec {
                    name: format!("mcp__{server}__search"),
                    description: "search".into(),
                    parameters: json!({"type":"object","properties":{}}),
                },
            });
        }
        let catalog = CapabilityCatalog::build(&tools, &sources, &SurfacePolicy::default()).unwrap();
        // An alias that was not discovered is filed under the namespace `mcp`.
        let alias = CapabilityId::new(CapabilityTarget::Mcp, "mcp", "mcp__late__search").unwrap();
        let mut ids: Vec<String> = catalog
            .view(&CatalogScope::default())
            .iter_entries()
            .map(|entry| entry.id.as_str().to_owned())
            .collect();
        ids.push(alias.as_str().to_owned());
        assert!(ids.iter().any(|id| id == "filesystem:native:shell"), "{ids:?}");
        assert!(ids.iter().any(|id| id == "filesystem:native:file_read"), "{ids:?}");
        assert!(ids.iter().any(|id| id == "mcp:my__srv:mcp__my__srv__search"), "{ids:?}");
        for id in &ids {
            let skill = format!("---\nname: ids\ndescription: ids\nrequired-tools: [{id}]\n---\n\nBody.\n");
            let bundle = Bundle::from_entries(vec![BundleEntry {
                path: std::path::PathBuf::from("SKILL.md"),
                bytes: skill.into_bytes(),
                executable: false,
            }])
            .unwrap();
            let info = validate_manifest_strict(&bundle).unwrap_or_else(|e| panic!("{id} must pass: {e}"));
            assert_eq!(info.required_tools, vec![id.clone()]);
        }
    }

    #[test]
    fn catalog_rejects_duplicate_public_names_before_execution_deduplicates() {
        let tools: Vec<Box<dyn Tool>> = vec![
            Box::new(ProbeTool {
                name: "prx_search",
                description: "first",
            }),
            Box::new(ProbeTool {
                name: "prx_search",
                description: "second",
            }),
        ];
        assert!(matches!(
            CapabilityCatalog::build(&tools, &CatalogSources::default(), &SurfacePolicy::default()),
            Err(CatalogError::Duplicate)
        ));
    }

    #[test]
    fn exact_name_precedes_ranked_search_and_exclusions() {
        let mut surface = SurfacePolicy::default();
        surface.excluded_names.insert("prx_read".to_string());
        let catalog = CapabilityCatalog::build(&registry(), &CatalogSources::default(), &surface).unwrap();
        let view = catalog.view(&CatalogScope::default());
        let hit = CatalogSearch::find(&view, "prx_search", &[], 5);
        assert_eq!(hit.len(), 1);
        assert_eq!(hit.first().unwrap().entry.public_name, "prx_search");
        assert!(
            CatalogSearch::find(&view, "prx_read", &[], 5)
                .iter()
                .all(|hit| hit.entry.public_name != "prx_read")
        );
        assert!(CatalogSearch::find(&view, "*", &[], 5).is_empty());
    }

    #[test]
    fn unspaced_script_query_matches_substring_of_longer_summary() {
        // Neutral code points exercise the same bigram branch as CJK without
        // embedding non-English prose in source or test fixtures.
        assert!(crate::tools::capability::search::character_is_unspaced_script(
            char::from_u32(0x4e00).unwrap()
        ));
        let description: String = (0x0e01..=0x0e08).filter_map(char::from_u32).collect();
        let query: String = description.chars().skip(2).take(3).collect();
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(ProbeTool {
            name: "prx_remote",
            description: Box::leak(description.into_boxed_str()),
        })];
        let catalog = CapabilityCatalog::build(&tools, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
        let view = catalog.view(&CatalogScope::default());
        let hits = CatalogSearch::find(&view, &query, &[], 5);
        assert_eq!(
            hits.first().map(|hit| hit.entry.public_name.as_str()),
            Some("prx_remote")
        );
    }

    #[test]
    fn excluding_dispatch_roots_hides_synthetic_children() {
        let temp = tempfile::tempdir().unwrap();
        let mut sources = CatalogSources::default();
        sources.node_key = Some(DecisionKey::load_or_create(temp.path()).unwrap());
        sources.skills.push(SkillMetadata {
            name: "prx_skill".into(),
            description: "Skill".into(),
            location: temp.path().join("SKILL.md"),
            frontmatter_sha: [1; 32],
        });
        sources.nodes.push(NodeSource {
            id: "prx_node".into(),
            availability: CapabilityAvailability::configured("configured"),
            binding_digest: [2; 32],
        });
        sources.mcp.push(McpCapabilitySpec {
            server: "prx_server".into(),
            tool: "read".into(),
            state_digest: [3; 32],
            full_only: false,
            spec: ToolSpec {
                name: "mcp__prx_server__read".into(),
                description: "MCP".into(),
                parameters: json!({"type":"object","properties":{}}),
            },
        });
        let mut surface = SurfacePolicy::default();
        surface
            .excluded_names
            .extend(["skill_read", "nodes", "mcp_call"].map(str::to_string));
        let catalog = CapabilityCatalog::build(&[], &sources, &surface).unwrap();
        assert!(catalog.view(&CatalogScope::default()).entries().is_empty());
        for entry in &catalog.entries {
            let reference = CapabilityRef {
                id: entry.id.clone(),
                fingerprint: entry.fingerprint,
                generation: *catalog.generation(),
            };
            assert!(matches!(
                catalog.resolve(&[reference]),
                Err(StaleCapability::Unavailable)
            ));
        }
    }

    #[test]
    fn changed_schema_invalidates_pinned_reference() {
        let original =
            CapabilityCatalog::build(&registry(), &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
        let entry = original
            .view(&CatalogScope::default())
            .entries()
            .first()
            .copied()
            .unwrap();
        let reference = CapabilityRef {
            id: entry.id.clone(),
            fingerprint: entry.fingerprint,
            generation: *original.generation(),
        };
        let changed: Vec<Box<dyn Tool>> = vec![Box::new(ProbeTool {
            name: "prx_search",
            description: "changed schema description",
        })];
        let changed =
            CapabilityCatalog::build(&changed, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
        assert!(matches!(changed.resolve(&[reference]), Err(StaleCapability::Stale)));
    }

    #[test]
    fn unchanged_mcp_schema_with_changed_destination_invalidates_generation() {
        let mut sources = CatalogSources::default();
        sources.mcp.push(McpCapabilitySpec {
            server: "prx_server".into(),
            tool: "read".into(),
            state_digest: [1; 32],
            full_only: false,
            spec: ToolSpec {
                name: "mcp__prx_server__read".into(),
                description: "Read remote data".into(),
                parameters: json!({"type":"object","properties":{}}),
            },
        });
        let first = CapabilityCatalog::build(&[], &sources, &SurfacePolicy::default()).unwrap();
        let entry = *first.view(&CatalogScope::default()).entries().first().unwrap();
        let reference = CapabilityRef {
            id: entry.id.clone(),
            fingerprint: entry.fingerprint,
            generation: *first.generation(),
        };
        sources.mcp.first_mut().unwrap().state_digest = [2; 32];
        let changed = CapabilityCatalog::build(&[], &sources, &SurfacePolicy::default()).unwrap();
        assert_ne!(first.generation(), changed.generation());
        assert!(matches!(changed.resolve(&[reference]), Err(StaleCapability::Stale)));
    }

    #[test]
    fn node_uses_keyed_canonical_identity_and_requires_key() {
        let mut sources = CatalogSources::default();
        sources.nodes.push(NodeSource {
            id: "Box:1".into(),
            availability: CapabilityAvailability::configured("configured"),
            binding_digest: [3; 32],
        });
        assert!(matches!(
            CapabilityCatalog::build(&[], &sources, &SurfacePolicy::default()),
            Err(CatalogError::Identity)
        ));
        let temp = tempfile::tempdir().unwrap();
        let key = DecisionKey::load_or_create(temp.path()).unwrap();
        let expected = CapabilityId::remote_node(&key, "Box:1").unwrap();
        sources.node_key = Some(key);
        let catalog = CapabilityCatalog::build(&[], &sources, &SurfacePolicy::default()).unwrap();
        let entry = catalog.entries.first().unwrap();
        assert_eq!(entry.id, expected);
        assert_ne!(entry.public_name, "Box:1");
    }

    #[test]
    fn surface_policy_changes_invalidate_refs() {
        let tools = registry();
        let first = CapabilityCatalog::build(&tools, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
        let entry = *first.view(&CatalogScope::default()).entries().first().unwrap();
        let reference = CapabilityRef {
            id: entry.id.clone(),
            fingerprint: entry.fingerprint,
            generation: *first.generation(),
        };
        let mut changed_surface = SurfacePolicy::default();
        changed_surface.channel_allowed = true;
        let changed = CapabilityCatalog::build(&tools, &CatalogSources::default(), &changed_surface).unwrap();
        assert!(matches!(changed.resolve(&[reference]), Err(StaleCapability::Stale)));
    }

    #[test]
    fn high_risk_root_actions_are_never_local_write() {
        for name in ["nodes", "mcp_call", "skill_execute", "sessions_spawn", "delegate"] {
            let tools: Vec<Box<dyn Tool>> = vec![Box::new(ProbeTool {
                name,
                description: "Act",
            })];
            let catalog =
                CapabilityCatalog::build(&tools, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
            let entry = catalog.entries.first().unwrap();
            assert!(entry.effect >= SideEffectClass::ProcessExec, "{name}");
            assert!(matches!(entry.approval, ApprovalRequirement::Required { .. }), "{name}");
            assert!(entry.op_ids.is_empty(), "{name} has call-scoped operations");
        }
    }

    struct DynamicAliasTool(Arc<AtomicUsize>);

    #[async_trait]
    impl Tool for DynamicAliasTool {
        fn name(&self) -> &str {
            "mcp_call"
        }
        fn description(&self) -> &str {
            "Dynamic tool"
        }
        fn parameters_schema(&self) -> Value {
            json!({"type":"object","properties":{}})
        }
        fn specs(&self) -> Vec<ToolSpec> {
            vec![self.spec()]
        }
        fn catalog_specs(&self) -> Vec<ToolSpec> {
            let mut specs = self.specs();
            if self.0.load(Ordering::SeqCst) != 0 {
                specs.push(ToolSpec {
                    name: "prx_collision".into(),
                    description: "new alias".into(),
                    parameters: self.parameters_schema(),
                });
            }
            specs
        }
        fn spec_for_name(&self, name: &str) -> Option<ToolSpec> {
            self.catalog_specs().into_iter().find(|spec| spec.name == name)
        }
        fn supports_name(&self, name: &str) -> bool {
            self.spec_for_name(name).is_some()
        }
        async fn execute(&self, _args: Value) -> anyhow::Result<ToolResult> {
            Ok(ToolResult {
                success: true,
                output: String::new(),
                error: None,
            })
        }
    }

    #[test]
    fn late_alias_collision_does_not_take_native_startup_owner_and_fails_parity() {
        let state = Arc::new(AtomicUsize::new(0));
        let registry: Vec<Box<dyn Tool>> = vec![
            Box::new(DynamicAliasTool(Arc::clone(&state))),
            Box::new(ProbeTool {
                name: "prx_collision",
                description: "native owner",
            }),
        ];
        let catalog =
            CapabilityCatalog::build(&registry, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
        let service =
            ToolExecutionService::with_security_policy(registry, Arc::new(crate::security::SecurityPolicy::default()));
        assert_eq!(
            service.current_descriptor("prx_collision").unwrap().backend_name,
            "prx_collision"
        );
        assert!(catalog.check_parity(&service).is_ok());
        state.store(1, Ordering::SeqCst);
        assert_eq!(
            service.current_descriptor("prx_collision").unwrap().backend_name,
            "prx_collision"
        );
        assert!(catalog.check_parity(&service).is_err());
    }

    struct MutableBindingTool(Arc<AtomicUsize>, [u8; 32]);

    #[async_trait]
    impl Tool for MutableBindingTool {
        fn name(&self) -> &str {
            "skill_read"
        }
        fn description(&self) -> &str {
            "Read a selected skill"
        }
        fn parameters_schema(&self) -> Value {
            json!({"type":"object","properties":{"name":{"type":"string"}},"required":["name"]})
        }
        fn catalog_binding_snapshot(&self) -> BTreeMap<String, [u8; 32]> {
            if self.0.load(Ordering::SeqCst) == 0 {
                BTreeMap::from([("prx_skill".into(), self.1)])
            } else {
                BTreeMap::new()
            }
        }
        async fn execute(&self, _args: Value) -> anyhow::Result<ToolResult> {
            Ok(ToolResult {
                success: true,
                output: String::new(),
                error: None,
            })
        }
    }

    #[test]
    fn synthetic_skill_parity_fails_when_exact_target_disappears() {
        let temp = tempfile::tempdir().unwrap();
        let skill = SkillMetadata {
            name: "prx_skill".into(),
            description: "Read it".into(),
            location: temp.path().join("SKILL.md"),
            frontmatter_sha: [4; 32],
        };
        let digest = crate::skills::catalog_skill_binding_digest(&skill);
        let mut sources = CatalogSources::default();
        sources.skills.push(skill);
        let state = Arc::new(AtomicUsize::new(0));
        let registry: Vec<Box<dyn Tool>> = vec![Box::new(MutableBindingTool(Arc::clone(&state), digest))];
        let catalog = CapabilityCatalog::build(&registry, &sources, &SurfacePolicy::default()).unwrap();
        let service =
            ToolExecutionService::with_security_policy(registry, Arc::new(crate::security::SecurityPolicy::default()));
        assert!(catalog.check_parity(&service).is_ok());
        state.store(1, Ordering::SeqCst);
        assert!(catalog.check_parity(&service).is_err());
    }

    #[test]
    fn instruction_read_wire_alias_collision_is_refused_instead_of_merging_targets() {
        let temp = tempfile::tempdir().unwrap();
        let skill = SkillMetadata {
            name: "Deploy helper".into(),
            description: "Read one skill".into(),
            location: temp.path().join("first/SKILL.md"),
            frontmatter_sha: [4; 32],
        };
        let mut colliding = skill.clone();
        colliding.name = crate::tools::skills::catalog_skill_read_name(&skill.name);
        colliding.location = temp.path().join("second/SKILL.md");
        let sources = CatalogSources {
            skills: vec![skill, colliding],
            ..Default::default()
        };
        assert!(matches!(
            CapabilityCatalog::build(&[], &sources, &SurfacePolicy::default()),
            Err(CatalogError::Duplicate)
        ));
    }

    #[test]
    fn native_identity_and_instruction_alias_survive_the_bound_process_snapshot() {
        let skill = SkillMetadata {
            name: "Deploy helper".into(),
            description: "Read one skill".into(),
            location: std::path::PathBuf::from("/prx-workspace/skills/deploy/SKILL.md"),
            frontmatter_sha: [4; 32],
        };
        let alias = crate::tools::skills::catalog_skill_read_name(&skill.name);
        let sources = CatalogSources {
            skills: vec![skill.clone()],
            ..Default::default()
        };
        let workspace = tempfile::tempdir().unwrap();
        let registry: Vec<Box<dyn Tool>> = vec![
            Box::new(crate::tools::shell::ShellTool::new(
                Arc::new(crate::runtime::NativeRuntime::new()),
                workspace.path().to_path_buf(),
            )),
            Box::new(DocumentCategoryProbe("document_ingest")),
            Box::new(DocumentCategoryProbe("document_sync")),
        ];
        let catalog = CapabilityCatalog::build(&registry, &sources, &SurfacePolicy::default()).unwrap();
        let key = crate::security::runtime_key::test_support::ephemeral_key();
        let snapshot = catalog.bind_process(&key).unwrap();
        let restored = CapabilityCatalog::restore_process(&snapshot, &key, *catalog.generation()).unwrap();
        for (name, family) in [
            ("shell", CapabilityTarget::Shell),
            ("document_ingest", CapabilityTarget::Document),
            ("document_sync", CapabilityTarget::Document),
        ] {
            let entry = restored.entry_for_public_name(name).unwrap();
            let parent = catalog.entry_for_public_name(name).unwrap();
            assert_eq!(entry.family, family);
            assert_eq!(entry.id.as_str(), format!("filesystem:native:{name}"));
            assert_eq!(entry.id, parent.id);
            assert_eq!(entry.fingerprint, parent.fingerprint);
            assert_eq!(entry.generation, parent.generation);
        }
        assert!(
            CapabilityCatalog::restore_process(
                &snapshot,
                &crate::security::runtime_key::test_support::ephemeral_key(),
                *catalog.generation()
            )
            .is_err(),
            "identity compatibility cannot bypass catalog authentication"
        );
        let entry = restored.entry_for_public_name(&alias).unwrap();
        assert_eq!(entry.skill_metadata.as_ref().unwrap().name, skill.name);
        assert_eq!(
            entry.binding_digest,
            Some(crate::skills::catalog_skill_binding_digest(&skill))
        );
        assert_eq!(
            entry.schema.pointer("/properties/name/const"),
            Some(&json!("Deploy helper"))
        );
        assert!(
            matches!(&entry.target, ExecutionTarget::Skill { name, root } if name=="Deploy helper" && root=="skill_read")
        );
    }

    struct CountingBindingTool {
        calls: Arc<AtomicUsize>,
        bindings: BTreeMap<String, [u8; 32]>,
    }

    #[async_trait]
    impl Tool for CountingBindingTool {
        fn name(&self) -> &str {
            "skill_read"
        }
        fn description(&self) -> &str {
            "Read selected skill"
        }
        fn parameters_schema(&self) -> Value {
            json!({"type":"object","properties":{"name":{"type":"string"}},"required":["name"]})
        }
        fn catalog_binding_snapshot(&self) -> BTreeMap<String, [u8; 32]> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.bindings.clone()
        }
        async fn execute(&self, _args: Value) -> anyhow::Result<ToolResult> {
            Ok(ToolResult {
                success: true,
                output: String::new(),
                error: None,
            })
        }
    }

    #[test]
    fn parity_reads_one_binding_snapshot_for_many_skills() {
        let temp = tempfile::tempdir().unwrap();
        let mut sources = CatalogSources::default();
        let mut bindings = BTreeMap::new();
        for index in 0..32 {
            let skill = SkillMetadata {
                name: format!("prx_skill_{index}"),
                description: "Skill".into(),
                location: temp.path().join(format!("skill_{index}/SKILL.md")),
                frontmatter_sha: [index; 32],
            };
            bindings.insert(skill.name.clone(), crate::skills::catalog_skill_binding_digest(&skill));
            sources.skills.push(skill);
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let registry: Vec<Box<dyn Tool>> = vec![Box::new(CountingBindingTool {
            calls: Arc::clone(&calls),
            bindings,
        })];
        let catalog = CapabilityCatalog::build(&registry, &sources, &SurfacePolicy::default()).unwrap();
        let service =
            ToolExecutionService::with_security_policy(registry, Arc::new(crate::security::SecurityPolicy::default()));
        assert!(catalog.check_parity(&service).is_ok());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    struct MutableMcpDestinationTool(Arc<AtomicUsize>);

    #[async_trait]
    impl Tool for MutableMcpDestinationTool {
        fn name(&self) -> &str {
            "mcp_call"
        }
        fn description(&self) -> &str {
            "MCP root"
        }
        fn parameters_schema(&self) -> Value {
            json!({"type":"object","properties":{}})
        }
        fn catalog_specs(&self) -> Vec<ToolSpec> {
            vec![
                self.spec(),
                ToolSpec {
                    name: "mcp__prx_server__read".into(),
                    description: "Read remote data".into(),
                    parameters: self.parameters_schema(),
                },
            ]
        }
        fn catalog_binding_snapshot(&self) -> BTreeMap<String, [u8; 32]> {
            let byte = u8::try_from(self.0.load(Ordering::SeqCst)).unwrap_or_default() + 1;
            BTreeMap::from([("mcp__prx_server__read".into(), [byte; 32])])
        }
        fn supports_name(&self, name: &str) -> bool {
            name == self.name() || name == "mcp__prx_server__read"
        }
        fn spec_for_name(&self, name: &str) -> Option<ToolSpec> {
            self.catalog_specs().into_iter().find(|spec| spec.name == name)
        }
        async fn execute(&self, _args: Value) -> anyhow::Result<ToolResult> {
            Ok(ToolResult {
                success: true,
                output: String::new(),
                error: None,
            })
        }
    }

    #[test]
    fn same_schema_mcp_destination_change_fails_live_parity() {
        let state = Arc::new(AtomicUsize::new(0));
        let mut sources = CatalogSources::default();
        sources.mcp.push(McpCapabilitySpec {
            server: "prx_server".into(),
            tool: "read".into(),
            state_digest: [1; 32],
            full_only: false,
            spec: ToolSpec {
                name: "mcp__prx_server__read".into(),
                description: "Read remote data".into(),
                parameters: json!({"type":"object","properties":{}}),
            },
        });
        let registry: Vec<Box<dyn Tool>> = vec![Box::new(MutableMcpDestinationTool(Arc::clone(&state)))];
        let catalog = CapabilityCatalog::build(&registry, &sources, &SurfacePolicy::default()).unwrap();
        let service =
            ToolExecutionService::with_security_policy(registry, Arc::new(crate::security::SecurityPolicy::default()));
        assert!(catalog.check_parity(&service).is_ok());
        state.store(1, Ordering::SeqCst);
        assert!(catalog.check_parity(&service).is_err());
        sources.mcp.first_mut().unwrap().state_digest = [2; 32];
        let rebuilt = CapabilityCatalog::build(
            &[Box::new(MutableMcpDestinationTool(state))],
            &sources,
            &SurfacePolicy::default(),
        )
        .unwrap();
        assert_ne!(catalog.generation(), rebuilt.generation());
    }

    #[test]
    fn child_parity_rejects_same_schema_mcp_destination_change() {
        use crate::tools::capability::child::{
            AuthorityEnvelope, CapabilityConstraints, ChildCapabilityManifest, VerifiedChildScope,
        };

        let state = Arc::new(AtomicUsize::new(0));
        let mut sources = CatalogSources::default();
        sources.mcp.push(McpCapabilitySpec {
            server: "prx_server".into(),
            tool: "read".into(),
            state_digest: [1; 32],
            full_only: false,
            spec: ToolSpec {
                name: "mcp__prx_server__read".into(),
                description: "Read remote data".into(),
                parameters: json!({"type":"object","properties":{}}),
            },
        });
        let registry: Vec<Box<dyn Tool>> = vec![Box::new(MutableMcpDestinationTool(Arc::clone(&state)))];
        let catalog = CapabilityCatalog::build(&registry, &sources, &SurfacePolicy::default()).unwrap();
        let service =
            ToolExecutionService::with_security_policy(registry, Arc::new(crate::security::SecurityPolicy::default()));
        let child_reference = catalog
            .entries
            .iter()
            .find(|entry| entry.public_name == "mcp__prx_server__read")
            .map(|entry| CapabilityRef {
                id: entry.id.clone(),
                fingerprint: entry.fingerprint,
                generation: entry.generation,
            })
            .expect("MCP alias reference");
        let allowed: BTreeSet<_> = catalog
            .entries
            .iter()
            .map(|entry| CapabilityRef {
                id: entry.id.clone(),
                fingerprint: entry.fingerprint,
                generation: entry.generation,
            })
            .collect();
        let workspace = tempfile::tempdir().unwrap();
        let key = DecisionKey::load_or_create(workspace.path()).unwrap();
        let parent = CapabilityConstraints {
            parent_authority_id: crate::security::decision_id::DecisionId::parse(
                "0192f0c4-0000-7000-8000-000000000007",
            )
            .expect("test: id")
            .into(),
            allowed,
            authority: AuthorityEnvelope {
                owner: "prx_user".into(),
                workspace: "prx_workspace".into(),
                level: crate::security::AutonomyLevel::Supervised,
                max_cost_cents: 100,
                remaining_depth: 2,
                delegation: None,
            },
            catalog_generation: *catalog.generation(),
            key,
        };
        let manifest = ChildCapabilityManifest::derive_strict_subset(&parent, &[child_reference]).unwrap();
        let scope = VerifiedChildScope::new(manifest, &parent).unwrap();

        assert!(catalog.check_child_parity(&service, &scope).is_ok());
        state.store(1, Ordering::SeqCst);
        assert!(catalog.check_child_parity(&service, &scope).is_err());
    }

    struct MutableSchemaTool(Arc<AtomicUsize>);

    #[async_trait]
    impl Tool for MutableSchemaTool {
        fn name(&self) -> &str {
            "prx_mutable"
        }

        fn description(&self) -> &str {
            "Mutable schema test tool"
        }

        fn parameters_schema(&self) -> Value {
            json!({"type":"object","properties":{"version":{"const":self.0.load(Ordering::SeqCst)}}})
        }

        async fn execute(&self, _args: Value) -> anyhow::Result<ToolResult> {
            Ok(ToolResult {
                success: true,
                output: String::new(),
                error: None,
            })
        }
    }

    #[test]
    fn execution_descriptor_rechecks_the_current_schema_after_startup() {
        let version = Arc::new(AtomicUsize::new(1));
        let service = ToolExecutionService::with_security_policy(
            vec![Box::new(MutableSchemaTool(Arc::clone(&version)))],
            Arc::new(crate::security::SecurityPolicy::default()),
        );
        let initial = service.current_descriptor("prx_mutable").unwrap();
        assert_eq!(initial.parameters.pointer("/properties/version/const"), Some(&json!(1)));
        version.store(2, Ordering::SeqCst);
        let current = service.current_descriptor("prx_mutable").unwrap();
        assert_eq!(current.parameters.pointer("/properties/version/const"), Some(&json!(2)));
    }

    #[test]
    fn excluded_executable_schema_change_still_fails_reverse_parity() {
        let version = Arc::new(AtomicUsize::new(1));
        let registry: Vec<Box<dyn Tool>> = vec![Box::new(MutableSchemaTool(Arc::clone(&version)))];
        let mut surface = SurfacePolicy::default();
        surface.excluded_names.insert("prx_mutable".into());
        let catalog = CapabilityCatalog::build(&registry, &CatalogSources::default(), &surface).unwrap();
        let service =
            ToolExecutionService::with_security_policy(registry, Arc::new(crate::security::SecurityPolicy::default()));
        assert!(catalog.check_parity(&service).is_ok());
        version.store(2, Ordering::SeqCst);
        assert!(catalog.check_parity(&service).is_err());
    }

    #[test]
    fn scoped_view_construction_allocates_nothing_for_any_inventory_size() {
        for size in [0, 1, 64, 256] {
            let sources = CatalogSources {
                mcp: (0..size)
                    .map(|index| McpCapabilitySpec {
                        server: "prx_server".into(),
                        tool: format!("read_{index}"),
                        state_digest: [0; 32],
                        full_only: false,
                        spec: ToolSpec {
                            name: format!("mcp__prx_server__read_{index}"),
                            description: "PRX inventory fixture".into(),
                            parameters: json!({"type":"object","properties":{}}),
                        },
                    })
                    .collect(),
                ..CatalogSources::default()
            };
            let surface = SurfacePolicy {
                allowed_tools: None,
                channel_allowed: true,
                ..SurfacePolicy::default()
            };
            let catalog = CapabilityCatalog::build(&[], &sources, &surface).unwrap();
            for families in [BTreeSet::new(), BTreeSet::from([CapabilityTarget::Mcp])] {
                let scope = CatalogScope {
                    families,
                    channel: true,
                };
                let (view, allocations) =
                    crate::decision_engine::prediction::builtin::allocation_counter::count(|| catalog.view(&scope));
                assert_eq!(allocations, 0, "view allocations at inventory size {size}");
                assert_eq!(view.entries().len(), size);
            }
        }
    }

    #[test]
    fn borrowed_visibility_matches_cached_entries_and_all_search_paths() {
        let sources = CatalogSources {
            mcp: vec![McpCapabilitySpec {
                server: "prx_server".into(),
                tool: "read".into(),
                state_digest: [0; 32],
                full_only: false,
                spec: ToolSpec {
                    name: "mcp__prx_server__read".into(),
                    description: "Search local evidence".into(),
                    parameters: json!({"type":"object","properties":{}}),
                },
            }],
            ..CatalogSources::default()
        };
        let surface = SurfacePolicy {
            allowed_tools: None,
            channel_allowed: true,
            excluded_names: BTreeSet::from(["prx_search".into()]),
            ..SurfacePolicy::default()
        };
        let mut catalog = CapabilityCatalog::build(&registry(), &sources, &surface).unwrap();
        catalog
            .entries
            .iter_mut()
            .find(|entry| entry.public_name == "prx_read")
            .unwrap()
            .channel_eligible = false;
        let scopes = std::iter::once(BTreeSet::new())
            .chain(CapabilityTarget::ALL.map(|family| BTreeSet::from([family])))
            .chain(std::iter::once(BTreeSet::from([
                CapabilityTarget::NativeTool,
                CapabilityTarget::Mcp,
            ])));
        for families in scopes {
            for channel in [false, true] {
                let scope = CatalogScope {
                    families: families.clone(),
                    channel,
                };
                let expected: Vec<_> = catalog
                    .entries
                    .iter()
                    .filter(|entry| {
                        (families.is_empty() || families.contains(&entry.family))
                            && (!channel || entry.channel_eligible)
                            && !matches!(entry.exposure, Exposure::Unavailable { .. })
                    })
                    .map(|entry| &entry.id)
                    .collect();
                for cache_first in [false, true] {
                    let view = catalog.view(&scope);
                    assert_eq!(view.iter_entries().map(|entry| &entry.id).collect::<Vec<_>>(), expected);
                    assert!(
                        view.entries.get().is_none(),
                        "borrowing iteration must not initialize compatibility cache"
                    );
                    if cache_first {
                        assert_eq!(
                            view.entries().iter().map(|entry| &entry.id).collect::<Vec<_>>(),
                            expected
                        );
                    }
                    for entry in &catalog.entries {
                        let visible = expected.contains(&&entry.id);
                        assert_eq!(view.contains(&entry.id), visible);
                        assert!(
                            std::ptr::eq(view.entry_any(&entry.id).unwrap(), entry),
                            "unfiltered gate lookup"
                        );
                        for query in std::iter::once(entry.public_name.as_str())
                            .chain(std::iter::once(entry.id.as_str()))
                            .chain(entry.aliases.iter().map(String::as_str))
                        {
                            for query_families in [Vec::new(), vec![entry.family], vec![CapabilityTarget::None]] {
                                let hits = CatalogSearch::find(&view, query, &query_families, 16);
                                let allowed =
                                    visible && (query_families.is_empty() || query_families.contains(&entry.family));
                                assert_eq!(
                                    hits.iter().any(|hit| hit.entry.id == entry.id),
                                    allowed,
                                    "exact search visibility for {query}"
                                );
                                assert!(hits.iter().all(|hit| expected.contains(&&hit.entry.id)));
                            }
                        }
                    }
                    let ranked = CatalogSearch::find(&view, "local evidence", &[], 16);
                    assert_eq!(
                        ranked.iter().map(|hit| &hit.entry.id).collect::<BTreeSet<_>>(),
                        expected.iter().copied().collect()
                    );
                    assert_eq!(
                        view.entries.get().is_some(),
                        cache_first,
                        "search must not initialize compatibility cache"
                    );
                    assert_eq!(
                        view.entries().iter().map(|entry| &entry.id).collect::<Vec<_>>(),
                        expected
                    );
                    assert_eq!(view.iter_entries().map(|entry| &entry.id).collect::<Vec<_>>(), expected);
                }
            }
        }
    }

    #[test]
    fn thousands_of_mcp_aliases_stay_bounded_and_hidden_metadata_never_matches() {
        let mut sources = CatalogSources::default();
        sources.mcp = (0..4096)
            .map(|index| McpCapabilitySpec {
                server: "prx_server".to_string(),
                tool: format!("tool_{index}"),
                state_digest: [0; 32],
                full_only: false,
                spec: ToolSpec {
                    name: format!("mcp__prx_server__tool_{index}"),
                    description: if index == 4095 {
                        "\u{200b}last tool\nsecret=PRX-FAKE-SENTINEL".to_string()
                    } else {
                        "searchable remote tool".to_string()
                    },
                    parameters: json!({"type":"object","properties":{}}),
                },
            })
            .collect();
        let mut surface = SurfacePolicy::default();
        surface.excluded_names.insert("mcp__prx_server__tool_4095".to_string());
        let catalog = CapabilityCatalog::build(&[], &sources, &surface).unwrap();
        let view = catalog.view(&CatalogScope::default());
        assert_eq!(view.entries().len(), 4095);
        let hits = CatalogSearch::find(&view, "searchable remote tool", &[], 5);
        assert_eq!(hits.len(), 5);
        assert!(
            hits.iter()
                .all(|hit| hit.entry.public_name != "mcp__prx_server__tool_4095")
        );
        assert!(
            CatalogSearch::find(&view, "tool_4095", &[], 5)
                .iter()
                .all(|hit| hit.entry.public_name != "mcp__prx_server__tool_4095")
        );
        assert!(
            catalog
                .entries
                .iter()
                .all(|entry| entry.summary.len() <= 160 * 4 && !entry.summary.contains('\n'))
        );
    }
}

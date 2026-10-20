use super::{Path, Result};

/// Boolean module gates were removed when capabilities became always available.
/// Consume the finite set of legacy keys explicitly so upgrades are compatible,
/// while `deserialize_merged` can reject every genuinely unknown path.
pub(super) const REMOVED_MODULE_GATES: &[&[&str]] = &[
    &["modules", "memory"],
    &["modules", "channels"],
    &["modules", "network"],
    &["modules", "security"],
    &["modules", "scheduler"],
    &["modules", "agent"],
    &["modules", "identity"],
    &["modules", "routing"],
    &["modules", "tools"],
    &["modules", "integrations"],
    &["modules", "nodes"],
    &["modules", "cost"],
    &["modules", "observability"],
    &["agent", "compaction", "os_paging", "enabled"],
    &["sessions_spawn", "enabled"],
    &["self_system", "enabled"],
    &["channels_config", "wacli", "enabled"],
    &["cost", "enabled"],
    &["composio", "enabled"],
    &["webhook", "enabled"],
    &["memory", "events", "enabled"],
    &["nodes", "enabled"],
    &["router", "enabled"],
    &["query_classification", "enabled"],
    &["task_routing", "enabled"],
    &["scheduler", "enabled"],
    &["heartbeat", "enabled"],
    &["xin", "enabled"],
    &["security", "audit", "enabled"],
    &["http_request", "enabled"],
    &["web_search", "enabled"],
    &["multimodal", "enabled"],
    &["skills", "enabled"],
    &["skills", "auto_discover"],
    &["skill_rag", "enabled"],
];

pub(super) fn take_toml_path(root: &mut toml::Value, path: &[&str]) -> Option<toml::Value> {
    let (key, rest) = path.split_first()?;
    let table = root.as_table_mut()?;
    if rest.is_empty() {
        table.remove(*key)
    } else {
        table.get_mut(*key).and_then(|value| take_toml_path(value, rest))
    }
}

pub(super) fn insert_toml_path_if_absent(root: &mut toml::Value, path: &[&str], value: toml::Value) -> bool {
    let Some((key, rest)) = path.split_first() else {
        return false;
    };
    let Some(table) = root.as_table_mut() else {
        return false;
    };
    if rest.is_empty() {
        if table.contains_key(*key) {
            false
        } else {
            table.insert((*key).to_owned(), value);
            true
        }
    } else {
        let child = table
            .entry((*key).to_owned())
            .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
        insert_toml_path_if_absent(child, rest, value)
    }
}

pub(super) fn remove_empty_toml_table(root: &mut toml::Value, path: &[&str]) {
    let Some((key, rest)) = path.split_first() else {
        return;
    };
    let Some(table) = root.as_table_mut() else {
        return;
    };
    if rest.is_empty() {
        if table
            .get(*key)
            .and_then(toml::Value::as_table)
            .is_some_and(toml::map::Map::is_empty)
        {
            table.remove(*key);
        }
        return;
    }
    if let Some(child) = table.get_mut(*key) {
        remove_empty_toml_table(child, rest);
    }
}

pub(super) fn prune_empty_toml_tables(value: &mut toml::Value) {
    let Some(table) = value.as_table_mut() else {
        return;
    };
    for (_, child) in table.iter_mut() {
        prune_empty_toml_tables(child);
    }
    table.retain(|_, child| !child.as_table().is_some_and(toml::map::Map::is_empty));
}

pub(super) fn migrate_toml_key(root: &mut toml::Value, old: &[&str], new: &[&str]) {
    let Some(value) = take_toml_path(root, old) else {
        return;
    };
    let old_name = old.join(".");
    let new_name = new.join(".");
    if insert_toml_path_if_absent(root, new, value) {
        tracing::warn!(old = %old_name, new = %new_name, "Migrated legacy configuration key");
    } else {
        tracing::warn!(old = %old_name, new = %new_name, "Ignored legacy configuration key because the canonical key is also present");
    }
}

pub(super) fn migrate_legacy_config_keys(root: &mut toml::Value, config_path: Option<&Path>) -> Result<()> {
    migrate_toml_key(
        root,
        &["http_request", "max_response_bytes"],
        &["http_request", "max_response_size"],
    );
    migrate_toml_key(
        root,
        &["memory", "embedding", "provider"],
        &["memory", "embedding_provider"],
    );
    migrate_toml_key(root, &["memory", "embedding", "model"], &["memory", "embedding_model"]);
    migrate_toml_key(
        root,
        &["memory", "embedding", "dimension"],
        &["memory", "embedding_dimensions"],
    );
    remove_empty_toml_table(root, &["memory", "embedding"]);

    for path in REMOVED_MODULE_GATES {
        if let Some(value) = take_toml_path(root, path) {
            if !value.is_bool() {
                anyhow::bail!("Removed module gate {} must be a boolean", path.join("."));
            }
            tracing::warn!(path = %path.join("."), "Ignoring removed module gate; capability availability is no longer switch-controlled");
        }
    }
    // Keys a `Config::save` wrote into `[causal_tree]` before the decision
    // plane map to their new homes before the unknown-path check, keeping
    // operator values; one without a faithful home stops the load.
    let locate = |path: &[&str]| config_path.and_then(|main| crate::config::deprecations::file_holding(main, path));
    let causal = crate::causal_tree::config::migrate_legacy(root, &locate)?;
    if causal.found_legacy_keys() {
        tracing::warn!(
            moved = ?causal.moved,
            dropped = ?causal.dropped,
            "Mapped legacy causal_tree keys; the next save writes the new form"
        );
    }
    // Retired keys are dropped *here*, before the unknown-path check below, so
    // that a config written against an older prx keeps loading with a warning
    // instead of failing to start. Only the paths on the published list are
    // absorbed; every other unrecognised key still stops the load, because a
    // typo must not inherit the leniency built for deliberate removals.
    crate::config::deprecations::strip_deprecated_keys(root, config_path);
    prune_empty_toml_tables(root);
    Ok(())
}

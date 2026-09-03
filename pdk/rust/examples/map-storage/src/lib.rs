#[allow(warnings)]
#[cfg(target_arch = "wasm32")]
mod bindings;

struct MapStorage;

#[cfg(target_arch = "wasm32")]
mod wasm_exports {
    use super::{bindings, MapStorage};
    use bindings::exports::prx::plugin::storage_exports::{Guest, MemoryEntry};
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    #[derive(Clone)]
    struct Stored {
        content: String,
        category: String,
        session_id: Option<String>,
    }

    thread_local! {
        static ENTRIES: RefCell<BTreeMap<String, Stored>> = RefCell::new(BTreeMap::new());
    }

    impl Guest for MapStorage {
        fn name() -> String {
            "wasm-map".to_string()
        }

        fn store_memory(
            key: String,
            content: String,
            category: String,
            session_id: Option<String>,
        ) -> Result<(), String> {
            ENTRIES.with(|entries| {
                entries.borrow_mut().insert(key, Stored { content, category, session_id });
            });
            Ok(())
        }

        fn recall_memory(query: String, limit: u32, session_id: Option<String>) -> Result<Vec<MemoryEntry>, String> {
            ENTRIES.with(|entries| {
                Ok(entries
                    .borrow()
                    .iter()
                    .filter(|(key, value)| {
                        (query.is_empty() || key.contains(&query) || value.content.contains(&query))
                            && session_id.as_ref().is_none_or(|session| value.session_id.as_ref() == Some(session))
                    })
                    .take(limit as usize)
                    .map(|(key, value)| MemoryEntry {
                        id: format!("wasm-map:{key}"),
                        key: key.clone(),
                        content: value.content.clone(),
                        category: value.category.clone(),
                        timestamp: "1970-01-01T00:00:00Z".to_string(),
                        score: Some(1.0),
                    })
                    .collect())
            })
        }

        fn forget_memory(key: String) -> Result<bool, String> {
            Ok(ENTRIES.with(|entries| entries.borrow_mut().remove(&key).is_some()))
        }

        fn count_memories() -> Result<u32, String> {
            Ok(ENTRIES.with(|entries| entries.borrow().len().try_into().unwrap_or(u32::MAX)))
        }

        fn health_check() -> bool {
            true
        }
    }

    bindings::export!(MapStorage with_types_in bindings);
}

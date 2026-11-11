//! Transactional child-session records, separate from operator saved chats.

/// An opaque serialized session record. The store owns read/modify/write
/// serialization; the chat layer owns the record schema and field updates.
pub trait ChildSessionStore: Send + Sync {
    fn load(&self, key: &str) -> anyhow::Result<Option<String>>;

    /// Invoke `update` once under the store transaction and commit its result.
    /// A failed callback leaves the previous record untouched.
    fn update(
        &self,
        key: &str,
        update: &mut dyn FnMut(Option<&str>) -> anyhow::Result<String>,
    ) -> anyhow::Result<String>;
}

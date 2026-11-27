//! Argument normalisation that runs before validation.

use super::CompiledToolSchema;
use serde_json::Value;

impl CompiledToolSchema {
    /// Remove every `null` that stands in for an absent optional property.
    ///
    /// Models routinely send `"deliver": null` for a field they mean to leave
    /// out. A key is dropped when this contract declares the property, does not
    /// list it in `required`, and the property's own schema does not accept
    /// `null` (a `null` type, an `anyOf` / `oneOf` null branch or
    /// `nullable: true` keeps its value). Callers compile the schema as
    /// published, before any provider-dialect cleaning, so the same call keeps
    /// or drops a `null` the same way on every provider. A `null` on a
    /// required property is left alone and is reported by the validator as
    /// before. The rule descends into declared properties and array items,
    /// each object using its own `required` list.
    pub fn drop_null_optional_properties(&self, value: &mut Value) {
        match value {
            Value::Object(entries) => {
                entries.retain(|name, entry| {
                    !(entry.is_null()
                        && !self.required.iter().any(|required| required == name)
                        && self
                            .properties
                            .get(name)
                            .is_some_and(|property| !property.nullable_keyword && !property.matches(&Value::Null)))
                });
                for (name, entry) in entries.iter_mut() {
                    if let Some(property) = self.properties.get(name) {
                        property.drop_null_optional_properties(entry);
                    }
                }
            }
            Value::Array(items) => {
                if let Some(item_schema) = self.items.as_ref() {
                    for item in items.iter_mut() {
                        item_schema.drop_null_optional_properties(item);
                    }
                }
            }
            _ => {}
        }
    }
}

//! Reading `agent.compaction.mode`, including the values that were removed.
//!
//! Every config source (TOML load, `POST /api/config` JSON, worker manifests,
//! stored chat state) deserializes through this impl, so the mapping applies
//! to all of them.

use super::agent::AgentCompactionMode;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer};

impl<'de> Deserialize<'de> for AgentCompactionMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        match raw.as_str() {
            "off" => Ok(Self::Off),
            "switch" => Ok(Self::Switch),
            "safeguard" | "aggressive" => {
                tracing::warn!(
                    value = %raw,
                    "agent.compaction.mode '{raw}' was removed; running as 'switch'. Set mode = \"switch\" to silence this warning"
                );
                Ok(Self::Switch)
            }
            _ => Err(D::Error::unknown_variant(&raw, &["off", "switch"])),
        }
    }
}

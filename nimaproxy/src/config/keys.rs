//! API key configuration.

use serde::Deserialize;

#[derive(Deserialize, Clone, Debug)]
pub struct KeyEntry {
    pub key: String,
    pub label: Option<String>,
    /// Parked key: stays in the config for reference but is never leased.
    /// Previously this field was silently ignored by serde, so a parked key
    /// still entered the rotation and was discovered broken per-request.
    #[serde(default)]
    pub disabled: bool,
}

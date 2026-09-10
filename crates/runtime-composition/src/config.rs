//! Minimal host configuration for composition activation.

use kernis_core::Id;
use std::collections::BTreeMap;

/// Configuration values the host provides to one composition activation.
///
/// The model is intentionally minimal: modules declare required keys with
/// [`ModuleDefinition::requiring_config`](crate::ModuleDefinition::requiring_config),
/// the host provides values here, and planning validates demand against it
/// before any activation. There is no secret manager, remote configuration,
/// filesystem discovery, hot reload, or dependency-injection container.
#[derive(Clone, Debug, Default)]
pub struct HostConfig {
    values: BTreeMap<Id, String>,
}

impl HostConfig {
    /// Creates an empty host configuration.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            values: BTreeMap::new(),
        }
    }

    /// Provides one configuration value.
    #[must_use]
    pub fn provide(mut self, key: Id, value: impl Into<String>) -> Self {
        self.values.insert(key, value.into());
        self
    }

    /// Returns the value for one configuration key.
    #[must_use]
    pub fn get(&self, key: &Id) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    /// Returns whether the configuration provides the key.
    #[must_use]
    pub fn contains(&self, key: &Id) -> bool {
        self.values.contains_key(key)
    }
}

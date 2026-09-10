//! Process-local module registrations.
//!
//! A [`ModuleRegistration`] pairs a stable
//! [`ModuleDefinition`](crate::ModuleDefinition) with the process-local
//! objects the host contributes: capability factories, plugin runtimes, and
//! lifecycle hooks. It is never serialized and never written to a durable
//! store.

use crate::definition::ModuleDefinition;
use capability_graph::{CapabilityValue, PluginConfig, PluginRuntime, ResolvedDependencies};
use kernis_core::Id;
use runtime_core::DefinitionIdentity;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Executor-neutral future returned by one lifecycle hook.
pub type HookFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;

/// One module lifecycle hook: `activate` or `dispose`.
pub type LifecycleHook = Arc<dyn Fn() -> HookFuture + Send + Sync + 'static>;

/// Wraps an ordinary async closure as an executor-neutral lifecycle hook.
#[must_use]
pub fn lifecycle_hook<F, Fut>(hook: F) -> LifecycleHook
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), String>> + Send + 'static,
{
    Arc::new(move || Box::pin(hook()))
}

/// Process-local constructor contribution for one declaratively owned
/// capability slot, matching the K2 factory contract.
pub type CapabilityFactoryFn =
    Arc<dyn Fn(&ResolvedDependencies) -> Result<CapabilityValue, String> + Send + Sync + 'static>;

/// One factory contribution attached to a module registration.
#[derive(Clone)]
pub(crate) struct FactoryContribution {
    pub(crate) capability_id: Id,
    pub(crate) definition_identity: DefinitionIdentity,
    pub(crate) factory: CapabilityFactoryFn,
}

/// One plugin contribution attached to a module registration.
#[derive(Clone)]
pub(crate) struct PluginContribution {
    pub(crate) plugin: Arc<PluginRuntime>,
    pub(crate) config: PluginConfig,
}

/// Process-local registration of one composition module.
///
/// The registration owns the executable side of a module: factories for
/// declaratively owned slots, plugin runtimes (with their instantiation
/// configuration) for reactively owned slots, and the two lifecycle hooks.
pub struct ModuleRegistration {
    pub(crate) definition: ModuleDefinition,
    pub(crate) factories: Vec<FactoryContribution>,
    pub(crate) plugins: Vec<PluginContribution>,
    pub(crate) on_activate: Option<LifecycleHook>,
    pub(crate) on_dispose: Option<LifecycleHook>,
}

impl ModuleRegistration {
    /// Creates a registration for one stable module declaration.
    #[must_use]
    pub fn new(definition: ModuleDefinition) -> Self {
        Self {
            definition,
            factories: Vec::new(),
            plugins: Vec::new(),
            on_activate: None,
            on_dispose: None,
        }
    }

    /// Returns the stable module declaration.
    #[must_use]
    pub fn definition(&self) -> &ModuleDefinition {
        &self.definition
    }

    /// Adds one factory for a declaratively owned capability slot.
    #[must_use]
    pub fn factory<I, F>(mut self, capability_id: Id, definition_identity: I, factory: F) -> Self
    where
        I: Into<DefinitionIdentity>,
        F: Fn(&ResolvedDependencies) -> Result<CapabilityValue, String> + Send + Sync + 'static,
    {
        self.factories.push(FactoryContribution {
            capability_id,
            definition_identity: definition_identity.into(),
            factory: Arc::new(factory),
        });
        self
    }

    /// Adds one plugin runtime contribution with empty plugin configuration.
    #[must_use]
    pub fn plugin(self, plugin: Arc<PluginRuntime>) -> Self {
        self.plugin_with_config(plugin, String::new())
    }

    /// Adds one plugin runtime contribution with its instantiation
    /// configuration.
    #[must_use]
    pub fn plugin_with_config(
        mut self,
        plugin: Arc<PluginRuntime>,
        config: impl Into<String>,
    ) -> Self {
        self.plugins.push(PluginContribution {
            plugin,
            config: config.into(),
        });
        self
    }

    /// Sets the module `activate` lifecycle hook, run after its plugins
    /// reach the active state.
    #[must_use]
    pub fn on_activate(mut self, hook: LifecycleHook) -> Self {
        self.on_activate = Some(hook);
        self
    }

    /// Sets the module `dispose` lifecycle hook, run first when the module's
    /// owned resources are released.
    #[must_use]
    pub fn on_dispose(mut self, hook: LifecycleHook) -> Self {
        self.on_dispose = Some(hook);
        self
    }
}

//! Explicit Rust counterparts for the Cordis capability-runtime semantics.
//!
//! The module deliberately does not reproduce Cordis's proxy or prototype
//! machinery. Context lookup, plugin identity, dependency epochs, and effect
//! ownership are represented by ordinary Rust values and owned handles.

use crate::runtime::allocate_monotonic;
use crate::{
    CapabilityDefinition, CapabilityHandle, CapabilityId, CapabilityValue, Generation,
    ResolvedDependencies, Scope, ScopeError,
};
use kernis_core::Id;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::Mutex as AsyncMutex;

/// Plugin configuration kept intentionally explicit at the kernel boundary.
/// Applications can encode typed configuration at their own API boundary.
pub type PluginConfig = String;

/// A boxed asynchronous plugin factory result.
pub type PluginFuture = Pin<Box<dyn Future<Output = Result<CapabilityValue, String>> + Send>>;

/// A factory that initializes one plugin fiber.
pub type PluginFactory = Arc<dyn Fn(PluginLoadContext) -> PluginFuture + Send + Sync>;

/// Synchronous configuration validation performed before a fiber is created.
pub type ConfigValidator = Arc<dyn Fn(&PluginConfig) -> Result<(), String> + Send + Sync>;

/// Explicit context view used by plugin fibers.
#[derive(Clone)]
pub struct CapabilityContext {
    state: Arc<ContextState>,
}

struct ContextState {
    scope: Scope,
    parent: Option<CapabilityContext>,
    isolation: BTreeSet<CapabilityId>,
    intercepts: BTreeMap<CapabilityId, PluginConfig>,
}

impl fmt::Debug for CapabilityContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CapabilityContext")
            .field("scope", &"owned capability scope")
            .field("isolation", &self.state.isolation)
            .field(
                "intercepts",
                &self.state.intercepts.keys().collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl CapabilityContext {
    /// Creates an empty root context and root capability scope.
    #[must_use]
    pub fn root() -> Self {
        Self::from_scope(Scope::root())
    }

    /// Wraps an existing scope without copying or changing its entries.
    #[must_use]
    pub fn from_scope(scope: Scope) -> Self {
        let parent = scope.parent().map(Self::from_scope);
        Self {
            state: Arc::new(ContextState {
                scope,
                parent,
                isolation: BTreeSet::new(),
                intercepts: BTreeMap::new(),
            }),
        }
    }

    /// Returns the capability scope owned by this context view.
    #[must_use]
    pub fn scope(&self) -> &Scope {
        &self.state.scope
    }

    /// Returns the root context in this explicit context chain.
    #[must_use]
    pub fn root_context(&self) -> Self {
        let mut current = self.clone();
        while let Some(parent) = current.state.parent.as_ref() {
            current = parent.clone();
        }
        current
    }

    /// Creates a child with local storage and parent fallback.
    #[must_use]
    pub fn child_context(&self) -> Self {
        Self {
            state: Arc::new(ContextState {
                scope: self.scope().child(),
                parent: Some(self.clone()),
                isolation: BTreeSet::new(),
                intercepts: BTreeMap::new(),
            }),
        }
    }

    /// Cordis-equivalent `extend`: create a child context without copying the
    /// parent's local registrations.
    #[must_use]
    pub fn extend(&self) -> Self {
        self.child_context()
    }

    /// Creates a child boundary that cannot fall back to the named parent
    /// capability unless that capability is provided locally.
    #[must_use]
    pub fn isolate(&self, capability: CapabilityId) -> Self {
        let scope = self.scope().child();
        let mut isolation = BTreeSet::new();
        isolation.insert(capability);
        Self {
            state: Arc::new(ContextState {
                scope,
                parent: Some(self.clone()),
                isolation,
                intercepts: BTreeMap::new(),
            }),
        }
    }

    /// Creates a child with a local configuration override for a plugin.
    #[must_use]
    pub fn intercept(&self, plugin: CapabilityId, config: PluginConfig) -> Self {
        let scope = self.scope().child();
        let mut intercepts = BTreeMap::new();
        intercepts.insert(plugin, config);
        Self {
            state: Arc::new(ContextState {
                scope,
                parent: Some(self.clone()),
                isolation: BTreeSet::new(),
                intercepts,
            }),
        }
    }

    /// Resolves a capability with local-first, deterministic parent fallback.
    #[must_use]
    pub fn get(&self, id: &CapabilityId) -> Option<CapabilityHandle> {
        let mut current = self.clone();
        loop {
            if current.scope().has_local(id) {
                return current.scope().get(id);
            }
            if current.state.isolation.contains(id) {
                return None;
            }
            current = current.state.parent.as_ref()?.clone();
        }
    }

    /// Returns the nearest configuration override, if any.
    #[must_use]
    pub fn intercepted_config(&self, plugin: &CapabilityId) -> Option<PluginConfig> {
        if let Some(config) = self.state.intercepts.get(plugin) {
            return Some(config.clone());
        }
        self.state
            .parent
            .as_ref()
            .and_then(|parent| parent.intercepted_config(plugin))
    }

    /// Resolves exact dependency handles for a definition in this context.
    pub(crate) fn resolve_dependencies(
        &self,
        definition: &CapabilityDefinition,
    ) -> Result<ResolvedDependencies, ScopeError> {
        let mut handles = BTreeMap::new();
        for dependency in &definition.dependencies {
            let handle = self
                .get(&dependency.id)
                .ok_or_else(|| ScopeError::MissingDependency {
                    capability: definition.id.clone(),
                    dependency: dependency.id.clone(),
                })?;
            handles.insert(dependency.id.clone(), handle);
        }
        Ok(ResolvedDependencies::from_handles(handles))
    }

    /// Publishes a synchronous local capability through the existing scope
    /// admission boundary.
    pub fn provide<F>(
        &self,
        definition: CapabilityDefinition,
        construct: F,
    ) -> Result<CapabilityHandle, ScopeError>
    where
        F: FnOnce(&ResolvedDependencies) -> Result<CapabilityValue, String>,
    {
        self.resolve_dependencies(&definition)?;
        self.scope().provide(definition, construct)
    }

    /// Closes this context's scope and all descendant scopes.
    pub fn teardown(&self) {
        self.scope().teardown();
    }
}

/// Exact provider bindings captured by one fiber.
///
/// This is deliberately independent from lifecycle transition identity. A
/// notification that observes the same providers is therefore neutral, while
/// a replacement with an equal value still produces a different binding.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DependencyBinding {
    pins: BTreeMap<CapabilityId, DependencyPin>,
}

/// One exact dependency generation retained by an epoch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DependencyPin {
    generation: Generation,
    entry_id: crate::EntryId,
}

impl DependencyPin {
    /// Returns the exact publication generation.
    #[must_use]
    pub const fn generation(self) -> Generation {
        self.generation
    }

    /// Returns the exact runtime entry identity.
    #[must_use]
    pub const fn entry_id(self) -> crate::EntryId {
        self.entry_id
    }
}

impl DependencyBinding {
    fn from_dependencies(dependencies: &ResolvedDependencies) -> Self {
        let pins = dependencies
            .iter()
            .map(|(id, handle)| {
                (
                    id.clone(),
                    DependencyPin {
                        generation: handle.generation(),
                        entry_id: handle.entry_id(),
                    },
                )
            })
            .collect();
        Self { pins }
    }

    /// Returns the exact provider pin for a dependency.
    #[must_use]
    pub fn pin(&self, id: &CapabilityId) -> Option<DependencyPin> {
        self.pins.get(id).copied()
    }

    /// Returns the exact generation pinned for a dependency.
    #[must_use]
    pub fn generation(&self, id: &CapabilityId) -> Option<Generation> {
        self.pins.get(id).map(|pin| pin.generation)
    }

    /// Returns the exact entry identity pinned for a dependency.
    #[must_use]
    pub fn entry_id(&self, id: &CapabilityId) -> Option<crate::EntryId> {
        self.pins.get(id).map(|pin| pin.entry_id)
    }

    /// Returns exact provider pins in deterministic identifier order.
    pub fn iter(&self) -> impl Iterator<Item = (&CapabilityId, DependencyPin)> {
        self.pins.iter().map(|(id, pin)| (id, *pin))
    }

    /// Returns whether this epoch has no dependencies.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pins.is_empty()
    }
}

/// A dependency binding plus the monotonic token for one lifecycle request.
///
/// The token is a race/transition identity only. Binding comparisons must use
/// [`Self::binding`], never the whole epoch, when deciding whether a provider
/// actually changed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DependencyEpoch {
    transition_token: u64,
    binding: DependencyBinding,
}

impl DependencyEpoch {
    fn from_dependencies(transition_token: u64, dependencies: &ResolvedDependencies) -> Self {
        Self {
            transition_token,
            binding: DependencyBinding::from_dependencies(dependencies),
        }
    }

    /// Returns the monotonic lifecycle transition token.
    #[must_use]
    pub const fn transition_token(&self) -> u64 {
        self.transition_token
    }

    /// Returns the exact provider binding identity.
    #[must_use]
    pub const fn binding(&self) -> &DependencyBinding {
        &self.binding
    }

    /// Backwards-compatible name for the transition token.
    #[must_use]
    pub const fn ordinal(&self) -> u64 {
        self.transition_token
    }

    /// Returns the exact generation pinned for a dependency.
    #[must_use]
    pub fn generation(&self, id: &CapabilityId) -> Option<Generation> {
        self.binding.generation(id)
    }

    /// Returns the exact entry identity pinned for a dependency.
    #[must_use]
    pub fn entry_id(&self, id: &CapabilityId) -> Option<crate::EntryId> {
        self.binding.entry_id(id)
    }

    /// Returns whether this epoch has no dependencies.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.binding.is_empty()
    }
}

/// A plugin definition with explicit requirements and factory.
#[derive(Clone)]
pub struct PluginDefinition {
    id: Id,
    capability: CapabilityDefinition,
    factory: PluginFactory,
    validator: ConfigValidator,
}

impl fmt::Debug for PluginDefinition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginDefinition")
            .field("id", &self.id)
            .field("capability", &self.capability)
            .finish_non_exhaustive()
    }
}

impl PluginDefinition {
    /// Creates a plugin definition. The capability definition's dependencies
    /// are the plugin's explicit requirements.
    #[must_use]
    pub fn new(id: Id, capability: CapabilityDefinition, factory: PluginFactory) -> Self {
        Self {
            id,
            capability,
            factory,
            validator: Arc::new(|_| Ok(())),
        }
    }

    /// Adds a synchronous configuration validator.
    #[must_use]
    pub fn with_validator(mut self, validator: ConfigValidator) -> Self {
        self.validator = validator;
        self
    }

    /// Returns the logical plugin identity.
    #[must_use]
    pub const fn id(&self) -> &Id {
        &self.id
    }

    /// Returns the capability definition published by each active fiber.
    #[must_use]
    pub const fn capability(&self) -> &CapabilityDefinition {
        &self.capability
    }
}

/// A plugin runtime metadata entry shared by multiple fibers.
pub struct PluginRuntime {
    definition: Arc<PluginDefinition>,
    fibers: Mutex<BTreeMap<FiberId, Arc<CapabilityFiber>>>,
}

static NEXT_FIBER_ID: AtomicU64 = AtomicU64::new(1);

impl fmt::Debug for PluginRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginRuntime")
            .field("id", self.id())
            .field("fiber_count", &self.fiber_count())
            .finish()
    }
}

impl PluginRuntime {
    /// Creates runtime metadata for one logical plugin identity.
    #[must_use]
    pub fn new(definition: PluginDefinition) -> Arc<Self> {
        Arc::new(Self {
            definition: Arc::new(definition),
            fibers: Mutex::new(BTreeMap::new()),
        })
    }

    /// Returns the logical plugin identity.
    #[must_use]
    pub fn id(&self) -> &Id {
        self.definition.id()
    }

    /// Returns the shared plugin definition.
    #[must_use]
    pub fn definition(&self) -> &PluginDefinition {
        &self.definition
    }

    /// Returns the number of live fiber instances.
    #[must_use]
    pub fn fiber_count(&self) -> usize {
        self.fibers
            .lock()
            .expect("plugin fiber lock poisoned")
            .len()
    }

    fn create_fiber(
        self: &Arc<Self>,
        context: CapabilityContext,
        config: PluginConfig,
    ) -> Result<Arc<CapabilityFiber>, FiberError> {
        (self.definition.validator)(&config).map_err(|reason| FiberError::InvalidConfig {
            plugin: self.id().clone(),
            reason,
        })?;
        let raw_id = allocate_monotonic(&NEXT_FIBER_ID).ok_or(FiberError::FiberIdExhausted)?;
        let fiber = Arc::new(CapabilityFiber::new(
            FiberId(raw_id),
            Arc::downgrade(self),
            context,
            Arc::clone(&self.definition),
            config,
        ));
        self.fibers
            .lock()
            .expect("plugin fiber lock poisoned")
            .insert(fiber.id(), Arc::clone(&fiber));
        Ok(fiber)
    }

    fn remove_fiber(&self, id: FiberId) {
        self.fibers
            .lock()
            .expect("plugin fiber lock poisoned")
            .remove(&id);
    }

    async fn dispose_fibers(&self) -> Vec<FiberError> {
        let fibers = self
            .fibers
            .lock()
            .expect("plugin fiber lock poisoned")
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut errors = Vec::new();
        for fiber in fibers {
            if let Err(error) = fiber.dispose().await {
                errors.push(error);
            }
        }
        self.fibers
            .lock()
            .expect("plugin fiber lock poisoned")
            .clear();
        errors
    }
}

/// Registry of logical plugin runtimes.
#[derive(Debug, Default)]
pub struct CapabilityRegistry {
    runtimes: Mutex<BTreeMap<Id, Arc<PluginRuntime>>>,
}

impl CapabilityRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers one logical runtime. Duplicate identity fails closed.
    pub fn register(&self, runtime: Arc<PluginRuntime>) -> Result<(), RegistryError> {
        let mut runtimes = self.runtimes.lock().expect("plugin registry lock poisoned");
        if runtimes.contains_key(runtime.id()) {
            return Err(RegistryError::Duplicate(runtime.id().clone()));
        }
        runtimes.insert(runtime.id().clone(), runtime);
        Ok(())
    }

    /// Returns a runtime metadata entry without granting disposal authority.
    #[must_use]
    pub fn get(&self, id: &Id) -> Option<Arc<PluginRuntime>> {
        self.runtimes
            .lock()
            .expect("plugin registry lock poisoned")
            .get(id)
            .cloned()
    }

    /// Returns whether a logical plugin is registered.
    #[must_use]
    pub fn contains(&self, id: &Id) -> bool {
        self.runtimes
            .lock()
            .expect("plugin registry lock poisoned")
            .contains_key(id)
    }

    /// Returns the number of logical plugin runtimes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.runtimes
            .lock()
            .expect("plugin registry lock poisoned")
            .len()
    }

    /// Returns whether no logical plugin runtimes are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Creates one fiber for a registered runtime. Multiple calls retain one
    /// runtime metadata record and produce separate lifecycle instances.
    pub fn instantiate(
        &self,
        id: &Id,
        context: CapabilityContext,
        config: PluginConfig,
    ) -> Result<Arc<CapabilityFiber>, RegistryError> {
        let runtimes = self.runtimes.lock().expect("plugin registry lock poisoned");
        let runtime = runtimes
            .get(id)
            .cloned()
            .ok_or_else(|| RegistryError::Unknown(id.clone()))?;
        runtime
            .create_fiber(context, config)
            .map_err(RegistryError::Fiber)
    }

    /// Removes a runtime and disposes all associated fibers.
    ///
    /// This is the one-shot convenience form of [`Self::take`] followed by
    /// [`Self::dispose_taken`]. It awaits inline, so a caller whose future is
    /// cancelled mid-disposal loses the started disposal with the future; use
    /// the two halves when the disposal must survive cancellation.
    pub async fn remove(&self, id: &Id) -> Result<Option<Arc<PluginRuntime>>, RegistryError> {
        let Some(runtime) = self.take(id) else {
            return Ok(None);
        };
        match Self::dispose_taken(&runtime).await {
            Ok(()) => Ok(Some(runtime)),
            Err(error) => Err(error),
        }
    }

    /// Removes a runtime from the registry synchronously, without disposing
    /// its fibers.
    ///
    /// This is the non-awaiting half of [`Self::remove`]. It exists so a
    /// cancellation-safe cleanup caller can take ownership of the runtime here
    /// and park [`Self::dispose_taken`] as its own resumable step, instead of
    /// a drop between the map removal and the disposal completing the removal
    /// while stranding a runtime that is no longer reachable through the
    /// registry.
    #[must_use = "the caller now owns the only registry-held reference and must dispose it"]
    pub fn take(&self, id: &Id) -> Option<Arc<PluginRuntime>> {
        self.runtimes
            .lock()
            .expect("plugin registry lock poisoned")
            .remove(id)
    }

    /// Disposes the fibers of one runtime previously removed with
    /// [`Self::take`], reporting the same [`RegistryError::Dispose`] that
    /// [`Self::remove`] reports for a failed cleanup.
    ///
    /// The future only borrows the runtime: the caller must keep it alive (the
    /// composition ledger owns the taken runtime for exactly this reason), so
    /// cancelling the future loses the started disposal but never drops a
    /// runtime the caller still owns. Disposing a runtime that is still
    /// registered is possible — this is a subsystem-tier primitive, not an
    /// authority gate — so only a caller that has just taken it should use it.
    pub async fn dispose_taken(runtime: &PluginRuntime) -> Result<(), RegistryError> {
        let errors = runtime.dispose_fibers().await;
        if errors.is_empty() {
            Ok(())
        } else {
            Err(RegistryError::Dispose {
                plugin: runtime.id().clone(),
                errors,
            })
        }
    }
}

/// Monotonic identity for one plugin fiber instance.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct FiberId(u64);

impl FiberId {
    /// Returns the process-local numeric identity.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Cordis-compatible semantic state names, represented explicitly.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FiberState {
    /// Requirements are not currently satisfied or the fiber has not started.
    Pending,
    /// Plugin initialization is in progress.
    Loading,
    /// The plugin value and effects are published.
    Active,
    /// Initialization failed and requires explicit restart/update.
    Failed,
    /// Cleanup is in progress.
    Unloading,
    /// Final state after explicit disposal.
    Disposed,
}

impl FiberState {
    fn as_raw(self) -> u8 {
        self as u8
    }

    fn from_raw(raw: u8) -> Self {
        match raw {
            0 => Self::Pending,
            1 => Self::Loading,
            2 => Self::Active,
            3 => Self::Failed,
            4 => Self::Unloading,
            _ => Self::Disposed,
        }
    }
}

/// A context passed to one plugin initialization attempt.
#[derive(Clone)]
pub struct PluginLoadContext {
    context: CapabilityContext,
    config: PluginConfig,
    dependencies: ResolvedDependencies,
    effects: EffectScope,
}

impl fmt::Debug for PluginLoadContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginLoadContext")
            .field("config", &self.config)
            .field("dependencies", &self.dependencies.len())
            .finish()
    }
}

impl PluginLoadContext {
    fn new(
        context: CapabilityContext,
        config: PluginConfig,
        dependencies: ResolvedDependencies,
        effects: EffectScope,
    ) -> Self {
        Self {
            context,
            config,
            dependencies,
            effects,
        }
    }

    /// Returns the context view for this attempt.
    #[must_use]
    pub fn context(&self) -> &CapabilityContext {
        &self.context
    }

    /// Returns the validated/effective configuration.
    #[must_use]
    pub fn config(&self) -> &str {
        &self.config
    }

    /// Returns the exact dependency snapshot for this attempt.
    #[must_use]
    pub fn dependencies(&self) -> &ResolvedDependencies {
        &self.dependencies
    }

    /// Returns the effect owner for resources created by this attempt.
    #[must_use]
    pub fn effects(&self) -> &EffectScope {
        &self.effects
    }

    /// Registers an effect owned by the current fiber epoch.
    pub fn effect(&self, effect: ScopedEffect) -> Result<(), EffectError> {
        self.effects.register(effect)
    }
}

/// A process-local capability fiber with serialized load/unload transitions.
pub struct CapabilityFiber {
    id: FiberId,
    runtime: Weak<PluginRuntime>,
    context: CapabilityContext,
    definition: Arc<PluginDefinition>,
    config: Mutex<PluginConfig>,
    state: AtomicU8,
    requested_epoch: AtomicU64,
    dispose_requested: AtomicBool,
    force_reload: AtomicBool,
    transition: AsyncMutex<()>,
    handle: AsyncMutex<Option<CapabilityHandle>>,
    dependency_epoch: AsyncMutex<Option<DependencyEpoch>>,
    loading_binding: AsyncMutex<Option<DependencyBinding>>,
    effects: Mutex<EffectScope>,
    last_cleanup_errors: AsyncMutex<Vec<EffectError>>,
    error: AsyncMutex<Option<String>>,
}

impl fmt::Debug for CapabilityFiber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CapabilityFiber")
            .field("id", &self.id)
            .field("plugin", self.definition.id())
            .field("state", &self.state())
            .finish()
    }
}

impl CapabilityFiber {
    fn new(
        id: FiberId,
        runtime: Weak<PluginRuntime>,
        context: CapabilityContext,
        definition: Arc<PluginDefinition>,
        config: PluginConfig,
    ) -> Self {
        Self {
            id,
            runtime,
            context,
            definition,
            config: Mutex::new(config),
            state: AtomicU8::new(FiberState::Pending.as_raw()),
            requested_epoch: AtomicU64::new(0),
            dispose_requested: AtomicBool::new(false),
            force_reload: AtomicBool::new(false),
            transition: AsyncMutex::new(()),
            handle: AsyncMutex::new(None),
            dependency_epoch: AsyncMutex::new(None),
            loading_binding: AsyncMutex::new(None),
            effects: Mutex::new(EffectScope::new()),
            last_cleanup_errors: AsyncMutex::new(Vec::new()),
            error: AsyncMutex::new(None),
        }
    }

    /// Returns this fiber's process-local identity.
    #[must_use]
    pub const fn id(&self) -> FiberId {
        self.id
    }

    /// Returns the logical plugin identity.
    #[must_use]
    pub fn plugin_id(&self) -> &Id {
        self.definition.id()
    }

    /// Returns the context used for future dependency resolution.
    #[must_use]
    pub fn context(&self) -> &CapabilityContext {
        &self.context
    }

    /// Returns the current state without requiring an async borrow.
    #[must_use]
    pub fn state(&self) -> FiberState {
        FiberState::from_raw(self.state.load(Ordering::Acquire))
    }

    /// Returns the latest initialization error, if any.
    pub async fn error(&self) -> Option<String> {
        self.error.lock().await.clone()
    }

    /// Returns the last cleanup errors. Cleanup always continues after one
    /// disposer fails, so callers can inspect all isolated failures.
    pub async fn cleanup_errors(&self) -> Vec<EffectError> {
        self.last_cleanup_errors.lock().await.clone()
    }

    /// Returns the currently published exact capability handle.
    pub async fn handle(&self) -> Option<CapabilityHandle> {
        self.handle.lock().await.clone()
    }

    /// Returns the current dependency epoch after a stable transition.
    pub async fn dependency_epoch(&self) -> Option<DependencyEpoch> {
        self.dependency_epoch.lock().await.clone()
    }

    /// Returns the exact dependency binding committed by the active fiber.
    #[must_use]
    pub async fn dependency_binding(&self) -> Option<DependencyBinding> {
        self.dependency_epoch
            .lock()
            .await
            .as_ref()
            .map(|epoch| epoch.binding().clone())
    }

    /// Returns whether this fiber declares a dependency on the capability.
    #[must_use]
    pub fn declares_dependency(&self, capability: &CapabilityId) -> bool {
        self.definition
            .capability
            .dependencies
            .iter()
            .any(|dependency| &dependency.id == capability)
    }

    /// Returns the exact provider binding currently being loaded, if any.
    pub(crate) async fn loading_binding(&self) -> Option<DependencyBinding> {
        self.loading_binding.lock().await.clone()
    }

    /// Deactivates this fiber as part of a provider withdrawal.
    ///
    /// The transition lock makes the return a quiescence boundary. Effects
    /// run before the committed handle is released, so cleanup can still use
    /// the exact dependency snapshot that activated the fiber.
    pub async fn deactivate_for_withdrawal(&self) -> Result<FiberState, FiberError> {
        self.deactivate_for_withdrawal_with_detached(None).await
    }

    /// Deactivates a provider fiber whose exact publication was already
    /// detached by the coordinator's withdrawal guard.
    pub(crate) async fn deactivate_for_withdrawal_after_detach(
        &self,
        detached: &CapabilityHandle,
    ) -> Result<FiberState, FiberError> {
        self.deactivate_for_withdrawal_with_detached(Some(detached))
            .await
    }

    async fn deactivate_for_withdrawal_with_detached(
        &self,
        detached: Option<&CapabilityHandle>,
    ) -> Result<FiberState, FiberError> {
        self.requested_epoch.fetch_add(1, Ordering::AcqRel);
        let _transition = self.transition.lock().await;
        if self.state() == FiberState::Disposed {
            return Ok(FiberState::Disposed);
        }
        if matches!(self.state(), FiberState::Active | FiberState::Failed) {
            let errors = self.unload(detached).await;
            *self.last_cleanup_errors.lock().await = errors;
        }
        Ok(self.state())
    }

    /// Registers an effect while the fiber is active.
    pub fn effect(&self, effect: ScopedEffect) -> Result<(), FiberError> {
        let effects = self.effects.lock().expect("fiber effect lock poisoned");
        let state = self.state();
        if state != FiberState::Active {
            return Err(FiberError::InactiveEffect(state));
        }
        effects.register(effect).map_err(FiberError::Effect)
    }

    pub(crate) async fn needs_reconciliation(&self) -> bool {
        let current = self
            .context
            .resolve_dependencies(&self.definition.capability)
            .ok()
            .map(|dependencies| DependencyBinding::from_dependencies(&dependencies));
        match self.state() {
            FiberState::Active => current != self.dependency_binding().await,
            FiberState::Loading => current != self.loading_binding().await,
            FiberState::Pending => {
                !self.definition.capability.dependencies.is_empty() && current.is_some()
            }
            FiberState::Failed | FiberState::Disposed => false,
            FiberState::Unloading => true,
        }
    }

    /// Marks a possible dependency change. The next stable operation observes
    /// the exact visible handles again; no fiber is replaced to escape a race.
    pub fn notify_dependency_change(&self) {
        self.requested_epoch.fetch_add(1, Ordering::AcqRel);
    }

    /// Loads the fiber when requirements are satisfied, or leaves it pending
    /// when a requirement is absent.
    pub async fn start(&self) -> Result<FiberState, FiberError> {
        let _transition = self.transition.lock().await;
        self.drive(false).await
    }

    /// Unloads and reloads the same fiber instance with its current config.
    pub async fn restart(&self) -> Result<FiberState, FiberError> {
        if self.state() == FiberState::Disposed {
            return Err(FiberError::Disposed);
        }
        self.force_reload.store(true, Ordering::Release);
        self.requested_epoch.fetch_add(1, Ordering::AcqRel);
        let _transition = self.transition.lock().await;
        self.drive(true).await
    }

    /// Validates and updates config, then restarts the same fiber.
    pub async fn update(&self, config: PluginConfig) -> Result<FiberState, FiberError> {
        if self.state() == FiberState::Disposed {
            return Err(FiberError::Disposed);
        }
        (self.definition.validator)(&config).map_err(|reason| FiberError::InvalidConfig {
            plugin: self.plugin_id().clone(),
            reason,
        })?;
        *self.config.lock().expect("fiber config lock poisoned") = config;
        self.restart().await
    }

    /// Waits for one load/unload transition to reach a stable state.
    pub async fn await_stable(&self) -> Result<FiberState, FiberError> {
        self.start().await
    }

    /// Disposes this fiber exactly once and waits for external async cleanup.
    pub async fn dispose(&self) -> Result<(), FiberError> {
        self.dispose_requested.store(true, Ordering::Release);
        self.requested_epoch.fetch_add(1, Ordering::AcqRel);
        let _transition = self.transition.lock().await;
        if self.state() == FiberState::Disposed {
            return Ok(());
        }
        let cleanup_errors = self.unload(None).await;
        self.set_state(FiberState::Disposed);
        if let Some(runtime) = self.runtime.upgrade() {
            runtime.remove_fiber(self.id);
        }
        if cleanup_errors.is_empty() {
            Ok(())
        } else {
            Err(FiberError::Cleanup(cleanup_errors))
        }
    }

    async fn drive(&self, force: bool) -> Result<FiberState, FiberError> {
        if self.dispose_requested.load(Ordering::Acquire) {
            return Err(FiberError::Disposed);
        }
        if !force && self.state() == FiberState::Failed {
            let reason = self
                .error
                .lock()
                .await
                .clone()
                .unwrap_or_else(|| "plugin initialization failed".to_owned());
            return Err(FiberError::InitializationFailed {
                plugin: self.plugin_id().clone(),
                reason,
                cleanup_errors: self.cleanup_errors().await,
            });
        }
        let requested_force = self.force_reload.swap(false, Ordering::AcqRel);
        let force = force || requested_force;
        if self.state() == FiberState::Pending {
            *self.last_cleanup_errors.lock().await = Vec::new();
        }

        loop {
            let dependencies = match self
                .context
                .resolve_dependencies(&self.definition.capability)
            {
                Ok(dependencies) => dependencies,
                Err(ScopeError::MissingDependency { .. }) => {
                    self.clear_active_state().await;
                    self.set_state(FiberState::Pending);
                    return Ok(FiberState::Pending);
                }
                Err(error) => return Err(FiberError::Scope(error)),
            };
            let epoch = DependencyEpoch::from_dependencies(
                self.requested_epoch.load(Ordering::Acquire),
                &dependencies,
            );

            let active_matches = if !force && self.state() == FiberState::Active {
                self.dependency_binding().await.as_ref() == Some(&epoch.binding)
            } else {
                false
            };
            if active_matches {
                *self.dependency_epoch.lock().await = Some(epoch);
                return Ok(FiberState::Active);
            }

            if matches!(self.state(), FiberState::Active | FiberState::Failed) {
                let errors = self.unload(None).await;
                *self.last_cleanup_errors.lock().await = errors;
                if self.dispose_requested.load(Ordering::Acquire) {
                    return Err(FiberError::Disposed);
                }
                continue;
            }
            if self.dispose_requested.load(Ordering::Acquire) {
                return Err(FiberError::Disposed);
            }

            self.set_state(FiberState::Loading);
            let config = self
                .context
                .intercepted_config(self.plugin_id())
                .unwrap_or_else(|| {
                    self.config
                        .lock()
                        .expect("fiber config lock poisoned")
                        .clone()
                });
            let effects = EffectScope::new();
            *self.effects.lock().expect("fiber effect lock poisoned") = effects.clone();
            *self.error.lock().await = None;
            *self.loading_binding.lock().await = Some(epoch.binding.clone());
            let load_context = PluginLoadContext::new(
                self.context.clone(),
                config,
                dependencies.clone(),
                effects.clone(),
            );
            let result = (self.definition.factory)(load_context).await;

            let current_dependencies = self
                .context
                .resolve_dependencies(&self.definition.capability);
            let current_binding = current_dependencies
                .as_ref()
                .ok()
                .map(DependencyBinding::from_dependencies);
            let stale = self.dispose_requested.load(Ordering::Acquire)
                || self.force_reload.load(Ordering::Acquire)
                || current_binding.as_ref() != Some(&epoch.binding);
            match result {
                Ok(value) if !stale => {
                    match self.context.scope().provide_value(
                        self.definition.capability.clone(),
                        dependencies.clone(),
                        value,
                    ) {
                        Ok(handle) => {
                            *self.handle.lock().await = Some(handle);
                            *self.dependency_epoch.lock().await =
                                Some(DependencyEpoch::from_dependencies(
                                    self.requested_epoch.load(Ordering::Acquire),
                                    &dependencies,
                                ));
                            *self.loading_binding.lock().await = None;
                            self.set_state(FiberState::Active);
                            return Ok(FiberState::Active);
                        }
                        Err(ScopeError::DependencyChanged { .. }) => {
                            let errors = effects.dispose_all().await;
                            *self.last_cleanup_errors.lock().await = errors;
                            *self.loading_binding.lock().await = None;
                            self.set_state(FiberState::Pending);
                        }
                        Err(error) => {
                            let errors = effects.dispose_all().await;
                            *self.loading_binding.lock().await = None;
                            *self.error.lock().await = Some(error.to_string());
                            *self.last_cleanup_errors.lock().await = errors.clone();
                            self.set_state(FiberState::Failed);
                            return Err(FiberError::Scope(error));
                        }
                    }
                }
                Ok(value) => {
                    drop(value);
                    let errors = effects.dispose_all().await;
                    *self.loading_binding.lock().await = None;
                    self.force_reload.store(false, Ordering::Release);
                    *self.last_cleanup_errors.lock().await = errors;
                    if self.dispose_requested.load(Ordering::Acquire) {
                        return Err(FiberError::Disposed);
                    }
                    self.set_state(FiberState::Pending);
                }
                Err(_) if stale => {
                    let errors = effects.dispose_all().await;
                    *self.loading_binding.lock().await = None;
                    self.force_reload.store(false, Ordering::Release);
                    *self.error.lock().await = None;
                    *self.last_cleanup_errors.lock().await = errors;
                    if self.dispose_requested.load(Ordering::Acquire) {
                        self.set_state(FiberState::Disposed);
                        return Err(FiberError::Disposed);
                    }
                    self.set_state(FiberState::Pending);
                }
                Err(reason) => {
                    let errors = effects.dispose_all().await;
                    *self.loading_binding.lock().await = None;
                    *self.error.lock().await = Some(reason.clone());
                    *self.last_cleanup_errors.lock().await = errors.clone();
                    self.set_state(FiberState::Failed);
                    return Err(FiberError::InitializationFailed {
                        plugin: self.plugin_id().clone(),
                        reason,
                        cleanup_errors: errors,
                    });
                }
            }
        }
    }

    async fn clear_active_state(&self) {
        if self.state() == FiberState::Active {
            let errors = self.unload(None).await;
            *self.last_cleanup_errors.lock().await = errors;
        }
    }

    async fn unload(&self, detached: Option<&CapabilityHandle>) -> Vec<EffectError> {
        self.set_state(FiberState::Unloading);
        let effects = self
            .effects
            .lock()
            .expect("fiber effect lock poisoned")
            .clone();
        let errors = effects.dispose_all().await;
        if let Some(handle) = self.handle.lock().await.take() {
            let was_detached = detached.is_some_and(|detached| {
                detached.id() == handle.id()
                    && detached.generation() == handle.generation()
                    && detached.entry_id() == handle.entry_id()
            });
            if !was_detached {
                let generation = handle.generation();
                if let Err(error) = self.context.scope().remove_local(handle.id(), generation) {
                    let mut all = errors;
                    all.push(EffectError::from_scope(error));
                    *self.dependency_epoch.lock().await = None;
                    self.set_state(FiberState::Pending);
                    return all;
                }
            }
        }
        *self.dependency_epoch.lock().await = None;
        *self.loading_binding.lock().await = None;
        *self.effects.lock().expect("fiber effect lock poisoned") = EffectScope::new();
        self.set_state(FiberState::Pending);
        errors
    }

    fn set_state(&self, state: FiberState) {
        self.state.store(state.as_raw(), Ordering::Release);
    }
}

/// An owned effect registration.
pub struct ScopedEffect {
    label: String,
    disposer: Option<Box<dyn FnOnce() -> EffectFuture + Send>>,
}

impl fmt::Debug for ScopedEffect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScopedEffect")
            .field("label", &self.label)
            .finish()
    }
}

impl ScopedEffect {
    /// Creates an effect with a synchronous disposer.
    #[must_use]
    pub fn sync(
        label: impl Into<String>,
        disposer: impl FnOnce() -> Result<(), String> + Send + 'static,
    ) -> Self {
        Self {
            label: label.into(),
            disposer: Some(Box::new(move || Box::pin(async move { disposer() }))),
        }
    }

    /// Creates an effect with an asynchronous disposer.
    #[must_use]
    pub fn asynchronous<F, Fut>(label: impl Into<String>, disposer: F) -> Self
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), String>> + Send + 'static,
    {
        Self {
            label: label.into(),
            disposer: Some(Box::new(move || Box::pin(disposer()))),
        }
    }
}

type EffectFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;

/// A reverse-order stack of owned effects.
#[derive(Debug, Default)]
pub struct EffectStack {
    effects: Vec<ScopedEffect>,
    closed: bool,
}

impl EffectStack {
    /// Creates an open stack.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            effects: Vec::new(),
            closed: false,
        }
    }

    /// Registers one effect unless the stack has begun disposal.
    pub fn push(&mut self, effect: ScopedEffect) -> Result<(), EffectError> {
        if self.closed {
            return Err(EffectError::Closed);
        }
        self.effects.push(effect);
        Ok(())
    }

    /// Disposes all effects in reverse registration order and continues after
    /// failures.
    pub async fn dispose_all(&mut self) -> Vec<EffectError> {
        self.closed = true;
        let effects = std::mem::take(&mut self.effects);
        run_effects(effects).await
    }
}

async fn run_effects(mut effects: Vec<ScopedEffect>) -> Vec<EffectError> {
    let mut errors = Vec::new();
    while let Some(mut effect) = effects.pop() {
        if let Some(disposer) = effect.disposer.take() {
            if let Err(reason) = disposer().await {
                errors.push(EffectError::Disposer {
                    label: effect.label,
                    reason,
                });
            }
        }
    }
    errors
}

/// A cloneable owner view used by a loading fiber and nested effects.
#[derive(Clone, Debug)]
pub struct EffectScope {
    stack: Arc<Mutex<EffectStack>>,
}

impl EffectScope {
    /// Creates an open effect scope.
    #[must_use]
    pub fn new() -> Self {
        Self {
            stack: Arc::new(Mutex::new(EffectStack::new())),
        }
    }

    /// Registers an effect in this scope.
    pub fn register(&self, effect: ScopedEffect) -> Result<(), EffectError> {
        self.stack
            .lock()
            .expect("effect stack lock poisoned")
            .push(effect)
    }

    /// Creates a child scope owned by this scope.
    pub fn child(&self, label: impl Into<String>) -> Result<Self, EffectError> {
        let child = Self::new();
        let owned = child.clone();
        self.register(ScopedEffect::asynchronous(label, move || async move {
            let errors = owned.dispose_all().await;
            if errors.is_empty() {
                Ok(())
            } else {
                Err(format!(
                    "nested effect cleanup failed: {} error(s)",
                    errors.len()
                ))
            }
        }))?;
        Ok(child)
    }

    /// Disposes the shared scope once; repeated calls see an empty stack.
    pub async fn dispose_all(&self) -> Vec<EffectError> {
        let mut effects = {
            let mut stack = self.stack.lock().expect("effect stack lock poisoned");
            stack.closed = true;
            std::mem::take(&mut stack.effects)
        };
        run_effects(std::mem::take(&mut effects)).await
    }
}

impl Default for EffectScope {
    fn default() -> Self {
        Self::new()
    }
}

/// Errors from effect registration or cleanup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EffectError {
    /// A new effect was registered after cleanup began.
    Closed,
    /// One disposer failed; remaining disposers still run.
    Disposer {
        /// Human-readable effect label.
        label: String,
        /// Disposer-provided failure description.
        reason: String,
    },
    /// Scope ownership cleanup surfaced a scope error.
    Scope(String),
}

impl EffectError {
    fn from_scope(error: ScopeError) -> Self {
        Self::Scope(error.to_string())
    }
}

impl fmt::Display for EffectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => f.write_str("effect scope is closed"),
            Self::Disposer { label, reason } => {
                write!(f, "effect {label} cleanup failed: {reason}")
            }
            Self::Scope(reason) => f.write_str(reason),
        }
    }
}

impl std::error::Error for EffectError {}

/// Fiber lifecycle failures.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FiberError {
    /// The fiber has reached its final state.
    Disposed,
    /// A plugin configuration was rejected before startup.
    InvalidConfig {
        /// Logical plugin identity whose config was rejected.
        plugin: Id,
        /// Validator-provided failure description.
        reason: String,
    },
    /// Plugin initialization failed after partial effect setup.
    InitializationFailed {
        /// Logical plugin identity whose initialization failed.
        plugin: Id,
        /// Factory-provided failure description.
        reason: String,
        /// Cleanup failures collected after partial initialization.
        cleanup_errors: Vec<EffectError>,
    },
    /// Existing capability graph admission rejected publication.
    Scope(ScopeError),
    /// An effect was attempted while the fiber was not active.
    InactiveEffect(FiberState),
    /// The process-local fiber counter is exhausted.
    FiberIdExhausted,
    /// A cleanup failure occurred while disposing a fiber.
    Cleanup(Vec<EffectError>),
    /// An effect registration failed.
    Effect(EffectError),
}

impl fmt::Display for FiberError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disposed => f.write_str("fiber is disposed"),
            Self::InvalidConfig { plugin, reason } => {
                write!(f, "invalid config for plugin {plugin}: {reason}")
            }
            Self::InitializationFailed { plugin, reason, .. } => {
                write!(f, "plugin {plugin} initialization failed: {reason}")
            }
            Self::Scope(error) => error.fmt(f),
            Self::InactiveEffect(state) => {
                write!(f, "cannot create effect while fiber is {state:?}")
            }
            Self::FiberIdExhausted => f.write_str("fiber id exhausted"),
            Self::Cleanup(errors) => {
                write!(f, "fiber cleanup failed with {} error(s)", errors.len())
            }
            Self::Effect(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for FiberError {}

/// Registry failures.
#[derive(Debug)]
pub enum RegistryError {
    /// The logical plugin identity is already registered.
    Duplicate(Id),
    /// No runtime exists for the requested identity.
    Unknown(Id),
    /// Fiber creation failed.
    Fiber(FiberError),
    /// Runtime removal disposed fibers with one or more cleanup errors.
    Dispose {
        /// Logical plugin identity being removed.
        plugin: Id,
        /// Fiber disposal failures collected for the runtime.
        errors: Vec<FiberError>,
    },
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Duplicate(id) => write!(f, "plugin runtime already registered: {id}"),
            Self::Unknown(id) => write!(f, "unknown plugin runtime: {id}"),
            Self::Fiber(error) => error.fmt(f),
            Self::Dispose { plugin, errors } => {
                write!(
                    f,
                    "disposing plugin {plugin} failed with {} error(s)",
                    errors.len()
                )
            }
        }
    }
}

impl std::error::Error for RegistryError {}

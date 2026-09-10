//! Explicit reactive coordination for capability fibers.
//!
//! [`Scope`] remains the ownership and admission primitive. This module adds
//! the small amount of coordination needed to make declared dependencies
//! reactive: fibers are registered with a coordinator, mutations are applied
//! through it, and an explicit async reconciliation boundary drives only
//! fibers whose effective provider binding changed.

use crate::{
    CapabilityContext, CapabilityDefinition, CapabilityFiber, CapabilityHandle, CapabilityId,
    CapabilityRegistry, DependencyBinding, DependencyPin, EffectError, FiberError, FiberId,
    Generation, PluginConfig, RegistryError, Scope, ScopeError,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{Arc, Mutex, Weak};

/// A process-local coordinator for reactive capability fibers.
pub struct ReactiveCapabilityRuntime {
    context: CapabilityContext,
    registry: CapabilityRegistry,
    fibers: Mutex<BTreeMap<FiberId, Weak<CapabilityFiber>>>,
}

/// Short name for [`ReactiveCapabilityRuntime`].
pub type ReactiveRuntime = ReactiveCapabilityRuntime;

impl fmt::Debug for ReactiveCapabilityRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReactiveCapabilityRuntime")
            .field("scope", &"owned capability scope")
            .field(
                "watched_fibers",
                &self.fibers.lock().map_or(0, |fibers| fibers.len()),
            )
            .finish()
    }
}

impl ReactiveCapabilityRuntime {
    /// Creates a coordinator over an explicit context view.
    #[must_use]
    pub fn new(context: CapabilityContext) -> Self {
        Self {
            context,
            registry: CapabilityRegistry::new(),
            fibers: Mutex::new(BTreeMap::new()),
        }
    }

    /// Returns the context used by coordinator-owned fibers and publications.
    #[must_use]
    pub const fn context(&self) -> &CapabilityContext {
        &self.context
    }

    /// Returns the scope used for local publication and withdrawal.
    #[must_use]
    pub fn scope(&self) -> &Scope {
        self.context.scope()
    }

    /// Returns the explicit plugin registry owned by this coordinator.
    #[must_use]
    pub const fn registry(&self) -> &CapabilityRegistry {
        &self.registry
    }

    /// Registers a fiber as a reactive dependent.
    pub fn watch(&self, fiber: &Arc<CapabilityFiber>) -> Result<(), ReactiveRuntimeError> {
        let mut fibers = self.fibers.lock().expect("reactive fiber lock poisoned");
        if fibers.get(&fiber.id()).and_then(Weak::upgrade).is_some() {
            return Err(ReactiveRuntimeError::DuplicateFiber(fiber.id()));
        }
        fibers.insert(fiber.id(), Arc::downgrade(fiber));
        Ok(())
    }

    /// Instantiates and watches one fiber in this coordinator's context.
    pub fn instantiate(
        &self,
        plugin: &CapabilityId,
        config: PluginConfig,
    ) -> Result<Arc<CapabilityFiber>, ReactiveRuntimeError> {
        let fiber = self
            .registry
            .instantiate(plugin, self.context.clone(), config)
            .map_err(ReactiveRuntimeError::Registry)?;
        self.watch(&fiber)?;
        Ok(fiber)
    }

    /// Publishes a local provider and records no lifecycle work until the
    /// caller reaches [`Self::reconcile`].
    pub fn provide<F>(
        &self,
        definition: CapabilityDefinition,
        construct: F,
    ) -> Result<CapabilityHandle, ScopeError>
    where
        F: FnOnce(&crate::ResolvedDependencies) -> Result<crate::CapabilityValue, String>,
    {
        self.scope().provide(definition, construct)
    }

    /// Replaces a local provider. A later reconciliation observes exact entry
    /// identity, so equal values from a new publication still rebind.
    pub fn replace<F>(
        &self,
        definition: CapabilityDefinition,
        expected_generation: Generation,
        construct: F,
    ) -> Result<CapabilityHandle, ScopeError>
    where
        F: FnOnce(&crate::ResolvedDependencies) -> Result<crate::CapabilityValue, String>,
    {
        self.scope()
            .replace(definition, expected_generation, construct)
    }

    /// Publishes a provider and then drives the explicit stable boundary.
    pub async fn provide_and_reconcile<F>(
        &self,
        definition: CapabilityDefinition,
        construct: F,
    ) -> Result<(CapabilityHandle, ReconcileReport), ReactiveRuntimeError>
    where
        F: FnOnce(&crate::ResolvedDependencies) -> Result<crate::CapabilityValue, String>,
    {
        let handle = self.provide(definition, construct)?;
        let report = self.reconcile().await;
        Ok((handle, report))
    }

    /// Replaces a provider and then drives the explicit stable boundary.
    pub async fn replace_and_reconcile<F>(
        &self,
        definition: CapabilityDefinition,
        expected_generation: Generation,
        construct: F,
    ) -> Result<(CapabilityHandle, ReconcileReport), ReactiveRuntimeError>
    where
        F: FnOnce(&crate::ResolvedDependencies) -> Result<crate::CapabilityValue, String>,
    {
        let handle = self.replace(definition, expected_generation, construct)?;
        let report = self.reconcile().await;
        Ok((handle, report))
    }

    /// Reconciles all watched fibers whose effective target changed.
    ///
    /// The method is intentionally an explicit async boundary. It coalesces
    /// mutations that happened before the call and never creates replacement
    /// fibers to escape an in-flight lifecycle race.
    pub async fn reconcile(&self) -> ReconcileReport {
        let mut report = ReconcileReport::default();
        loop {
            let fibers = self.snapshot_fibers();
            let mut drove_fiber = false;
            for fiber in fibers {
                if !fiber.needs_reconciliation().await {
                    continue;
                }
                drove_fiber = true;
                report.driven.push(fiber.id());
                if let Err(error) = fiber.await_stable().await {
                    report.errors.push(FiberReconcileError {
                        fiber_id: fiber.id(),
                        error,
                    });
                }
                append_cleanup_failure(&mut report, &fiber).await;
            }
            if !drove_fiber {
                return report;
            }
        }
    }

    /// Withdraws a provider, drains committed dependents deepest-first, and
    /// releases provider ownership only after every affected fiber is stable.
    pub async fn withdraw_and_reconcile(
        &self,
        capability: &CapabilityId,
        expected_generation: Generation,
    ) -> Result<ReconcileReport, ReactiveRuntimeError> {
        if self.scope().get_local(capability).is_none() {
            return Ok(ReconcileReport {
                provider_finalized: true,
                ..ReconcileReport::default()
            });
        }
        let retired = self.scope().withdraw(capability, expected_generation)?;
        let fibers = self.snapshot_fibers();
        let identities = provider_identities(&fibers).await;
        let affected = withdrawal_closure(&fibers, &retired, &identities).await;
        let ordered = withdrawal_order(&fibers, &affected, &identities).await;

        let mut report = ReconcileReport::default();
        for fiber in ordered {
            report.driven.push(fiber.id());
            let provider_is_retired_publication =
                fiber.handle().await.as_ref().is_some_and(|handle| {
                    identity_of_handle(handle) == identity_of_handle(&retired)
                });
            let result = if provider_is_retired_publication {
                fiber.deactivate_for_withdrawal_after_detach(&retired).await
            } else {
                fiber.deactivate_for_withdrawal().await
            };
            if let Err(error) = result {
                report.errors.push(FiberReconcileError {
                    fiber_id: fiber.id(),
                    error,
                });
            }
            append_cleanup_failure(&mut report, &fiber).await;
        }

        // This is deliberately after the dependent loop. The guard is the
        // provider-finalization boundary, not merely an Arc lifetime detail.
        drop(retired);
        report.provider_finalized = true;
        Ok(report)
    }

    fn snapshot_fibers(&self) -> Vec<Arc<CapabilityFiber>> {
        let mut fibers = self.fibers.lock().expect("reactive fiber lock poisoned");
        let mut live = Vec::new();
        fibers.retain(|_, weak| {
            let Some(fiber) = weak.upgrade() else {
                return false;
            };
            live.push(fiber);
            true
        });
        live.sort_by_key(|fiber| fiber.id());
        live
    }
}

async fn append_cleanup_failure(report: &mut ReconcileReport, fiber: &CapabilityFiber) {
    let errors = fiber.cleanup_errors().await;
    if !errors.is_empty() {
        report.cleanup_errors.push(FiberCleanupFailure {
            fiber_id: fiber.id(),
            errors,
        });
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ProviderIdentity {
    capability: CapabilityId,
    generation: Generation,
    entry_id: crate::EntryId,
}

fn identity_of_handle(handle: &CapabilityHandle) -> ProviderIdentity {
    ProviderIdentity {
        capability: handle.id().clone(),
        generation: handle.generation(),
        entry_id: handle.entry_id(),
    }
}

fn identity_of_pin(capability: &CapabilityId, pin: DependencyPin) -> ProviderIdentity {
    ProviderIdentity {
        capability: capability.clone(),
        generation: pin.generation(),
        entry_id: pin.entry_id(),
    }
}

async fn provider_identities(
    fibers: &[Arc<CapabilityFiber>],
) -> BTreeMap<ProviderIdentity, FiberId> {
    let mut identities = BTreeMap::new();
    for fiber in fibers {
        if let Some(handle) = fiber.handle().await {
            identities.insert(identity_of_handle(&handle), fiber.id());
        }
    }
    identities
}

async fn dependency_binding(fiber: &CapabilityFiber) -> Option<DependencyBinding> {
    if let Some(binding) = fiber.dependency_binding().await {
        Some(binding)
    } else {
        fiber.loading_binding().await
    }
}

async fn withdrawal_closure(
    fibers: &[Arc<CapabilityFiber>],
    retired: &CapabilityHandle,
    identities: &BTreeMap<ProviderIdentity, FiberId>,
) -> BTreeSet<FiberId> {
    let retired_identity = identity_of_handle(retired);
    let mut affected = BTreeSet::new();
    for fiber in fibers {
        if fiber
            .handle()
            .await
            .as_ref()
            .is_some_and(|handle| identity_of_handle(handle) == retired_identity)
            || dependency_binding(fiber)
                .await
                .is_some_and(|binding| binding_contains(&binding, &retired_identity))
        {
            affected.insert(fiber.id());
        }
    }

    loop {
        let mut changed = false;
        for fiber in fibers {
            if affected.contains(&fiber.id()) {
                continue;
            }
            let Some(binding) = dependency_binding(fiber).await else {
                continue;
            };
            let depends_on_affected = binding.iter().any(|(capability, pin)| {
                identities
                    .get(&identity_of_pin(capability, pin))
                    .is_some_and(|provider| affected.contains(provider))
            });
            if depends_on_affected {
                affected.insert(fiber.id());
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    affected
}

fn binding_contains(binding: &DependencyBinding, target: &ProviderIdentity) -> bool {
    binding
        .iter()
        .any(|(capability, pin)| identity_of_pin(capability, pin) == target.clone())
}

async fn withdrawal_order(
    fibers: &[Arc<CapabilityFiber>],
    affected: &BTreeSet<FiberId>,
    identities: &BTreeMap<ProviderIdentity, FiberId>,
) -> Vec<Arc<CapabilityFiber>> {
    let mut by_id = BTreeMap::new();
    let mut bindings = BTreeMap::new();
    for fiber in fibers {
        by_id.insert(fiber.id(), Arc::clone(fiber));
        if affected.contains(&fiber.id()) {
            if let Some(binding) = dependency_binding(fiber).await {
                bindings.insert(fiber.id(), binding);
            }
        }
    }

    let mut ordered = affected
        .iter()
        .filter_map(|id| by_id.get(id).cloned())
        .map(|fiber| {
            let depth = dependency_depth(
                fiber.id(),
                &bindings,
                affected,
                identities,
                &mut BTreeSet::new(),
            );
            (depth, fiber)
        })
        .collect::<Vec<_>>();
    ordered.sort_by(|(left_depth, left), (right_depth, right)| {
        right_depth
            .cmp(left_depth)
            .then_with(|| left.id().cmp(&right.id()))
    });
    ordered.into_iter().map(|(_, fiber)| fiber).collect()
}

fn dependency_depth(
    fiber_id: FiberId,
    bindings: &BTreeMap<FiberId, DependencyBinding>,
    affected: &BTreeSet<FiberId>,
    identities: &BTreeMap<ProviderIdentity, FiberId>,
    visiting: &mut BTreeSet<FiberId>,
) -> usize {
    if !visiting.insert(fiber_id) {
        return 0;
    }
    let depth = bindings.get(&fiber_id).map_or(0, |binding| {
        binding
            .iter()
            .filter_map(|(capability, pin)| {
                identities.get(&identity_of_pin(capability, pin)).copied()
            })
            .filter(|provider| affected.contains(provider) && *provider != fiber_id)
            .map(|provider| {
                1 + dependency_depth(provider, bindings, affected, identities, visiting)
            })
            .max()
            .unwrap_or(0)
    });
    visiting.remove(&fiber_id);
    depth
}

/// One fiber error collected without stopping sibling reconciliation.
#[derive(Debug)]
pub struct FiberReconcileError {
    /// Fiber that failed to reach its requested stable state.
    pub fiber_id: FiberId,
    /// Lifecycle error returned by the fiber.
    pub error: FiberError,
}

/// Cleanup errors collected after a fiber transition.
#[derive(Debug)]
pub struct FiberCleanupFailure {
    /// Fiber whose cleanup reported failures.
    pub fiber_id: FiberId,
    /// All cleanup errors observed for that fiber.
    pub errors: Vec<EffectError>,
}

/// Result of one explicit reactive reconciliation boundary.
#[derive(Debug, Default)]
pub struct ReconcileReport {
    /// Fibers driven in deterministic identity order, or withdrawal order.
    pub driven: Vec<FiberId>,
    /// Lifecycle failures collected without skipping other fibers.
    pub errors: Vec<FiberReconcileError>,
    /// Cleanup failures collected without changing provider finalization order.
    pub cleanup_errors: Vec<FiberCleanupFailure>,
    /// Whether a withdrawal completed the coordinator-owned provider-finalization
    /// boundary: the provider was removed from future resolution, affected
    /// reactive dependents were quiesced, and the retirement guard was released.
    /// This does not mean all external [`CapabilityHandle`] values were dropped
    /// or that [`crate::CapabilityValue`] cleanup necessarily ran.
    pub provider_finalized: bool,
}

impl ReconcileReport {
    /// Returns whether all driven fibers reported no lifecycle or cleanup error.
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.errors.is_empty() && self.cleanup_errors.is_empty()
    }
}

/// Errors that prevent a reactive mutation from reaching reconciliation.
#[derive(Debug)]
pub enum ReactiveRuntimeError {
    /// Capability admission or withdrawal failed.
    Scope(ScopeError),
    /// Plugin registry operation failed.
    Registry(RegistryError),
    /// A fiber identity was watched twice while still live.
    DuplicateFiber(FiberId),
}

impl fmt::Display for ReactiveRuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Scope(error) => write!(f, "reactive scope error: {error}"),
            Self::Registry(error) => write!(f, "reactive registry error: {error}"),
            Self::DuplicateFiber(id) => write!(f, "fiber {id:?} is already watched"),
        }
    }
}

impl std::error::Error for ReactiveRuntimeError {}

impl From<ScopeError> for ReactiveRuntimeError {
    fn from(error: ScopeError) -> Self {
        Self::Scope(error)
    }
}

//! Activated composition assemblies and their exactly-once release.

use crate::config::HostConfig;
use crate::error::{CleanupResource, CompositionShutdownFailure, RollbackFailure, RollbackReport};
use crate::registration::LifecycleHook;
use capability_graph::CapabilityFiber;
use kernis_core::Id;
use runtime_core::{EffectDispatcher, Runtime, RuntimeDriver, RuntimeHandle};
use std::fmt;
use std::sync::Arc;
use workflow_recovery::{DurableStore, InMemoryDurableStore};

/// One module whose activation resources are owned by the composition.
///
/// The ledger records only what activation actually acquired: registered
/// plugin identities, successfully started fibers, and whether the module's
/// `activate` hook completed (which arms its `dispose` hook). Pre-existing
/// runtime state and resources a module never acquired are deliberately
/// absent and are therefore never fake-disposed during rollback.
pub(crate) struct ActivatedModule {
    pub(crate) module_id: Id,
    pub(crate) plugins: Vec<Id>,
    pub(crate) fibers: Vec<(Id, Arc<CapabilityFiber>)>,
    pub(crate) on_dispose: Option<LifecycleHook>,
    pub(crate) hook_armed: bool,
}

impl ActivatedModule {
    pub(crate) fn new(module_id: Id, on_dispose: Option<LifecycleHook>) -> Self {
        Self {
            module_id,
            plugins: Vec::new(),
            fibers: Vec::new(),
            on_dispose,
            hook_armed: false,
        }
    }
}

/// Releases composition-owned resources in reverse activation order.
///
/// Each owned module is disposed as: `dispose` hook (only when its
/// `activate` hook completed), then its started fibers in reverse
/// contribution order. Cleanup continues after an individual failure and
/// every failure is collected; a module enters `cleaned` only when all of
/// its owned cleanup succeeded.
pub(crate) async fn rollback_modules(activated: &mut Vec<ActivatedModule>) -> RollbackReport {
    let mut report = RollbackReport::default();
    while let Some(module) = activated.pop() {
        let mut clean = true;
        if module.hook_armed {
            if let Some(hook) = &module.on_dispose {
                if let Err(reason) = hook().await {
                    report.failures.push(RollbackFailure {
                        module_id: module.module_id.clone(),
                        resource: CleanupResource::DisposeHook,
                        reason,
                    });
                    clean = false;
                }
            }
        }
        for (plugin_id, fiber) in module.fibers.iter().rev() {
            if let Err(source) = fiber.dispose().await {
                report.failures.push(RollbackFailure {
                    module_id: module.module_id.clone(),
                    resource: CleanupResource::Fiber {
                        plugin_id: plugin_id.clone(),
                    },
                    reason: source.to_string(),
                });
                clean = false;
            }
        }
        if clean {
            report.cleaned.push(module.module_id);
        }
    }
    report
}

/// An activated composition: one K2 [`Runtime`] plus the module resources
/// activation acquired on top of it.
///
/// The assembly is the stable reactive boundary: construction or restore
/// succeeded through K2, every module's plugins are registered, their fibers
/// are started, `activate` hooks completed, and reconciliation reached a
/// stable boundary. No task execution happens during activation.
pub struct RuntimeAssembly<S = InMemoryDurableStore>
where
    S: DurableStore,
{
    runtime: Runtime<S>,
    order: Vec<Id>,
    owned: Vec<ActivatedModule>,
    config: HostConfig,
}

impl<S: DurableStore> fmt::Debug for RuntimeAssembly<S> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RuntimeAssembly")
            .field("order", &self.order)
            .finish_non_exhaustive()
    }
}

impl<S> RuntimeAssembly<S>
where
    S: DurableStore,
{
    pub(crate) fn new(
        runtime: Runtime<S>,
        order: Vec<Id>,
        owned: Vec<ActivatedModule>,
        config: HostConfig,
    ) -> Self {
        Self {
            runtime,
            order,
            owned,
            config,
        }
    }

    /// Borrows the activated runtime.
    #[must_use]
    pub fn runtime(&self) -> &Runtime<S> {
        &self.runtime
    }

    /// Mutably borrows the activated runtime.
    pub fn runtime_mut(&mut self) -> &mut Runtime<S> {
        &mut self.runtime
    }

    /// Returns the deterministic activation order of the composition.
    #[must_use]
    pub fn module_order(&self) -> &[Id] {
        &self.order
    }

    /// Returns the host configuration the assembly was activated with.
    #[must_use]
    pub fn host_config(&self) -> &HostConfig {
        &self.config
    }
}

impl<S> RuntimeAssembly<S>
where
    S: DurableStore,
{
    /// Releases the composition exactly once: hooks and fibers in reverse
    /// activation order, then the runtime itself.
    ///
    /// Cleanup continues after individual failures and all failures are
    /// reported; the runtime is dropped in every case.
    pub async fn shutdown(self) -> Result<RollbackReport, CompositionShutdownFailure> {
        let Self {
            runtime, mut owned, ..
        } = self;
        let rollback = rollback_modules(&mut owned).await;
        drop(runtime);
        if rollback.is_success() {
            Ok(rollback)
        } else {
            Err(CompositionShutdownFailure { rollback })
        }
    }
}

impl<S> RuntimeAssembly<S>
where
    S: DurableStore + Send,
{
    /// Hands runtime ownership to the K3 [`RuntimeDriver`] and returns the
    /// composition-owned resources as a separate [`CompositionHandle`] so
    /// they can be released independently of the driver lifecycle.
    pub fn into_driver<D>(
        self,
        dispatcher: D,
    ) -> (RuntimeDriver<S, D>, RuntimeHandle, CompositionHandle)
    where
        D: EffectDispatcher,
    {
        let Self {
            runtime,
            order,
            owned,
            ..
        } = self;
        let (driver, handle) = RuntimeDriver::new(runtime, dispatcher);
        (driver, handle, CompositionHandle { order, owned })
    }
}

/// Composition-owned activation resources separated from the runtime by
/// [`RuntimeAssembly::into_driver`].
///
/// [`Self::dispose`] releases the hooks and fibers exactly once. Dropping
/// the handle without disposing only drops the fiber references; the host
/// must call `dispose` for ordered cleanup.
pub struct CompositionHandle {
    order: Vec<Id>,
    owned: Vec<ActivatedModule>,
}

impl CompositionHandle {
    /// Returns the deterministic activation order of the composition.
    #[must_use]
    pub fn module_order(&self) -> &[Id] {
        &self.order
    }

    /// Releases the composition-owned hooks and fibers exactly once, in
    /// reverse activation order, continuing after individual failures.
    pub async fn dispose(mut self) -> Result<RollbackReport, CompositionShutdownFailure> {
        let rollback = rollback_modules(&mut self.owned).await;
        if rollback.is_success() {
            Ok(rollback)
        } else {
            Err(CompositionShutdownFailure { rollback })
        }
    }
}

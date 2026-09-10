//! Activated composition assemblies and their exactly-once release.

use crate::config::HostConfig;
use crate::error::{CleanupResource, CompositionShutdownFailure, RollbackFailure, RollbackReport};
use crate::registration::LifecycleHook;
use capability_graph::{CapabilityFiber, CapabilityRegistry};
use kernis_core::Id;
use runtime_core::{
    DriverExit, EffectDispatcher, Runtime, RuntimeDriver, RuntimeHandle, ShutdownStatus,
};
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

/// Registry authority available to one composition cleanup sweep.
///
/// The cleanup contract is only complete while the capability registry is
/// reachable: a sweep with [`Self::Registry`] authority unregisters every
/// composition-owned plugin registration. A sweep with [`Self::OwnerLost`]
/// runs after the Runtime owner disappeared with the registry, so the
/// registration entries are already unreachable and the sweep records each
/// outstanding registration as a cleanup failure instead of claiming
/// orderly completion.
pub(crate) enum CleanupAuthority<'a> {
    /// The live Runtime's capability registry is reachable.
    Registry(&'a CapabilityRegistry),
    /// The driver owner was lost and the registry went with it.
    OwnerLost,
}

/// Releases composition-owned resources in reverse activation order.
///
/// Each owned module is disposed as: `dispose` hook (only when its
/// `activate` hook completed), then its started fibers in reverse
/// contribution order, then its successfully registered plugin runtimes in
/// reverse registration order through the registry authority. Cleanup
/// continues after an individual failure and every failure is collected; a
/// module enters `cleaned` only when all of its owned cleanup succeeded.
pub(crate) async fn cleanup_modules(
    activated: &mut Vec<ActivatedModule>,
    authority: CleanupAuthority<'_>,
) -> RollbackReport {
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
        for plugin_id in module.plugins.iter().rev() {
            match authority {
                CleanupAuthority::Registry(registry) => {
                    if let Err(source) = registry.remove(plugin_id).await {
                        report.failures.push(RollbackFailure {
                            module_id: module.module_id.clone(),
                            resource: CleanupResource::PluginRegistration {
                                plugin_id: plugin_id.clone(),
                            },
                            reason: source.to_string(),
                        });
                        clean = false;
                    }
                }
                CleanupAuthority::OwnerLost => {
                    report.failures.push(RollbackFailure {
                        module_id: module.module_id.clone(),
                        resource: CleanupResource::PluginRegistration {
                            plugin_id: plugin_id.clone(),
                        },
                        reason: "the capability registry authority was released with the \
                                 runtime owner, so the registration could not be unregistered"
                            .to_owned(),
                    });
                    clean = false;
                }
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
    /// Releases the composition exactly once: hooks, fibers, and plugin
    /// registrations in reverse activation order, then the runtime itself.
    ///
    /// The sweep runs with full registry authority, so when the report says
    /// cleanup completed, no composition-owned plugin registration remains
    /// in the runtime. Cleanup continues after individual failures and all
    /// failures are reported; the runtime is dropped in every case.
    pub async fn shutdown(self) -> Result<RollbackReport, CompositionShutdownFailure> {
        let Self {
            runtime, mut owned, ..
        } = self;
        let rollback = cleanup_modules(
            &mut owned,
            CleanupAuthority::Registry(runtime.capability_registry()),
        )
        .await;
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
    /// composition-owned resources as a separate [`CompositionHandle`].
    ///
    /// The handle does not own registry authority while the driver is
    /// alive. The host releases the composition through exactly one of the
    /// handle's two lifecycle paths: [`CompositionHandle::dispose_after_driver`]
    /// after an orderly K3 shutdown, or [`CompositionHandle::release_after_owner_loss`]
    /// after the driver owner was lost.
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
/// The handle deliberately exposes no unconditional `dispose`: claiming
/// composition cleanup requires the capability registry, and the runtime
/// that hosts it lives in exactly one place at a time.
///
/// * Orderly path — after `handle.shutdown()` completes and the driver task
///   returns its [`DriverExit`], the host passes that exit to
///   [`Self::dispose_after_driver`]. The sweep re-acquires the Runtime and
///   its registry, unregisters every composition-owned plugin registration,
///   and returns the released runtime for final inspection and release.
/// * Owner-loss path — when the driver was dropped, aborted, or unwound by
///   a panic, the Runtime and its registry are already released and no
///   [`DriverExit`] exists. [`Self::release_after_owner_loss`] then performs
///   best-effort release of the process-local handles the composition still
///   holds (dispose hooks and fiber disposal) and records every outstanding
///   plugin registration as a [`CleanupResource::PluginRegistration`]
///   failure, because the registry authority disappeared with the runtime
///   owner.
///
/// Dropping the handle without either call only drops the fiber references.
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

    /// Completes the orderly driver lifecycle: releases the Runtime from
    /// the K3 [`DriverExit`], runs the exactly-once composition cleanup with
    /// full registry authority in reverse activation order, and hands the
    /// released runtime back to the host for final release.
    ///
    /// The K3 shutdown classification is preserved unchanged. The
    /// composition-owned plugin runtimes are unregistered through the live
    /// registry, so a successful report means no composition-owned plugin
    /// registration remains in the returned runtime.
    pub async fn dispose_after_driver<S>(self, exit: DriverExit<S>) -> CompositionDriverShutdown<S>
    where
        S: DurableStore + Send,
    {
        let Self { mut owned, .. } = self;
        let shutdown_status = exit.shutdown_status().clone();
        let runtime = exit.into_runtime();
        let rollback = cleanup_modules(
            &mut owned,
            CleanupAuthority::Registry(runtime.capability_registry()),
        )
        .await;
        drop(owned);
        CompositionDriverShutdown {
            rollback,
            shutdown_status,
            runtime,
        }
    }

    /// Best-effort release after the driver owner was lost.
    ///
    /// The Runtime dropped with the driver owner, so its capability
    /// registry — and with it the authority to unregister plugin
    /// registrations — is already released. This sweep releases only the
    /// process-local handles the composition still holds: armed `dispose`
    /// hooks once and started fibers once, in reverse activation order,
    /// continuing after individual failures. Every outstanding
    /// composition-owned plugin registration is reported as a
    /// [`CleanupResource::PluginRegistration`] failure naming the lost
    /// registry authority, so the report never claims orderly composition
    /// cleanup on a path that could not perform it.
    pub async fn release_after_owner_loss(self) -> RollbackReport {
        let Self { mut owned, .. } = self;
        cleanup_modules(&mut owned, CleanupAuthority::OwnerLost).await
    }
}

/// The outcome of an orderly K3 driver shutdown followed by composition
/// cleanup through [`CompositionHandle::dispose_after_driver`].
///
/// The runtime is returned so the host can verify the invariant — a
/// successful `rollback` means no composition-owned plugin registration
/// remains in the still-live runtime — and then perform the final release
/// by dropping it.
pub struct CompositionDriverShutdown<S = InMemoryDurableStore>
where
    S: DurableStore + Send,
{
    /// The composition cleanup report for hooks, fibers, and registrations.
    pub rollback: RollbackReport,
    /// The K3 durable-work classification from the driver shutdown,
    /// preserved unchanged.
    pub shutdown_status: ShutdownStatus,
    /// The released runtime, returned with full composition cleanup done.
    pub runtime: Runtime<S>,
}

impl<S> fmt::Debug for CompositionDriverShutdown<S>
where
    S: DurableStore + Send,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CompositionDriverShutdown")
            .field("rollback", &self.rollback)
            .field("shutdown_status", &self.shutdown_status)
            .finish_non_exhaustive()
    }
}

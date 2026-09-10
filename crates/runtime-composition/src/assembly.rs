//! Activated composition assemblies and their exactly-once release.

use crate::config::HostConfig;
use crate::error::{
    CleanupResource, CompositionShutdownFailure, OwnerLossReleaseError, RollbackFailure,
    RollbackReport,
};
use crate::registration::LifecycleHook;
use capability_graph::{CapabilityFiber, CapabilityRegistry};
use kernis_core::Id;
use runtime_core::{
    DriverExit, DriverOwnerState, EffectDispatcher, Runtime, RuntimeDriver, RuntimeHandle,
    ShutdownStatus,
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
    /// alive. The handle is bound to this exact driver: it carries a
    /// private observation of the same command mailbox the driver's owner
    /// guard marks, so [`CompositionHandle::release_after_owner_loss`]
    /// proves the loss against this driver and no other. The host releases
    /// the composition through exactly one of the handle's two lifecycle
    /// paths: [`CompositionHandle::dispose_after_driver`] after an orderly
    /// K3 shutdown, or [`CompositionHandle::release_after_owner_loss`]
    /// after the driver owner was actually lost.
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
        (
            driver,
            handle.clone(),
            CompositionHandle {
                order,
                owned: Some(owned),
                driver_owner: handle,
            },
        )
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
///   owner. This path is guarded: the handle is bound to the exact driver
///   separated by the same `into_driver` call and verifies through that
///   driver's own mailbox that its owner state is
///   [`DriverOwnerState::OwnerDropped`] before performing any cleanup.
///   While the driver is [`DriverOwnerState::Running`] or [`DriverOwnerState::Shutdown`],
///   the release is rejected with a typed [`OwnerLossReleaseError`], the
///   handle stays fully usable, and nothing is disposed — an orderly
///   shutdown in particular always keeps its `DriverExit` and must go
///   through [`Self::dispose_after_driver`].
///
/// Dropping the handle without either call only drops the bound handle
/// clone and the fiber references.
pub struct CompositionHandle {
    order: Vec<Id>,
    /// `None` only after a successful [`Self::release_after_owner_loss`],
    /// which is how exactly-once release is enforced without consuming the
    /// handle on a rejected attempt.
    owned: Option<Vec<ActivatedModule>>,
    /// Process-local observation of this exact driver's owner state. The
    /// mailbox is the single driver-owner truth (K3); this field only
    /// reads it and takes no runtime authority. It is never serialized or
    /// handed out, so no other driver's owner state can be presented as
    /// proof for this composition.
    driver_owner: RuntimeHandle,
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
        let Self { owned, .. } = self;
        // A `DriverExit` exists only after an orderly shutdown, which is
        // mutually exclusive with owner loss, so the only state that could
        // have emptied the ledger cannot be standing here.
        let mut owned = owned.unwrap_or_default();
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

    /// Best-effort release after the driver owner was actually lost.
    ///
    /// The release first proves the loss against this exact driver: the
    /// handle is bound to the [`RuntimeHandle`] separated by the same
    /// [`RuntimeAssembly::into_driver`] call, and only
    /// [`DriverOwnerState::OwnerDropped`] — the state the K3 driver-owner
    /// guard marks when the driver is dropped, aborted, or unwound — lets
    /// the sweep start. While the driver is
    /// [`DriverOwnerState::Running`], the release is rejected with
    /// [`OwnerLossReleaseError::OwnerStillRunning`] and nothing is
    /// disposed; a driver that completed an orderly shutdown is rejected
    /// with [`OwnerLossReleaseError::OrderlyShutdownCompleted`] because
    /// that path produced a [`DriverExit`] and must go through
    /// [`Self::dispose_after_driver`] with full registry authority. A
    /// rejected release consumes nothing: the handle stays usable for the
    /// orderly path or for a later, genuine owner-loss release.
    ///
    /// Once the guard passes, the sweep performs the release exactly once
    /// and future calls return [`OwnerLossReleaseError::AlreadyReleased`].
    ///
    /// The Runtime dropped with the driver owner, so its capability
    /// registry — and with it the authority to unregister plugin
    /// registrations — is already released. The sweep therefore releases
    /// only the process-local handles the composition still holds: armed
    /// `dispose` hooks once and started fibers once, in reverse activation
    /// order, continuing after individual failures. Every outstanding
    /// composition-owned plugin registration is reported as a
    /// [`CleanupResource::PluginRegistration`] failure naming the lost
    /// registry authority, so the report never claims orderly composition
    /// cleanup on a path that could not perform it.
    pub async fn release_after_owner_loss(
        &mut self,
    ) -> Result<RollbackReport, OwnerLossReleaseError> {
        match self.driver_owner.owner_state() {
            DriverOwnerState::OwnerDropped => {}
            DriverOwnerState::Running => {
                return Err(OwnerLossReleaseError::OwnerStillRunning);
            }
            DriverOwnerState::Shutdown => {
                return Err(OwnerLossReleaseError::OrderlyShutdownCompleted);
            }
        }
        let Some(mut owned) = self.owned.take() else {
            return Err(OwnerLossReleaseError::AlreadyReleased);
        };
        Ok(cleanup_modules(&mut owned, CleanupAuthority::OwnerLost).await)
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

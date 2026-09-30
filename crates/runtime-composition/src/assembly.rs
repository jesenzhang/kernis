//! Activated composition assemblies and their exactly-once release.
//!
//! Composition cleanup is legal only while the composition can name the
//! authority it is cleaning through ([`CleanupAuthority`]): either the live
//! capability registry of a `Runtime` released by an orderly driver shutdown, or
//! the acknowledged fact that the driver owner was lost and the registry went
//! with it. Two entry points implement that rule, and both are cancellation-safe
//! at every point a future can be dropped: cleanup progress lives in the
//! handle's ledger rather than in a temporary future, a step whose future
//! already started stays parked there so a re-entry resumes it instead of
//! re-running a started non-idempotent hook, and an accepted orderly release is
//! recorded in the ledger when it is accepted — not when its future is first
//! polled — so even a never-polled drop leaves its remaining work reachable.

use crate::config::HostConfig;
use crate::error::{
    CleanupResource, CompositionShutdownFailure, OwnerLossReleaseError, RollbackFailure,
    RollbackReport,
};
use crate::registration::{HookFuture, LifecycleHook};
use capability_graph::{CapabilityFiber, CapabilityRegistry};
use kernis_core::Id;
use runtime_core::{
    DriverExit, DriverOwnerState, EffectDispatcher, Runtime, RuntimeDriver, RuntimeHandle,
    ShutdownStatus,
};
use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
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
#[derive(Clone, Copy)]
pub(crate) enum CleanupAuthority<'a> {
    /// The live Runtime's capability registry is reachable.
    Registry(&'a CapabilityRegistry),
    /// The driver owner was lost and the registry went with it.
    OwnerLost,
}

/// A cleanup step whose future has already started.
///
/// The future is owned by the ledger, so cancelling the caller's future neither
/// drops the started step nor allows the same step to be started twice.
type StepFuture = HookFuture;

enum InFlight {
    /// The current module's armed `dispose` hook, started and awaiting its
    /// external cleanup.
    DisposeHook {
        /// The module whose hook is running.
        module_id: Id,
        /// The started hook future, parked until it completes.
        future: StepFuture,
    },
    /// One owned fiber's disposal, started and awaiting its external cleanup.
    FiberDisposal {
        /// The module that owns the fiber.
        module_id: Id,
        /// The plugin identity the fiber was started for.
        plugin_id: Id,
        /// The started disposal future, parked until it completes.
        future: StepFuture,
    },
}

/// Resumable record of one composition's outstanding cleanup.
///
/// Every fact a cancelled sweep would otherwise lose lives here: the modules
/// that have not started, the module in progress with its remaining fibers and
/// registrations, the in-flight step future, and the report accumulated so far.
pub(crate) struct CleanupLedger {
    /// Modules whose cleanup has not started; the next module to clean is last.
    queue: VecDeque<ActivatedModule>,
    /// The module whose cleanup is in progress.
    current: Option<Id>,
    /// The current module's armed dispose hook, until its step starts.
    current_hook: Option<LifecycleHook>,
    /// The current module's remaining fibers, next-to-dispose last.
    current_fibers: Vec<(Id, Arc<CapabilityFiber>)>,
    /// The current module's remaining registrations, next-to-unregister last.
    current_plugins: Vec<Id>,
    /// Whether the steps completed so far for the current module succeeded.
    current_clean: bool,
    in_flight: Option<InFlight>,
    report: RollbackReport,
    /// Set the moment an orderly release is accepted for this ledger, before
    /// its future is ever polled: from then on the `Runtime` whose registry the
    /// sweep cleans through lives in a future the caller may drop at any time,
    /// so a full-authority sweep can no longer be promised and only the
    /// best-effort path is left if this ledger is still outstanding.
    orderly_release_accepted: bool,
}

impl CleanupLedger {
    fn new(activated: Vec<ActivatedModule>) -> Self {
        Self {
            queue: activated.into_iter().collect(),
            current: None,
            current_hook: None,
            current_fibers: Vec::new(),
            current_plugins: Vec::new(),
            current_clean: true,
            in_flight: None,
            report: RollbackReport::default(),
            orderly_release_accepted: false,
        }
    }

    /// Starts cleanup of the next module, or reports that none is left.
    fn begin_next(&mut self) -> bool {
        let Some(module) = self.queue.pop_back() else {
            return false;
        };
        let ActivatedModule {
            module_id,
            plugins,
            fibers,
            on_dispose,
            hook_armed,
        } = module;
        self.current = Some(module_id);
        self.current_hook = if hook_armed { on_dispose } else { None };
        self.current_fibers = fibers;
        self.current_plugins = plugins;
        self.current_clean = true;
        true
    }

    fn record_failure(&mut self, module_id: Id, resource: CleanupResource, reason: String) {
        self.current_clean = false;
        self.report.failures.push(RollbackFailure {
            module_id,
            resource,
            reason,
        });
    }

    fn finish_current(&mut self) {
        if let Some(module_id) = self.current.take() {
            if self.current_clean {
                self.report.cleaned.push(module_id);
            }
            self.current_clean = true;
        }
    }

    /// Returns whether every module completed every owned cleanup step.
    fn is_complete(&self) -> bool {
        self.queue.is_empty() && self.current.is_none() && self.in_flight.is_none()
    }

    /// Returns whether an orderly release has been accepted for this ledger.
    /// Once it has, the registry authority lives in a future the caller may
    /// drop at any time, so only the best-effort path can still be promised.
    fn orderly_release_accepted(&self) -> bool {
        self.orderly_release_accepted
    }
}

/// Owns the fiber for the duration of its disposal so the started step can be
/// parked in the ledger as a `'static` future.
async fn dispose_fiber(fiber: Arc<CapabilityFiber>) -> Result<(), String> {
    fiber.dispose().await.map_err(|source| source.to_string())
}

/// Awaits the already-started step without touching the rest of the ledger.
async fn await_parked(step: &mut InFlight) -> (Id, CleanupResource, Result<(), String>) {
    match step {
        InFlight::DisposeHook { module_id, future } => (
            module_id.clone(),
            CleanupResource::DisposeHook,
            future.await,
        ),
        InFlight::FiberDisposal {
            module_id,
            plugin_id,
            future,
        } => (
            module_id.clone(),
            CleanupResource::Fiber {
                plugin_id: plugin_id.clone(),
            },
            future.await,
        ),
    }
}

/// Runs one reverse-order cleanup sweep to completion or to cancellation.
///
/// Each module is disposed as: its `dispose` hook (only when `activate`
/// completed), then its started fibers in reverse contribution order, then its
/// successfully registered plugin runtimes in reverse registration order through
/// the registry authority. Cleanup continues after an individual failure and
/// every failure is collected; a module enters `cleaned` only when all of its
/// owned cleanup succeeded.
///
/// A module leaves the ledger only after its last step completed, and a started
/// step is awaited *in place* from the ledger, so dropping this future preserves
/// both the outstanding resources and everything already recorded.
async fn run_sweep(ledger: &mut CleanupLedger, authority: CleanupAuthority<'_>) -> RollbackReport {
    loop {
        if ledger.in_flight.is_some() {
            let (module_id, resource, result) =
                await_parked(ledger.in_flight.as_mut().expect("checked above")).await;
            ledger.in_flight = None;
            if let Err(reason) = result {
                ledger.record_failure(module_id, resource, reason);
            }
            continue;
        }

        if ledger.current.is_none() && !ledger.begin_next() {
            return std::mem::take(&mut ledger.report);
        }
        let module_id = ledger
            .current
            .clone()
            .expect("a module is current once the queue has yielded one");

        if let Some(hook) = ledger.current_hook.take() {
            ledger.in_flight = Some(InFlight::DisposeHook {
                module_id,
                future: hook(),
            });
            continue;
        }

        if let Some((plugin_id, fiber)) = ledger.current_fibers.pop() {
            ledger.in_flight = Some(InFlight::FiberDisposal {
                module_id,
                plugin_id,
                future: Box::pin(dispose_fiber(fiber)),
            });
            continue;
        }

        if let Some(plugin_id) = ledger.current_plugins.last().cloned() {
            let result = match authority {
                CleanupAuthority::Registry(registry) => registry
                    .remove(&plugin_id)
                    .await
                    .map_err(|source| source.to_string()),
                CleanupAuthority::OwnerLost => {
                    Err("the capability registry authority was released with the \
                     runtime owner, so the registration could not be unregistered"
                        .to_owned())
                }
            };
            ledger.current_plugins.pop();
            if let Err(reason) = result {
                ledger.record_failure(
                    module_id,
                    CleanupResource::PluginRegistration { plugin_id },
                    reason,
                );
            }
            continue;
        }

        ledger.finish_current();
    }
}

/// Releases the given activated modules in reverse activation order.
///
/// Used by the paths that own the modules outright (activation rollback and
/// [`RuntimeAssembly::shutdown`]); the driver paths keep their ledger alive in
/// the [`CompositionHandle`] across awaits instead, so a cancelled sweep there
/// stays resumable.
pub(crate) async fn cleanup_modules(
    activated: &mut Vec<ActivatedModule>,
    authority: CleanupAuthority<'_>,
) -> RollbackReport {
    let mut ledger = CleanupLedger::new(std::mem::take(activated));
    let report = run_sweep(&mut ledger, authority).await;
    debug_assert!(ledger.is_complete());
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
    ///
    /// # Dropping the returned future
    ///
    /// This is the one composition release path that is **not**
    /// cancellation-safe. The assembly is consumed by the call and the whole
    /// sweep lives in the returned future, so dropping it — before its first
    /// poll or at any await — drops the runtime and every composition-owned
    /// resource with no report and no way to resume. A host that must be able
    /// to cancel, or to continue after a shutdown timeout, uses
    /// [`Self::into_driver`] and the two [`CompositionHandle`] release paths,
    /// whose ledger is owned by the handle and survives cancellation
    /// (ADR 0009 §3).
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
                ledger: Some(CleanupLedger::new(owned)),
                driver_owner: handle,
            },
        )
    }
}

/// Composition-owned activation resources separated from the runtime by
/// [`RuntimeAssembly::into_driver`].
///
/// The handle deliberately exposes no unconditional `dispose`: claiming
/// composition cleanup requires the capability registry, and the runtime that
/// hosts it lives in exactly one place at a time.
///
/// * Orderly path — after `handle.shutdown()` completes and the driver task
///   returns its [`DriverExit`], the host passes that exit to
///   [`Self::dispose_after_driver`]. The exit must come from *this* handle's
///   driver: the handle carries the owner identity of the driver separated by
///   the same `into_driver` call, so an exit produced by another driver is
///   rejected and handed back untouched.
/// * Owner-loss path — when the driver was dropped, aborted, or unwound by a
///   panic, the Runtime and its registry are already released and no
///   [`DriverExit`] exists. [`Self::release_after_owner_loss`] then performs
///   best-effort release of the process-local handles the composition still
///   holds (dispose hooks and fiber disposal) and records every outstanding
///   plugin registration as a [`CleanupResource::PluginRegistration`] failure,
///   because the registry authority disappeared with the runtime owner. This
///   path is guarded: the handle verifies through that driver's own mailbox
///   that its owner state is [`DriverOwnerState::OwnerDropped`] before
///   performing any cleanup. While the driver is [`DriverOwnerState::Running`]
///   the release is rejected with a typed [`OwnerLossReleaseError`], the handle
///   stays fully usable, and nothing is disposed. An orderly shutdown normally
///   keeps its `DriverExit` and must go through [`Self::dispose_after_driver`];
///   the single exception is this handle's own accepted-but-unfinished orderly
///   release, whose ledger knows the registry authority left with the exit.
///
/// # Cancellation
///
/// Cleanup progress lives in this handle, not in the caller's future. Dropping
/// either release future — before its first poll or between polls — keeps the
/// remaining resources, the report collected so far, and the future of the step
/// that had already started (so a re-entry resumes it rather than re-triggering
/// a started non-idempotent hook). The two paths differ only in authority: an
/// interrupted owner-loss release resumes with the same authority, while an
/// interrupted orderly release has handed its `Runtime`, and with it the
/// registry authority, to the dropped future, so what is left runs best-effort
/// through [`Self::release_after_owner_loss`] and is reported as such. Dropping
/// the handle without either call only drops the bound handle clone and the
/// fiber references.
pub struct CompositionHandle {
    order: Vec<Id>,
    /// Outstanding composition cleanup. `None` only after a release flow ran to
    /// completion, which is how exactly-once release is enforced without
    /// consuming the handle on a rejected or interrupted attempt.
    ledger: Option<CleanupLedger>,
    /// Process-local observation of this exact driver's owner state. The
    /// mailbox is the single driver-owner truth (K3); this field only reads it
    /// and takes no runtime authority. It also supplies the [`DriverOwner`]
    /// identity that [`Self::dispose_after_driver`] requires an exit to match,
    /// so no other driver's exit or owner state can be presented as proof for
    /// this composition.
    driver_owner: RuntimeHandle,
}

impl CompositionHandle {
    /// Returns the deterministic activation order of the composition.
    #[must_use]
    pub fn module_order(&self) -> &[Id] {
        &self.order
    }

    /// Completes the orderly driver lifecycle: releases the Runtime from the K3
    /// [`DriverExit`], runs the exactly-once composition cleanup with full
    /// registry authority in reverse activation order, and hands the released
    /// runtime back to the host for final release.
    ///
    /// The exit is validated before any hook runs, fiber disposes, or
    /// registration is unregistered: it must carry the owner identity of the
    /// driver this handle is bound to, so a shutdown exit from another
    /// composition (same store type, same plugin identities, same run id) cannot
    /// release resources here. A rejection disposes nothing, leaves this ledger
    /// usable, and returns the exit untouched through
    /// [`CompositionReleaseRejection`].
    ///
    /// The K3 shutdown classification is preserved unchanged. Composition-owned
    /// plugin runtimes are unregistered through the live registry, so a
    /// successful report means no composition-owned plugin registration remains
    /// in the returned runtime.
    ///
    /// # Dropping the returned future
    ///
    /// Acceptance — the owner check above and the ledger's record that this
    /// handle's full-authority sweep has been handed its exit — happens *here*,
    /// when the method is called, not when the returned future is first polled.
    /// Dropping that future is therefore recoverable at every point, including
    /// before its first poll: the ledger stays here with its remaining
    /// resources, its collected failures, and the future of the step that had
    /// already started, and the remaining cleanup continues best-effort through
    /// [`Self::release_after_owner_loss`]. That continuation cannot recover the
    /// `Runtime` the accepted call took with the exit, so it reports each
    /// registration it can no longer unregister instead of pretending the
    /// sweep finished.
    #[must_use = "composition cleanup releases the runtime only through this outcome"]
    pub fn dispose_after_driver<'a, S>(
        &'a mut self,
        exit: DriverExit<S>,
    ) -> CompositionReleaseFuture<'a, S>
    where
        S: DurableStore + Send + 'a,
    {
        // Deliberately not an `async fn`: an async body runs nothing until it
        // is polled, and a caller that drops the future before its first poll
        // (a `select!` whose other branch is already ready) would take the
        // exit — and with it the registry authority — while this ledger still
        // claimed a full-authority sweep could run. The decision and its
        // bookkeeping are synchronous, so they happen at call time.
        if !self.driver_owner.owner().same_driver_as(exit.owner()) {
            return Box::pin(async move {
                CompositionReleaseOutcome::Rejected(
                    CompositionReleaseRejection::ForeignDriverExit {
                        exit: Box::new(exit),
                    },
                )
            });
        }
        if self.ledger.is_none() {
            return Box::pin(async move {
                CompositionReleaseOutcome::Rejected(CompositionReleaseRejection::AlreadyReleased {
                    exit: Box::new(exit),
                })
            });
        }
        if self
            .ledger
            .as_ref()
            .is_some_and(CleanupLedger::orderly_release_accepted)
        {
            return Box::pin(async move {
                CompositionReleaseOutcome::Rejected(
                    CompositionReleaseRejection::OrderlySweepInterrupted,
                )
            });
        }
        self.ledger
            .as_mut()
            .expect("the ledger was present when the exit was accepted")
            .orderly_release_accepted = true;
        Box::pin(self.orderly_sweep(exit))
    }

    /// Runs the accepted full-authority sweep. Reached only through
    /// [`Self::dispose_after_driver`], which has already validated the exit and
    /// recorded the acceptance.
    async fn orderly_sweep<S>(&mut self, exit: DriverExit<S>) -> CompositionReleaseOutcome<S>
    where
        S: DurableStore + Send,
    {
        let shutdown_status = exit.shutdown_status().clone();
        let runtime = exit.into_runtime();
        let rollback = {
            let ledger = self
                .ledger
                .as_mut()
                .expect("the ledger was present when the exit was accepted");
            run_sweep(
                ledger,
                CleanupAuthority::Registry(runtime.capability_registry()),
            )
            .await
        };
        self.ledger = None;
        CompositionReleaseOutcome::Released(Box::new(CompositionDriverShutdown {
            rollback,
            shutdown_status,
            runtime,
        }))
    }

    /// Best-effort release after the driver owner was actually lost.
    ///
    /// The release first proves the loss against this exact driver: the handle
    /// is bound to the [`RuntimeHandle`] separated by the same
    /// [`RuntimeAssembly::into_driver`] call, and only
    /// [`DriverOwnerState::OwnerDropped`] — the state the K3 driver-owner guard
    /// marks when the driver is dropped, aborted, or unwound — lets the sweep
    /// start. While the driver is [`DriverOwnerState::Running`], the release is
    /// rejected with [`OwnerLossReleaseError::OwnerStillRunning`] and nothing is
    /// disposed; a driver that completed an orderly shutdown is rejected with
    /// [`OwnerLossReleaseError::OrderlyShutdownCompleted`] because that path
    /// produced a [`DriverExit`] and must go through
    /// [`Self::dispose_after_driver`] with full registry authority. A rejected
    /// release consumes nothing: the handle stays usable for the orderly path or
    /// for a later, genuine owner-loss release. The one exception is a ledger
    /// whose orderly release was accepted and never completed: there the
    /// registry authority really left with the exit that accepted call took, so
    /// best-effort release is the only path left and this guard accepts it.
    ///
    /// Once the guard passes, the sweep completes the release exactly once. A
    /// completed flow is terminal and answers
    /// [`OwnerLossReleaseError::AlreadyReleased`] whatever the driver reports
    /// afterwards; while work still remains, the guard above reports the driver
    /// state first, so `AlreadyReleased` always means the whole cleanup flow
    /// completed and never that a cancelled sweep dropped the ledger.
    ///
    /// The Runtime dropped with the driver owner, so its capability registry —
    /// and with it the authority to unregister plugin registrations — is already
    /// released. The sweep therefore releases only the process-local handles the
    /// composition still holds: armed `dispose` hooks once and started fibers
    /// once, in reverse activation order, continuing after individual failures.
    /// Every outstanding composition-owned plugin registration is reported as a
    /// [`CleanupResource::PluginRegistration`] failure naming the lost registry
    /// authority, so the report never claims orderly composition cleanup on a
    /// path that could not perform it.
    pub async fn release_after_owner_loss(
        &mut self,
    ) -> Result<RollbackReport, OwnerLossReleaseError> {
        // A completed flow is checked first: once the ledger is gone the answer
        // is terminal and does not depend on what the driver reports now.
        if self.ledger.is_none() {
            return Err(OwnerLossReleaseError::AlreadyReleased);
        }
        match self.driver_owner.owner_state() {
            DriverOwnerState::OwnerDropped => {}
            DriverOwnerState::Running => {
                return Err(OwnerLossReleaseError::OwnerStillRunning);
            }
            DriverOwnerState::Shutdown => {
                let accepted = self
                    .ledger
                    .as_ref()
                    .is_some_and(CleanupLedger::orderly_release_accepted);
                if !accepted {
                    return Err(OwnerLossReleaseError::OrderlyShutdownCompleted);
                }
            }
        }
        let Some(ledger) = self.ledger.as_mut() else {
            return Err(OwnerLossReleaseError::AlreadyReleased);
        };
        let report = run_sweep(ledger, CleanupAuthority::OwnerLost).await;
        self.ledger = None;
        Ok(report)
    }
}

impl fmt::Debug for CompositionHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CompositionHandle")
            .field("order", &self.order)
            .field("released", &self.ledger.is_none())
            .finish_non_exhaustive()
    }
}

/// The future returned by [`CompositionHandle::dispose_after_driver`].
///
/// Boxed for the same reason [`HookFuture`](crate::HookFuture) is: it keeps one
/// executor-neutral type for a release that may complete in a single poll or be
/// dropped at any point. The acceptance it carries out was already recorded
/// when the method was called, so dropping this future never hides the release
/// from [`CompositionHandle::release_after_owner_loss`].
pub type CompositionReleaseFuture<'a, S = InMemoryDurableStore> =
    Pin<Box<dyn Future<Output = CompositionReleaseOutcome<S>> + Send + 'a>>;

/// Outcome of an orderly composition release.
#[must_use = "the released runtime or the untouched DriverExit must be handled"]
pub enum CompositionReleaseOutcome<S = InMemoryDurableStore>
where
    S: DurableStore + Send,
{
    /// The exit came from this handle's own driver, cleanup ran exactly once,
    /// and the runtime is returned with composition cleanup done.
    ///
    /// Boxed so the outcome stays a pointer-sized success arm next to a small
    /// rejection, and so returning it from the async release does not move a
    /// whole runtime per call. [`Self::into_released`] unboxes it, which is the
    /// shape every supported host path uses.
    Released(Box<CompositionDriverShutdown<S>>),
    /// Cleanup was refused before touching any resource; see
    /// [`CompositionReleaseRejection`] for what stays usable.
    Rejected(CompositionReleaseRejection<S>),
}

impl<S> CompositionReleaseOutcome<S>
where
    S: DurableStore + Send,
{
    /// Returns whether the composition released its resources through this
    /// exit.
    #[must_use]
    pub const fn is_released(&self) -> bool {
        matches!(self, Self::Released(_))
    }

    /// Returns the released composition, or the rejection that refused it.
    ///
    /// A host whose exit is known to belong to this handle's own driver uses
    /// this to reach the released runtime directly; the rejection it can return
    /// is the same typed one the outcome carries.
    #[must_use = "the released composition or the reason it was refused must be handled"]
    pub fn into_released(
        self,
    ) -> Result<CompositionDriverShutdown<S>, CompositionReleaseRejection<S>> {
        match self {
            Self::Released(shutdown) => Ok(*shutdown),
            Self::Rejected(rejection) => Err(rejection),
        }
    }
}

impl<S> fmt::Debug for CompositionReleaseOutcome<S>
where
    S: DurableStore + Send,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Released(shutdown) => formatter.debug_tuple("Released").field(shutdown).finish(),
            Self::Rejected(rejection) => {
                formatter.debug_tuple("Rejected").field(rejection).finish()
            }
        }
    }
}

/// Why an orderly composition release was refused, with resources preserved.
///
/// Every rejection is inert: no hook ran, no fiber was disposed, no
/// registration was unregistered, and the handle stays usable for the correct
/// exit or for [`CompositionHandle::release_after_owner_loss`].
#[must_use = "a refused DriverExit must be returned to the composition that owns it"]
pub enum CompositionReleaseRejection<S = InMemoryDurableStore>
where
    S: DurableStore + Send,
{
    /// The exit was produced by a different driver owner than the one this
    /// handle is bound to. The exit is returned untouched so the composition it
    /// does belong to can still release through its own orderly path.
    ///
    /// Boxed for the same reason [`StartupFailure::cause`](crate::StartupFailure)
    /// is: an exit carries the whole runtime, and a rejection travels by value,
    /// so the refusal path must not copy it. [`Self::into_exit`] hands back the
    /// owned exit either way.
    ForeignDriverExit {
        /// The untouched exit, so its owning composition can still use it.
        exit: Box<DriverExit<S>>,
    },
    /// This handle completed its cleanup exactly once, so nothing remains to
    /// release. The exit is returned untouched.
    ///
    /// Defensive: one driver mints exactly one [`DriverExit`], so a host that
    /// has already released cannot legitimately hold a matching second exit.
    /// The arm exists so a released handle can never clean twice, whatever it
    /// is handed.
    AlreadyReleased {
        /// The untouched exit.
        exit: Box<DriverExit<S>>,
    },
    /// An orderly release was accepted for this handle and never completed, so
    /// the registry authority it cleaned through left with the exit and no
    /// sweep with full authority can run here again. Only
    /// [`CompositionHandle::release_after_owner_loss`] can finish the remaining
    /// best-effort cleanup. No exit is carried: acceptance consumed the one it
    /// was given, which is why this rejection cannot hand one back.
    ///
    /// Defensive in the same way as [`Self::AlreadyReleased`]: the accepted
    /// exit cannot be presented twice, so a host reaches best-effort release
    /// directly rather than through this rejection.
    OrderlySweepInterrupted,
}

impl<S> CompositionReleaseRejection<S>
where
    S: DurableStore + Send,
{
    /// Returns the untouched exit, if this rejection preserved one.
    #[must_use = "the returned DriverExit is the only authority for its own composition"]
    pub fn into_exit(self) -> Option<DriverExit<S>> {
        match self {
            Self::ForeignDriverExit { exit } | Self::AlreadyReleased { exit } => Some(*exit),
            Self::OrderlySweepInterrupted => None,
        }
    }
}

impl<S> fmt::Debug for CompositionReleaseRejection<S>
where
    S: DurableStore + Send,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ForeignDriverExit { .. } => formatter
                .debug_struct("ForeignDriverExit")
                .finish_non_exhaustive(),
            Self::AlreadyReleased { .. } => formatter
                .debug_struct("AlreadyReleased")
                .finish_non_exhaustive(),
            Self::OrderlySweepInterrupted => {
                formatter.debug_struct("OrderlySweepInterrupted").finish()
            }
        }
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

//! R2-03 regression: an interrupted composition cleanup loses neither the
//! outstanding resources nor the facts already recorded.
//!
//! Every case cancels a release at a real `Pending` await — a started `dispose`
//! hook or a started fiber disposal — and then re-enters:
//!
//! * the not-yet-started modules are still cleaned;
//! * a step whose future already started is resumed, never started again, so a
//!   non-idempotent hook runs exactly once;
//! * failures recorded before the cancellation are still in the report;
//! * `AlreadyReleased` appears only after the whole flow completed, never as a
//!   way of quietly forgetting an unfinished ledger;
//! * an interrupted orderly sweep, whose registry authority went with the
//!   cancelled future, continues best-effort and reports the registrations it
//!   can no longer unregister instead of claiming orderly completion.

mod k4_common;

use k4_common::{
    Recorder, SuccessDispatcher, id, plugin_factory, run_id, service_definition, value,
};
use runtime_composition::{
    CleanupResource, CompositionBuilder, CompositionHandle, DriverOwnerState, HostConfig,
    LifecycleHook, ModuleDefinition, ModuleRegistration, OwnerLossReleaseError, PluginDefinition,
    PluginRuntime, RuntimeHandle, ScopedEffect, lifecycle_hook,
};
use std::future::poll_fn;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::task::{Context, Poll, Waker};

/// A one-shot gate: the waiting step is genuinely pending until `release`.
#[derive(Default)]
struct Gate {
    opened: AtomicBool,
    waker: Mutex<Option<Waker>>,
}

impl Gate {
    fn release(&self) {
        self.opened.store(true, Ordering::SeqCst);
        if let Some(waker) = self.waker.lock().expect("gate lock is healthy").take() {
            waker.wake();
        }
    }

    async fn wait(&self) {
        poll_fn(|context| {
            if self.opened.load(Ordering::SeqCst) {
                Poll::Ready(())
            } else {
                *self.waker.lock().expect("gate lock is healthy") = Some(context.waker().clone());
                Poll::Pending
            }
        })
        .await;
    }
}

/// A hook that counts its own starts, then waits on `gate` until it is open.
fn counted_hook(counter: &Arc<AtomicUsize>, gate: &Arc<Gate>) -> LifecycleHook {
    let counter = Arc::clone(counter);
    let gate = Arc::clone(gate);
    lifecycle_hook(move || {
        let counter = Arc::clone(&counter);
        let gate = Arc::clone(&gate);
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
            gate.wait().await;
            Ok(())
        }
    })
}

/// A plugin whose owned effect has an asynchronous disposer that counts its own
/// starts and waits on `gate`.
fn gated_plugin(gate: &Arc<Gate>, disposals: &Arc<AtomicUsize>) -> Arc<PluginRuntime> {
    let gate = Arc::clone(gate);
    let disposals = Arc::clone(disposals);
    PluginRuntime::new(PluginDefinition::new(
        id("gated-plugin"),
        service_definition("gated", "gated-v1"),
        plugin_factory(move |context| {
            let gate = Arc::clone(&gate);
            let disposals = Arc::clone(&disposals);
            async move {
                context
                    .effect(ScopedEffect::asynchronous("gated-disposal", move || {
                        disposals.fetch_add(1, Ordering::SeqCst);
                        let gate = Arc::clone(&gate);
                        async move {
                            gate.wait().await;
                            Ok(())
                        }
                    }))
                    .expect("the fiber accepts the gated cleanup effect");
                Ok(value("published"))
            }
        }),
    ))
}

/// Polls a release future once and asserts it is still pending, which is the
/// cancellation point the review named.
fn poll_once_pending<F: ?Sized + std::future::Future>(
    future: &mut std::pin::Pin<Box<F>>,
    what: &str,
) {
    let mut context = Context::from_waker(Waker::noop());
    assert!(
        matches!(future.as_mut().poll(&mut context), Poll::Pending),
        "{what} must be pending at the cancellation point"
    );
}

/// Separates a composition into a dropped driver (owner loss) plus its handle.
async fn owner_lost(
    plan: runtime_composition::CompositionPlan,
    run: &str,
) -> (RuntimeHandle, CompositionHandle) {
    let assembly = plan
        .start(run_id(run), &HostConfig::new())
        .await
        .expect("the composition activates");
    let (driver, handle, composition) = assembly.into_driver(SuccessDispatcher::new());
    drop(driver);
    assert_eq!(
        handle.owner_state(),
        DriverOwnerState::OwnerDropped,
        "dropping the driver releases the runtime with its owner"
    );
    (handle, composition)
}

/// Cancelling at a pending `dispose` hook keeps the untouched module's cleanup
/// and the started step, and only the completed flow ends in `AlreadyReleased`.
#[tokio::test(flavor = "current_thread")]
async fn cancelling_a_pending_dispose_hook_never_forgets_the_ledger() {
    let recorder = Recorder::new();
    let gate = Arc::new(Gate::default());
    let blocked_starts = Arc::new(AtomicUsize::new(0));
    let plan = CompositionBuilder::new()
        .register(
            ModuleRegistration::new(ModuleDefinition::new(id("audit")))
                .on_activate(recorder.hook("activate:audit", None))
                .on_dispose(recorder.hook("dispose:audit", None)),
        )
        .expect("the audit module registers")
        .register(
            ModuleRegistration::new(ModuleDefinition::new(id("blocked")).depends_on(id("audit")))
                .on_activate(recorder.hook("activate:blocked", None))
                .on_dispose(counted_hook(&blocked_starts, &gate)),
        )
        .expect("the blocked module registers")
        .build()
        .expect("the composition validates");
    let (_handle, mut composition) = owner_lost(plan, "r2-cancel-hook").await;

    let mut interrupted = Box::pin(composition.release_after_owner_loss());
    poll_once_pending(&mut interrupted, "the pending dispose hook");
    assert_eq!(
        blocked_starts.load(Ordering::SeqCst),
        1,
        "the blocked hook started once"
    );
    assert_eq!(
        recorder.count("dispose:audit"),
        0,
        "the untouched module has not been cleaned yet"
    );
    drop(interrupted);

    let mut resumed = Box::pin(composition.release_after_owner_loss());
    poll_once_pending(&mut resumed, "the resumed dispose hook");
    assert_eq!(
        blocked_starts.load(Ordering::SeqCst),
        1,
        "re-entering resumes the started hook instead of starting it again"
    );
    drop(resumed);

    gate.release();
    let report = composition
        .release_after_owner_loss()
        .await
        .expect("the cancelled flow is still resumable, not AlreadyReleased");
    assert_eq!(
        blocked_starts.load(Ordering::SeqCst),
        1,
        "the non-idempotent hook ran exactly once across two cancellations"
    );
    assert_eq!(recorder.count("dispose:audit"), 1);
    assert_eq!(report.cleaned, vec![id("blocked"), id("audit")]);
    assert!(report.failures.is_empty());
    assert_eq!(
        composition
            .release_after_owner_loss()
            .await
            .expect_err("a completed flow reports AlreadyReleased"),
        OwnerLossReleaseError::AlreadyReleased,
        "AlreadyReleased only follows a completed cleanup flow"
    );
}

/// Cancelling at a pending fiber disposal keeps the disposal single-shot and
/// keeps the failure recorded before the cancellation.
#[tokio::test(flavor = "current_thread")]
async fn cancelling_a_pending_fiber_disposal_keeps_the_step_and_the_failures() {
    let recorder = Recorder::new();
    let gate = Arc::new(Gate::default());
    let disposals = Arc::new(AtomicUsize::new(0));
    let plugin = gated_plugin(&gate, &disposals);
    let plan = CompositionBuilder::new()
        .register(
            ModuleRegistration::new(ModuleDefinition::new(id("audit")))
                .on_activate(recorder.hook("activate:audit", None))
                .on_dispose(recorder.hook("dispose:audit", Some("audit cleanup failed"))),
        )
        .expect("the failing audit module registers")
        .register(
            ModuleRegistration::new(
                ModuleDefinition::new(id("gated"))
                    .depends_on(id("audit"))
                    .with_reactive_capability(id("gated"), "service", "gated-v1"),
            )
            .plugin(Arc::clone(&plugin))
            .on_activate(recorder.hook("activate:gated", None)),
        )
        .expect("the gated module registers")
        .build()
        .expect("the composition validates");
    let (_handle, mut composition) = owner_lost(plan, "r2-cancel-fiber").await;

    let mut interrupted = Box::pin(composition.release_after_owner_loss());
    poll_once_pending(&mut interrupted, "the pending fiber disposal");
    assert_eq!(disposals.load(Ordering::SeqCst), 1, "the disposer started");
    assert_eq!(
        recorder.count("dispose:audit"),
        0,
        "reverse order has not reached the audit module yet"
    );
    drop(interrupted);

    let mut resumed = Box::pin(composition.release_after_owner_loss());
    poll_once_pending(&mut resumed, "the resumed fiber disposal");
    assert_eq!(
        disposals.load(Ordering::SeqCst),
        1,
        "re-entering resumes the started disposal instead of disposing twice"
    );
    drop(resumed);

    gate.release();
    let report = composition
        .release_after_owner_loss()
        .await
        .expect("the cancelled flow is still resumable");
    assert_eq!(
        disposals.load(Ordering::SeqCst),
        1,
        "the fiber disposer ran exactly once across two cancellations"
    );
    assert_eq!(recorder.count("dispose:audit"), 1);
    assert!(
        report.cleaned.is_empty(),
        "a module whose registration could not be unregistered is not reported clean"
    );
    assert_eq!(report.failures.len(), 2, "both failures are still recorded");
    assert!(
        report
            .failures
            .iter()
            .any(|failure| failure.module_id == id("audit")
                && failure.resource == CleanupResource::DisposeHook
                && failure.reason == "audit cleanup failed"),
        "the failure recorded before the cancellation survived the ledger"
    );
    assert!(
        report
            .failures
            .iter()
            .any(|failure| failure.module_id == id("gated")
                && matches!(failure.resource, CleanupResource::PluginRegistration { .. })),
        "the owner-loss path reports the registration it can no longer unregister"
    );
    assert_eq!(
        composition
            .release_after_owner_loss()
            .await
            .expect_err("a completed flow reports AlreadyReleased"),
        OwnerLossReleaseError::AlreadyReleased
    );
}

/// An orderly sweep cancelled mid-flight loses its registry authority with the
/// cancelled future; the remaining cleanup continues best-effort and says so.
#[tokio::test(flavor = "current_thread")]
async fn an_interrupted_orderly_sweep_continues_best_effort_and_says_so() {
    let recorder = Recorder::new();
    let gate = Arc::new(Gate::default());
    let hook_starts = Arc::new(AtomicUsize::new(0));
    let disposals = Arc::new(AtomicUsize::new(0));
    let plugin = gated_plugin(&gate, &disposals);
    let plan = CompositionBuilder::new()
        .register(
            ModuleRegistration::new(ModuleDefinition::new(id("gated")).with_reactive_capability(
                id("gated"),
                "service",
                "gated-v1",
            ))
            .plugin(Arc::clone(&plugin))
            .on_activate(recorder.hook("activate:gated", None))
            .on_dispose(counted_hook(&hook_starts, &gate)),
        )
        .expect("the gated module registers")
        .build()
        .expect("the composition validates");
    let assembly = plan
        .start(run_id("r2-cancel-orderly"), &HostConfig::new())
        .await
        .expect("the composition activates");
    let (driver, handle, mut composition) = assembly.into_driver(SuccessDispatcher::new());
    let driver_task = tokio::spawn(async move { driver.run().await });
    handle
        .shutdown()
        .await
        .expect("the driver shuts down cleanly");
    let exit = driver_task.await.expect("the driver task joins");
    assert_eq!(handle.owner_state(), DriverOwnerState::Shutdown);

    let mut interrupted = Box::pin(composition.dispose_after_driver(exit));
    poll_once_pending(&mut interrupted, "the orderly sweep at its pending hook");
    assert_eq!(hook_starts.load(Ordering::SeqCst), 1);
    drop(interrupted);

    // The cancelled sweep consumed the only DriverExit and dropped the Runtime
    // it was cleaning through, so full-authority release is no longer possible
    // and best-effort release is the honest continuation.
    gate.release();
    let report = composition
        .release_after_owner_loss()
        .await
        .expect("an interrupted orderly sweep continues best-effort");
    assert_eq!(
        hook_starts.load(Ordering::SeqCst),
        1,
        "the hook already started is resumed, not restarted"
    );
    assert_eq!(
        disposals.load(Ordering::SeqCst),
        1,
        "the started fiber disposal completed exactly once"
    );
    assert!(
        !report.is_success(),
        "an interrupted sweep never reports orderly completion"
    );
    assert!(
        report.cleaned.is_empty(),
        "the module is not clean while its registration is unreachable"
    );
    assert_eq!(report.failures.len(), 1);
    assert!(matches!(
        report.failures[0].resource,
        CleanupResource::PluginRegistration { .. }
    ));
    assert_eq!(
        report.failures[0].module_id,
        id("gated"),
        "the module owning the unreachable registration is named"
    );
    assert_eq!(
        composition
            .release_after_owner_loss()
            .await
            .expect_err("a completed flow reports AlreadyReleased"),
        OwnerLossReleaseError::AlreadyReleased,
        "the resumed flow ended exactly once"
    );
}

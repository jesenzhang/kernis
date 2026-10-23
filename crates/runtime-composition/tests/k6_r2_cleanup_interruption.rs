//! R2-03 regression: an interrupted composition cleanup loses neither the
//! outstanding resources nor the facts already recorded.
//!
//! Every case cancels a release and then re-enters: by dropping the future
//! before it is ever polled, or at a real `Pending` await — a started `dispose`
//! hook or a started fiber disposal:
//!
//! * the not-yet-started modules are still cleaned;
//! * a step whose future already started is resumed, never started again, so a
//!   non-idempotent hook runs exactly once;
//! * failures recorded before the cancellation are still in the report;
//! * `AlreadyReleased` appears only after the whole flow completed, never as a
//!   way of quietly forgetting an unfinished ledger;
//! * an interrupted orderly sweep, whose registry authority went with the exit
//!   the accepted call took, continues best-effort and reports the registrations
//!   it can no longer unregister instead of claiming orderly completion — and
//!   that holds even when the release future is dropped before its first poll,
//!   because acceptance is recorded when `dispose_after_driver` is called.

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
/// keeps the failure recorded *before* the cancellation.
///
/// The `noisy` module is activated last, so it is cleaned first, and its armed
/// `dispose` hook fails immediately: by the time the cancellation point (the
/// `gated` module's fiber disposal) is pending, the ledger already holds one
/// recorded failure. Without that ordering the assertion below would hold for an
/// implementation that dropped the accumulated report on every cancellation.
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
        .register(
            ModuleRegistration::new(ModuleDefinition::new(id("noisy")).depends_on(id("gated")))
                .on_activate(recorder.hook("activate:noisy", None))
                .on_dispose(recorder.hook("dispose:noisy", Some("noisy cleanup failed"))),
        )
        .expect("the noisy module registers")
        .build()
        .expect("the composition validates");
    let (_handle, mut composition) = owner_lost(plan, "r2-cancel-fiber").await;

    let mut interrupted = Box::pin(composition.release_after_owner_loss());
    poll_once_pending(&mut interrupted, "the pending fiber disposal");
    assert_eq!(disposals.load(Ordering::SeqCst), 1, "the disposer started");
    assert_eq!(
        recorder.count("dispose:noisy"),
        1,
        "the module cleaned before the blocked step was already cleaned, and failed"
    );
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
    assert_eq!(
        recorder.count("dispose:noisy"),
        1,
        "the already-cleaned module is not cleaned a second time"
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
    assert_eq!(report.failures.len(), 3, "all three failures are recorded");
    assert!(
        report
            .failures
            .first()
            .is_some_and(|failure| failure.module_id == id("noisy")
                && failure.resource == CleanupResource::DisposeHook
                && failure.reason == "noisy cleanup failed"),
        "the failure recorded before the cancellation survived both drops, in order"
    );
    assert!(
        report.failures[1].module_id == id("gated")
            && matches!(
                report.failures[1].resource,
                CleanupResource::PluginRegistration { .. }
            ),
        "the owner-loss path reports the registration it can no longer unregister"
    );
    assert!(
        report.failures[2].module_id == id("audit")
            && report.failures[2].resource == CleanupResource::DisposeHook
            && report.failures[2].reason == "audit cleanup failed",
        "the module reached after the cancellation is cleaned and reported too"
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

/// The strictest form of cancelling the **orderly** path: dropping the release
/// future before it is ever polled. Acceptance is recorded when
/// `dispose_after_driver` is called — not when its future is first polled — so
/// the ledger already knows its full-authority sweep can no longer be promised,
/// and the remaining cleanup stays reachable best-effort instead of being
/// stranded behind an `OrderlyShutdownCompleted` answer that points at an exit
/// the dropped future already consumed.
#[tokio::test(flavor = "current_thread")]
async fn dropping_the_orderly_release_before_its_first_poll_stays_recoverable() {
    let recorder = Recorder::new();
    let plan = CompositionBuilder::new()
        .register(
            ModuleRegistration::new(ModuleDefinition::new(id("solo")))
                .on_activate(recorder.hook("activate:solo", None))
                .on_dispose(recorder.hook("dispose:solo", None)),
        )
        .expect("the module registers")
        .build()
        .expect("the composition validates");
    let assembly = plan
        .start(run_id("r2-cancel-orderly-before-poll"), &HostConfig::new())
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

    drop(Box::pin(composition.dispose_after_driver(exit)));
    assert_eq!(
        recorder.count("dispose:solo"),
        0,
        "a release future that was never polled started no dispose hook"
    );

    // The accepted call already took the exit, so its registry authority is
    // gone: what remains must still be reachable, and must say so.
    let report = composition
        .release_after_owner_loss()
        .await
        .expect("a release dropped before its first poll is still recoverable");
    assert_eq!(
        recorder.count("dispose:solo"),
        1,
        "the armed hook is cleaned exactly once, after the drop"
    );
    assert_eq!(
        report.cleaned,
        vec![id("solo")],
        "the module the dropped future never reached is cleaned after all"
    );
    assert!(
        report.failures.is_empty(),
        "this module owned no registration, so nothing needed the lost authority"
    );
    assert_eq!(
        composition
            .release_after_owner_loss()
            .await
            .expect_err("the continuation ended exactly once"),
        OwnerLossReleaseError::AlreadyReleased
    );
}

/// Dropping a release future before it is ever polled is the strictest form of
/// "cancelled before the first await": nothing has started and nothing has been
/// popped, so re-entry must still perform the whole cleanup and only a completed
/// flow may end in `AlreadyReleased`.
#[tokio::test(flavor = "current_thread")]
async fn dropping_a_release_future_before_its_first_poll_starts_nothing() {
    let recorder = Recorder::new();
    let plan = CompositionBuilder::new()
        .register(
            ModuleRegistration::new(ModuleDefinition::new(id("audit")))
                .on_activate(recorder.hook("activate:audit", None))
                .on_dispose(recorder.hook("dispose:audit", None)),
        )
        .expect("the audit module registers")
        .register(
            ModuleRegistration::new(ModuleDefinition::new(id("ledger")).depends_on(id("audit")))
                .on_activate(recorder.hook("activate:ledger", None))
                .on_dispose(recorder.hook("dispose:ledger", None)),
        )
        .expect("the ledger module registers")
        .build()
        .expect("the composition validates");
    let (_handle, mut composition) = owner_lost(plan, "r2-cancel-before-poll").await;

    drop(Box::pin(composition.release_after_owner_loss()));
    assert_eq!(
        recorder.count("dispose:ledger"),
        0,
        "a release future that was never polled started no dispose hook"
    );
    assert_eq!(
        recorder.count("dispose:audit"),
        0,
        "a release future that was never polled cleaned nothing"
    );

    let report = composition
        .release_after_owner_loss()
        .await
        .expect("the release still runs after a pre-poll drop");
    assert!(
        report.is_success(),
        "re-entry performs the whole cleanup, failures: {:?}",
        report.failures
    );
    assert_eq!(
        recorder.count("dispose:ledger"),
        1,
        "the module whose release was dropped first is still cleaned exactly once"
    );
    assert_eq!(
        recorder.count("dispose:audit"),
        1,
        "the remaining module is cleaned exactly once"
    );
    assert_eq!(
        composition
            .release_after_owner_loss()
            .await
            .expect_err("a completed flow is terminal"),
        OwnerLossReleaseError::AlreadyReleased,
        "only a completed flow clears the ledger"
    );
}

/// B3: a `dispose` hook whose *construction* panics keeps its step in the
/// ledger. Losing it there would let the next attempt skip the hook and report
/// the module clean; with the step retained, the retry reaches the same host
/// code again and still never produces a "clean" report.
#[tokio::test(flavor = "current_thread")]
async fn a_panicking_hook_constructor_is_retried_never_reported_clean() {
    let starts = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&starts);
    let hook = lifecycle_hook(move || -> runtime_composition::HookFuture {
        counter.fetch_add(1, Ordering::SeqCst);
        panic!("the dispose hook constructor panicked")
    });
    let plan = CompositionBuilder::new()
        .register(
            ModuleRegistration::new(ModuleDefinition::new(id("panicky")))
                .on_activate(lifecycle_hook(|| async { Ok(()) }))
                .on_dispose(hook),
        )
        .expect("the panicking module registers")
        .build()
        .expect("the composition validates");
    let assembly = plan
        .start(run_id("b3-panicky"), &HostConfig::new())
        .await
        .expect("the composition activates");
    let (driver, handle, mut composition) = assembly.into_driver(SuccessDispatcher::new());
    let driver_task = tokio::spawn(async move { driver.run().await });
    handle
        .shutdown()
        .await
        .expect("the driver shuts down cleanly");
    let exit = driver_task.await.expect("the driver task joins");

    let mut release = Box::pin(composition.dispose_after_driver(exit));
    let first = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut context = Context::from_waker(Waker::noop());
        let _ = release.as_mut().poll(&mut context);
    }));
    assert!(first.is_err(), "the panicking hook construction propagates");
    assert_eq!(starts.load(Ordering::SeqCst), 1);
    drop(release);

    // The step survived the panic: the continuation reaches the same host
    // code again instead of returning a report that calls the module clean.
    let mut retry = Box::pin(composition.release_after_owner_loss());
    let second = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut context = Context::from_waker(Waker::noop());
        let _ = retry.as_mut().poll(&mut context);
    }));
    assert!(
        second.is_err(),
        "a panicked step is retried (and panics again), never reported as completed"
    );
    assert_eq!(
        starts.load(Ordering::SeqCst),
        2,
        "the ledger kept the armed hook across the panic"
    );
}

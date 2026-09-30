#![allow(missing_docs)]
//! Review R2 finding proofs (repair regressions).
//!
//! A [`DriverExit`](runtime_composition::DriverExit) authorizes only the
//! composition whose own driver produced it. Three crossings are refused with a
//! typed rejection, zero disposal, and the exit handed back intact: a live
//! driver (A `Running`), a second driver that also shut down cleanly (both
//! `Shutdown`), and a driver whose owner was lost. A and B deliberately share a
//! run id, module id, plugin id, capability kind, and replay identity, so
//! nothing but the driver identity distinguishes them, and the refused path
//! removes no registry entry and starts no hook or fiber disposer on either
//! side. The cancellation proofs for the same repair live in
//! `k6_r2_cleanup_interruption.rs`.

mod k4_common;

use k4_common::{
    Recorder, SuccessDispatcher, id, plugin_factory, run_id, service_definition, value,
};
use runtime_composition::{
    CleanupResource, CompositionBuilder, CompositionHandle, CompositionReleaseOutcome,
    CompositionReleaseRejection, DriverExit, DriverOwnerState, HostConfig, InMemoryDurableStore,
    ModuleDefinition, ModuleRegistration, PluginDefinition, PluginRuntime, ScopedEffect,
    lifecycle_hook,
};
use std::future::poll_fn;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::task::{Poll, Waker};
use tokio::task::JoinHandle;

/// A one-shot gate whose async step waits until opened, so a test can start a
/// cleanup step and know it is genuinely pending.
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

/// A plugin whose owned effect has an asynchronous disposer that counts its own
/// start and then waits on `gate`, so fiber disposal is observable mid-flight.
fn gated_plugin(gate: Arc<Gate>, disposals: Arc<AtomicUsize>) -> Arc<PluginRuntime> {
    PluginRuntime::new(PluginDefinition::new(
        id("shared-plugin"),
        service_definition("shared", "shared-v1"),
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

type Store = InMemoryDurableStore;

/// One composition under test, its driver task, and the counters that make
/// cleanup observable. Every fixture is built the same way, so two of them are
/// indistinguishable except by the driver instance behind them.
struct Fixture {
    handle: runtime_composition::RuntimeHandle,
    join: Option<JoinHandle<DriverExit<Store>>>,
    composition: CompositionHandle,
    gate: Arc<Gate>,
    hook_starts: Arc<AtomicUsize>,
    disposals: Arc<AtomicUsize>,
}

impl Fixture {
    /// Orderly K3 shutdown, returning this driver's own exit.
    async fn orderly_exit(&mut self) -> DriverExit<Store> {
        self.handle
            .shutdown()
            .await
            .expect("the driver shuts down cleanly");
        self.join
            .take()
            .expect("each fixture joins its driver once")
            .await
            .expect("the driver task joins")
    }

    /// Abnormal owner loss: the driver task is aborted, so no exit exists.
    async fn lose_owner(&mut self) {
        let join = self.join.take().expect("each fixture loses its owner once");
        join.abort();
        match join.await {
            Err(error) => assert!(error.is_cancelled(), "the driver task was aborted"),
            Ok(exit) => {
                drop(exit);
                panic!("an aborted driver task must not produce a DriverExit");
            }
        }
        assert_eq!(
            self.handle.owner_state(),
            DriverOwnerState::OwnerDropped,
            "the bound driver observation reports the lost owner"
        );
    }

    /// Asserts that this composition is untouched, and returns the refused exit.
    ///
    /// The exit is recovered through the public
    /// [`CompositionReleaseRejection::into_exit`] accessor, so the hand-back
    /// path the host relies on is what this file proves.
    fn refused_foreign(&self, outcome: CompositionReleaseOutcome<Store>) -> DriverExit<Store> {
        let rejection = match outcome.into_released() {
            Err(rejection) => rejection,
            Ok(shutdown) => {
                panic!("a foreign DriverExit must never release this composition: {shutdown:?}")
            }
        };
        assert!(
            matches!(
                rejection,
                CompositionReleaseRejection::ForeignDriverExit { .. }
            ),
            "a foreign DriverExit must be refused as ForeignDriverExit, got {rejection:?}"
        );
        let exit = rejection
            .into_exit()
            .expect("a refused release hands the exit back untouched");
        assert_eq!(
            self.hook_starts.load(Ordering::SeqCst),
            0,
            "a foreign exit must not start a dispose hook"
        );
        assert_eq!(
            self.disposals.load(Ordering::SeqCst),
            0,
            "a foreign exit must not dispose a fiber"
        );
        assert!(
            exit.runtime()
                .capability_registry()
                .contains(&id("shared-plugin")),
            "the refused path removed no plugin registration from the exit's registry"
        );
        exit
    }

    /// Releases through this fixture's own exit and asserts full cleanup.
    async fn release_with_own_exit(&mut self, exit: DriverExit<Store>) {
        self.gate.release();
        let released = self
            .composition
            .dispose_after_driver(exit)
            .await
            .into_released()
            .expect("the exit from the composition's own driver releases it");
        assert!(
            released.rollback.is_success(),
            "the owning composition cleans with full registry authority"
        );
        assert!(
            !released
                .runtime
                .capability_registry()
                .contains(&id("shared-plugin")),
            "the owning composition removed its own registration"
        );
        assert_eq!(
            self.hook_starts.load(Ordering::SeqCst),
            1,
            "the correct source runs its dispose hook exactly once"
        );
        assert_eq!(
            self.disposals.load(Ordering::SeqCst),
            1,
            "the correct source disposes its fiber exactly once"
        );
    }
}

/// Activates one composition and hands its runtime to a spawned driver.
async fn fixture(run: &str) -> Fixture {
    let recorder = Recorder::new();
    let gate = Arc::new(Gate::default());
    let hook_starts = Arc::new(AtomicUsize::new(0));
    let disposals = Arc::new(AtomicUsize::new(0));
    let plugin = gated_plugin(Arc::clone(&gate), Arc::clone(&disposals));
    let starts = Arc::clone(&hook_starts);
    let waiting_gate = Arc::clone(&gate);
    let blocked_dispose = lifecycle_hook(move || {
        let starts = Arc::clone(&starts);
        let waiting_gate = Arc::clone(&waiting_gate);
        async move {
            starts.fetch_add(1, Ordering::SeqCst);
            waiting_gate.wait().await;
            Ok(())
        }
    });
    let plan = CompositionBuilder::new()
        .register(
            ModuleRegistration::new(
                ModuleDefinition::new(id("owner-module")).with_reactive_capability(
                    id("shared"),
                    "service",
                    "shared-v1",
                ),
            )
            .plugin(Arc::clone(&plugin))
            .on_activate(recorder.hook("activate:owner", None))
            .on_dispose(blocked_dispose),
        )
        .expect("the module registers")
        .build()
        .expect("the composition validates");
    let assembly = plan
        .start(run_id(run), &HostConfig::new())
        .await
        .expect("the composition activates");
    assert!(
        assembly
            .runtime()
            .capability_registry()
            .contains(&id("shared-plugin")),
        "activation registered the shared plugin identity"
    );
    let (driver, handle, composition) = assembly.into_driver(SuccessDispatcher::new());
    let join = tokio::spawn(driver.run());
    Fixture {
        handle,
        join: Some(join),
        composition,
        gate,
        hook_starts,
        disposals,
    }
}

/// A `Running` + B exit: a live driver's composition cannot be cleaned at all.
#[tokio::test(flavor = "current_thread")]
async fn review_foreign_exit_must_not_dispose_live_composition() {
    let mut a = fixture("cross-owner").await;
    let mut b = fixture("cross-owner").await;

    let exit_b = b.orderly_exit().await;
    assert_eq!(
        a.handle.owner_state(),
        DriverOwnerState::Running,
        "A's driver still owns and serves its runtime"
    );

    let refused = a.composition.dispose_after_driver(exit_b).await;
    let exit_b = a.refused_foreign(refused);
    a.handle
        .drive()
        .await
        .expect("the refused release left A's driver serving commands");

    b.release_with_own_exit(exit_b).await;
    let exit_a = a.orderly_exit().await;
    a.release_with_own_exit(exit_a).await;
}

/// A `Shutdown` + B exit, with the same run id and an identical composition
/// shape: lifecycle state and shape cannot substitute for driver identity.
#[tokio::test(flavor = "current_thread")]
async fn review_a_shut_down_exit_from_another_instance_is_still_foreign() {
    let mut a = fixture("twin-instances").await;
    let mut b = fixture("twin-instances").await;
    let exit_b = b.orderly_exit().await;
    let exit_a = a.orderly_exit().await;
    assert_eq!(
        a.handle.owner_state(),
        DriverOwnerState::Shutdown,
        "both drivers are in the same lifecycle state"
    );
    assert_eq!(b.handle.owner_state(), DriverOwnerState::Shutdown);

    let refused = a.composition.dispose_after_driver(exit_b).await;
    let exit_b = a.refused_foreign(refused);
    b.release_with_own_exit(exit_b).await;
    a.release_with_own_exit(exit_a).await;
}

/// A `OwnerDropped` + B exit: another driver's exit is not a substitute for the
/// missing authority, and A's own best-effort path still works afterwards.
#[tokio::test(flavor = "current_thread")]
async fn review_a_lost_owner_does_not_accept_another_drivers_exit() {
    let mut a = fixture("owner-loss-cross").await;
    let mut b = fixture("owner-loss-cross").await;
    a.lose_owner().await;
    let exit_b = b.orderly_exit().await;

    let refused = a.composition.dispose_after_driver(exit_b).await;
    let exit_b = a.refused_foreign(refused);
    b.release_with_own_exit(exit_b).await;

    a.gate.release();
    let report = a
        .composition
        .release_after_owner_loss()
        .await
        .expect("A's own owner loss still releases best-effort");
    assert_eq!(a.hook_starts.load(Ordering::SeqCst), 1);
    assert_eq!(a.disposals.load(Ordering::SeqCst), 1);
    assert!(
        !report.is_success(),
        "the lost registry authority is reported, not hidden"
    );
    assert!(
        report.cleaned.is_empty(),
        "the module is not clean while its registration could not be unregistered"
    );
    assert_eq!(report.failures.len(), 1);
    assert!(matches!(
        report.failures[0].resource,
        CleanupResource::PluginRegistration { .. }
    ));
}

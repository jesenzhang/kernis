#![allow(missing_docs)]

mod k4_common;

use k4_common::{Recorder, SuccessDispatcher, id, run_id};
use runtime_composition::{
    CompositionBuilder, DriverOwnerState, HostConfig, ModuleDefinition, ModuleRegistration,
    OwnerLossReleaseError, lifecycle_hook,
};
use std::future::{Future, pending};
use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
use std::task::{Context, Poll, Waker};

#[tokio::test]
async fn review_foreign_exit_must_not_dispose_live_composition() {
    let recorder = Recorder::new();
    let plan_a = CompositionBuilder::new()
        .register(ModuleRegistration::new(ModuleDefinition::new(id("a")))
            .on_activate(recorder.hook("activate:a", None))
            .on_dispose(recorder.hook("dispose:a", None)))
        .unwrap().build().unwrap();
    let plan_b = CompositionBuilder::new()
        .register(ModuleRegistration::new(ModuleDefinition::new(id("b"))))
        .unwrap().build().unwrap();
    let a = plan_a.start(run_id("review-a"), &HostConfig::new()).await.unwrap();
    let b = plan_b.start(run_id("review-b"), &HostConfig::new()).await.unwrap();
    let (driver_a, handle_a, composition_a) = a.into_driver(SuccessDispatcher::new());
    let (driver_b, handle_b, composition_b) = b.into_driver(SuccessDispatcher::new());
    let join_a = tokio::spawn(driver_a.run());
    let join_b = tokio::spawn(driver_b.run());
    handle_b.shutdown().await.unwrap();
    let exit_b = join_b.await.unwrap();
    assert_eq!(handle_a.owner_state(), DriverOwnerState::Running);
    let outcome = composition_a.dispose_after_driver(exit_b).await;
    let disposed_while_running = recorder.count("dispose:a");
    eprintln!("foreign exit: A state={:?}, A disposed={}, cleanup success={}, returned run={}",
        handle_a.owner_state(), disposed_while_running, outcome.rollback.is_success(), outcome.runtime.run_id());
    handle_a.shutdown().await.unwrap();
    drop(join_a.await.unwrap());
    drop(composition_b);
    assert_eq!(disposed_while_running, 0, "foreign DriverExit must not authorize cleanup of live A");
}

#[tokio::test]
async fn review_cancelled_owner_loss_cleanup_must_not_claim_already_released() {
    let started = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&started);
    let recorder = Recorder::new();
    let blocked_hook = lifecycle_hook(move || {
        let observed = Arc::clone(&observed);
        async move {
            observed.fetch_add(1, Ordering::SeqCst);
            pending::<()>().await;
            Ok(())
        }
    });
    let plan = CompositionBuilder::new()
        .register(ModuleRegistration::new(ModuleDefinition::new(id("a")))
            .on_activate(recorder.hook("activate:a", None))
            .on_dispose(recorder.hook("dispose:a", None))).unwrap()
        .register(ModuleRegistration::new(ModuleDefinition::new(id("b")).depends_on(id("a")))
            .on_activate(recorder.hook("activate:b", None))
            .on_dispose(blocked_hook)).unwrap()
        .build().unwrap();
    let assembly = plan.start(run_id("review-cancel"), &HostConfig::new()).await.unwrap();
    let (driver, _handle, mut composition) = assembly.into_driver(SuccessDispatcher::new());
    drop(driver);
    let mut release = Box::pin(composition.release_after_owner_loss());
    let mut context = Context::from_waker(Waker::noop());
    assert!(matches!(release.as_mut().poll(&mut context), Poll::Pending));
    assert_eq!(started.load(Ordering::SeqCst), 1);
    drop(release);
    let retry = composition.release_after_owner_loss().await;
    eprintln!("cancelled cleanup: retry={retry:?}, untouched A dispose count={}", recorder.count("dispose:a"));
    assert!(!matches!(retry, Err(OwnerLossReleaseError::AlreadyReleased)),
        "cancellation cannot discard untouched cleanup resources and claim AlreadyReleased");
}

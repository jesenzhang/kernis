//! The read-only `RuntimeHandle::owner_state` observation tracks the one
//! driver-owner truth in the mailbox and never changes it: fresh drivers
//! are `Running`, orderly shutdown is `Shutdown` (not owner loss), and
//! dropped, aborted, or unwound owners are `OwnerDropped`.

#![allow(missing_docs)]

use kernis_core::Id;
use runtime_core::{
    DriverError, DriverOwnerState, EffectDispatchFuture, EffectDispatchRequest, EffectDispatcher,
    FactoryRegistry, RunDefinition, RunId, Runtime, RuntimeDriver, ShutdownStatus, TaskDefinition,
};
use workflow_recovery::{EffectSemantics, InMemoryDurableStore, OperationId};

struct NoEffect;

impl EffectDispatcher for NoEffect {
    fn dispatch(&mut self, _: EffectDispatchRequest) -> EffectDispatchFuture {
        panic!("empty workflow cannot dispatch")
    }
}

struct PanickingEffect;

impl EffectDispatcher for PanickingEffect {
    fn dispatch(&mut self, _: EffectDispatchRequest) -> EffectDispatchFuture {
        Box::pin(async { panic!("adapter panic for owner-state observation") })
    }
}

fn empty_driver() -> (
    RuntimeDriver<InMemoryDurableStore, NoEffect>,
    runtime_core::RuntimeHandle,
) {
    let runtime = Runtime::start_from_definition(
        RunId::new("owner-state").unwrap(),
        RunDefinition::new(),
        &FactoryRegistry::new(),
    )
    .unwrap();
    RuntimeDriver::new(runtime, NoEffect)
}

fn effect_driver() -> (
    RuntimeDriver<InMemoryDurableStore, PanickingEffect>,
    runtime_core::RuntimeHandle,
) {
    let definition = RunDefinition::new().with_task(
        TaskDefinition::new(Id::new("task").unwrap(), "task").with_effect(
            OperationId::new("operation").unwrap(),
            EffectSemantics::NonIdempotent,
        ),
    );
    let runtime = Runtime::start_from_definition(
        RunId::new("owner-state-panic").unwrap(),
        definition,
        &FactoryRegistry::new(),
    )
    .unwrap();
    RuntimeDriver::new(runtime, PanickingEffect)
}

#[test]
fn fresh_driver_observes_running_until_its_owner_is_dropped() {
    let (driver, handle) = empty_driver();
    assert_eq!(handle.owner_state(), DriverOwnerState::Running);
    drop(driver);
    assert_eq!(handle.owner_state(), DriverOwnerState::OwnerDropped);
    // The observation is read-only and repeatable: it never mutates the
    // one owner-drop state the command path rejects with.
    assert_eq!(handle.owner_state(), DriverOwnerState::OwnerDropped);
}

#[tokio::test]
async fn orderly_shutdown_observes_shutdown_not_owner_loss() {
    let (driver, handle) = empty_driver();
    let owner = tokio::spawn(driver.run());
    assert_eq!(
        handle.shutdown().await.expect("clean shutdown"),
        ShutdownStatus::Clean
    );
    let exit = owner.await.expect("the driver task joins");
    drop(exit);
    assert_eq!(handle.owner_state(), DriverOwnerState::Shutdown);
    // Closing the mailbox through shutdown never sets the owner-drop bit:
    // post-shutdown commands resolve ShuttingDown, and the observation
    // stays Shutdown instead of drifting into OwnerDropped.
    assert!(matches!(
        handle.drive().await,
        Err(DriverError::ShuttingDown)
    ));
    assert_eq!(handle.owner_state(), DriverOwnerState::Shutdown);
}

#[tokio::test]
async fn aborted_owner_observes_owner_dropped() {
    let (driver, handle) = empty_driver();
    let owner = tokio::spawn(driver.run());
    assert_eq!(handle.owner_state(), DriverOwnerState::Running);
    owner.abort();
    let error = owner
        .await
        .err()
        .expect("the aborted driver task fails to join");
    assert!(error.is_cancelled());
    assert_eq!(handle.owner_state(), DriverOwnerState::OwnerDropped);
    assert!(matches!(
        handle.drive().await,
        Err(DriverError::OwnerDropped)
    ));
}

#[tokio::test]
async fn unwound_panic_observes_owner_dropped() {
    let (driver, handle) = effect_driver();
    assert_eq!(handle.owner_state(), DriverOwnerState::Running);
    let owner = tokio::spawn(driver.run());
    let active = handle.drive();
    // The dispatch panics inside the driver task; unwinding drops the
    // owner guard exactly like a drop or abort would.
    assert!(matches!(active.await, Err(DriverError::OwnerDropped)));
    let error = owner
        .await
        .err()
        .expect("the panicking driver task fails to join");
    assert!(error.is_panic());
    assert_eq!(handle.owner_state(), DriverOwnerState::OwnerDropped);
}

#![allow(missing_docs)]

use kernis_core::Id;
use runtime_core::{DriverError, TaskDefinition};
use runtime_core::{
    EffectDispatchFuture, EffectDispatchRequest, EffectDispatcher, FactoryRegistry, RunDefinition,
    RunId, Runtime, RuntimeDriver,
};
use std::future::Future;
use std::task::{Context, Poll, Waker};
use workflow_recovery::{EffectSemantics, OperationId};

struct InterruptedEffect {
    started: Option<tokio::sync::oneshot::Sender<()>>,
    panic: bool,
}

impl EffectDispatcher for InterruptedEffect {
    fn dispatch(&mut self, _: EffectDispatchRequest) -> EffectDispatchFuture {
        let started = self.started.take().unwrap();
        let panic = self.panic;
        Box::pin(async move {
            started.send(()).unwrap();
            assert!(!panic, "adapter panic for review regression");
            std::future::pending().await
        })
    }
}

async fn assert_interrupted_owner(panic: bool) {
    let definition = RunDefinition::new().with_task(
        TaskDefinition::new(Id::new("task").unwrap(), "task").with_effect(
            OperationId::new("operation").unwrap(),
            EffectSemantics::NonIdempotent,
        ),
    );
    let runtime = Runtime::start_from_definition(
        RunId::new("interrupted-owner").unwrap(),
        definition,
        &FactoryRegistry::new(),
    )
    .unwrap();
    let (started, observed) = tokio::sync::oneshot::channel();
    let (driver, handle) = RuntimeDriver::new(
        runtime,
        InterruptedEffect {
            started: Some(started),
            panic,
        },
    );
    let active = handle.drive();
    let queued = handle.shutdown();
    // Poll the response before owner loss to prove its registered waker is notified.
    let waiter = tokio::spawn(active);
    tokio::task::yield_now().await;
    let owner = tokio::spawn(driver.run());
    observed.await.unwrap();
    if !panic {
        owner.abort();
    }
    assert!(owner.await.is_err());
    assert_eq!(waiter.await.unwrap(), Err(DriverError::OwnerDropped));
    assert_eq!(queued.await, Err(DriverError::OwnerDropped));
    assert_eq!(handle.drive().await, Err(DriverError::OwnerDropped));
    assert_eq!(
        handle.drain_execution_events().await,
        Err(DriverError::OwnerDropped)
    );
}

#[tokio::test]
async fn abort_rejects_active_queued_and_future_commands() {
    assert_interrupted_owner(false).await;
}

#[tokio::test]
async fn dispatcher_panic_rejects_active_queued_and_future_commands() {
    assert_interrupted_owner(true).await;
}

struct NoEffect;
impl EffectDispatcher for NoEffect {
    fn dispatch(&mut self, _: EffectDispatchRequest) -> EffectDispatchFuture {
        panic!("empty workflow cannot dispatch")
    }
}

#[test]
fn commands_resolve_after_driver_owner_is_dropped() {
    let runtime = Runtime::start_from_definition(
        RunId::new("review-owner-drop").unwrap(),
        RunDefinition::new(),
        &FactoryRegistry::new(),
    )
    .unwrap();
    let (driver, handle) = RuntimeDriver::new(runtime, NoEffect);
    let mut pending = Box::pin(handle.drive());
    drop(driver);
    let mut context = Context::from_waker(Waker::noop());
    assert!(
        matches!(pending.as_mut().poll(&mut context), Poll::Ready(Err(_))),
        "owner is gone, but the queued command remains pending forever"
    );
    let mut later = Box::pin(handle.shutdown());
    assert!(
        matches!(later.as_mut().poll(&mut context), Poll::Ready(Err(_))),
        "commands submitted after owner loss must also fail"
    );
}

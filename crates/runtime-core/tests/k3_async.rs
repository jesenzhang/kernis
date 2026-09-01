#![allow(missing_docs)]

use capability_graph::Scope;
use kernis_core::Id;
use runtime_core::{
    Cancellation, DriveResult, DriverError, DriverExit, EffectDispatchError, EffectDispatchFuture,
    EffectDispatchRequest, EffectDispatcher, RunId, Runtime, RuntimeDriver, RuntimeEvent,
    RuntimeHandle, ShutdownStatus, StepResult, TaskConfig,
};
use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;
use workflow_graph::{Task, WorkflowGraph, WorkflowMutation};
use workflow_recovery::{
    AttemptId, DurableStore, EffectSemantics, FileDurableStore, KnownEffectOutcome, OperationId,
    RecoveryAction,
};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn id(value: &str) -> Id {
    Id::new(value).expect("test id is valid")
}

fn operation(value: &str) -> OperationId {
    OperationId::new(value).expect("test operation is valid")
}

fn workflow_with_task() -> WorkflowGraph {
    let mut workflow = WorkflowGraph::default();
    workflow
        .apply_batch(
            workflow.revision(),
            [WorkflowMutation::AddTask {
                task: Task {
                    id: id("task"),
                    label: "task".to_owned(),
                },
            }],
        )
        .expect("workflow is valid");
    workflow
}

fn effect_config(operation_id: &OperationId, semantics: EffectSemantics) -> TaskConfig {
    TaskConfig::new().with_effect(operation_id.clone(), semantics)
}

fn effect_runtime(run_id: &str, semantics: EffectSemantics) -> Runtime {
    let operation_id = operation("operation");
    Runtime::start_run(
        RunId::new(run_id).expect("run id is valid"),
        workflow_with_task(),
        Scope::root(),
        [(id("task"), effect_config(&operation_id, semantics))],
    )
    .expect("runtime starts")
}

#[derive(Debug)]
enum ScriptedReply {
    Known(KnownEffectOutcome),
    Unknown(String),
}

#[derive(Clone, Debug)]
struct ScriptedDispatcher {
    requests: Arc<Mutex<Vec<EffectDispatchRequest>>>,
    replies: Arc<Mutex<VecDeque<ScriptedReply>>>,
}

impl ScriptedDispatcher {
    fn new(replies: impl IntoIterator<Item = ScriptedReply>) -> Self {
        Self {
            requests: Arc::new(Mutex::new(Vec::new())),
            replies: Arc::new(Mutex::new(replies.into_iter().collect())),
        }
    }
}

impl EffectDispatcher for ScriptedDispatcher {
    fn dispatch(&mut self, request: EffectDispatchRequest) -> EffectDispatchFuture {
        self.requests
            .lock()
            .expect("requests lock is healthy")
            .push(request);
        let reply = self
            .replies
            .lock()
            .expect("replies lock is healthy")
            .pop_front()
            .unwrap_or(ScriptedReply::Known(KnownEffectOutcome::Succeeded));
        Box::pin(async move {
            match reply {
                ScriptedReply::Known(outcome) => Ok(outcome),
                ScriptedReply::Unknown(reason) => Err(EffectDispatchError::unknown(reason)),
            }
        })
    }
}

struct WaitGate {
    started: Mutex<Option<oneshot::Sender<()>>>,
    release: Mutex<Option<oneshot::Receiver<Result<KnownEffectOutcome, EffectDispatchError>>>>,
}

#[derive(Clone)]
struct WaitingDispatcher {
    requests: Arc<Mutex<Vec<EffectDispatchRequest>>>,
    gate: Arc<WaitGate>,
}

impl WaitingDispatcher {
    fn new(
        started: oneshot::Sender<()>,
        release: oneshot::Receiver<Result<KnownEffectOutcome, EffectDispatchError>>,
    ) -> Self {
        Self {
            requests: Arc::new(Mutex::new(Vec::new())),
            gate: Arc::new(WaitGate {
                started: Mutex::new(Some(started)),
                release: Mutex::new(Some(release)),
            }),
        }
    }
}

impl EffectDispatcher for WaitingDispatcher {
    fn dispatch(&mut self, request: EffectDispatchRequest) -> EffectDispatchFuture {
        self.requests
            .lock()
            .expect("requests lock is healthy")
            .push(request);
        if let Some(started) = self
            .gate
            .started
            .lock()
            .expect("started lock is healthy")
            .take()
        {
            let _ = started.send(());
        }
        let release = self
            .gate
            .release
            .lock()
            .expect("release lock is healthy")
            .take()
            .expect("one waiting dispatch is expected");
        Box::pin(async move {
            release
                .await
                .unwrap_or_else(|_| Err(EffectDispatchError::unknown("release dropped")))
        })
    }
}

fn start_driver<S, D>(
    runtime: Runtime<S>,
    dispatcher: D,
) -> (RuntimeHandle, tokio::task::JoinHandle<DriverExit<S>>)
where
    S: DurableStore + Send + 'static,
    D: EffectDispatcher + 'static,
{
    let (driver, handle) = RuntimeDriver::new(runtime, dispatcher);
    let join = tokio::spawn(driver.run());
    (handle, join)
}

fn assert_send<T: Send>() {}

fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn async_boundary_types_are_executor_neutral_and_send_sync() {
    assert_send_sync::<RuntimeHandle>();
    assert_send_sync::<runtime_core::DriverFuture<DriveResult>>();
    assert_send::<
        runtime_core::RuntimeDriver<workflow_recovery::InMemoryDurableStore, ScriptedDispatcher>,
    >();
}

struct OrderingStore {
    inner: workflow_recovery::InMemoryDurableStore,
    events: Arc<Mutex<Vec<&'static str>>>,
}

impl DurableStore for OrderingStore {
    fn create_run(
        &mut self,
        run_id: RunId,
    ) -> Result<workflow_recovery::StoreRevision, workflow_recovery::StoreError> {
        self.inner.create_run(run_id)
    }

    fn load_run(
        &self,
        run_id: &RunId,
    ) -> Result<workflow_recovery::DurableRunState, workflow_recovery::StoreError> {
        self.inner.load_run(run_id)
    }

    fn commit(
        &mut self,
        request: workflow_recovery::CommitRequest,
    ) -> Result<workflow_recovery::CommitResult, workflow_recovery::StoreError> {
        self.events
            .lock()
            .expect("events lock is healthy")
            .push("durable-commit");
        self.inner.commit(request)
    }
}

struct OrderingDispatcher {
    events: Arc<Mutex<Vec<&'static str>>>,
}

impl EffectDispatcher for OrderingDispatcher {
    fn dispatch(&mut self, _request: EffectDispatchRequest) -> EffectDispatchFuture {
        self.events
            .lock()
            .expect("events lock is healthy")
            .push("external-dispatch");
        Box::pin(async { Ok(KnownEffectOutcome::Succeeded) })
    }
}

#[tokio::test]
async fn durable_dispatch_precedes_external_call_and_known_outcome_commit() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let operation_id = operation("operation");
    let runtime = Runtime::<OrderingStore>::start_run_with_store(
        RunId::new("ordering").expect("run id is valid"),
        workflow_with_task(),
        Scope::root(),
        [(
            id("task"),
            effect_config(&operation_id, EffectSemantics::Idempotent),
        )],
        OrderingStore {
            inner: workflow_recovery::InMemoryDurableStore::new(),
            events: Arc::clone(&events),
        },
    )
    .expect("runtime starts");
    events.lock().expect("events lock is healthy").clear();
    let (handle, join) = start_driver(
        runtime,
        OrderingDispatcher {
            events: Arc::clone(&events),
        },
    );
    assert!(matches!(
        handle.drive().await.expect("ordered effect drive succeeds"),
        DriveResult::EffectCompleted { .. }
    ));
    let events = events.lock().expect("events lock is healthy").clone();
    let external_index = events
        .iter()
        .position(|event| *event == "external-dispatch")
        .expect("external call is recorded");
    assert!(
        events[..external_index]
            .iter()
            .all(|event| *event == "durable-commit")
    );
    assert!(
        events[external_index + 1..]
            .iter()
            .all(|event| *event == "durable-commit")
    );
    assert_eq!(
        handle.shutdown().await.expect("shutdown succeeds"),
        ShutdownStatus::Clean
    );
    let _ = join.await.expect("driver task joins");
}

#[tokio::test]
async fn async_driver_matches_equivalent_synchronous_durable_facts() {
    let operation_id = operation("operation");
    let mut synchronous = Runtime::start_run(
        RunId::new("sync-equivalence").expect("run id is valid"),
        workflow_with_task(),
        Scope::root(),
        [(
            id("task"),
            effect_config(&operation_id, EffectSemantics::Idempotent),
        )],
    )
    .expect("runtime starts");
    let pending = synchronous.step().expect("synchronous step succeeds");
    let (attempt_id, pending_operation) = match pending {
        StepResult::EffectPending {
            attempt_id,
            operation_id,
            ..
        } => (attempt_id, operation_id),
        other => panic!("expected pending effect, got {other:?}"),
    };
    synchronous
        .dispatch_effect(&pending_operation)
        .expect("synchronous dispatch succeeds");
    synchronous
        .record_effect_outcome(
            &pending_operation,
            attempt_id.clone(),
            KnownEffectOutcome::Succeeded,
        )
        .expect("synchronous outcome records");
    assert_eq!(
        synchronous.step().expect("synchronous completion succeeds"),
        StepResult::Completed {
            task_id: id("task"),
            attempt_id: attempt_id.clone(),
        }
    );
    let expected = synchronous
        .durable_state()
        .expect("synchronous state loads");

    let dispatcher = ScriptedDispatcher::new([ScriptedReply::Known(KnownEffectOutcome::Succeeded)]);
    let (handle, join) = start_driver(
        effect_runtime("sync-equivalence", EffectSemantics::Idempotent),
        dispatcher,
    );
    let first = handle.drive().await.expect("async effect drive succeeds");
    assert!(matches!(
        first,
        DriveResult::EffectCompleted {
            request: EffectDispatchRequest { attempt_id: ref actual, .. },
            outcome: KnownEffectOutcome::Succeeded,
        } if actual == &attempt_id
    ));
    assert_eq!(
        handle.drive().await.expect("async completion succeeds"),
        DriveResult::Step(StepResult::Completed {
            task_id: id("task"),
            attempt_id,
        })
    );
    assert_eq!(
        handle.shutdown().await.expect("shutdown succeeds"),
        ShutdownStatus::Clean
    );
    let exit = join.await.expect("driver task joins");
    assert_eq!(
        exit.runtime().durable_state().expect("async state loads"),
        expected
    );
}

#[tokio::test]
async fn concurrent_wakeups_serialize_to_one_effect_dispatch() {
    let dispatcher = ScriptedDispatcher::new([ScriptedReply::Known(KnownEffectOutcome::Succeeded)]);
    let requests = dispatcher.requests.clone();
    let (handle, join) = start_driver(
        effect_runtime("concurrent-wakeup", EffectSemantics::Idempotent),
        dispatcher,
    );
    let first = handle.drive();
    let second = handle.wake();
    let (first, second) = tokio::join!(first, second);
    assert!(matches!(
        first.expect("first wake succeeds"),
        DriveResult::EffectCompleted { .. }
    ));
    assert_eq!(
        second.expect("second wake succeeds"),
        DriveResult::Step(StepResult::Completed {
            task_id: id("task"),
            attempt_id: AttemptId::new("concurrent-wakeup-attempt-1").expect("attempt id is valid"),
        })
    );
    assert_eq!(requests.lock().expect("requests lock is healthy").len(), 1);
    assert_eq!(
        handle.shutdown().await.expect("shutdown succeeds"),
        ShutdownStatus::Clean
    );
    let _ = join.await.expect("driver task joins");
}

#[tokio::test]
async fn cancellation_before_dispatch_prevents_external_call() {
    let dispatcher = ScriptedDispatcher::new([]);
    let requests = dispatcher.requests.clone();
    let (handle, join) = start_driver(
        effect_runtime("cancel-before", EffectSemantics::Idempotent),
        dispatcher,
    );
    assert_eq!(
        handle
            .cancel_task(id("task"))
            .await
            .expect("cancellation succeeds"),
        Cancellation::NotDispatched {
            task_id: id("task")
        }
    );
    assert_eq!(
        handle.drive().await.expect("cancelled drive succeeds"),
        DriveResult::Step(StepResult::Idle)
    );
    assert!(
        requests
            .lock()
            .expect("requests lock is healthy")
            .is_empty()
    );
    assert_eq!(
        handle.shutdown().await.expect("shutdown succeeds"),
        ShutdownStatus::Clean
    );
    let _ = join.await.expect("driver task joins");
}

#[tokio::test]
async fn cancellation_during_dispatch_waits_for_outcome_barrier() {
    let (started_sender, started_receiver) = oneshot::channel();
    let (release_sender, release_receiver) = oneshot::channel();
    let dispatcher = WaitingDispatcher::new(started_sender, release_receiver);
    let (handle, join) = start_driver(
        effect_runtime("cancel-during", EffectSemantics::Idempotent),
        dispatcher,
    );
    let drive = handle.drive();
    started_receiver.await.expect("dispatcher starts");
    let cancel = handle.cancel_task(id("task"));
    release_sender
        .send(Err(EffectDispatchError::unknown("connection lost")))
        .expect("dispatcher release sends");

    let drive_result = drive.await.expect("unknown outcome is a drive result");
    let request = match drive_result {
        DriveResult::EffectUnknown { request, error } => {
            assert_eq!(error.reason(), "connection lost");
            request
        }
        other => panic!("expected unknown effect, got {other:?}"),
    };
    assert_eq!(
        cancel.await.expect("cancellation reaches owner"),
        Cancellation::AlreadyDispatched {
            task_id: id("task"),
            operation_id: request.operation_id,
            attempt_id: request.attempt_id,
        }
    );
    assert_eq!(
        handle.shutdown().await.expect("shutdown succeeds"),
        ShutdownStatus::PendingUnknown {
            operation_id: operation("operation")
        }
    );
    let _ = join.await.expect("driver task joins");
}

#[tokio::test]
async fn cancellation_after_known_outcome_cannot_erase_dispatch() {
    let dispatcher = ScriptedDispatcher::new([ScriptedReply::Known(KnownEffectOutcome::Succeeded)]);
    let requests = dispatcher.requests.clone();
    let (handle, join) = start_driver(
        effect_runtime("cancel-after", EffectSemantics::Idempotent),
        dispatcher,
    );
    let request = match handle.drive().await.expect("effect drive succeeds") {
        DriveResult::EffectCompleted { request, .. } => request,
        other => panic!("expected completed effect, got {other:?}"),
    };
    assert_eq!(
        handle
            .cancel_task(id("task"))
            .await
            .expect("cancellation succeeds"),
        Cancellation::AlreadyDispatched {
            task_id: id("task"),
            operation_id: request.operation_id,
            attempt_id: request.attempt_id,
        }
    );
    assert_eq!(
        handle.drive().await.expect("cancelled task is skipped"),
        DriveResult::Step(StepResult::Idle)
    );
    assert_eq!(requests.lock().expect("requests lock is healthy").len(), 1);
    assert_eq!(
        handle.shutdown().await.expect("shutdown succeeds"),
        ShutdownStatus::Clean
    );
    let _ = join.await.expect("driver task joins");
}

#[tokio::test]
async fn idempotent_unknown_outcome_is_explicitly_recoverable_and_not_retried_implicitly() {
    let dispatcher = ScriptedDispatcher::new([ScriptedReply::Unknown("lost reply".to_owned())]);
    let requests = dispatcher.requests.clone();
    let (handle, join) = start_driver(
        effect_runtime("unknown-idempotent", EffectSemantics::Idempotent),
        dispatcher,
    );
    assert!(matches!(
        handle.drive().await.expect("unknown outcome is returned"),
        DriveResult::EffectUnknown { .. }
    ));
    let decision = handle
        .recover(operation("operation"))
        .await
        .expect("recovery classification succeeds");
    assert_eq!(decision.action, RecoveryAction::RetrySameOperation);
    assert_eq!(requests.lock().expect("requests lock is healthy").len(), 1);
    assert_eq!(
        handle.shutdown().await.expect("shutdown succeeds"),
        ShutdownStatus::PendingUnknown {
            operation_id: operation("operation")
        }
    );
    let _ = join.await.expect("driver task joins");
}

#[tokio::test]
async fn idempotent_unknown_retry_reuses_operation_with_a_new_attempt() {
    let dispatcher = ScriptedDispatcher::new([
        ScriptedReply::Unknown("lost first reply".to_owned()),
        ScriptedReply::Known(KnownEffectOutcome::Succeeded),
    ]);
    let requests = dispatcher.requests.clone();
    let (handle, join) = start_driver(
        effect_runtime("unknown-retry", EffectSemantics::Idempotent),
        dispatcher,
    );
    let first_request = match handle
        .drive()
        .await
        .expect("first dispatch returns unknown")
    {
        DriveResult::EffectUnknown { request, .. } => request,
        other => panic!("expected unknown first dispatch, got {other:?}"),
    };
    assert_eq!(
        handle
            .recover(operation("operation"))
            .await
            .expect("retry classification succeeds")
            .action,
        RecoveryAction::RetrySameOperation
    );
    let second_request = match handle
        .dispatch_effect(operation("operation"))
        .await
        .expect("explicit retry succeeds")
    {
        DriveResult::EffectCompleted { request, outcome } => {
            assert_eq!(outcome, KnownEffectOutcome::Succeeded);
            request
        }
        other => panic!("expected known retry outcome, got {other:?}"),
    };
    assert_eq!(first_request.operation_id, second_request.operation_id);
    assert_ne!(first_request.attempt_id, second_request.attempt_id);
    assert_eq!(requests.lock().expect("requests lock is healthy").len(), 2);
    assert!(matches!(
        handle
            .drive()
            .await
            .expect("retried operation completes task"),
        DriveResult::Step(StepResult::Completed { .. })
    ));
    assert_eq!(
        handle.shutdown().await.expect("shutdown succeeds"),
        ShutdownStatus::Clean
    );
    let _ = join.await.expect("driver task joins");
}

#[tokio::test]
async fn non_idempotent_unknown_outcome_requires_reconciliation() {
    let dispatcher = ScriptedDispatcher::new([ScriptedReply::Unknown("lost reply".to_owned())]);
    let (handle, join) = start_driver(
        effect_runtime("unknown-non-idempotent", EffectSemantics::NonIdempotent),
        dispatcher,
    );
    assert!(matches!(
        handle.drive().await.expect("unknown outcome is returned"),
        DriveResult::EffectUnknown { .. }
    ));
    assert_eq!(
        handle
            .recover(operation("operation"))
            .await
            .expect("recovery classification succeeds")
            .action,
        RecoveryAction::Reconcile
    );
    assert_eq!(
        handle.shutdown().await.expect("shutdown succeeds"),
        ShutdownStatus::ReconciliationRequired {
            operation_id: operation("operation")
        }
    );
    let _ = join.await.expect("driver task joins");
}

#[tokio::test]
async fn prepared_work_is_classified_without_external_dispatch_on_shutdown() {
    let operation_id = operation("operation");
    let mut runtime = effect_runtime("prepared-shutdown", EffectSemantics::Idempotent);
    runtime
        .record_effect_intent(
            id("task"),
            operation_id.clone(),
            EffectSemantics::Idempotent,
        )
        .expect("intent records");
    let dispatcher = ScriptedDispatcher::new([]);
    let requests = dispatcher.requests.clone();
    let (handle, join) = start_driver(runtime, dispatcher);
    assert_eq!(
        handle.shutdown().await.expect("shutdown succeeds"),
        ShutdownStatus::PendingDispatch { operation_id }
    );
    assert!(
        requests
            .lock()
            .expect("requests lock is healthy")
            .is_empty()
    );
    assert!(matches!(
        handle.drive().await,
        Err(DriverError::ShuttingDown)
    ));
    let _ = join.await.expect("driver task joins");
}

fn many_task_runtime(count: usize) -> Runtime {
    let mut workflow = WorkflowGraph::default();
    let mutations = (0..count)
        .map(|index| WorkflowMutation::AddTask {
            task: Task {
                id: id(&format!("task-{index:03}")),
                label: format!("task-{index:03}"),
            },
        })
        .collect::<Vec<_>>();
    workflow
        .apply_batch(workflow.revision(), mutations)
        .expect("workflow is valid");
    Runtime::start_run(
        RunId::new("backpressure").expect("run id is valid"),
        workflow,
        Scope::root(),
        [(
            id("task-064"),
            TaskConfig::new().with_effect(
                operation("backpressure-operation"),
                EffectSemantics::Idempotent,
            ),
        )],
    )
    .expect("runtime starts")
}

#[tokio::test]
async fn lossless_backpressure_pauses_and_resumes_without_duplicate_attempts() {
    let dispatcher = ScriptedDispatcher::new([]);
    let requests = dispatcher.requests.clone();
    let (handle, join) = start_driver(many_task_runtime(65), dispatcher);
    for _ in 0..64 {
        assert!(matches!(
            handle.drive().await.expect("completed task drive succeeds"),
            DriveResult::Step(StepResult::Completed { .. })
        ));
    }
    let blocked_item = match handle
        .drive()
        .await
        .expect("backpressure is a drive result")
    {
        DriveResult::Backpressured { item } => item,
        other => panic!("expected lifecycle backpressure, got {other:?}"),
    };
    let blocked_attempt = match &blocked_item.payload {
        RuntimeEvent::TaskStarted {
            task_id,
            attempt_id,
            ..
        } => {
            assert_eq!(task_id, &id("task-064"));
            attempt_id.clone()
        }
        other => panic!("expected retained start event, got {other:?}"),
    };
    assert_eq!(blocked_item.sequence.get(), 129);
    let drained = handle
        .drain_execution_events()
        .await
        .expect("lifecycle stream drains");
    assert_eq!(drained.len(), 128);
    assert_eq!(
        drained.first().expect("first event exists").sequence.get(),
        1
    );
    assert_eq!(
        drained.last().expect("last event exists").sequence.get(),
        128
    );

    let resumed = handle.drive().await.expect("backpressured work resumes");
    let dispatched_request = match resumed {
        DriveResult::EffectCompleted { request, outcome } => {
            assert_eq!(outcome, KnownEffectOutcome::Succeeded);
            request
        }
        other => panic!("expected resumed effect dispatch, got {other:?}"),
    };
    assert_eq!(dispatched_request.task_id, id("task-064"));
    assert_eq!(
        dispatched_request.operation_id,
        operation("backpressure-operation")
    );
    assert_eq!(dispatched_request.attempt_id, blocked_attempt.clone());
    assert_eq!(requests.lock().expect("requests lock is healthy").len(), 1);

    let completed = handle.drive().await.expect("known effect completes task");
    assert_eq!(
        completed,
        DriveResult::Step(StepResult::Completed {
            task_id: id("task-064"),
            attempt_id: blocked_attempt.clone(),
        })
    );
    let tail = handle
        .drain_execution_events()
        .await
        .expect("tail lifecycle events drain");
    assert_eq!(tail.len(), 2);
    assert_eq!(tail[0].sequence.get(), 129);
    assert_eq!(tail[1].sequence.get(), 130);
    assert!(matches!(
        &tail[0].payload,
        RuntimeEvent::TaskStarted { attempt_id, .. } if attempt_id == &blocked_attempt
    ));
    assert!(matches!(
        &tail[1].payload,
        RuntimeEvent::TaskCompleted { attempt_id, .. } if attempt_id == &blocked_attempt
    ));
    assert_eq!(
        handle.shutdown().await.expect("shutdown succeeds"),
        ShutdownStatus::Clean
    );
    let exit = join.await.expect("driver task joins");
    let state = exit.runtime().durable_state().expect("durable state loads");
    assert_eq!(state.attempts().count(), 65);
    assert_eq!(state.completion_history().len(), 65);
}

struct TempStore {
    directory: PathBuf,
    path: PathBuf,
}

impl TempStore {
    fn new(label: &str) -> Self {
        let number = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "kernis-k3-runtime-{label}-{}-{number}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).expect("temporary directory creates");
        Self {
            path: directory.join("runtime.redb"),
            directory,
        }
    }
}

impl Drop for TempStore {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

#[tokio::test]
async fn physical_restart_does_not_redispatch_a_known_effect() {
    let temp = TempStore::new("restart");
    let run_id = RunId::new("physical-restart").expect("run id is valid");
    let operation_id = operation("operation");
    let workflow = workflow_with_task();
    let config = effect_config(&operation_id, EffectSemantics::Idempotent);
    let first_dispatcher =
        ScriptedDispatcher::new([ScriptedReply::Known(KnownEffectOutcome::Succeeded)]);
    let first_requests = first_dispatcher.requests.clone();
    let runtime = Runtime::<FileDurableStore>::start_run_with_store(
        run_id.clone(),
        workflow.clone(),
        Scope::root(),
        [(id("task"), config.clone())],
        FileDurableStore::open(&temp.path).expect("physical store opens"),
    )
    .expect("physical runtime starts");
    let (handle, join) = {
        let (driver, handle) = RuntimeDriver::new(runtime, first_dispatcher);
        (handle, tokio::spawn(driver.run()))
    };
    assert!(matches!(
        handle
            .drive()
            .await
            .expect("physical effect drive succeeds"),
        DriveResult::EffectCompleted { .. }
    ));
    assert_eq!(
        handle.shutdown().await.expect("physical shutdown succeeds"),
        ShutdownStatus::Clean
    );
    let first_exit = join.await.expect("first driver joins");
    assert_eq!(
        first_requests
            .lock()
            .expect("requests lock is healthy")
            .len(),
        1
    );
    drop(first_exit.into_runtime());

    let restored = Runtime::<FileDurableStore>::restore_run(
        run_id,
        workflow,
        Scope::root(),
        [(id("task"), config)],
        FileDurableStore::open(&temp.path).expect("physical store reopens"),
    )
    .expect("physical runtime restores");
    let second_dispatcher = ScriptedDispatcher::new([]);
    let second_requests = second_dispatcher.requests.clone();
    let (handle, join) = {
        let (driver, handle) = RuntimeDriver::new(restored, second_dispatcher);
        (handle, tokio::spawn(driver.run()))
    };
    assert_eq!(
        handle.drive().await.expect("restored drive succeeds"),
        DriveResult::Step(StepResult::Idle)
    );
    assert!(
        second_requests
            .lock()
            .expect("requests lock is healthy")
            .is_empty()
    );
    assert_eq!(
        handle.shutdown().await.expect("restored shutdown succeeds"),
        ShutdownStatus::Clean
    );
    let second_exit = join.await.expect("second driver joins");
    assert!(
        second_exit
            .runtime()
            .durable_state()
            .expect("restored state loads")
            .is_completed(&id("task"))
    );
}

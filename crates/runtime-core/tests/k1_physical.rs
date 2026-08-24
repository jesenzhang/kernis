#![allow(missing_docs)]

use kernis_core::Id;
use runtime_core::{RunId, Runtime, RuntimeError, StepResult, TaskConfig};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use workflow_graph::{Task, WorkflowGraph, WorkflowMutation};
use workflow_recovery::{
    EffectSemantics, FileDurableStore, KnownEffectOutcome, OperationId, RecoveryAction, StoreError,
};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempStore {
    directory: PathBuf,
    path: PathBuf,
}

impl TempStore {
    fn new(label: &str) -> Self {
        let number = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "kernis-k1-runtime-{label}-{}-{number}",
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

fn id(value: &str) -> Id {
    Id::new(value).expect("test id is valid")
}

fn operation(value: &str) -> OperationId {
    OperationId::new(value).expect("test operation is valid")
}

fn single_task_workflow(task_id: &str) -> WorkflowGraph {
    let mut workflow = WorkflowGraph::default();
    workflow
        .apply_batch(
            workflow.revision(),
            [WorkflowMutation::AddTask {
                task: Task {
                    id: id(task_id),
                    label: task_id.to_owned(),
                },
            }],
        )
        .expect("workflow is valid");
    workflow
}

#[test]
fn runtime_reopens_physical_store_without_redispatch_or_duplicate_completion() {
    let temp = TempStore::new("effect");
    let run_id = RunId::new("physical-runtime-effect").expect("run id is valid");
    let operation_id = operation("physical-operation");
    let workflow = single_task_workflow("task");
    let mut runtime = Runtime::<FileDurableStore>::start_run_with_store(
        run_id.clone(),
        workflow.clone(),
        capability_graph::Scope::root(),
        [(
            id("task"),
            TaskConfig::new().with_effect(operation_id.clone(), EffectSemantics::Idempotent),
        )],
        FileDurableStore::open(&temp.path).expect("physical store opens"),
    )
    .expect("runtime starts");
    let attempt_id = match runtime.step().expect("attempt admits") {
        StepResult::EffectPending { attempt_id, .. } => attempt_id,
        other => panic!("expected pending effect, got {other:?}"),
    };
    runtime
        .dispatch_effect(&operation_id)
        .expect("dispatch commits");
    runtime
        .record_effect_outcome(
            &operation_id,
            attempt_id.clone(),
            KnownEffectOutcome::Succeeded,
        )
        .expect("outcome commits");
    drop(runtime);

    let mut first_restart = Runtime::<FileDurableStore>::restore_run(
        run_id.clone(),
        workflow.clone(),
        capability_graph::Scope::root(),
        [(
            id("task"),
            TaskConfig::new().with_effect(operation_id.clone(), EffectSemantics::Idempotent),
        )],
        FileDurableStore::open(&temp.path).expect("physical store reopens"),
    )
    .expect("first cold restore succeeds");
    assert_eq!(
        first_restart.step().expect("known outcome completes"),
        StepResult::Completed {
            task_id: id("task"),
            attempt_id,
        }
    );
    let completed = first_restart
        .durable_state()
        .expect("completed state loads");
    assert_eq!(completed.dispatch_history().len(), 1);
    assert_eq!(completed.completion_history().len(), 1);
    drop(first_restart);

    let mut second_restart = Runtime::<FileDurableStore>::restore_run(
        run_id,
        workflow,
        capability_graph::Scope::root(),
        [(
            id("task"),
            TaskConfig::new().with_effect(operation_id, EffectSemantics::Idempotent),
        )],
        FileDurableStore::open(&temp.path).expect("physical store reopens again"),
    )
    .expect("second cold restore succeeds");
    assert_eq!(
        second_restart.step().expect("completed task is idle"),
        StepResult::Idle
    );
    let replayed = second_restart
        .durable_state()
        .expect("replayed state loads");
    assert_eq!(replayed.dispatch_history().len(), 1);
    assert_eq!(replayed.completion_history().len(), 1);
}

#[test]
fn runtime_reopens_physical_store_before_and_after_dispatch_boundaries() {
    let temp = TempStore::new("dispatch-windows");
    let run_id = RunId::new("physical-runtime-dispatch-windows").expect("run id is valid");
    let operation_id = operation("physical-dispatch-operation");
    let workflow = single_task_workflow("task");
    let mut runtime = Runtime::<FileDurableStore>::start_run_with_store(
        run_id.clone(),
        workflow.clone(),
        capability_graph::Scope::root(),
        [(
            id("task"),
            TaskConfig::new().with_effect(operation_id.clone(), EffectSemantics::Idempotent),
        )],
        FileDurableStore::open(&temp.path).expect("physical store opens"),
    )
    .expect("runtime starts");
    let admitted_attempt = match runtime.step().expect("attempt admits") {
        StepResult::EffectPending { attempt_id, .. } => attempt_id,
        other => panic!("expected pending effect, got {other:?}"),
    };
    drop(runtime);

    let mut before_dispatch = Runtime::<FileDurableStore>::restore_run(
        run_id.clone(),
        workflow.clone(),
        capability_graph::Scope::root(),
        [(
            id("task"),
            TaskConfig::new().with_effect(operation_id.clone(), EffectSemantics::Idempotent),
        )],
        FileDurableStore::open(&temp.path).expect("physical store reopens before dispatch"),
    )
    .expect("restore before dispatch succeeds");
    assert_eq!(
        before_dispatch
            .step()
            .expect("admitted attempt remains pending"),
        StepResult::EffectPending {
            task_id: id("task"),
            attempt_id: admitted_attempt.clone(),
            operation_id: operation_id.clone(),
        }
    );
    drop(before_dispatch);

    let mut after_dispatch = Runtime::<FileDurableStore>::restore_run(
        run_id.clone(),
        workflow.clone(),
        capability_graph::Scope::root(),
        [(
            id("task"),
            TaskConfig::new().with_effect(operation_id.clone(), EffectSemantics::Idempotent),
        )],
        FileDurableStore::open(&temp.path).expect("physical store reopens for dispatch"),
    )
    .expect("restore for dispatch succeeds");
    after_dispatch
        .dispatch_effect(&operation_id)
        .expect("dispatch commits");
    drop(after_dispatch);

    let mut unknown_outcome = Runtime::<FileDurableStore>::restore_run(
        run_id.clone(),
        workflow,
        capability_graph::Scope::root(),
        [(
            id("task"),
            TaskConfig::new().with_effect(operation_id.clone(), EffectSemantics::Idempotent),
        )],
        FileDurableStore::open(&temp.path).expect("physical store reopens after dispatch"),
    )
    .expect("restore after dispatch succeeds");
    assert_eq!(
        unknown_outcome
            .step()
            .expect("unknown outcome blocks safely"),
        StepResult::Blocked {
            task_id: id("task"),
            operation_id: Some(operation_id.clone()),
            action: RecoveryAction::RetrySameOperation,
        }
    );
    let retried_attempt = unknown_outcome
        .dispatch_effect(&operation_id)
        .expect("idempotent retry commits");
    assert_ne!(retried_attempt, admitted_attempt);
    let state = unknown_outcome
        .durable_state()
        .expect("physical state loads");
    assert_eq!(state.dispatches(&operation_id).count(), 2);
}

#[test]
fn runtime_reopens_physical_no_effect_completion_and_dependency_chain() {
    let temp = TempStore::new("chain");
    let first_task = id("first");
    let second_task = id("second");
    let run_id = RunId::new("physical-runtime-chain").expect("run id is valid");
    let mut workflow = WorkflowGraph::default();
    workflow
        .apply_batch(
            workflow.revision(),
            [
                WorkflowMutation::AddTask {
                    task: Task {
                        id: first_task.clone(),
                        label: "first".to_owned(),
                    },
                },
                WorkflowMutation::AddTask {
                    task: Task {
                        id: second_task.clone(),
                        label: "second".to_owned(),
                    },
                },
                WorkflowMutation::AddDependency {
                    task_id: second_task.clone(),
                    dependency_id: first_task.clone(),
                },
            ],
        )
        .expect("workflow is valid");
    let mut runtime = Runtime::<FileDurableStore>::start_run_with_store(
        run_id.clone(),
        workflow.clone(),
        capability_graph::Scope::root(),
        std::iter::empty::<(Id, TaskConfig)>(),
        FileDurableStore::open(&temp.path).expect("physical store opens"),
    )
    .expect("runtime starts");
    assert!(matches!(
        runtime.step().expect("first no-effect task completes"),
        StepResult::Completed { ref task_id, .. } if task_id == &first_task
    ));
    drop(runtime);

    let mut restart = Runtime::<FileDurableStore>::restore_run(
        run_id.clone(),
        workflow.clone(),
        capability_graph::Scope::root(),
        std::iter::empty::<(Id, TaskConfig)>(),
        FileDurableStore::open(&temp.path).expect("physical store reopens"),
    )
    .expect("cold restore succeeds");
    assert!(restart.workflow().is_completed(&first_task));
    assert!(matches!(
        restart.step().expect("dependent task becomes ready"),
        StepResult::Completed { ref task_id, .. } if task_id == &second_task
    ));
    drop(restart);

    let mut final_restart = Runtime::<FileDurableStore>::restore_run(
        run_id,
        workflow,
        capability_graph::Scope::root(),
        std::iter::empty::<(Id, TaskConfig)>(),
        FileDurableStore::open(&temp.path).expect("physical store reopens finally"),
    )
    .expect("second cold restore succeeds");
    assert_eq!(
        final_restart.step().expect("completed chain is idle"),
        StepResult::Idle
    );
    let state = final_restart.durable_state().expect("chain state loads");
    assert_eq!(state.completion_history().len(), 2);
}

#[test]
fn runtime_restore_rejects_corrupt_physical_state_before_recovery() {
    let temp = TempStore::new("corruption");
    let run_id = RunId::new("physical-runtime-corruption").expect("run id is valid");
    let workflow = single_task_workflow("task");
    let runtime = Runtime::<FileDurableStore>::start_run_with_store(
        run_id.clone(),
        workflow.clone(),
        capability_graph::Scope::root(),
        std::iter::empty::<(Id, TaskConfig)>(),
        FileDurableStore::open(&temp.path).expect("physical store opens"),
    )
    .expect("runtime starts");
    let store = runtime.store().clone();
    drop(runtime);

    fs::write(&temp.path, b"not a redb database").expect("test corrupts backend");
    let restored = Runtime::<FileDurableStore>::restore_run(
        run_id,
        workflow,
        capability_graph::Scope::root(),
        std::iter::empty::<(Id, TaskConfig)>(),
        store,
    );

    assert!(matches!(
        restored,
        Err(RuntimeError::Store(StoreError::DataCorruption(_)))
    ));
}

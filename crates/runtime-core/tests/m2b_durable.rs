#![allow(missing_docs)]

use capability_graph::{CapabilityDefinition, CapabilityValue, Scope};
use kernis_core::Id;
use runtime_core::{Cancellation, RunId, Runtime, RuntimeError, StepResult, TaskConfig};
use std::sync::{Arc, Mutex};
use workflow_graph::{Task, WorkflowGraph, WorkflowMutation};
use workflow_recovery::{
    AttemptAdmission, AttemptId, CommitRequest, CommitResult, CompletionRecord, DurableMutation,
    DurableStore, EffectSemantics, InMemoryDurableStore, KnownEffectOutcome, OperationId,
    RecoveredEffectState, RecoveryAction, StoreError, StoreRevision,
};

fn id(value: &str) -> Id {
    Id::new(value).expect("test id is valid")
}

fn operation(value: &str) -> OperationId {
    OperationId::new(value).expect("test operation is valid")
}

fn workflow() -> WorkflowGraph {
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

fn config(operation_id: &OperationId) -> TaskConfig {
    TaskConfig::new().with_effect(operation_id.clone(), EffectSemantics::Idempotent)
}

#[derive(Debug, Default)]
struct StoreObservations {
    creates: Vec<RunId>,
    loads: Vec<RunId>,
    commits: Vec<Vec<DurableMutation>>,
}

#[derive(Clone, Debug)]
struct ObservingStore {
    inner: InMemoryDurableStore,
    observations: Arc<Mutex<StoreObservations>>,
}

#[derive(Clone, Debug)]
struct FailingStore {
    error: StoreError,
}

impl DurableStore for FailingStore {
    fn create_run(&mut self, _run_id: RunId) -> Result<StoreRevision, StoreError> {
        Ok(StoreRevision::INITIAL)
    }

    fn load_run(&self, _run_id: &RunId) -> Result<workflow_recovery::DurableRunState, StoreError> {
        Err(self.error.clone())
    }

    fn commit(&mut self, _request: CommitRequest) -> Result<CommitResult, StoreError> {
        Err(self.error.clone())
    }
}

#[derive(Clone, Debug)]
struct CompletionFailingStore {
    inner: InMemoryDurableStore,
    error: StoreError,
}

impl CompletionFailingStore {
    fn new(error: StoreError) -> Self {
        Self {
            inner: InMemoryDurableStore::new(),
            error,
        }
    }
}

impl DurableStore for CompletionFailingStore {
    fn create_run(&mut self, run_id: RunId) -> Result<StoreRevision, StoreError> {
        self.inner.create_run(run_id)
    }

    fn load_run(&self, run_id: &RunId) -> Result<workflow_recovery::DurableRunState, StoreError> {
        self.inner.load_run(run_id)
    }

    fn commit(&mut self, request: CommitRequest) -> Result<CommitResult, StoreError> {
        if request
            .mutations
            .iter()
            .any(|mutation| matches!(mutation, DurableMutation::RecordCompletion(_)))
        {
            return Err(self.error.clone());
        }
        self.inner.commit(request)
    }
}

impl ObservingStore {
    fn new() -> Self {
        Self {
            inner: InMemoryDurableStore::new(),
            observations: Arc::new(Mutex::new(StoreObservations::default())),
        }
    }
}

impl DurableStore for ObservingStore {
    fn create_run(&mut self, run_id: RunId) -> Result<StoreRevision, StoreError> {
        self.observations
            .lock()
            .expect("store observations are not poisoned")
            .creates
            .push(run_id.clone());
        self.inner.create_run(run_id)
    }

    fn load_run(&self, run_id: &RunId) -> Result<workflow_recovery::DurableRunState, StoreError> {
        self.observations
            .lock()
            .expect("store observations are not poisoned")
            .loads
            .push(run_id.clone());
        self.inner.load_run(run_id)
    }

    fn commit(&mut self, request: CommitRequest) -> Result<CommitResult, StoreError> {
        self.observations
            .lock()
            .expect("store observations are not poisoned")
            .commits
            .push(request.mutations.clone());
        self.inner.commit(request)
    }
}

fn start(workflow: WorkflowGraph, scope: Scope, operation_id: &OperationId) -> Runtime {
    Runtime::start_run(
        RunId::new("run-1").expect("run id is valid"),
        workflow,
        scope,
        [(id("task"), config(operation_id))],
    )
    .expect("runtime starts")
}

#[test]
fn crash_after_admission_reuses_the_admitted_attempt_without_dispatch() {
    let operation_id = operation("operation");
    let workflow = workflow();
    let mut runtime = start(workflow.clone(), Scope::root(), &operation_id);
    let first = match runtime.step().expect("attempt admission succeeds") {
        StepResult::EffectPending { attempt_id, .. } => attempt_id,
        other => panic!("expected pending effect, got {other:?}"),
    };
    let durable = runtime.store().clone();
    let state = runtime.durable_state().expect("durable state loads");
    assert_eq!(state.attempts().count(), 1);
    assert!(state.dispatch_history().is_empty());

    let mut restarted = Runtime::restore_run(
        RunId::new("run-1").expect("run id is valid"),
        workflow,
        Scope::root(),
        [(id("task"), config(&operation_id))],
        durable,
    )
    .expect("restart reconstructs");
    assert_eq!(restarted.attempts().len(), 1);
    assert_eq!(
        match restarted.step().expect("admitted attempt is explainable") {
            StepResult::EffectPending { attempt_id, .. } => attempt_id,
            other => panic!("expected pending effect, got {other:?}"),
        },
        first
    );
    assert_eq!(restarted.dispatch_effect(&operation_id), Ok(first));
}

#[test]
fn cancellation_after_admission_survives_restart_and_blocks_dispatch() {
    let operation_id = operation("operation");
    let workflow = workflow();
    let mut runtime = start(workflow.clone(), Scope::root(), &operation_id);
    let admitted = match runtime.step().expect("attempt admission succeeds") {
        StepResult::EffectPending { attempt_id, .. } => attempt_id,
        other => panic!("expected pending effect, got {other:?}"),
    };
    assert_eq!(
        runtime
            .cancel_task(&id("task"))
            .expect("cancellation commits"),
        Cancellation::NotDispatched {
            task_id: id("task")
        }
    );
    let state = runtime.durable_state().expect("durable state loads");
    assert!(state.attempt(&admitted).is_some());
    assert!(state.is_cancelled(&id("task")));

    let mut restarted = Runtime::restore_run(
        RunId::new("run-1").expect("run id is valid"),
        workflow,
        Scope::root(),
        [(id("task"), config(&operation_id))],
        runtime.store().clone(),
    )
    .expect("restart reconstructs");
    assert_eq!(
        restarted.step().expect("cancelled task is skipped"),
        StepResult::Idle
    );
    assert!(matches!(
        restarted.dispatch_effect(&operation_id),
        Err(runtime_core::RuntimeError::CancelledBeforeDispatch(_))
    ));
}

#[test]
fn outcome_before_restart_completes_without_a_second_dispatch() {
    let operation_id = operation("operation");
    let workflow = workflow();
    let mut runtime = start(workflow.clone(), Scope::root(), &operation_id);
    let attempt_id = match runtime.step().expect("attempt admission succeeds") {
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
    let mut restarted = Runtime::restore_run(
        RunId::new("run-1").expect("run id is valid"),
        workflow,
        Scope::root(),
        [(id("task"), config(&operation_id))],
        runtime.store().clone(),
    )
    .expect("restart reconstructs");
    assert_eq!(
        restarted.step().expect("known outcome completes workflow"),
        StepResult::Completed {
            task_id: id("task"),
            attempt_id,
        }
    );
    assert_eq!(
        restarted
            .store()
            .load_run(&RunId::new("run-1").unwrap())
            .unwrap()
            .dispatch_history()
            .len(),
        1
    );
    assert_eq!(
        restarted.step().expect("completion is not replayed"),
        StepResult::Idle
    );
}

#[test]
fn no_effect_completion_survives_a_cold_restart_and_unblocks_dependents() {
    let first_task = id("first-task");
    let second_task = id("second-task");
    let third_task = id("third-task");
    let mut workflow = WorkflowGraph::default();
    workflow
        .apply_batch(
            workflow.revision(),
            [
                WorkflowMutation::AddTask {
                    task: Task {
                        id: first_task.clone(),
                        label: "first task".to_owned(),
                    },
                },
                WorkflowMutation::AddTask {
                    task: Task {
                        id: second_task.clone(),
                        label: "second task".to_owned(),
                    },
                },
                WorkflowMutation::AddTask {
                    task: Task {
                        id: third_task.clone(),
                        label: "third task".to_owned(),
                    },
                },
                WorkflowMutation::AddDependency {
                    task_id: second_task.clone(),
                    dependency_id: first_task.clone(),
                },
                WorkflowMutation::AddDependency {
                    task_id: third_task.clone(),
                    dependency_id: second_task.clone(),
                },
            ],
        )
        .expect("workflow is valid");
    let run_id = RunId::new("no-effect-restart").expect("run id is valid");
    let mut runtime = Runtime::start_run(
        run_id.clone(),
        workflow.clone(),
        Scope::root(),
        std::iter::empty::<(Id, TaskConfig)>(),
    )
    .expect("runtime starts");

    assert!(matches!(
        runtime.step().expect("first task completes"),
        StepResult::Completed { ref task_id, .. } if task_id == &first_task
    ));

    let mut restarted = Runtime::restore_run(
        run_id,
        workflow,
        Scope::root(),
        std::iter::empty::<(Id, TaskConfig)>(),
        runtime.store().clone(),
    )
    .expect("cold restart reconstructs");

    assert!(restarted.workflow().is_completed(&first_task));
    assert!(matches!(
        restarted.step().expect("dependent task becomes ready"),
        StepResult::Completed { ref task_id, .. } if task_id == &second_task
    ));
    assert!(matches!(
        restarted.step().expect("third task becomes ready"),
        StepResult::Completed { ref task_id, .. } if task_id == &third_task
    ));
}

#[test]
fn completion_is_not_repeated_after_two_cold_restarts() {
    let operation_id = operation("completion-replay-operation");
    let workflow = workflow();
    let run_id = RunId::new("completion-replay").expect("run id is valid");
    let mut runtime = Runtime::start_run(
        run_id.clone(),
        workflow.clone(),
        Scope::root(),
        [(id("task"), config(&operation_id))],
    )
    .expect("runtime starts");
    let attempt_id = match runtime.step().expect("attempt admission succeeds") {
        StepResult::EffectPending { attempt_id, .. } => attempt_id,
        other => panic!("expected pending effect, got {other:?}"),
    };
    runtime
        .dispatch_effect(&operation_id)
        .expect("dispatch commits");
    runtime
        .record_effect_outcome(&operation_id, attempt_id, KnownEffectOutcome::Succeeded)
        .expect("outcome commits");

    let mut first_restart = Runtime::restore_run(
        run_id.clone(),
        workflow.clone(),
        Scope::root(),
        [(id("task"), config(&operation_id))],
        runtime.store().clone(),
    )
    .expect("first cold restart reconstructs");
    assert!(matches!(
        first_restart
            .step()
            .expect("known success commits completion"),
        StepResult::Completed { .. }
    ));
    let committed = first_restart.durable_state().expect("completion loads");
    assert_eq!(committed.completion_history().len(), 1);
    let mut second_restart = Runtime::restore_run(
        run_id,
        workflow,
        Scope::root(),
        [(id("task"), config(&operation_id))],
        first_restart.store().clone(),
    )
    .expect("second cold restart reconstructs");

    assert!(second_restart.workflow().is_completed(&id("task")));
    assert_eq!(
        second_restart.step().expect("completed task is idle"),
        StepResult::Idle
    );
    let replayed = second_restart.durable_state().expect("completion loads");
    assert_eq!(replayed.completion_history().len(), 1);
    assert_eq!(replayed.dispatch_history().len(), 1);
}

#[test]
fn supplied_topology_or_configuration_mismatch_fails_closed() {
    let operation_id = operation("identity-operation");
    let run_id = RunId::new("identity-run").expect("run id is valid");
    let workflow = workflow();
    let runtime = Runtime::start_run(
        run_id.clone(),
        workflow.clone(),
        Scope::root(),
        [(id("task"), config(&operation_id))],
    )
    .expect("runtime starts");

    let mut changed_topology = workflow.clone();
    changed_topology
        .apply_batch(
            changed_topology.revision(),
            [WorkflowMutation::AddTask {
                task: Task {
                    id: id("extra-task"),
                    label: "extra task".to_owned(),
                },
            }],
        )
        .expect("changed topology is valid");
    let topology_result = Runtime::restore_run(
        run_id.clone(),
        changed_topology,
        Scope::root(),
        [(id("task"), config(&operation_id))],
        runtime.store().clone(),
    );
    assert!(matches!(
        topology_result,
        Err(RuntimeError::WorkflowReplayIdentityMismatch { .. })
    ));

    let changed_operation = operation("different-operation");
    let configuration_result = Runtime::restore_run(
        run_id,
        workflow,
        Scope::root(),
        [(id("task"), config(&changed_operation))],
        runtime.store().clone(),
    );
    assert!(matches!(
        configuration_result,
        Err(RuntimeError::WorkflowReplayIdentityMismatch { .. })
    ));
}

#[test]
fn replay_identity_transitions_are_not_replayed_from_an_old_revision() {
    let operation_a = operation("identity-operation-a");
    let operation_b = operation("identity-operation-b");
    let run_id = RunId::new("identity-transition-run").expect("run id is valid");
    let workflow = workflow();
    let mut runtime = Runtime::start_run(
        run_id.clone(),
        workflow.clone(),
        Scope::root(),
        [(id("task"), config(&operation_a))],
    )
    .expect("runtime starts");

    runtime
        .configure_task(id("task"), config(&operation_b))
        .expect("identity changes to B");
    runtime
        .configure_task(id("task"), config(&operation_a))
        .expect("identity changes back to A");
    runtime
        .configure_task(id("task"), config(&operation_b))
        .expect("identity changes to B again");

    Runtime::restore_run(
        run_id,
        workflow,
        Scope::root(),
        [(id("task"), config(&operation_b))],
        runtime.store().clone(),
    )
    .expect("latest identity is durable");
}

#[test]
fn starting_from_local_completion_facts_fails_closed() {
    let task_id = id("task");
    let mut completed_workflow = workflow();
    completed_workflow
        .complete(&task_id)
        .expect("test workflow completes");
    let result = Runtime::start_run(
        RunId::new("precompleted-run").expect("run id is valid"),
        completed_workflow,
        Scope::root(),
        std::iter::empty::<(Id, TaskConfig)>(),
    );
    assert!(matches!(
        result,
        Err(RuntimeError::PrecompletedWorkflow(id)) if id == task_id
    ));
}

#[test]
fn completion_replay_rejects_an_invalid_prerequisite_order() {
    let first_task = id("first-task");
    let second_task = id("second-task");
    let mut workflow = WorkflowGraph::default();
    workflow
        .apply_batch(
            workflow.revision(),
            [
                WorkflowMutation::AddTask {
                    task: Task {
                        id: first_task.clone(),
                        label: "first task".to_owned(),
                    },
                },
                WorkflowMutation::AddTask {
                    task: Task {
                        id: second_task.clone(),
                        label: "second task".to_owned(),
                    },
                },
                WorkflowMutation::AddDependency {
                    task_id: second_task.clone(),
                    dependency_id: first_task.clone(),
                },
            ],
        )
        .expect("workflow is valid");
    let run_id = RunId::new("invalid-completion-order").expect("run id is valid");
    let runtime = Runtime::start_run(
        run_id.clone(),
        workflow.clone(),
        Scope::root(),
        std::iter::empty::<(Id, TaskConfig)>(),
    )
    .expect("runtime starts");
    let mut store = runtime.store().clone();
    let first_attempt = AttemptId::new("first-attempt").expect("attempt is valid");
    let second_attempt = AttemptId::new("second-attempt").expect("attempt is valid");
    let revision = store.load_run(&run_id).expect("state loads").revision();
    store
        .commit(CommitRequest {
            run_id: run_id.clone(),
            expected_revision: revision,
            idempotency_key: workflow_recovery::IdempotencyKey::new("admit-chain")
                .expect("key is valid"),
            mutations: vec![
                DurableMutation::AdmitAttempt(AttemptAdmission {
                    run_id: run_id.clone(),
                    task_id: first_task.clone(),
                    attempt_id: first_attempt.clone(),
                    operation_id: None,
                    capabilities: Vec::new(),
                }),
                DurableMutation::AdmitAttempt(AttemptAdmission {
                    run_id: run_id.clone(),
                    task_id: second_task.clone(),
                    attempt_id: second_attempt.clone(),
                    operation_id: None,
                    capabilities: Vec::new(),
                }),
            ],
        })
        .expect("attempt admissions commit");
    let revision = store.load_run(&run_id).expect("state loads").revision();
    store
        .commit(CommitRequest::single(
            run_id.clone(),
            revision,
            workflow_recovery::IdempotencyKey::new("complete-second-first").expect("key is valid"),
            DurableMutation::RecordCompletion(CompletionRecord {
                task_id: second_task,
                attempt_id: second_attempt,
            }),
        ))
        .expect("store retains facts without topology semantics");

    let result = Runtime::restore_run(
        run_id,
        workflow,
        Scope::root(),
        std::iter::empty::<(Id, TaskConfig)>(),
        store,
    );
    assert!(matches!(
        result,
        Err(RuntimeError::Store(StoreError::InvariantViolation(
            workflow_recovery::StoreInvariant::CompletionPrerequisiteMismatch { .. }
        )))
    ));
}

#[test]
fn backend_failures_remain_classifiable_through_runtime_error() {
    let run_id = RunId::new("failing-run").expect("run id is valid");
    let workflow = workflow();
    for error in [
        StoreError::BackendUnavailable,
        StoreError::IoFailure("read failed".to_owned()),
        StoreError::DataCorruption("invalid durable bytes".to_owned()),
    ] {
        let expected_kind = error.kind();
        let result = Runtime::<FailingStore>::restore_run(
            run_id.clone(),
            workflow.clone(),
            Scope::root(),
            std::iter::empty::<(Id, TaskConfig)>(),
            FailingStore { error },
        );
        match result {
            Err(RuntimeError::Store(actual)) => assert_eq!(actual.kind(), expected_kind),
            _ => panic!("expected classified store error"),
        }
    }

    let result = Runtime::<FailingStore>::start_run_with_store(
        RunId::new("failing-start").expect("run id is valid"),
        workflow,
        Scope::root(),
        std::iter::empty::<(Id, TaskConfig)>(),
        FailingStore {
            error: StoreError::BackendUnavailable,
        },
    );
    assert!(matches!(
        result,
        Err(RuntimeError::Store(StoreError::BackendUnavailable))
    ));
}

#[test]
fn completion_commit_backend_failure_prevents_local_completion_and_observation() {
    let operation_id = operation("completion-commit-failure");
    let run_id = RunId::new("completion-commit-failure-run").expect("run id is valid");
    let workflow = workflow();
    let mut runtime = Runtime::start_run_with_store(
        run_id.clone(),
        workflow,
        Scope::root(),
        [(id("task"), config(&operation_id))],
        CompletionFailingStore::new(StoreError::DataCorruption(
            "completion write rejected".to_owned(),
        )),
    )
    .expect("runtime starts before completion failure");
    let attempt_id = match runtime.step().expect("attempt admits") {
        StepResult::EffectPending { attempt_id, .. } => attempt_id,
        other => panic!("expected pending effect, got {other:?}"),
    };
    runtime
        .dispatch_effect(&operation_id)
        .expect("dispatch commits");
    runtime
        .record_effect_outcome(&operation_id, attempt_id, KnownEffectOutcome::Succeeded)
        .expect("outcome commits");

    let result = runtime.step();
    assert!(matches!(
        result,
        Err(RuntimeError::Store(StoreError::DataCorruption(_)))
    ));
    assert!(!runtime.workflow().is_completed(&id("task")));
    assert!(
        runtime
            .drain_execution_events()
            .iter()
            .all(|item| !matches!(
                item.payload,
                runtime_core::RuntimeEvent::TaskCompleted { .. }
            ))
    );
    assert!(
        runtime
            .durable_state()
            .expect("state loads")
            .completion_history()
            .is_empty()
    );
}

#[test]
fn completion_commit_is_present_before_task_completed_can_be_observed() {
    let run_id = RunId::new("completion-order-run").expect("run id is valid");
    let store = ObservingStore::new();
    let observations = store.observations.clone();
    let mut runtime = Runtime::<ObservingStore>::start_run_with_store(
        run_id,
        workflow(),
        Scope::root(),
        std::iter::empty::<(Id, TaskConfig)>(),
        store,
    )
    .expect("runtime starts");

    assert!(matches!(
        runtime.step().expect("task completes"),
        StepResult::Completed { .. }
    ));
    let commits = observations
        .lock()
        .expect("store observations are not poisoned")
        .commits
        .clone();
    assert!(commits.iter().any(|mutations| {
        mutations
            .iter()
            .any(|mutation| matches!(mutation, DurableMutation::RecordCompletion(_)))
    }));
    assert!(runtime.drain_execution_events().iter().any(|item| matches!(
        &item.payload,
        runtime_core::RuntimeEvent::TaskCompleted { .. }
    )));
}

#[test]
fn retry_keeps_operation_identity_and_retains_both_attempts() {
    let operation_id = operation("operation");
    let mut runtime = start(workflow(), Scope::root(), &operation_id);
    let first = match runtime.step().expect("attempt admission succeeds") {
        StepResult::EffectPending { attempt_id, .. } => attempt_id,
        other => panic!("expected pending effect, got {other:?}"),
    };
    runtime
        .dispatch_effect(&operation_id)
        .expect("first dispatch");
    assert_eq!(
        runtime.recover(&operation_id).unwrap().action,
        RecoveryAction::RetrySameOperation
    );
    let second = runtime
        .dispatch_effect(&operation_id)
        .expect("retry dispatch");
    assert_ne!(first, second);
    let state = runtime.durable_state().expect("durable state loads");
    assert_eq!(state.attempts().count(), 2);
    assert_eq!(state.dispatches(&operation_id).count(), 2);
    assert_eq!(
        state.effect_state(&operation_id),
        RecoveredEffectState::OutcomeUnknown
    );
}

#[test]
fn late_older_outcome_does_not_change_latest_dispatch_authority() {
    let operation_id = operation("operation");
    let mut runtime = start(workflow(), Scope::root(), &operation_id);
    let first = match runtime.step().expect("attempt admission succeeds") {
        StepResult::EffectPending { attempt_id, .. } => attempt_id,
        other => panic!("expected pending effect, got {other:?}"),
    };
    runtime
        .dispatch_effect(&operation_id)
        .expect("first dispatch");
    let second = runtime
        .dispatch_effect(&operation_id)
        .expect("retry dispatch");
    runtime
        .record_effect_outcome(&operation_id, first, KnownEffectOutcome::Failed)
        .expect("late outcome is retained");
    assert_eq!(
        runtime
            .journal()
            .latest_dispatch(&operation_id)
            .unwrap()
            .attempt_id,
        second
    );
    assert_eq!(
        runtime.journal().state(&operation_id),
        RecoveredEffectState::OutcomeUnknown
    );
    assert_eq!(
        runtime.recover(&operation_id).unwrap().action,
        RecoveryAction::RetrySameOperation
    );
}

#[test]
fn capability_replay_identity_survives_new_runtime_entry_identity() {
    let provider_id = id("provider");
    let make_scope = |value: &str| {
        let scope = Scope::root();
        let handle = scope
            .provide(
                CapabilityDefinition::new(provider_id.clone(), "provider")
                    .with_replay_identity("provider-v1"),
                |_| Ok(CapabilityValue::from_value(value.to_owned())),
            )
            .expect("provider is admitted");
        (scope, handle.entry_id())
    };
    let (scope, first_entry) = make_scope("v1");
    let operation_id = operation("operation");
    let workflow = workflow();
    let config = TaskConfig::new()
        .require_capability_with_identity(provider_id.clone(), "provider-v1")
        .with_effect(operation_id.clone(), EffectSemantics::Idempotent);
    let mut runtime = Runtime::start_run(
        RunId::new("run-1").unwrap(),
        workflow.clone(),
        scope,
        [(id("task"), config.clone())],
    )
    .unwrap();
    runtime.step().unwrap();
    let (new_scope, second_entry) = make_scope("v1");
    assert_ne!(first_entry, second_entry);
    let restarted = Runtime::restore_run(
        RunId::new("run-1").unwrap(),
        workflow,
        new_scope,
        [(id("task"), config)],
        runtime.store().clone(),
    )
    .expect("stable replay identity validates");
    assert_ne!(
        runtime.attempts()[0]
            .capability(&provider_id)
            .unwrap()
            .entry_id,
        restarted.attempts()[0]
            .capability(&provider_id)
            .unwrap()
            .entry_id
    );
    assert_eq!(
        runtime.attempts()[0]
            .capability(&provider_id)
            .unwrap()
            .replay_identity,
        restarted.attempts()[0]
            .capability(&provider_id)
            .unwrap()
            .replay_identity
    );
}

#[test]
fn capability_replay_identity_mismatch_fails_closed_on_restart() {
    let provider_id = id("provider");
    let make_scope = |replay_identity: &str| {
        let scope = Scope::root();
        scope
            .provide(
                CapabilityDefinition::new(provider_id.clone(), "provider")
                    .with_replay_identity(replay_identity),
                |_| Ok(CapabilityValue::from_value("v1".to_owned())),
            )
            .expect("provider is admitted");
        scope
    };
    let operation_id = operation("operation");
    let workflow = workflow();
    let config = TaskConfig::new()
        .require_capability_with_identity(provider_id.clone(), "provider-v1")
        .with_effect(operation_id.clone(), EffectSemantics::Idempotent);
    let mut runtime = Runtime::start_run(
        RunId::new("run-1").unwrap(),
        workflow.clone(),
        make_scope("provider-v1"),
        [(id("task"), config.clone())],
    )
    .unwrap();
    runtime.step().unwrap();
    let result = Runtime::restore_run(
        RunId::new("run-1").unwrap(),
        workflow,
        make_scope("provider-v2"),
        [(id("task"), config)],
        runtime.store().clone(),
    );
    assert!(matches!(
        result,
        Err(runtime_core::RuntimeError::CapabilityReplayMismatch { .. })
    ));
}

#[test]
fn cancellation_after_dispatch_keeps_outcome_recordable() {
    let operation_id = operation("operation");
    let mut runtime = start(workflow(), Scope::root(), &operation_id);
    let attempt_id = match runtime.step().unwrap() {
        StepResult::EffectPending { attempt_id, .. } => attempt_id,
        other => panic!("expected pending effect, got {other:?}"),
    };
    runtime.dispatch_effect(&operation_id).unwrap();
    assert!(matches!(
        runtime.cancel_task(&id("task")).unwrap(),
        Cancellation::AlreadyDispatched { .. }
    ));
    runtime
        .record_effect_outcome(&operation_id, attempt_id, KnownEffectOutcome::Succeeded)
        .unwrap();
    assert_eq!(
        runtime.recover(&operation_id).unwrap().action,
        RecoveryAction::CompleteWithoutReexecution
    );
}

#[test]
fn injected_store_receives_mutations_and_supports_fresh_restore() {
    let operation_id = operation("injected-operation");
    let run_id = RunId::new("injected-run").expect("run id is valid");
    let workflow = workflow();
    let store = ObservingStore::new();
    let mut runtime = Runtime::<ObservingStore>::start_run_with_store(
        run_id.clone(),
        workflow.clone(),
        Scope::root(),
        [(id("task"), config(&operation_id))],
        store,
    )
    .expect("runtime starts with injected store");

    let attempt_id = match runtime.step().expect("attempt admission succeeds") {
        StepResult::EffectPending { attempt_id, .. } => attempt_id,
        other => panic!("expected pending effect, got {other:?}"),
    };
    runtime
        .dispatch_effect(&operation_id)
        .expect("dispatch commits through injected store");

    let state = runtime.durable_state().expect("durable state loads");
    assert_eq!(
        state.attempt(&attempt_id).unwrap().operation_id,
        Some(operation_id.clone())
    );
    let observations = runtime
        .store()
        .observations
        .lock()
        .expect("store observations are not poisoned");
    assert_eq!(observations.creates, vec![run_id.clone()]);
    assert!(observations.loads.iter().any(|loaded| loaded == &run_id));
    assert!(observations.commits.iter().any(|mutations| {
        mutations
            .iter()
            .any(|mutation| matches!(mutation, DurableMutation::RecordIntent(_)))
    }));
    assert!(observations.commits.iter().any(|mutations| {
        mutations
            .iter()
            .any(|mutation| matches!(mutation, DurableMutation::AdmitAttempt(_)))
    }));
    assert!(observations.commits.iter().any(|mutations| {
        mutations
            .iter()
            .any(|mutation| matches!(mutation, DurableMutation::RecordDispatch(_)))
    }));
    drop(observations);

    let mut restarted = Runtime::<ObservingStore>::restore_run(
        run_id.clone(),
        workflow,
        Scope::root(),
        [(id("task"), config(&operation_id))],
        runtime.store().clone(),
    )
    .expect("fresh runtime restores through injected store");
    assert_eq!(restarted.attempts().len(), 1);
    assert_eq!(
        restarted
            .journal()
            .latest_dispatch(&operation_id)
            .unwrap()
            .attempt_id,
        attempt_id
    );
    assert_eq!(
        restarted.recover(&operation_id).unwrap().action,
        RecoveryAction::RetrySameOperation
    );
}

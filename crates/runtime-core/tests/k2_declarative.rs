#![allow(missing_docs)]

use capability_graph::{CapabilityValue, Scope};
use kernis_core::Id;
use runtime_core::{
    CapabilityDeclaration, CapabilityRequirement, DefinitionError, DefinitionIdentity,
    FactoryRegistry, FactoryResolutionError, LegacyMutationOperation, RunDefinition, Runtime,
    RuntimeError, StepResult, TaskConfig, TaskDefinition,
};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use workflow_graph::{Task, WorkflowGraph, WorkflowMutation};
use workflow_recovery::{
    AttemptAdmission, AttemptId, CommitRequest, CompletionRecord, DurableMutation, DurableStore,
    EffectSemantics, FileDurableStore, IdempotencyKey, KnownEffectOutcome, OperationId, StoreError,
    WorkflowReplayIdentity,
};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
const CHILD_PATH_ENV: &str = "KERNIS_K2_CHILD_PATH";
const LEGACY_K1_TASK_IDENTITY: &str =
    "kernis-workflow-replay-v1:5:tasks:4:task:4:task:4:task:6:config:9:no-effect:5:edges";

struct TempStore {
    directory: PathBuf,
    path: PathBuf,
}

impl TempStore {
    fn new(label: &str) -> Self {
        let number = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "kernis-k2-runtime-{label}-{}-{number}",
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

fn requirement(capability_id: &str, definition_identity: &str) -> CapabilityRequirement {
    CapabilityRequirement::new(id(capability_id), definition_identity)
}

fn provider_declaration(identity: &str) -> CapabilityDeclaration {
    CapabilityDeclaration::new(id("provider"), "provider", identity)
}

fn effect_definition(operation_id: &str) -> RunDefinition {
    RunDefinition::new()
        .with_capability(provider_declaration("provider-v1"))
        .with_task(
            TaskDefinition::new(id("task"), "task")
                .require_capability(requirement("provider", "provider-v1"))
                .with_effect(operation(operation_id), EffectSemantics::Idempotent),
        )
}

fn provider_registry(value: &str) -> FactoryRegistry {
    let mut registry = FactoryRegistry::new();
    let value = value.to_owned();
    registry
        .register(id("provider"), "provider-v1", move |_| {
            Ok(CapabilityValue::from_value(value.clone()))
        })
        .expect("provider factory registers");
    registry
}

fn workflow_with_task() -> WorkflowGraph {
    workflow_with_task_label("task")
}

fn workflow_with_task_label(label: &str) -> WorkflowGraph {
    let mut workflow = WorkflowGraph::default();
    workflow
        .apply_batch(
            workflow.revision(),
            [WorkflowMutation::AddTask {
                task: Task {
                    id: id("task"),
                    label: label.to_owned(),
                },
            }],
        )
        .expect("workflow is valid");
    workflow
}

#[test]
fn legacy_live_object_restore_keeps_main_identity_contract() {
    let runtime = Runtime::start_run(
        runtime_id(),
        workflow_with_task(),
        Scope::root(),
        std::iter::empty::<(Id, TaskConfig)>(),
    )
    .expect("runtime starts");
    let changed_label = Runtime::restore_run(
        runtime_id(),
        workflow_with_task_label("new display label"),
        Scope::root(),
        std::iter::empty::<(Id, TaskConfig)>(),
        runtime.store().clone(),
    );
    assert!(matches!(
        changed_label,
        Err(RuntimeError::WorkflowReplayIdentityMismatch { .. })
    ));

    let restored = Runtime::restore_run(
        runtime_id(),
        workflow_with_task(),
        Scope::root(),
        std::iter::empty::<(Id, TaskConfig)>(),
        runtime.store().clone(),
    )
    .expect("unchanged live-object definition restores");
    assert_eq!(
        restored
            .workflow()
            .task(&id("task"))
            .expect("task exists")
            .label,
        "task"
    );
}

#[test]
fn restore_run_accepts_fixed_main_legacy_identity_fixture() {
    let temp = TempStore::new("legacy-k1-fixture");
    let run_id = runtime_core::RunId::new("legacy-k1-run").expect("run id is valid");
    let attempt_id = AttemptId::new("legacy-k1-attempt").expect("attempt id is valid");
    let identity = WorkflowReplayIdentity::new(LEGACY_K1_TASK_IDENTITY)
        .expect("fixed legacy identity is valid");
    let mut store = FileDurableStore::open(&temp.path).expect("physical store opens");
    let initial_revision = store.create_run(run_id.clone()).expect("run creates");
    let identity_revision = store
        .commit(CommitRequest::single(
            run_id.clone(),
            initial_revision,
            IdempotencyKey::new("legacy-k1-identity").expect("idempotency key is valid"),
            DurableMutation::RecordWorkflowReplayIdentity(identity.clone()),
        ))
        .expect("legacy identity commits")
        .revision;
    let admission_revision = store
        .commit(CommitRequest::single(
            run_id.clone(),
            identity_revision,
            IdempotencyKey::new("legacy-k1-attempt").expect("idempotency key is valid"),
            DurableMutation::AdmitAttempt(AttemptAdmission {
                run_id: run_id.clone(),
                task_id: id("task"),
                attempt_id: attempt_id.clone(),
                operation_id: None,
                capabilities: Vec::new(),
            }),
        ))
        .expect("legacy attempt commits")
        .revision;
    store
        .commit(CommitRequest::single(
            run_id.clone(),
            admission_revision,
            IdempotencyKey::new("legacy-k1-completion").expect("idempotency key is valid"),
            DurableMutation::RecordCompletion(CompletionRecord {
                task_id: id("task"),
                attempt_id,
            }),
        ))
        .expect("legacy completion commits");
    drop(store);

    let mut restored = Runtime::<FileDurableStore>::restore_run(
        run_id,
        workflow_with_task(),
        Scope::root(),
        std::iter::empty::<(Id, TaskConfig)>(),
        FileDurableStore::open(&temp.path).expect("physical store reopens"),
    )
    .expect("current legacy restore accepts main fixture");
    assert_eq!(restored.attempts().len(), 1);
    assert_eq!(restored.workflow().completed_tasks().len(), 1);
    assert_eq!(
        restored
            .durable_state()
            .expect("legacy state loads")
            .workflow_replay_identity(),
        Some(&identity)
    );
    assert_eq!(
        restored.step().expect("completed legacy run is idle"),
        StepResult::Idle
    );
}

#[test]
fn canonical_identity_is_order_independent_and_excludes_display_labels() {
    let first = RunDefinition::new()
        .with_capability(CapabilityDeclaration::new(
            id("other"),
            "service",
            "other-v1",
        ))
        .with_capability(CapabilityDeclaration::new(
            id("service"),
            "service",
            "service-v1",
        ))
        .with_task(
            TaskDefinition::new(id("sink"), "sink label")
                .depends_on(id("source-b"))
                .depends_on(id("source-a"))
                .require_capability(requirement("other", "other-v1"))
                .require_capability(requirement("service", "service-v1"))
                .with_effect(operation("effect"), EffectSemantics::Idempotent),
        )
        .with_task(TaskDefinition::new(id("source-b"), "source b"))
        .with_task(TaskDefinition::new(id("source-a"), "source a"));
    let reordered = RunDefinition::new()
        .with_task(TaskDefinition::new(id("source-a"), "changed source label"))
        .with_task(
            TaskDefinition::new(id("sink"), "changed sink label")
                .depends_on(id("source-a"))
                .depends_on(id("source-b"))
                .require_capability(requirement("service", "service-v1"))
                .require_capability(requirement("other", "other-v1"))
                .with_effect(operation("effect"), EffectSemantics::Idempotent),
        )
        .with_task(TaskDefinition::new(id("source-b"), "another source label"))
        .with_capability(CapabilityDeclaration::new(
            id("service"),
            "service",
            "service-v1",
        ))
        .with_capability(CapabilityDeclaration::new(
            id("other"),
            "service",
            "other-v1",
        ));

    let first_identity = first.identity().expect("first definition validates");
    assert_eq!(
        first_identity,
        reordered.identity().expect("reordered validates")
    );
    assert!(
        first_identity
            .as_str()
            .starts_with("kernis-run-definition-v1")
    );

    let mut topology_changed = first.clone();
    topology_changed.tasks[0].dependencies.pop();
    assert_ne!(
        first_identity,
        topology_changed.identity().expect("topology remains valid")
    );

    let mut capability_changed = first.clone();
    capability_changed.capabilities[0].definition_identity =
        DefinitionIdentity::new("other-v2").expect("identity is valid");
    capability_changed.tasks[0].required_capabilities[0].definition_identity =
        DefinitionIdentity::new("other-v2").expect("identity is valid");
    assert_ne!(
        first_identity,
        capability_changed
            .identity()
            .expect("changed capability definition remains valid")
    );

    let mut effect_changed = first.clone();
    effect_changed.tasks[0]
        .effect
        .as_mut()
        .expect("effect exists")
        .semantics = EffectSemantics::NonIdempotent;
    assert_ne!(
        first_identity,
        effect_changed.identity().expect("effect remains valid")
    );
}

#[test]
fn duplicate_declarative_declarations_and_factory_identities_fail_typed() {
    let duplicate_task = RunDefinition::new()
        .with_task(TaskDefinition::new(id("same"), "one"))
        .with_task(TaskDefinition::new(id("same"), "two"));
    assert_eq!(
        duplicate_task.validate(),
        Err(DefinitionError::DuplicateTask(id("same")))
    );

    let duplicate_capability = RunDefinition::new()
        .with_capability(provider_declaration("provider-v1"))
        .with_capability(provider_declaration("provider-v2"));
    assert_eq!(
        duplicate_capability.validate(),
        Err(DefinitionError::DuplicateCapability(id("provider")))
    );

    let mut factories = FactoryRegistry::new();
    factories
        .register(id("provider"), "provider-v1", |_| {
            Ok(CapabilityValue::from_value(()))
        })
        .expect("first factory registers");
    assert!(matches!(
        factories.register(id("provider"), "provider-v1", |_| {
            Ok(CapabilityValue::from_value(()))
        }),
        Err(FactoryResolutionError::DuplicateDefinitionIdentity { .. })
    ));
}

#[test]
fn legacy_live_object_duplicate_task_configs_keep_main_behavior() {
    let mut runtime = Runtime::start_run(
        runtime_id(),
        workflow_with_task(),
        Scope::root(),
        [
            (
                id("task"),
                TaskConfig::new()
                    .with_effect(operation("legacy-first"), EffectSemantics::Idempotent),
            ),
            (
                id("task"),
                TaskConfig::new()
                    .with_effect(operation("legacy-second"), EffectSemantics::Idempotent),
            ),
        ],
    )
    .expect("legacy task configuration keeps last-write-wins behavior");
    assert!(matches!(
        runtime.step().expect("last task config drives the attempt"),
        StepResult::EffectPending { operation_id, .. } if operation_id == operation("legacy-second")
    ));
}

#[test]
fn missing_factory_fails_before_store_creation_or_task_observation() {
    let temp = TempStore::new("missing-factory");
    let store = FileDurableStore::open(&temp.path).expect("physical store opens");
    let result = Runtime::<FileDurableStore>::start_from_definition_with_store(
        runtime_id(),
        effect_definition("missing-factory-effect"),
        &FactoryRegistry::new(),
        store,
    );
    assert!(matches!(
        result,
        Err(RuntimeError::Factory(
            FactoryResolutionError::MissingFactory { .. }
        ))
    ));

    let reopened = FileDurableStore::open(&temp.path).expect("store reopens");
    assert!(matches!(
        reopened.load_run(&runtime_id()),
        Err(StoreError::RunNotFound(_))
    ));
}

#[test]
fn explicit_capability_dependencies_are_constructed_in_dependency_order() {
    let mut factories = FactoryRegistry::new();
    factories
        .register(id("base"), "base-v1", |_| {
            Ok(CapabilityValue::from_value("base".to_owned()))
        })
        .expect("base factory registers");
    factories
        .register(id("service"), "service-v1", |dependencies| {
            let base = dependencies
                .get(&id("base"))
                .expect("base dependency is present")
                .downcast_ref::<String>()
                .expect("base value is a string");
            Ok(CapabilityValue::from_value(format!("service:{base}")))
        })
        .expect("service factory registers");

    let definition = RunDefinition::new()
        .with_capability(
            CapabilityDeclaration::new(id("service"), "service", "service-v1")
                .depends_on(requirement("base", "base-v1")),
        )
        .with_capability(CapabilityDeclaration::new(id("base"), "service", "base-v1"))
        .with_task(
            TaskDefinition::new(id("task"), "task")
                .require_capability(requirement("service", "service-v1")),
        );
    let mut runtime = Runtime::start_from_definition(runtime_id(), definition, &factories)
        .expect("definition reconstructs");
    assert!(matches!(
        runtime.step().expect("task starts"),
        StepResult::Completed { .. }
    ));
    assert_eq!(
        runtime.attempts()[0]
            .capability(&id("service"))
            .expect("service is pinned")
            .handle()
            .downcast_ref::<String>(),
        Some(&"service:base".to_owned())
    );
}

#[test]
fn label_only_change_keeps_identity_but_changes_reconstructed_display_data() {
    let definition = effect_definition("label-effect");
    let mut changed_label = definition.clone();
    changed_label.tasks[0].label = "new display label".to_owned();
    let factories = provider_registry("provider-v1");
    let runtime = Runtime::start_from_definition(runtime_id(), definition, &factories)
        .expect("runtime starts");
    let restarted = Runtime::restore_from_definition(
        runtime_id(),
        changed_label,
        &factories,
        runtime.store().clone(),
    )
    .expect("label-only definition restores");
    assert_eq!(
        restarted
            .workflow()
            .task(&id("task"))
            .expect("task exists")
            .label,
        "new display label"
    );
}

#[test]
fn declarative_runtime_rejects_legacy_mutations_without_identity_change() {
    let definition = effect_definition("provenance-effect");
    let identity = definition
        .identity()
        .expect("definition identity is stable");
    let factories = provider_registry("provenance-process");
    let mut runtime = Runtime::start_from_definition(runtime_id(), definition.clone(), &factories)
        .expect("declarative runtime starts");
    let before = runtime.durable_state().expect("initial state loads");
    let workflow_revision = runtime.workflow().revision();

    assert!(matches!(
        runtime.configure_task(
            id("task"),
            TaskConfig::new().with_effect(
                operation("legacy-reconfiguration"),
                EffectSemantics::Idempotent,
            ),
        ),
        Err(RuntimeError::DeclarativeMutationUnsupported(
            LegacyMutationOperation::ConfigureTask
        ))
    ));

    let mut restored = Runtime::restore_from_definition(
        runtime_id(),
        definition,
        &factories,
        runtime.store().clone(),
    )
    .expect("declarative runtime restores");
    let restored_before = restored.durable_state().expect("restored state loads");
    let restored_workflow_revision = restored.workflow().revision();
    assert!(matches!(
        restored.apply_workflow_mutation(
            restored_workflow_revision,
            [WorkflowMutation::AddTask {
                task: Task {
                    id: id("later"),
                    label: "later".to_owned(),
                },
            }],
        ),
        Err(RuntimeError::DeclarativeMutationUnsupported(
            LegacyMutationOperation::ApplyWorkflowMutation
        ))
    ));

    let after = runtime.durable_state().expect("unchanged state loads");
    assert_eq!(before.revision(), after.revision());
    assert_eq!(after.workflow_replay_identity(), Some(&identity));
    assert_eq!(runtime.workflow().revision(), workflow_revision);
    let restored_after = restored
        .durable_state()
        .expect("restored unchanged state loads");
    assert_eq!(restored_before.revision(), restored_after.revision());
    assert_eq!(restored_after.workflow_replay_identity(), Some(&identity));
    assert_eq!(restored.workflow().revision(), restored_workflow_revision);
}

#[test]
fn incompatible_definition_fails_closed_before_factory_construction() {
    let definition_a = effect_definition("definition-a-effect");
    let definition_b = effect_definition("definition-b-effect");
    let factories_a = provider_registry("process-a");
    let runtime = Runtime::start_from_definition(runtime_id(), definition_a, &factories_a)
        .expect("runtime starts");

    let constructions = Arc::new(AtomicUsize::new(0));
    let mut factories_b = FactoryRegistry::new();
    let constructions_for_factory = Arc::clone(&constructions);
    factories_b
        .register(id("provider"), "provider-v1", move |_| {
            constructions_for_factory.fetch_add(1, Ordering::Relaxed);
            Ok(CapabilityValue::from_value("process-b".to_owned()))
        })
        .expect("replacement factory registers");

    let result = Runtime::restore_from_definition(
        runtime_id(),
        definition_b,
        &factories_b,
        runtime.store().clone(),
    );
    assert!(matches!(
        result,
        Err(RuntimeError::DefinitionMismatch { .. })
    ));
    assert_eq!(constructions.load(Ordering::Relaxed), 0);
}

#[test]
fn legacy_live_identity_transition_is_cas_protected_against_stale_writer() {
    let temp = TempStore::new("identity-transition");
    let run_id = runtime_id();
    let operation_a = operation("transition-a");
    let operation_b = operation("transition-b");
    let workflow = workflow_with_task();
    let mut runtime = Runtime::<FileDurableStore>::start_run_with_store(
        run_id.clone(),
        workflow,
        Scope::root(),
        [(
            id("task"),
            TaskConfig::new().with_effect(operation_a, EffectSemantics::Idempotent),
        )],
        FileDurableStore::open(&temp.path).expect("physical store opens"),
    )
    .expect("runtime starts");
    let mut stale_writer = FileDurableStore::open(&temp.path).expect("stale store opens");
    let initial = stale_writer.load_run(&run_id).expect("initial state loads");
    let identity_a = initial
        .workflow_replay_identity()
        .cloned()
        .expect("identity A is durable");
    assert!(identity_a.as_str().starts_with("kernis-workflow-replay-v1"));

    runtime
        .configure_task(
            id("task"),
            TaskConfig::new().with_effect(operation_b, EffectSemantics::Idempotent),
        )
        .expect("identity transitions to B");
    let stale_result = stale_writer.commit(CommitRequest::single(
        run_id.clone(),
        initial.revision(),
        IdempotencyKey::new("stale-definition-a").expect("key is valid"),
        DurableMutation::RecordWorkflowReplayIdentity(identity_a.clone()),
    ));
    assert!(matches!(
        stale_result,
        Err(StoreError::RevisionConflict {
            expected,
            actual,
        }) if expected == initial.revision() && actual > expected
    ));
    assert_ne!(
        runtime
            .durable_state()
            .expect("B state loads")
            .workflow_replay_identity(),
        Some(&identity_a)
    );

    runtime
        .configure_task(
            id("task"),
            TaskConfig::new().with_effect(
                OperationId::new("transition-a").expect("operation is valid"),
                EffectSemantics::Idempotent,
            ),
        )
        .expect("identity transitions back to A");
    assert_eq!(
        runtime
            .durable_state()
            .expect("A state loads")
            .workflow_replay_identity(),
        Some(&identity_a)
    );
}

#[test]
fn declarative_identity_transition_is_cas_protected_at_durable_boundary() {
    let temp = TempStore::new("declarative-identity-transition");
    let run_id = runtime_core::RunId::new("declarative-transition-run").expect("run id is valid");
    let identity_a = effect_definition("declarative-transition-a")
        .identity()
        .expect("definition A identity is valid");
    let identity_b = effect_definition("declarative-transition-b")
        .identity()
        .expect("definition B identity is valid");
    assert!(identity_a.as_str().starts_with("kernis-run-definition-v1"));
    assert_ne!(identity_a, identity_b);

    let mut writer = FileDurableStore::open(&temp.path).expect("physical store opens");
    let initial_revision = writer.create_run(run_id.clone()).expect("run creates");
    let revision_a = writer
        .commit(CommitRequest::single(
            run_id.clone(),
            initial_revision,
            IdempotencyKey::new("declarative-identity-a").expect("idempotency key is valid"),
            DurableMutation::RecordWorkflowReplayIdentity(identity_a.clone()),
        ))
        .expect("identity A commits")
        .revision;
    let mut stale_writer = FileDurableStore::open(&temp.path).expect("stale store opens");
    let stale_revision = stale_writer
        .load_run(&run_id)
        .expect("stale state loads")
        .revision();
    assert_eq!(stale_revision, revision_a);

    let revision_b = writer
        .commit(CommitRequest::single(
            run_id.clone(),
            revision_a,
            IdempotencyKey::new("declarative-identity-b").expect("idempotency key is valid"),
            DurableMutation::RecordWorkflowReplayIdentity(identity_b),
        ))
        .expect("identity B commits")
        .revision;
    let revision_a_again = writer
        .commit(CommitRequest::single(
            run_id.clone(),
            revision_b,
            IdempotencyKey::new("declarative-identity-a-again").expect("idempotency key is valid"),
            DurableMutation::RecordWorkflowReplayIdentity(identity_a.clone()),
        ))
        .expect("identity A transition commits")
        .revision;
    let stale_result = stale_writer.commit(CommitRequest::single(
        run_id.clone(),
        stale_revision,
        IdempotencyKey::new("declarative-stale-identity-a").expect("idempotency key is valid"),
        DurableMutation::RecordWorkflowReplayIdentity(identity_a.clone()),
    ));
    assert!(matches!(
        stale_result,
        Err(StoreError::RevisionConflict { expected, actual })
            if expected == stale_revision && actual == revision_a_again
    ));

    let final_state = FileDurableStore::open(&temp.path)
        .expect("final store opens")
        .load_run(&run_id)
        .expect("final state loads");
    assert_eq!(final_state.workflow_replay_identity(), Some(&identity_a));
    assert_eq!(final_state.revision(), revision_a_again);
}

#[test]
fn child_process_state_is_reconstructed_from_definition_and_reopened_store() {
    if let Some(path) = std::env::var_os(CHILD_PATH_ENV) {
        let store = FileDurableStore::open(PathBuf::from(path)).expect("child opens store");
        let factories = provider_registry("child-process");
        let mut runtime = Runtime::<FileDurableStore>::start_from_definition_with_store(
            runtime_id(),
            effect_definition("cross-process-effect"),
            &factories,
            store,
        )
        .expect("child starts declaratively");
        let attempt_id = match runtime.step().expect("child admits attempt") {
            StepResult::EffectPending { attempt_id, .. } => attempt_id,
            other => panic!("expected pending effect, got {other:?}"),
        };
        let operation_id = operation("cross-process-effect");
        runtime
            .dispatch_effect(&operation_id)
            .expect("child dispatches effect");
        runtime
            .record_effect_outcome(&operation_id, attempt_id, KnownEffectOutcome::Succeeded)
            .expect("child records outcome");
        return;
    }

    let temp = TempStore::new("cross-process");
    let status = Command::new(std::env::current_exe().expect("test executable exists"))
        .arg("--exact")
        .arg("child_process_state_is_reconstructed_from_definition_and_reopened_store")
        .arg("--nocapture")
        .env(CHILD_PATH_ENV, &temp.path)
        .status()
        .expect("child process starts");
    assert!(status.success(), "child process failed: {status}");

    let definition = effect_definition("cross-process-effect");
    let identity = definition
        .identity()
        .expect("definition identity is stable");
    let before_completion = FileDurableStore::open(&temp.path)
        .expect("parent reopens store")
        .load_run(&runtime_id())
        .expect("parent loads child state");
    assert_eq!(
        before_completion.workflow_replay_identity(),
        Some(&identity)
    );
    assert_eq!(before_completion.attempts().count(), 1);
    assert_eq!(before_completion.dispatch_history().len(), 1);
    assert_eq!(before_completion.outcome_history_all().len(), 1);
    assert!(before_completion.completion_history().is_empty());

    let factories = provider_registry("fresh-parent-process");
    let mut restored = Runtime::<FileDurableStore>::restore_from_definition(
        runtime_id(),
        definition.clone(),
        &factories,
        FileDurableStore::open(&temp.path).expect("parent opens fresh store connection"),
    )
    .expect("parent reconstructs without old runtime objects");
    assert_eq!(restored.attempts().len(), 1);
    assert_eq!(
        restored.attempts()[0]
            .capability(&id("provider"))
            .expect("provider is reconstructed")
            .handle()
            .downcast_ref::<String>(),
        Some(&"fresh-parent-process".to_owned())
    );
    assert!(matches!(
        restored
            .step()
            .expect("known effect completes without redispatch"),
        StepResult::Completed { .. }
    ));
    let after_completion = restored.durable_state().expect("completion is durable");
    assert_eq!(after_completion.dispatch_history().len(), 1);
    assert_eq!(after_completion.completion_history().len(), 1);
    drop(restored);

    let second = Runtime::<FileDurableStore>::restore_from_definition(
        runtime_id(),
        definition,
        &provider_registry("second-fresh-process"),
        FileDurableStore::open(&temp.path).expect("store reopens after completion"),
    )
    .expect("second fresh reconstruction succeeds");
    assert_eq!(second.workflow().completed_tasks().len(), 1);
    assert_eq!(
        second
            .workflow()
            .task(&id("task"))
            .expect("task exists")
            .label,
        "task"
    );
}

fn runtime_id() -> runtime_core::RunId {
    runtime_core::RunId::new("k2-run").expect("run id is valid")
}

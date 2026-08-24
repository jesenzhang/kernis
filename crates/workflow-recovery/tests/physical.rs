#![allow(missing_docs)]

use kernis_core::Id;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use workflow_recovery::{
    AttemptAdmission, AttemptId, CancellationRecord, CapabilityReplayIdentity, CommitRequest,
    CompletionRecord, DispatchRecord, DurableMutation, DurableStore, EffectIntent, EffectSemantics,
    FileDurableStore, IdempotencyKey, InMemoryDurableStore, KnownEffectOutcome, OperationId,
    OutcomeRecord, RunId, StoreError, StoreErrorKind, StoreInvariant, StoreRevision,
    WorkflowReplayIdentity,
};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempStore {
    directory: PathBuf,
    path: PathBuf,
}

impl TempStore {
    fn new(label: &str) -> Self {
        let number = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let directory =
            std::env::temp_dir().join(format!("kernis-k1-{label}-{}-{number}", std::process::id()));
        fs::create_dir_all(&directory).expect("temporary directory creates");
        Self {
            path: directory.join("durable.redb"),
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

fn run() -> RunId {
    RunId::new("run-physical").expect("test run is valid")
}

fn operation(value: &str) -> OperationId {
    OperationId::new(value).expect("test operation is valid")
}

fn attempt(value: &str) -> AttemptId {
    AttemptId::new(value).expect("test attempt is valid")
}

fn key(value: &str) -> IdempotencyKey {
    IdempotencyKey::new(value).expect("test key is valid")
}

fn admission(
    task_id: &str,
    attempt_id: &AttemptId,
    operation_id: Option<OperationId>,
) -> AttemptAdmission {
    AttemptAdmission {
        run_id: run(),
        task_id: id(task_id),
        attempt_id: attempt_id.clone(),
        operation_id,
        capabilities: vec![CapabilityReplayIdentity::new(id("provider"), "provider-v1")],
    }
}

fn commit<S: DurableStore>(
    store: &mut S,
    revision: StoreRevision,
    key_value: &str,
    mutation: DurableMutation,
) -> workflow_recovery::CommitResult {
    store
        .commit(CommitRequest::single(
            run(),
            revision,
            key(key_value),
            mutation,
        ))
        .expect("durable commit is valid")
}

fn supported_fact_sequence() -> Vec<DurableMutation> {
    let operation_id = operation("conformance-operation");
    let attempt_id = attempt("conformance-attempt");
    let cancelled_attempt = attempt("conformance-cancelled");
    vec![
        DurableMutation::RecordWorkflowReplayIdentity(WorkflowReplayIdentity::new(
            "workflow:conformance:v1",
        )),
        DurableMutation::RecordIntent(EffectIntent {
            task_id: id("conformance-effect-task"),
            operation_id: operation_id.clone(),
            semantics: EffectSemantics::Idempotent,
        }),
        DurableMutation::AdmitAttempt(admission(
            "conformance-effect-task",
            &attempt_id,
            Some(operation_id.clone()),
        )),
        DurableMutation::RecordDispatch(DispatchRecord {
            operation_id: operation_id.clone(),
            attempt_id: attempt_id.clone(),
        }),
        DurableMutation::RecordOutcome(OutcomeRecord {
            operation_id,
            attempt_id: attempt_id.clone(),
            outcome: KnownEffectOutcome::Succeeded,
        }),
        DurableMutation::RecordCompletion(CompletionRecord {
            task_id: id("conformance-effect-task"),
            attempt_id,
        }),
        DurableMutation::AdmitAttempt(admission(
            "conformance-cancelled-task",
            &cancelled_attempt,
            None,
        )),
        DurableMutation::RecordCancellation(CancellationRecord {
            run_id: run(),
            task_id: id("conformance-cancelled-task"),
            operation_id: None,
            attempt_id: Some(cancelled_attempt),
        }),
    ]
}

#[test]
fn physical_store_reopens_with_all_supported_fact_classes() {
    let temp = TempStore::new("round-trip");
    let mut store = FileDurableStore::open(&temp.path).expect("physical store opens");
    assert_eq!(
        store.create_run(run()).expect("run creates"),
        StoreRevision::INITIAL
    );

    commit(
        &mut store,
        StoreRevision::INITIAL,
        "identity",
        DurableMutation::RecordWorkflowReplayIdentity(WorkflowReplayIdentity::new(
            "workflow:v1:stable",
        )),
    );
    let operation_id = operation("operation-1");
    let attempt_id = attempt("attempt-1");
    let revision = store.load_run(&run()).expect("state loads").revision();
    commit(
        &mut store,
        revision,
        "intent",
        DurableMutation::RecordIntent(EffectIntent {
            task_id: id("effect-task"),
            operation_id: operation_id.clone(),
            semantics: EffectSemantics::Idempotent,
        }),
    );
    let revision = store.load_run(&run()).expect("state loads").revision();
    commit(
        &mut store,
        revision,
        "admit-effect",
        DurableMutation::AdmitAttempt(admission(
            "effect-task",
            &attempt_id,
            Some(operation_id.clone()),
        )),
    );
    let revision = store.load_run(&run()).expect("state loads").revision();
    commit(
        &mut store,
        revision,
        "dispatch",
        DurableMutation::RecordDispatch(DispatchRecord {
            operation_id: operation_id.clone(),
            attempt_id: attempt_id.clone(),
        }),
    );
    let revision = store.load_run(&run()).expect("state loads").revision();
    commit(
        &mut store,
        revision,
        "outcome",
        DurableMutation::RecordOutcome(OutcomeRecord {
            operation_id,
            attempt_id: attempt_id.clone(),
            outcome: KnownEffectOutcome::Succeeded,
        }),
    );
    let revision = store.load_run(&run()).expect("state loads").revision();
    commit(
        &mut store,
        revision,
        "completion",
        DurableMutation::RecordCompletion(CompletionRecord {
            task_id: id("effect-task"),
            attempt_id,
        }),
    );
    let revision = store.load_run(&run()).expect("state loads").revision();
    let cancelled_attempt = attempt("attempt-cancelled");
    commit(
        &mut store,
        revision,
        "admit-cancelled",
        DurableMutation::AdmitAttempt(admission("cancelled-task", &cancelled_attempt, None)),
    );
    let revision = store.load_run(&run()).expect("state loads").revision();
    commit(
        &mut store,
        revision,
        "cancellation",
        DurableMutation::RecordCancellation(CancellationRecord {
            run_id: run(),
            task_id: id("cancelled-task"),
            operation_id: None,
            attempt_id: Some(cancelled_attempt),
        }),
    );

    let before_reopen = store.load_run(&run()).expect("state loads");
    drop(store);

    let reopened = FileDurableStore::open(&temp.path).expect("store reopens");
    assert_eq!(
        reopened.load_run(&run()).expect("reopened state loads"),
        before_reopen
    );
}

#[test]
fn concurrent_first_openers_wait_for_bootstrap_instead_of_reporting_corruption() {
    let temp = TempStore::new("bootstrap-race");
    let first_path = temp.path.clone();
    let second_path = temp.path.clone();
    let (first, second) = std::thread::scope(|scope| {
        let first = scope.spawn(|| FileDurableStore::open(first_path));
        let second = scope.spawn(|| FileDurableStore::open(second_path));
        (
            first.join().expect("first opener does not panic"),
            second.join().expect("second opener does not panic"),
        )
    });
    assert!(first.is_ok(), "first opener failed: {first:?}");
    assert!(second.is_ok(), "second opener failed: {second:?}");
}

#[test]
fn physical_store_preserves_atomic_batches_cas_and_idempotent_replay() {
    let temp = TempStore::new("cas");
    let mut first = FileDurableStore::open(&temp.path).expect("first store opens");
    let mut second = FileDurableStore::open(&temp.path).expect("second store opens");
    first.create_run(run()).expect("run creates");

    let invalid_batch = first.commit(CommitRequest {
        run_id: run(),
        expected_revision: StoreRevision::INITIAL,
        idempotency_key: key("invalid-batch"),
        mutations: vec![
            DurableMutation::RecordWorkflowReplayIdentity(WorkflowReplayIdentity::new(
                "workflow:v1",
            )),
            DurableMutation::RecordDispatch(DispatchRecord {
                operation_id: operation("missing"),
                attempt_id: attempt("missing"),
            }),
        ],
    });
    assert!(matches!(
        invalid_batch,
        Err(StoreError::InvariantViolation(
            StoreInvariant::DispatchWithoutAdmission { .. }
        ))
    ));
    let unchanged = first.load_run(&run()).expect("state loads");
    assert_eq!(unchanged.revision(), StoreRevision::INITIAL);
    assert!(unchanged.workflow_replay_identity().is_none());

    let request = CommitRequest::single(
        run(),
        StoreRevision::INITIAL,
        key("identity"),
        DurableMutation::RecordWorkflowReplayIdentity(WorkflowReplayIdentity::new("workflow:v1")),
    );
    let committed = first.commit(request.clone()).expect("commit succeeds");
    assert!(!committed.replayed);
    let replayed = second.commit(request).expect("idempotent replay succeeds");
    assert!(replayed.replayed);
    assert_eq!(replayed.revision, committed.revision);

    let stale = second.commit(CommitRequest::single(
        run(),
        StoreRevision::INITIAL,
        key("stale"),
        DurableMutation::RecordWorkflowReplayIdentity(WorkflowReplayIdentity::new("workflow:v2")),
    ));
    assert!(matches!(stale, Err(StoreError::RevisionConflict { .. })));
    assert_eq!(
        first
            .load_run(&run())
            .expect("state loads")
            .workflow_replay_identity(),
        Some(&WorkflowReplayIdentity::new("workflow:v1"))
    );
}

#[test]
fn physical_adapter_matches_in_memory_for_the_typed_fact_sequence() {
    let temp = TempStore::new("conformance");
    let mut memory = InMemoryDurableStore::new();
    let mut physical = FileDurableStore::open(&temp.path).expect("physical store opens");
    memory.create_run(run()).expect("memory run creates");
    physical.create_run(run()).expect("physical run creates");

    for (index, mutation) in supported_fact_sequence().into_iter().enumerate() {
        let memory_revision = memory.load_run(&run()).expect("memory loads").revision();
        let physical_revision = physical
            .load_run(&run())
            .expect("physical loads")
            .revision();
        assert_eq!(memory_revision, physical_revision);
        let key_value = format!("conformance-{index}");
        let request = CommitRequest::single(run(), memory_revision, key(&key_value), mutation);
        assert_eq!(
            memory.commit(request.clone()).expect("memory commits"),
            physical.commit(request).expect("physical commits")
        );
        assert_eq!(
            memory.load_run(&run()).expect("memory state loads"),
            physical.load_run(&run()).expect("physical state loads")
        );
    }
}

#[test]
fn physical_store_survives_process_restart_between_outcome_and_completion() {
    let temp = TempStore::new("child");
    let status = Command::new(std::env::current_exe().expect("test executable exists"))
        .arg("--exact")
        .arg("child_writes_after_outcome")
        .arg("--nocapture")
        .env("KERNIS_K1_CHILD_PATH", &temp.path)
        .status()
        .expect("child process starts");
    assert!(status.success(), "child process failed: {status}");

    let mut store = FileDurableStore::open(&temp.path).expect("parent reopens store");
    let before_completion = store.load_run(&run()).expect("parent loads state");
    assert_eq!(before_completion.dispatch_history().len(), 1);
    assert_eq!(before_completion.outcome_history_all().len(), 1);
    assert_eq!(before_completion.attempts().count(), 3);
    assert_eq!(before_completion.completion_history().len(), 1);
    assert!(
        before_completion
            .completion_for_task(&id("child-no-effect-task"))
            .is_some()
    );
    assert_eq!(before_completion.cancellations().count(), 1);
    assert!(
        before_completion
            .cancellation(&id("child-cancelled-task"))
            .is_some()
    );

    let attempt_id = attempt("child-attempt");
    let completion = store.commit(CommitRequest::single(
        run(),
        before_completion.revision(),
        key("completion-after-restart"),
        DurableMutation::RecordCompletion(CompletionRecord {
            task_id: id("effect-task"),
            attempt_id,
        }),
    ));
    assert!(
        completion.is_ok(),
        "completion must be recoverable: {completion:?}"
    );
    let after_completion = store.load_run(&run()).expect("completed state loads");
    assert_eq!(after_completion.dispatch_history().len(), 1);
    assert_eq!(after_completion.completion_history().len(), 2);
}

#[test]
fn child_writes_after_outcome() {
    let Some(path) = std::env::var_os("KERNIS_K1_CHILD_PATH") else {
        return;
    };
    let mut store = FileDurableStore::open(path).expect("child opens store");
    store.create_run(run()).expect("child creates run");
    commit(
        &mut store,
        StoreRevision::INITIAL,
        "identity",
        DurableMutation::RecordWorkflowReplayIdentity(WorkflowReplayIdentity::new("workflow:v1")),
    );
    let operation_id = operation("child-operation");
    let attempt_id = attempt("child-attempt");
    let revision = store
        .load_run(&run())
        .expect("child loads state")
        .revision();
    commit(
        &mut store,
        revision,
        "intent",
        DurableMutation::RecordIntent(EffectIntent {
            task_id: id("effect-task"),
            operation_id: operation_id.clone(),
            semantics: EffectSemantics::Idempotent,
        }),
    );
    let revision = store
        .load_run(&run())
        .expect("child loads state")
        .revision();
    commit(
        &mut store,
        revision,
        "admission",
        DurableMutation::AdmitAttempt(admission(
            "effect-task",
            &attempt_id,
            Some(operation_id.clone()),
        )),
    );
    let revision = store
        .load_run(&run())
        .expect("child loads state")
        .revision();
    commit(
        &mut store,
        revision,
        "dispatch",
        DurableMutation::RecordDispatch(DispatchRecord {
            operation_id: operation_id.clone(),
            attempt_id: attempt_id.clone(),
        }),
    );
    let revision = store
        .load_run(&run())
        .expect("child loads state")
        .revision();
    commit(
        &mut store,
        revision,
        "outcome",
        DurableMutation::RecordOutcome(OutcomeRecord {
            operation_id,
            attempt_id,
            outcome: KnownEffectOutcome::Succeeded,
        }),
    );
    let no_effect_attempt = attempt("child-no-effect-attempt");
    let revision = store
        .load_run(&run())
        .expect("child loads state")
        .revision();
    commit(
        &mut store,
        revision,
        "no-effect-admission",
        DurableMutation::AdmitAttempt(admission("child-no-effect-task", &no_effect_attempt, None)),
    );
    let revision = store
        .load_run(&run())
        .expect("child loads state")
        .revision();
    commit(
        &mut store,
        revision,
        "no-effect-completion",
        DurableMutation::RecordCompletion(CompletionRecord {
            task_id: id("child-no-effect-task"),
            attempt_id: no_effect_attempt,
        }),
    );
    let cancelled_attempt = attempt("child-cancelled-attempt");
    let revision = store
        .load_run(&run())
        .expect("child loads state")
        .revision();
    commit(
        &mut store,
        revision,
        "cancelled-admission",
        DurableMutation::AdmitAttempt(admission("child-cancelled-task", &cancelled_attempt, None)),
    );
    let revision = store
        .load_run(&run())
        .expect("child loads state")
        .revision();
    commit(
        &mut store,
        revision,
        "cancellation",
        DurableMutation::RecordCancellation(CancellationRecord {
            run_id: run(),
            task_id: id("child-cancelled-task"),
            operation_id: None,
            attempt_id: Some(cancelled_attempt),
        }),
    );
}

#[test]
fn physical_backend_errors_keep_categories_distinct() {
    let temp = TempStore::new("errors");
    let mut store = FileDurableStore::open(&temp.path).expect("store opens");
    store.create_run(run()).expect("run creates");
    fs::remove_file(&temp.path).expect("test removes backend");
    let unavailable = store.load_run(&run()).expect_err("missing backend fails");
    assert_eq!(unavailable.kind(), StoreErrorKind::BackendUnavailable);
    assert!(!matches!(unavailable, StoreError::RunNotFound(_)));

    fs::write(&temp.path, b"not a redb database").expect("test corrupts backend");
    let corruption = FileDurableStore::open(&temp.path).expect_err("corrupt backend fails");
    assert_eq!(corruption.kind(), StoreErrorKind::DataCorruption);
}

#[test]
fn physical_open_distinguishes_incomplete_bootstrap_from_incompatible_schema() {
    let empty = TempStore::new("empty-bootstrap");
    let database = redb::Database::create(&empty.path).expect("bare redb database creates");
    drop(database);
    let incomplete = FileDurableStore::open(&empty.path).expect_err("empty bootstrap is rejected");
    assert_eq!(incomplete.kind(), StoreErrorKind::BackendUnavailable);

    let incompatible = TempStore::new("incompatible-schema");
    const FOREIGN: redb::TableDefinition<&str, &[u8]> =
        redb::TableDefinition::new("foreign_table_v1");
    let database = redb::Database::create(&incompatible.path).expect("bare redb database creates");
    let write = database.begin_write().expect("foreign schema write begins");
    {
        let mut table = write.open_table(FOREIGN).expect("foreign table opens");
        table
            .insert("foreign", b"payload".as_slice())
            .expect("foreign row inserts");
    }
    write.commit().expect("foreign schema commits");
    drop(database);

    let corruption =
        FileDurableStore::open(&incompatible.path).expect_err("incompatible schema is rejected");
    assert_eq!(corruption.kind(), StoreErrorKind::DataCorruption);
}

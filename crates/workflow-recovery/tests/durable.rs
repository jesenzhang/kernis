#![allow(missing_docs)]

use kernis_core::Id;
use workflow_recovery::{
    AttemptAdmission, AttemptId, CapabilityReplayIdentity, CommitRequest, DurableMutation,
    DurableStore, EffectIntent, EffectSemantics, IdempotencyKey, InMemoryDurableStore,
    KnownEffectOutcome, OperationId, OutcomeRecord, RunId, StoreError, StoreErrorKind,
    StoreInvariant, StoreRevision,
};
use workflow_recovery::{CancellationRecord, CompletionRecord, DispatchRecord};

fn id(value: &str) -> Id {
    Id::new(value).expect("test id is valid")
}

fn run() -> RunId {
    RunId::new("run-1").expect("test run is valid")
}

fn op(value: &str) -> OperationId {
    OperationId::new(value).expect("test operation is valid")
}

fn attempt(value: &str) -> AttemptId {
    AttemptId::new(value).expect("test attempt is valid")
}

fn key(value: &str) -> IdempotencyKey {
    IdempotencyKey::new(value).expect("test key is valid")
}

fn admission(attempt_id: &AttemptId, operation_id: Option<OperationId>) -> AttemptAdmission {
    AttemptAdmission {
        run_id: run(),
        task_id: id("task"),
        attempt_id: attempt_id.clone(),
        operation_id,
        capabilities: vec![CapabilityReplayIdentity::new(id("provider"), "provider-v1")],
    }
}

fn completion(task_id: &str, attempt_id: &AttemptId) -> CompletionRecord {
    CompletionRecord {
        task_id: id(task_id),
        attempt_id: attempt_id.clone(),
    }
}

fn commit(
    store: &mut InMemoryDurableStore,
    revision: StoreRevision,
    key_value: &str,
    mutation: DurableMutation,
) {
    store
        .commit(CommitRequest::single(
            run(),
            revision,
            key(key_value),
            mutation,
        ))
        .expect("durable commit is valid");
}

#[test]
fn admission_is_durable_before_dispatch_and_survives_store_clone() {
    let mut store = InMemoryDurableStore::new();
    assert_eq!(
        store.create_run(run()).expect("run creates"),
        StoreRevision::INITIAL
    );
    let attempt_id = attempt("attempt-1");
    commit(
        &mut store,
        StoreRevision::INITIAL,
        "admit-1",
        DurableMutation::AdmitAttempt(admission(&attempt_id, None)),
    );

    let restarted = store.clone();
    let state = restarted.load_run(&run()).expect("restart loads state");
    assert!(state.attempt(&attempt_id).is_some());
    assert!(state.dispatch_history().is_empty());
}

#[test]
fn stale_cas_and_conflicting_idempotency_leave_state_unchanged() {
    let mut store = InMemoryDurableStore::new();
    store.create_run(run()).expect("run creates");
    let attempt_id = attempt("attempt-1");
    commit(
        &mut store,
        StoreRevision::INITIAL,
        "admit-1",
        DurableMutation::AdmitAttempt(admission(&attempt_id, None)),
    );
    let state = store.load_run(&run()).expect("state loads");
    let before = state.clone();

    let stale = store.commit(CommitRequest::single(
        run(),
        StoreRevision::INITIAL,
        key("stale"),
        DurableMutation::AdmitAttempt(admission(&attempt("attempt-2"), None)),
    ));
    assert!(matches!(stale, Err(StoreError::RevisionConflict { .. })));
    assert_eq!(store.load_run(&run()).expect("state loads"), before);

    let conflict = store.commit(CommitRequest::single(
        run(),
        before.revision(),
        key("admit-1"),
        DurableMutation::AdmitAttempt(admission(&attempt("attempt-2"), None)),
    ));
    assert!(matches!(
        conflict,
        Err(StoreError::IdempotencyConflict { .. })
    ));
    assert_eq!(store.load_run(&run()).expect("state loads"), before);
}

#[test]
fn latest_dispatch_owns_recovery_while_old_outcomes_remain_history() {
    let mut store = InMemoryDurableStore::new();
    store.create_run(run()).expect("run creates");
    let operation_id = op("operation");
    let first = attempt("attempt-1");
    let second = attempt("attempt-2");
    commit(
        &mut store,
        StoreRevision::INITIAL,
        "intent",
        DurableMutation::RecordIntent(EffectIntent {
            task_id: id("task"),
            operation_id: operation_id.clone(),
            semantics: EffectSemantics::Idempotent,
        }),
    );
    let revision = store.load_run(&run()).expect("state loads").revision();
    commit(
        &mut store,
        revision,
        "admit-1",
        DurableMutation::AdmitAttempt(admission(&first, Some(operation_id.clone()))),
    );
    let revision = store.load_run(&run()).expect("state loads").revision();
    commit(
        &mut store,
        revision,
        "dispatch-1",
        DurableMutation::RecordDispatch(DispatchRecord {
            operation_id: operation_id.clone(),
            attempt_id: first.clone(),
        }),
    );
    let revision = store.load_run(&run()).expect("state loads").revision();
    commit(
        &mut store,
        revision,
        "outcome-1",
        DurableMutation::RecordOutcome(OutcomeRecord {
            operation_id: operation_id.clone(),
            attempt_id: first.clone(),
            outcome: KnownEffectOutcome::Failed,
        }),
    );
    let revision = store.load_run(&run()).expect("state loads").revision();
    store
        .commit(CommitRequest {
            run_id: run(),
            expected_revision: revision,
            idempotency_key: key("retry-2"),
            mutations: vec![
                DurableMutation::AdmitAttempt(admission(&second, Some(operation_id.clone()))),
                DurableMutation::RecordDispatch(DispatchRecord {
                    operation_id: operation_id.clone(),
                    attempt_id: second.clone(),
                }),
            ],
        })
        .expect("retry is durable");

    let state = store.load_run(&run()).expect("state loads");
    assert_eq!(
        state.latest_dispatch(&operation_id).unwrap().attempt_id,
        second
    );
    assert_eq!(
        state.effect_state(&operation_id),
        workflow_recovery::RecoveredEffectState::OutcomeUnknown
    );
    assert_eq!(state.outcome_history(&operation_id).count(), 1);
}

#[test]
fn cancellation_retains_admission_and_prevents_dispatch() {
    let mut store = InMemoryDurableStore::new();
    store.create_run(run()).expect("run creates");
    let operation_id = op("operation");
    let attempt_id = attempt("attempt-1");
    store
        .commit(CommitRequest {
            run_id: run(),
            expected_revision: StoreRevision::INITIAL,
            idempotency_key: key("admit-intent"),
            mutations: vec![
                DurableMutation::RecordIntent(EffectIntent {
                    task_id: id("task"),
                    operation_id: operation_id.clone(),
                    semantics: EffectSemantics::Idempotent,
                }),
                DurableMutation::AdmitAttempt(admission(&attempt_id, Some(operation_id.clone()))),
            ],
        })
        .expect("admission is durable");
    let revision = store.load_run(&run()).expect("state loads").revision();
    commit(
        &mut store,
        revision,
        "cancel",
        DurableMutation::RecordCancellation(CancellationRecord {
            run_id: run(),
            task_id: id("task"),
            operation_id: Some(operation_id.clone()),
            attempt_id: Some(attempt_id.clone()),
        }),
    );
    let revision = store.load_run(&run()).expect("state loads").revision();
    let dispatch = store.commit(CommitRequest::single(
        run(),
        revision,
        key("dispatch"),
        DurableMutation::RecordDispatch(DispatchRecord {
            operation_id,
            attempt_id: attempt_id.clone(),
        }),
    ));
    assert!(matches!(
        dispatch,
        Err(StoreError::InvariantViolation(
            StoreInvariant::DispatchCancelled { .. }
        ))
    ));
    let state = store.load_run(&run()).expect("state loads");
    assert!(state.attempt(&attempt_id).is_some());
    assert!(state.is_cancelled(&id("task")));
    assert!(state.dispatch_history().is_empty());
}

#[test]
fn identical_commit_replay_does_not_advance_revision() {
    let mut store = InMemoryDurableStore::new();
    store.create_run(run()).expect("run creates");
    let request = CommitRequest::single(
        run(),
        StoreRevision::INITIAL,
        key("admit"),
        DurableMutation::AdmitAttempt(admission(&attempt("attempt-1"), None)),
    );
    let first = store.commit(request.clone()).expect("first commit");
    let replay = store.commit(request).expect("replay commit");
    assert!(!first.replayed);
    assert!(replay.replayed);
    assert_eq!(first.revision, replay.revision);
    assert_eq!(
        store
            .load_run(&run())
            .expect("state loads")
            .attempts()
            .count(),
        1
    );
}

#[test]
fn replay_after_newer_commit_returns_live_head_for_the_next_cas() {
    let mut store = InMemoryDurableStore::new();
    store.create_run(run()).expect("run creates");
    let first_request = CommitRequest::single(
        run(),
        StoreRevision::INITIAL,
        key("first"),
        DurableMutation::AdmitAttempt(admission(&attempt("attempt-1"), None)),
    );
    let first = store.commit(first_request.clone()).expect("first commit");
    let newer = store
        .commit(CommitRequest::single(
            run(),
            first.revision,
            key("newer"),
            DurableMutation::AdmitAttempt(admission(&attempt("attempt-2"), None)),
        ))
        .expect("newer commit");

    let replay = store.commit(first_request).expect("replay commit");
    assert!(replay.replayed);
    assert_eq!(replay.revision, newer.revision);

    store
        .commit(CommitRequest::single(
            run(),
            replay.revision,
            key("after-replay"),
            DurableMutation::AdmitAttempt(admission(&attempt("attempt-3"), None)),
        ))
        .expect("live replay head remains valid for the next CAS");
}

#[test]
fn outcome_requires_admitted_dispatched_attempt_and_cannot_conflict() {
    let mut store = InMemoryDurableStore::new();
    store.create_run(run()).expect("run creates");
    let operation_id = op("operation");
    let attempt_id = attempt("attempt-1");
    store
        .commit(CommitRequest {
            run_id: run(),
            expected_revision: StoreRevision::INITIAL,
            idempotency_key: key("intent-admit"),
            mutations: vec![
                DurableMutation::RecordIntent(EffectIntent {
                    task_id: id("task"),
                    operation_id: operation_id.clone(),
                    semantics: EffectSemantics::Idempotent,
                }),
                DurableMutation::AdmitAttempt(admission(&attempt_id, Some(operation_id.clone()))),
            ],
        })
        .expect("intent and admission commit");
    let revision = store.load_run(&run()).expect("state loads").revision();
    let unknown_outcome = store.commit(CommitRequest::single(
        run(),
        revision,
        key("unknown-outcome"),
        DurableMutation::RecordOutcome(OutcomeRecord {
            operation_id: operation_id.clone(),
            attempt_id: attempt("not-admitted"),
            outcome: KnownEffectOutcome::Succeeded,
        }),
    ));
    assert!(matches!(
        unknown_outcome,
        Err(StoreError::InvariantViolation(
            StoreInvariant::OutcomeWithoutAdmission { .. }
        ))
    ));

    let revision = store.load_run(&run()).expect("state loads").revision();
    store
        .commit(CommitRequest::single(
            run(),
            revision,
            key("dispatch"),
            DurableMutation::RecordDispatch(DispatchRecord {
                operation_id: operation_id.clone(),
                attempt_id: attempt_id.clone(),
            }),
        ))
        .expect("dispatch commits");
    let revision = store.load_run(&run()).expect("state loads").revision();
    store
        .commit(CommitRequest::single(
            run(),
            revision,
            key("outcome-ok"),
            DurableMutation::RecordOutcome(OutcomeRecord {
                operation_id: operation_id.clone(),
                attempt_id: attempt_id.clone(),
                outcome: KnownEffectOutcome::Succeeded,
            }),
        ))
        .expect("outcome commits");
    let revision = store.load_run(&run()).expect("state loads").revision();
    let conflicting = store.commit(CommitRequest::single(
        run(),
        revision,
        key("outcome-conflict"),
        DurableMutation::RecordOutcome(OutcomeRecord {
            operation_id,
            attempt_id,
            outcome: KnownEffectOutcome::Failed,
        }),
    ));
    assert!(matches!(
        conflicting,
        Err(StoreError::InvariantViolation(
            StoreInvariant::ConflictingOutcome { .. }
        ))
    ));
}

#[test]
fn completion_requires_attempt_lineage_and_replay_is_idempotent() {
    let mut store = InMemoryDurableStore::new();
    store.create_run(run()).expect("run creates");
    let attempt_id = attempt("attempt-1");
    commit(
        &mut store,
        StoreRevision::INITIAL,
        "admit",
        DurableMutation::AdmitAttempt(admission(&attempt_id, None)),
    );
    let revision = store.load_run(&run()).expect("state loads").revision();
    let request = CommitRequest::single(
        run(),
        revision,
        key("completion"),
        DurableMutation::RecordCompletion(completion("task", &attempt_id)),
    );
    let first = store.commit(request.clone()).expect("completion commits");
    let replay = store.commit(request).expect("completion replay succeeds");
    assert!(!first.replayed);
    assert!(replay.replayed);
    let state = store.load_run(&run()).expect("state loads");
    assert_eq!(
        state.completion_history(),
        &[completion("task", &attempt_id)]
    );
    assert!(state.is_completed(&id("task")));

    let revision = state.revision();
    let wrong_task = store.commit(CommitRequest::single(
        run(),
        revision,
        key("completion-wrong-task"),
        DurableMutation::RecordCompletion(completion("other-task", &attempt_id)),
    ));
    assert!(matches!(
        wrong_task,
        Err(StoreError::InvariantViolation(
            StoreInvariant::CompletionAttemptMismatch { .. }
        ))
    ));
}

#[test]
fn conflicting_completion_lineage_is_rejected_without_a_second_fact() {
    let mut store = InMemoryDurableStore::new();
    store.create_run(run()).expect("run creates");
    let first = attempt("attempt-1");
    let second = attempt("attempt-2");
    store
        .commit(CommitRequest {
            run_id: run(),
            expected_revision: StoreRevision::INITIAL,
            idempotency_key: key("admit-both"),
            mutations: vec![
                DurableMutation::AdmitAttempt(admission(&first, None)),
                DurableMutation::AdmitAttempt(admission(&second, None)),
            ],
        })
        .expect("attempts commit");
    let revision = store.load_run(&run()).expect("state loads").revision();
    commit(
        &mut store,
        revision,
        "completion-first",
        DurableMutation::RecordCompletion(completion("task", &first)),
    );
    let before = store.load_run(&run()).expect("state loads");
    let conflict = store.commit(CommitRequest::single(
        run(),
        before.revision(),
        key("completion-second"),
        DurableMutation::RecordCompletion(completion("task", &second)),
    ));
    assert!(matches!(
        conflict,
        Err(StoreError::InvariantViolation(
            StoreInvariant::ConflictingCompletion { .. }
        ))
    ));
    assert_eq!(
        store
            .load_run(&run())
            .expect("state loads")
            .completion_history(),
        before.completion_history()
    );
}

#[test]
fn effect_completion_requires_latest_successful_dispatch() {
    let mut store = InMemoryDurableStore::new();
    store.create_run(run()).expect("run creates");
    let operation_id = op("operation");
    let first = attempt("attempt-1");
    let second = attempt("attempt-2");
    store
        .commit(CommitRequest {
            run_id: run(),
            expected_revision: StoreRevision::INITIAL,
            idempotency_key: key("intent-admit"),
            mutations: vec![
                DurableMutation::RecordIntent(EffectIntent {
                    task_id: id("task"),
                    operation_id: operation_id.clone(),
                    semantics: EffectSemantics::Idempotent,
                }),
                DurableMutation::AdmitAttempt(admission(&first, Some(operation_id.clone()))),
            ],
        })
        .expect("intent and admission commit");
    let revision = store.load_run(&run()).expect("state loads").revision();
    let without_dispatch = store.commit(CommitRequest::single(
        run(),
        revision,
        key("completion-before-dispatch"),
        DurableMutation::RecordCompletion(completion("task", &first)),
    ));
    assert!(matches!(
        without_dispatch,
        Err(StoreError::InvariantViolation(
            StoreInvariant::CompletionWithoutDispatch { .. }
        ))
    ));

    let revision = store.load_run(&run()).expect("state loads").revision();
    commit(
        &mut store,
        revision,
        "dispatch-first",
        DurableMutation::RecordDispatch(DispatchRecord {
            operation_id: operation_id.clone(),
            attempt_id: first.clone(),
        }),
    );
    let revision = store.load_run(&run()).expect("state loads").revision();
    let without_outcome = store.commit(CommitRequest::single(
        run(),
        revision,
        key("completion-before-outcome"),
        DurableMutation::RecordCompletion(completion("task", &first)),
    ));
    assert!(matches!(
        without_outcome,
        Err(StoreError::InvariantViolation(
            StoreInvariant::CompletionWithoutSuccessfulOutcome { outcome: None, .. }
        ))
    ));

    let revision = store.load_run(&run()).expect("state loads").revision();
    commit(
        &mut store,
        revision,
        "outcome-first",
        DurableMutation::RecordOutcome(OutcomeRecord {
            operation_id: operation_id.clone(),
            attempt_id: first.clone(),
            outcome: KnownEffectOutcome::Succeeded,
        }),
    );
    let revision = store.load_run(&run()).expect("state loads").revision();
    store
        .commit(CommitRequest {
            run_id: run(),
            expected_revision: revision,
            idempotency_key: key("retry"),
            mutations: vec![
                DurableMutation::AdmitAttempt(admission(&second, Some(operation_id.clone()))),
                DurableMutation::RecordDispatch(DispatchRecord {
                    operation_id: operation_id.clone(),
                    attempt_id: second.clone(),
                }),
            ],
        })
        .expect("latest dispatch commits");
    let revision = store.load_run(&run()).expect("state loads").revision();
    let old_completion = store.commit(CommitRequest::single(
        run(),
        revision,
        key("completion-old-attempt"),
        DurableMutation::RecordCompletion(completion("task", &first)),
    ));
    assert!(matches!(
        old_completion,
        Err(StoreError::InvariantViolation(
            StoreInvariant::CompletionNotLatestDispatch { .. }
        ))
    ));
}

#[test]
fn store_error_categories_are_distinct_from_domain_conflicts() {
    assert_eq!(
        StoreError::BackendUnavailable.kind(),
        StoreErrorKind::BackendUnavailable
    );
    assert_eq!(
        StoreError::IoFailure("read failed".to_owned()).kind(),
        StoreErrorKind::IoFailure
    );
    assert_eq!(
        StoreError::DataCorruption("bad checksum".to_owned()).kind(),
        StoreErrorKind::DataCorruption
    );
    assert_eq!(
        StoreError::RevisionConflict {
            expected: StoreRevision::INITIAL,
            actual: StoreRevision::INITIAL,
        }
        .kind(),
        StoreErrorKind::Domain
    );
}

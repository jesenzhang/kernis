//! Minimal durable execution contract and deterministic in-memory adapter.
//!
//! This module is a persistence seam, not a recovery policy engine.  The
//! adapter validates durable fact relationships and exposes enough state for
//! [`crate::classify_recovery`] to make a decision after a process restart.

use crate::model::{
    AttemptId, DispatchRecord, EffectIntent, KnownEffectOutcome, OperationId, OutcomeRecord,
    RecoveredEffectState,
};
use kernis_core::Id;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// Identity of one workflow execution, stable across task attempts.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct RunId(Id);

impl RunId {
    /// Creates a non-empty run identity.
    pub fn new(value: impl Into<String>) -> Result<Self, kernis_core::InvalidId> {
        Id::new(value).map(Self)
    }

    /// Returns the run identity as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Stable identity of the supplied workflow topology and runtime
/// configuration used for replay validation.
///
/// The value is an opaque, versioned canonical representation owned by the
/// caller. It is deliberately not a topology revision: equivalent topology
/// and configuration must produce the same identity even when their mutation
/// history was assembled differently.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct WorkflowReplayIdentity(String);

impl WorkflowReplayIdentity {
    /// Creates an identity from a caller-generated canonical representation.
    pub fn new(value: impl Into<String>) -> Result<Self, StoreError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(StoreError::EmptyWorkflowReplayIdentity);
        }
        Ok(Self(value))
    }

    /// Returns the canonical identity representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for WorkflowReplayIdentity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for WorkflowReplayIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Monotonic revision of durable state.
///
/// This is deliberately distinct from `kernis_core::Revision`, which belongs
/// to workflow topology.  A store revision advances for a successful durable
/// commit, not for a topology mutation.
#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
pub struct StoreRevision(u64);

impl StoreRevision {
    /// The revision of a newly created run.
    pub const INITIAL: Self = Self(0);

    /// Returns the numeric revision.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    pub(crate) fn checked_next(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }
}

impl fmt::Display for StoreRevision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Stable logical capability identity used to reconstruct or validate a
/// capability after restart.
///
/// It intentionally contains no generation, entry id, fiber, handle, mutex,
/// or disposer.  The definition identity is supplied by the capability
/// runtime and must be stable for equivalent configuration.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct CapabilityReplayIdentity {
    capability_id: Id,
    definition_identity: String,
}

impl CapabilityReplayIdentity {
    /// Creates a replay identity from a logical id and stable definition key.
    #[must_use]
    pub fn new(capability_id: Id, definition_identity: impl Into<String>) -> Self {
        Self {
            capability_id,
            definition_identity: definition_identity.into(),
        }
    }

    /// Returns the logical capability id.
    #[must_use]
    pub const fn capability_id(&self) -> &Id {
        &self.capability_id
    }

    /// Returns the stable definition/configuration identity.
    #[must_use]
    pub fn definition_identity(&self) -> &str {
        &self.definition_identity
    }
}

/// Durable admission of one exact attempt, independent of provider dispatch.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AttemptAdmission {
    /// Workflow execution containing the attempt.
    pub run_id: RunId,
    /// Logical workflow task being attempted.
    pub task_id: Id,
    /// Exact attempt identity, never reused after admission.
    pub attempt_id: AttemptId,
    /// Logical external operation, when this task owns one.
    pub operation_id: Option<OperationId>,
    /// Stable capability identities needed to reconstruct this attempt.
    pub capabilities: Vec<CapabilityReplayIdentity>,
}

/// Durable cancellation fact.  Cancellation never deletes prior facts.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CancellationRecord {
    /// Workflow execution containing the cancelled task.
    pub run_id: RunId,
    /// Logical task whose future dispatch is cancelled.
    pub task_id: Id,
    /// Operation owned by the task, when known at cancellation time.
    pub operation_id: Option<OperationId>,
    /// Attempt admitted at cancellation time, if any.
    pub attempt_id: Option<AttemptId>,
}

/// Durable completion fact for one admitted task attempt.
///
/// The workflow graph remains the authority for prerequisite semantics. The
/// attempt identity here lets the store validate that the completion belongs
/// to the admitted task lineage without copying graph scheduling rules.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CompletionRecord {
    /// Task whose completion is being recorded.
    pub task_id: Id,
    /// Admitted attempt responsible for the completion.
    pub attempt_id: AttemptId,
}

/// Caller-supplied identity for an idempotent durable mutation.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct IdempotencyKey(String);

impl IdempotencyKey {
    /// Creates a non-empty idempotency key.
    ///
    /// Empty keys are rejected because an omitted identity cannot safely
    /// describe a replay.
    pub fn new(value: impl Into<String>) -> Result<Self, StoreError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(StoreError::EmptyIdempotencyKey);
        }
        Ok(Self(value))
    }

    /// Returns the key as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for IdempotencyKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for IdempotencyKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One typed correctness mutation accepted by a [`DurableStore`].
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum DurableMutation {
    /// Establish or update the workflow replay identity for this run.
    RecordWorkflowReplayIdentity(WorkflowReplayIdentity),
    /// Admit an attempt before its observation or external dispatch.
    AdmitAttempt(AttemptAdmission),
    /// Establish ownership of one logical external effect.
    RecordIntent(EffectIntent),
    /// Retain a cancellation fact.
    RecordCancellation(CancellationRecord),
    /// Record the append-before-effect dispatch boundary.
    RecordDispatch(DispatchRecord),
    /// Record a result for one exact attempt.
    RecordOutcome(OutcomeRecord),
    /// Record a workflow task completion after its durable prerequisites pass.
    RecordCompletion(CompletionRecord),
}

/// One atomic compare-and-swap request against a run's durable state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitRequest {
    /// Run whose state is being changed.
    pub run_id: RunId,
    /// Revision observed by the caller before preparing this request.
    pub expected_revision: StoreRevision,
    /// Stable replay identity for this mutation batch.
    pub idempotency_key: IdempotencyKey,
    /// Typed mutations applied atomically and in order.
    pub mutations: Vec<DurableMutation>,
}

/// One ordered durable commit entry.
///
/// This ledger is the replay authority for commit order. The idempotency map
/// in [`DurableRunState`] is only a materialized lookup index and must match
/// this ledger exactly when a state is loaded.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CommitLedgerEntry {
    /// Key used to replay the commit idempotently.
    pub idempotency_key: IdempotencyKey,
    /// Typed mutation batch committed at this revision.
    pub mutations: Vec<DurableMutation>,
    /// Durable revision assigned to this commit.
    pub revision: StoreRevision,
}

impl CommitRequest {
    /// Creates a request containing one typed mutation.
    #[must_use]
    pub fn single(
        run_id: RunId,
        expected_revision: StoreRevision,
        idempotency_key: IdempotencyKey,
        mutation: DurableMutation,
    ) -> Self {
        Self {
            run_id,
            expected_revision,
            idempotency_key,
            mutations: vec![mutation],
        }
    }
}

/// Result of a durable commit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommitResult {
    /// Current durable revision after applying or replaying the request.
    ///
    /// An idempotent replay does not mutate state. Its revision is therefore
    /// the current store head, rather than the revision at which the key was
    /// first committed.
    pub revision: StoreRevision,
    /// Whether this result came from an idempotent replay.
    pub replayed: bool,
}

/// Structured durable-state invariant violation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StoreInvariant {
    /// A durable completion referenced a task absent from the supplied
    /// workflow topology during replay.
    CompletionUnknownTask {
        /// Task named by the invalid completion fact.
        task_id: Id,
    },
    /// A durable completion was replayed before one of its prerequisites.
    CompletionPrerequisiteMismatch {
        /// Task whose completion cannot be replayed.
        task_id: Id,
        /// Prerequisite that was not yet completed.
        dependency: Id,
    },
    /// A durable completion log could not be replayed by the workflow graph.
    CompletionReplayRejected {
        /// Task involved in the rejected replay, when known.
        task_id: Option<Id>,
        /// Stable description of the graph rejection.
        reason: String,
    },
    /// A completion referenced no admitted attempt.
    CompletionWithoutAdmission {
        /// Attempt named by the completion.
        attempt_id: AttemptId,
    },
    /// A completion attempt belongs to another task.
    CompletionAttemptMismatch {
        /// Task named by the completion.
        task_id: Id,
        /// Attempt named by the completion.
        attempt_id: AttemptId,
    },
    /// An effect completion referenced an operation without its intent.
    CompletionWithoutIntent {
        /// Operation named by the admitted attempt.
        operation_id: OperationId,
    },
    /// An effect completion referenced an attempt without a dispatch fact.
    CompletionWithoutDispatch {
        /// Attempt named by the completion.
        attempt_id: AttemptId,
    },
    /// An effect completion was not for the latest dispatch attempt.
    CompletionNotLatestDispatch {
        /// Operation whose latest dispatch wins recovery authority.
        operation_id: OperationId,
        /// Older attempt incorrectly used for completion.
        attempt_id: AttemptId,
    },
    /// An effect completion lacked a known successful outcome.
    CompletionWithoutSuccessfulOutcome {
        /// Attempt named by the completion.
        attempt_id: AttemptId,
        /// Known outcome, if any.
        outcome: Option<KnownEffectOutcome>,
    },
    /// A task already has a different durable completion lineage.
    ConflictingCompletion {
        /// Task whose completion facts conflict.
        task_id: Id,
        /// Attempt that was recorded first.
        existing_attempt_id: AttemptId,
        /// Attempt supplied by the conflicting fact.
        attempted_attempt_id: AttemptId,
    },
    /// An attempt id was already admitted with a different identity.
    AttemptIdReuse {
        /// Reused attempt identity.
        attempt_id: AttemptId,
    },
    /// A dispatch referenced no admitted attempt.
    DispatchWithoutAdmission {
        /// Referenced attempt.
        attempt_id: AttemptId,
    },
    /// A dispatch operation did not match the admitted attempt.
    DispatchAttemptMismatch {
        /// Attempt named by the dispatch.
        attempt_id: AttemptId,
        /// Operation named by the dispatch.
        operation_id: OperationId,
    },
    /// An outcome referenced no admitted attempt.
    OutcomeWithoutAdmission {
        /// Referenced attempt.
        attempt_id: AttemptId,
    },
    /// An outcome referenced no dispatch for this attempt.
    OutcomeWithoutDispatch {
        /// Referenced attempt.
        attempt_id: AttemptId,
    },
    /// An outcome operation did not match its attempt lineage.
    OutcomeAttemptMismatch {
        /// Attempt named by the outcome.
        attempt_id: AttemptId,
        /// Operation named by the outcome.
        operation_id: OperationId,
    },
    /// A second outcome disagreed with the first for one attempt.
    ConflictingOutcome {
        /// Attempt with conflicting facts.
        attempt_id: AttemptId,
        /// Existing known outcome.
        existing: KnownEffectOutcome,
        /// New conflicting outcome.
        attempted: KnownEffectOutcome,
    },
    /// An intent was attempted after a different operation claimed the task.
    TaskOperationConflict {
        /// Task claimed by another operation.
        task_id: Id,
        /// Existing owner.
        existing: OperationId,
        /// Attempted owner.
        attempted: OperationId,
    },
    /// A dispatch was attempted after pre-dispatch cancellation.
    DispatchCancelled {
        /// Task whose dispatch was prevented.
        task_id: Id,
    },
    /// A cancellation did not match the task's durable operation lineage.
    CancellationLineageMismatch {
        /// Task whose cancellation was invalid.
        task_id: Id,
    },
    /// A task already has a different durable cancellation fact.
    ConflictingCancellation {
        /// Task whose cancellation facts conflict.
        task_id: Id,
    },
}

/// Error returned by a durable store.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StoreError {
    /// The requested run does not exist.
    RunNotFound(RunId),
    /// A run with this identity already exists.
    RunAlreadyExists(RunId),
    /// The request was prepared against a stale durable revision.
    RevisionConflict {
        /// Revision supplied by the caller.
        expected: StoreRevision,
        /// Current durable revision.
        actual: StoreRevision,
    },
    /// The same idempotency key was reused for different mutations.
    IdempotencyConflict {
        /// Key reused with a different mutation.
        key: IdempotencyKey,
    },
    /// An empty idempotency key was supplied.
    EmptyIdempotencyKey,
    /// An empty workflow replay identity was supplied.
    EmptyWorkflowReplayIdentity,
    /// The mutation list was empty.
    EmptyCommit,
    /// A typed mutation violated a durable invariant.
    InvariantViolation(StoreInvariant),
    /// The durable revision cannot advance further.
    RevisionExhausted {
        /// Last representable revision.
        current: StoreRevision,
    },
    /// The physical or remote backend is temporarily unavailable.
    BackendUnavailable,
    /// The backend reported an I/O failure without implying data corruption.
    IoFailure(String),
    /// The backend returned data that cannot be trusted as a durable state.
    DataCorruption(String),
}

/// Broad category of a durable-store failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoreErrorKind {
    /// A domain-level conflict or invariant rejection.
    Domain,
    /// The backend could not currently be reached or used.
    BackendUnavailable,
    /// The backend reported an I/O failure.
    IoFailure,
    /// Persisted data failed integrity or decoding validation.
    DataCorruption,
}

impl StoreError {
    /// Returns the backend-neutral category of this error.
    #[must_use]
    pub const fn kind(&self) -> StoreErrorKind {
        match self {
            Self::BackendUnavailable => StoreErrorKind::BackendUnavailable,
            Self::IoFailure(_) => StoreErrorKind::IoFailure,
            Self::DataCorruption(_) => StoreErrorKind::DataCorruption,
            _ => StoreErrorKind::Domain,
        }
    }

    /// Returns whether this is a backend-unavailable failure.
    #[must_use]
    pub const fn is_backend_unavailable(&self) -> bool {
        matches!(self, Self::BackendUnavailable)
    }

    /// Returns whether this is an I/O failure.
    #[must_use]
    pub const fn is_io_failure(&self) -> bool {
        matches!(self, Self::IoFailure(_))
    }

    /// Returns whether this is a persistent-data corruption failure.
    #[must_use]
    pub const fn is_data_corruption(&self) -> bool {
        matches!(self, Self::DataCorruption(_))
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RunNotFound(run_id) => write!(f, "durable run not found: {run_id}"),
            Self::RunAlreadyExists(run_id) => write!(f, "durable run already exists: {run_id}"),
            Self::RevisionConflict { expected, actual } => {
                write!(
                    f,
                    "durable revision conflict: expected {expected}, actual {actual}"
                )
            }
            Self::IdempotencyConflict { key } => {
                write!(
                    f,
                    "idempotency key reused with a conflicting mutation: {key}"
                )
            }
            Self::EmptyIdempotencyKey => f.write_str("idempotency key is empty"),
            Self::EmptyWorkflowReplayIdentity => f.write_str("workflow replay identity is empty"),
            Self::EmptyCommit => f.write_str("durable commit contains no mutations"),
            Self::InvariantViolation(invariant) => {
                write!(f, "durable invariant violation: {invariant:?}")
            }
            Self::RevisionExhausted { current } => {
                write!(f, "durable revision exhausted at {current}")
            }
            Self::BackendUnavailable => f.write_str("durable store backend is unavailable"),
            Self::IoFailure(message) => write!(f, "durable store I/O failure: {message}"),
            Self::DataCorruption(message) => {
                write!(f, "durable store data corruption: {message}")
            }
        }
    }
}

impl std::error::Error for StoreError {}

/// Materialized durable facts for one run, suitable for restart inspection.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DurableRunState {
    run_id: RunId,
    pub(crate) revision: StoreRevision,
    workflow_replay_identity: Option<WorkflowReplayIdentity>,
    admissions: BTreeMap<AttemptId, AttemptAdmission>,
    admission_history: Vec<AttemptAdmission>,
    intents: BTreeMap<OperationId, EffectIntent>,
    task_operations: BTreeMap<Id, OperationId>,
    cancellations: BTreeMap<Id, CancellationRecord>,
    dispatches: BTreeMap<AttemptId, DispatchRecord>,
    dispatch_history: Vec<DispatchRecord>,
    outcomes: BTreeMap<AttemptId, OutcomeRecord>,
    outcome_history: Vec<OutcomeRecord>,
    completions: BTreeMap<Id, CompletionRecord>,
    completion_history: Vec<CompletionRecord>,
    commit_ledger: Vec<CommitLedgerEntry>,
    /// Materialized lookup index derived from [`Self::commit_ledger`].
    pub(crate) idempotency: BTreeMap<IdempotencyKey, (Vec<DurableMutation>, StoreRevision)>,
}

impl DurableRunState {
    pub(crate) fn new(run_id: RunId) -> Self {
        Self {
            run_id,
            revision: StoreRevision::INITIAL,
            workflow_replay_identity: None,
            admissions: BTreeMap::new(),
            admission_history: Vec::new(),
            intents: BTreeMap::new(),
            task_operations: BTreeMap::new(),
            cancellations: BTreeMap::new(),
            dispatches: BTreeMap::new(),
            dispatch_history: Vec::new(),
            outcomes: BTreeMap::new(),
            outcome_history: Vec::new(),
            completions: BTreeMap::new(),
            completion_history: Vec::new(),
            commit_ledger: Vec::new(),
            idempotency: BTreeMap::new(),
        }
    }

    /// Returns the run identity.
    #[must_use]
    pub const fn run_id(&self) -> &RunId {
        &self.run_id
    }

    /// Returns the current durable revision.
    #[must_use]
    pub const fn revision(&self) -> StoreRevision {
        self.revision
    }

    /// Returns the durable workflow replay identity, if it has been set.
    #[must_use]
    pub const fn workflow_replay_identity(&self) -> Option<&WorkflowReplayIdentity> {
        self.workflow_replay_identity.as_ref()
    }

    /// Returns all admitted attempts in durable admission order.
    pub fn attempts(&self) -> impl Iterator<Item = &AttemptAdmission> {
        self.admission_history.iter()
    }

    /// Returns one admitted attempt.
    #[must_use]
    pub fn attempt(&self, attempt_id: &AttemptId) -> Option<&AttemptAdmission> {
        self.admissions.get(attempt_id)
    }

    /// Returns the intent for an operation.
    #[must_use]
    pub fn intent(&self, operation_id: &OperationId) -> Option<&EffectIntent> {
        self.intents.get(operation_id)
    }

    /// Returns all durable intents in deterministic operation order.
    pub fn intents(&self) -> impl Iterator<Item = &EffectIntent> {
        self.intents.values()
    }

    /// Returns the cancellation fact for a task, if any.
    #[must_use]
    pub fn cancellation(&self, task_id: &Id) -> Option<&CancellationRecord> {
        self.cancellations.get(task_id)
    }

    /// Returns all durable cancellation facts in deterministic task order.
    pub fn cancellations(&self) -> impl Iterator<Item = &CancellationRecord> {
        self.cancellations.values()
    }

    /// Returns whether the task has a durable cancellation fact.
    #[must_use]
    pub fn is_cancelled(&self, task_id: &Id) -> bool {
        self.cancellations.contains_key(task_id)
    }

    /// Returns all dispatch facts in durable append order.
    #[must_use]
    pub fn dispatch_history(&self) -> &[DispatchRecord] {
        &self.dispatch_history
    }

    /// Returns all dispatches for one logical operation in durable order.
    pub fn dispatches(&self, operation_id: &OperationId) -> impl Iterator<Item = &DispatchRecord> {
        self.dispatch_history
            .iter()
            .filter(move |dispatch| &dispatch.operation_id == operation_id)
    }

    /// Returns the latest durable dispatch, which owns recovery authority.
    #[must_use]
    pub fn latest_dispatch(&self, operation_id: &OperationId) -> Option<&DispatchRecord> {
        self.dispatches(operation_id).last()
    }

    /// Returns the known outcome for one exact attempt.
    #[must_use]
    pub fn outcome_for_attempt(&self, attempt_id: &AttemptId) -> Option<&OutcomeRecord> {
        self.outcomes.get(attempt_id)
    }

    /// Returns known outcomes for all attempts of one operation.
    pub fn outcome_history(
        &self,
        operation_id: &OperationId,
    ) -> impl Iterator<Item = &OutcomeRecord> {
        self.outcome_history
            .iter()
            .filter(move |outcome| &outcome.operation_id == operation_id)
    }

    /// Returns all known outcomes in durable append order.
    pub fn outcome_history_all(&self) -> &[OutcomeRecord] {
        &self.outcome_history
    }

    /// Returns all durable completion facts in append order.
    #[must_use]
    pub fn completion_history(&self) -> &[CompletionRecord] {
        &self.completion_history
    }

    /// Returns whether a task has a durable completion fact.
    #[must_use]
    pub fn is_completed(&self, task_id: &Id) -> bool {
        self.completions.contains_key(task_id)
    }

    /// Returns the durable completion fact for one task, if present.
    #[must_use]
    pub fn completion_for_task(&self, task_id: &Id) -> Option<&CompletionRecord> {
        self.completions.get(task_id)
    }

    /// Returns the ordered durable commit ledger.
    #[must_use]
    pub fn commit_ledger(&self) -> &[CommitLedgerEntry] {
        &self.commit_ledger
    }

    /// Returns the outcome of the latest dispatch, if known.
    #[must_use]
    pub fn latest_outcome(&self, operation_id: &OperationId) -> Option<&OutcomeRecord> {
        self.latest_dispatch(operation_id)
            .and_then(|dispatch| self.outcome_for_attempt(&dispatch.attempt_id))
    }

    /// Derives the local effect state from the latest dispatch only.
    #[must_use]
    pub fn effect_state(&self, operation_id: &OperationId) -> RecoveredEffectState {
        if !self.intents.contains_key(operation_id) {
            return RecoveredEffectState::NotPrepared;
        }
        match self.latest_dispatch(operation_id) {
            None => RecoveredEffectState::Prepared,
            Some(dispatch) => self
                .outcome_for_attempt(&dispatch.attempt_id)
                .map_or(RecoveredEffectState::OutcomeUnknown, |outcome| {
                    RecoveredEffectState::OutcomeKnown(outcome.outcome)
                }),
        }
    }

    /// Returns operation identities with durable intent.
    #[must_use]
    pub fn operation_ids(&self) -> BTreeSet<OperationId> {
        self.intents.keys().cloned().collect()
    }

    /// Returns the operation owned by a task, if any.
    #[must_use]
    pub fn operation_for_task(&self, task_id: &Id) -> Option<&OperationId> {
        self.task_operations.get(task_id)
    }

    pub(crate) fn validate_persisted(&self) -> Result<(), StoreError> {
        if self
            .workflow_replay_identity
            .as_ref()
            .is_some_and(|identity| identity.as_str().trim().is_empty())
        {
            return Err(StoreError::DataCorruption(
                "persisted workflow replay identity is empty".to_string(),
            ));
        }
        for key in self.idempotency.keys() {
            if key.as_str().trim().is_empty() {
                return Err(StoreError::DataCorruption(
                    "persisted idempotency metadata is invalid".to_string(),
                ));
            }
        }

        let mut reconstructed = Self::new(self.run_id.clone());
        let mut reconstructed_index = BTreeMap::new();
        let mut expected_revision = StoreRevision::INITIAL.get() + 1;
        for entry in &self.commit_ledger {
            if entry.idempotency_key.as_str().trim().is_empty()
                || entry.mutations.is_empty()
                || entry.revision.get() != expected_revision
            {
                return Err(StoreError::DataCorruption(
                    "persisted commit ledger is invalid".to_string(),
                ));
            }
            if reconstructed_index
                .insert(
                    entry.idempotency_key.clone(),
                    (entry.mutations.clone(), entry.revision),
                )
                .is_some()
            {
                return Err(StoreError::DataCorruption(
                    "persisted commit ledger reuses an idempotency key".to_string(),
                ));
            }
            for mutation in &entry.mutations {
                apply_mutation(&mut reconstructed, mutation).map_err(persisted_mutation_error)?;
            }
            reconstructed.revision = entry.revision;
            expected_revision = expected_revision.checked_add(1).ok_or_else(|| {
                StoreError::DataCorruption(
                    "persisted idempotency revision sequence is exhausted".to_string(),
                )
            })?;
        }
        if self.revision.get() != expected_revision - 1 {
            return Err(StoreError::DataCorruption(
                "persisted revision does not match commit ledger".to_string(),
            ));
        }

        if self.idempotency != reconstructed_index {
            return Err(StoreError::DataCorruption(
                "persisted idempotency index does not match commit ledger".to_string(),
            ));
        }

        if reconstructed.revision != self.revision
            || reconstructed.workflow_replay_identity != self.workflow_replay_identity
            || reconstructed.admissions != self.admissions
            || reconstructed.admission_history != self.admission_history
            || reconstructed.intents != self.intents
            || reconstructed.task_operations != self.task_operations
            || reconstructed.cancellations != self.cancellations
            || reconstructed.dispatches != self.dispatches
            || reconstructed.dispatch_history != self.dispatch_history
            || reconstructed.outcomes != self.outcomes
            || reconstructed.outcome_history != self.outcome_history
            || reconstructed.completions != self.completions
            || reconstructed.completion_history != self.completion_history
        {
            return Err(StoreError::DataCorruption(
                "persisted durable facts do not form a consistent lineage".to_string(),
            ));
        }
        Ok(())
    }

    pub(crate) fn record_commit(
        &mut self,
        idempotency_key: IdempotencyKey,
        mutations: Vec<DurableMutation>,
        revision: StoreRevision,
    ) {
        self.revision = revision;
        self.commit_ledger.push(CommitLedgerEntry {
            idempotency_key: idempotency_key.clone(),
            mutations: mutations.clone(),
            revision,
        });
        self.idempotency
            .insert(idempotency_key, (mutations, revision));
    }
}

fn persisted_mutation_error(error: StoreError) -> StoreError {
    StoreError::DataCorruption(format!("persisted durable fact is invalid: {error}"))
}

/// Minimal synchronous persistence port for durable run facts.
pub trait DurableStore {
    /// Creates an empty run at [`StoreRevision::INITIAL`].
    fn create_run(&mut self, run_id: RunId) -> Result<StoreRevision, StoreError>;

    /// Loads a detached durable view suitable for restart reconstruction.
    fn load_run(&self, run_id: &RunId) -> Result<DurableRunState, StoreError>;

    /// Applies one typed mutation batch atomically with compare-and-swap.
    fn commit(&mut self, request: CommitRequest) -> Result<CommitResult, StoreError>;
}

/// Deterministic in-memory conformance adapter.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct InMemoryDurableStore {
    runs: BTreeMap<RunId, DurableRunState>,
}

impl InMemoryDurableStore {
    /// Creates an empty store.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            runs: BTreeMap::new(),
        }
    }
}

impl DurableStore for InMemoryDurableStore {
    fn create_run(&mut self, run_id: RunId) -> Result<StoreRevision, StoreError> {
        if self.runs.contains_key(&run_id) {
            return Err(StoreError::RunAlreadyExists(run_id));
        }
        self.runs
            .insert(run_id.clone(), DurableRunState::new(run_id));
        Ok(StoreRevision::INITIAL)
    }

    fn load_run(&self, run_id: &RunId) -> Result<DurableRunState, StoreError> {
        self.runs
            .get(run_id)
            .cloned()
            .ok_or_else(|| StoreError::RunNotFound(run_id.clone()))
    }

    fn commit(&mut self, request: CommitRequest) -> Result<CommitResult, StoreError> {
        if request.mutations.is_empty() {
            return Err(StoreError::EmptyCommit);
        }
        let state = self
            .runs
            .get_mut(&request.run_id)
            .ok_or_else(|| StoreError::RunNotFound(request.run_id.clone()))?;

        if let Some((existing, _revision)) = state.idempotency.get(&request.idempotency_key) {
            if existing == &request.mutations {
                return Ok(CommitResult {
                    revision: state.revision,
                    replayed: true,
                });
            }
            return Err(StoreError::IdempotencyConflict {
                key: request.idempotency_key,
            });
        }
        if request.expected_revision != state.revision {
            return Err(StoreError::RevisionConflict {
                expected: request.expected_revision,
                actual: state.revision,
            });
        }
        let revision = state
            .revision
            .checked_next()
            .ok_or(StoreError::RevisionExhausted {
                current: state.revision,
            })?;
        let mut candidate = state.clone();
        for mutation in &request.mutations {
            apply_mutation(&mut candidate, mutation)?;
        }
        candidate.revision = revision;
        candidate.record_commit(request.idempotency_key, request.mutations, revision);
        *state = candidate;
        Ok(CommitResult {
            revision,
            replayed: false,
        })
    }
}

pub(crate) fn apply_mutation(
    state: &mut DurableRunState,
    mutation: &DurableMutation,
) -> Result<(), StoreError> {
    match mutation {
        DurableMutation::RecordWorkflowReplayIdentity(identity) => {
            if identity.as_str().trim().is_empty() {
                return Err(StoreError::EmptyWorkflowReplayIdentity);
            }
            state.workflow_replay_identity = Some(identity.clone());
        }
        DurableMutation::AdmitAttempt(admission) => {
            if admission.run_id != state.run_id {
                return Err(StoreError::InvariantViolation(
                    StoreInvariant::AttemptIdReuse {
                        attempt_id: admission.attempt_id.clone(),
                    },
                ));
            }
            if state.admissions.contains_key(&admission.attempt_id) {
                return Err(StoreError::InvariantViolation(
                    StoreInvariant::AttemptIdReuse {
                        attempt_id: admission.attempt_id.clone(),
                    },
                ));
            }
            state
                .admissions
                .insert(admission.attempt_id.clone(), admission.clone());
            state.admission_history.push(admission.clone());
        }
        DurableMutation::RecordIntent(intent) => {
            if let Some(existing) = state.intents.get(&intent.operation_id) {
                if existing == intent {
                    return Ok(());
                }
                return Err(StoreError::InvariantViolation(
                    StoreInvariant::TaskOperationConflict {
                        task_id: intent.task_id.clone(),
                        existing: existing.operation_id.clone(),
                        attempted: intent.operation_id.clone(),
                    },
                ));
            }
            if let Some(existing) = state.task_operations.get(&intent.task_id) {
                return Err(StoreError::InvariantViolation(
                    StoreInvariant::TaskOperationConflict {
                        task_id: intent.task_id.clone(),
                        existing: existing.clone(),
                        attempted: intent.operation_id.clone(),
                    },
                ));
            }
            state
                .task_operations
                .insert(intent.task_id.clone(), intent.operation_id.clone());
            state
                .intents
                .insert(intent.operation_id.clone(), intent.clone());
        }
        DurableMutation::RecordCancellation(cancellation) => {
            if cancellation.run_id != state.run_id {
                return Err(StoreError::InvariantViolation(
                    StoreInvariant::CancellationLineageMismatch {
                        task_id: cancellation.task_id.clone(),
                    },
                ));
            }
            if let Some(existing) = state.cancellations.get(&cancellation.task_id) {
                if existing == cancellation {
                    return Ok(());
                }
                return Err(StoreError::InvariantViolation(
                    StoreInvariant::ConflictingCancellation {
                        task_id: cancellation.task_id.clone(),
                    },
                ));
            }
            if let Some(operation_id) = &cancellation.operation_id {
                if state.task_operations.get(&cancellation.task_id) != Some(operation_id) {
                    return Err(StoreError::InvariantViolation(
                        StoreInvariant::CancellationLineageMismatch {
                            task_id: cancellation.task_id.clone(),
                        },
                    ));
                }
            }
            if let Some(attempt_id) = &cancellation.attempt_id {
                let Some(admission) = state.admissions.get(attempt_id) else {
                    return Err(StoreError::InvariantViolation(
                        StoreInvariant::CancellationLineageMismatch {
                            task_id: cancellation.task_id.clone(),
                        },
                    ));
                };
                if admission.task_id != cancellation.task_id
                    || admission.operation_id != cancellation.operation_id
                {
                    return Err(StoreError::InvariantViolation(
                        StoreInvariant::CancellationLineageMismatch {
                            task_id: cancellation.task_id.clone(),
                        },
                    ));
                }
            }
            state
                .cancellations
                .insert(cancellation.task_id.clone(), cancellation.clone());
        }
        DurableMutation::RecordDispatch(dispatch) => {
            let Some(intent) = state.intents.get(&dispatch.operation_id) else {
                return Err(StoreError::InvariantViolation(
                    StoreInvariant::DispatchWithoutAdmission {
                        attempt_id: dispatch.attempt_id.clone(),
                    },
                ));
            };
            let Some(admission) = state.admissions.get(&dispatch.attempt_id) else {
                return Err(StoreError::InvariantViolation(
                    StoreInvariant::DispatchWithoutAdmission {
                        attempt_id: dispatch.attempt_id.clone(),
                    },
                ));
            };
            if admission.operation_id.as_ref() != Some(&dispatch.operation_id) {
                return Err(StoreError::InvariantViolation(
                    StoreInvariant::DispatchAttemptMismatch {
                        attempt_id: dispatch.attempt_id.clone(),
                        operation_id: dispatch.operation_id.clone(),
                    },
                ));
            }
            if intent.task_id != admission.task_id {
                return Err(StoreError::InvariantViolation(
                    StoreInvariant::DispatchAttemptMismatch {
                        attempt_id: dispatch.attempt_id.clone(),
                        operation_id: dispatch.operation_id.clone(),
                    },
                ));
            }
            if state.cancellations.contains_key(&admission.task_id)
                && !state.dispatches.contains_key(&dispatch.attempt_id)
            {
                return Err(StoreError::InvariantViolation(
                    StoreInvariant::DispatchCancelled {
                        task_id: admission.task_id.clone(),
                    },
                ));
            }
            if state.dispatches.contains_key(&dispatch.attempt_id) {
                return Err(StoreError::InvariantViolation(
                    StoreInvariant::DispatchAttemptMismatch {
                        attempt_id: dispatch.attempt_id.clone(),
                        operation_id: dispatch.operation_id.clone(),
                    },
                ));
            }
            state
                .dispatches
                .insert(dispatch.attempt_id.clone(), dispatch.clone());
            state.dispatch_history.push(dispatch.clone());
        }
        DurableMutation::RecordOutcome(outcome) => {
            let Some(admission) = state.admissions.get(&outcome.attempt_id) else {
                return Err(StoreError::InvariantViolation(
                    StoreInvariant::OutcomeWithoutAdmission {
                        attempt_id: outcome.attempt_id.clone(),
                    },
                ));
            };
            let Some(dispatch) = state.dispatches.get(&outcome.attempt_id) else {
                return Err(StoreError::InvariantViolation(
                    StoreInvariant::OutcomeWithoutDispatch {
                        attempt_id: outcome.attempt_id.clone(),
                    },
                ));
            };
            if dispatch.operation_id != outcome.operation_id
                || admission.operation_id.as_ref() != Some(&outcome.operation_id)
            {
                return Err(StoreError::InvariantViolation(
                    StoreInvariant::OutcomeAttemptMismatch {
                        attempt_id: outcome.attempt_id.clone(),
                        operation_id: outcome.operation_id.clone(),
                    },
                ));
            }
            if let Some(existing) = state.outcomes.get(&outcome.attempt_id) {
                if existing.outcome == outcome.outcome {
                    return Ok(());
                }
                return Err(StoreError::InvariantViolation(
                    StoreInvariant::ConflictingOutcome {
                        attempt_id: outcome.attempt_id.clone(),
                        existing: existing.outcome,
                        attempted: outcome.outcome,
                    },
                ));
            }
            state
                .outcomes
                .insert(outcome.attempt_id.clone(), outcome.clone());
            state.outcome_history.push(outcome.clone());
        }
        DurableMutation::RecordCompletion(completion) => {
            let Some(admission) = state.admissions.get(&completion.attempt_id) else {
                return Err(StoreError::InvariantViolation(
                    StoreInvariant::CompletionWithoutAdmission {
                        attempt_id: completion.attempt_id.clone(),
                    },
                ));
            };
            if admission.task_id != completion.task_id {
                return Err(StoreError::InvariantViolation(
                    StoreInvariant::CompletionAttemptMismatch {
                        task_id: completion.task_id.clone(),
                        attempt_id: completion.attempt_id.clone(),
                    },
                ));
            }
            if let Some(operation_id) = &admission.operation_id {
                if !state.intents.contains_key(operation_id) {
                    return Err(StoreError::InvariantViolation(
                        StoreInvariant::CompletionWithoutIntent {
                            operation_id: operation_id.clone(),
                        },
                    ));
                }
                if state
                    .dispatches
                    .get(&completion.attempt_id)
                    .is_none_or(|dispatch| &dispatch.operation_id != operation_id)
                {
                    return Err(StoreError::InvariantViolation(
                        StoreInvariant::CompletionWithoutDispatch {
                            attempt_id: completion.attempt_id.clone(),
                        },
                    ));
                }
                if state
                    .latest_dispatch(operation_id)
                    .is_none_or(|dispatch| dispatch.attempt_id != completion.attempt_id)
                {
                    return Err(StoreError::InvariantViolation(
                        StoreInvariant::CompletionNotLatestDispatch {
                            operation_id: operation_id.clone(),
                            attempt_id: completion.attempt_id.clone(),
                        },
                    ));
                }
                let outcome = state
                    .outcomes
                    .get(&completion.attempt_id)
                    .map(|outcome| outcome.outcome);
                if outcome != Some(KnownEffectOutcome::Succeeded) {
                    return Err(StoreError::InvariantViolation(
                        StoreInvariant::CompletionWithoutSuccessfulOutcome {
                            attempt_id: completion.attempt_id.clone(),
                            outcome,
                        },
                    ));
                }
            }
            if let Some(existing) = state.completions.get(&completion.task_id) {
                if existing == completion {
                    return Ok(());
                }
                return Err(StoreError::InvariantViolation(
                    StoreInvariant::ConflictingCompletion {
                        task_id: completion.task_id.clone(),
                        existing_attempt_id: existing.attempt_id.clone(),
                        attempted_attempt_id: completion.attempt_id.clone(),
                    },
                ));
            }
            state
                .completions
                .insert(completion.task_id.clone(), completion.clone());
            state.completion_history.push(completion.clone());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persisted_validation_replays_idempotency_batches() {
        let run_id = RunId::new("persisted-validation-run").expect("run id is valid");
        let identity = WorkflowReplayIdentity::new("workflow:v1").expect("identity is valid");
        let idempotency_key = IdempotencyKey::new("identity").expect("key is valid");
        let mut state = DurableRunState::new(run_id);
        state.workflow_replay_identity = Some(identity.clone());
        state.record_commit(
            idempotency_key,
            vec![DurableMutation::RecordWorkflowReplayIdentity(
                identity.clone(),
            )],
            StoreRevision(1),
        );
        state
            .validate_persisted()
            .expect("valid idempotency batch replays");

        state
            .idempotency
            .values_mut()
            .next()
            .expect("entry exists")
            .0 = vec![DurableMutation::RecordWorkflowReplayIdentity(
            WorkflowReplayIdentity::new("workflow:v2").expect("identity is valid"),
        )];
        assert!(matches!(
            state.validate_persisted(),
            Err(StoreError::DataCorruption(_))
        ));
    }

    #[test]
    fn persisted_validation_rejects_rekeyed_idempotency_history() {
        let run_id = RunId::new("rekeyed-history-run").expect("run id is valid");
        let identity = WorkflowReplayIdentity::new("workflow:v1").expect("identity is valid");
        let original_key = IdempotencyKey::new("original-key").expect("key is valid");
        let replacement_key = IdempotencyKey::new("replacement-key").expect("key is valid");
        let mut state = DurableRunState::new(run_id);
        state.workflow_replay_identity = Some(identity.clone());
        state.record_commit(
            original_key.clone(),
            vec![DurableMutation::RecordWorkflowReplayIdentity(identity)],
            StoreRevision(1),
        );
        let entry = state
            .idempotency
            .remove(&original_key)
            .expect("original entry exists");
        state.idempotency.insert(replacement_key, entry);

        assert!(matches!(
            state.validate_persisted(),
            Err(StoreError::DataCorruption(_))
        ));
    }

    #[test]
    fn persisted_validation_rejects_reordered_commit_revisions() {
        let run_id = RunId::new("reordered-history-run").expect("run id is valid");
        let first_identity =
            WorkflowReplayIdentity::new("workflow:first").expect("identity is valid");
        let second_identity =
            WorkflowReplayIdentity::new("workflow:second").expect("identity is valid");
        let first_key = IdempotencyKey::new("first-key").expect("key is valid");
        let second_key = IdempotencyKey::new("second-key").expect("key is valid");
        let mut state = DurableRunState::new(run_id);
        state.workflow_replay_identity = Some(second_identity.clone());
        state.record_commit(
            first_key.clone(),
            vec![DurableMutation::RecordWorkflowReplayIdentity(
                first_identity.clone(),
            )],
            StoreRevision(1),
        );
        state.record_commit(
            second_key.clone(),
            vec![DurableMutation::RecordWorkflowReplayIdentity(
                second_identity,
            )],
            StoreRevision(2),
        );
        let first_entry = state
            .idempotency
            .remove(&first_key)
            .expect("first entry exists");
        let second_entry = state
            .idempotency
            .remove(&second_key)
            .expect("second entry exists");
        state.idempotency.insert(first_key, second_entry);
        state.idempotency.insert(second_key, first_entry);

        assert!(matches!(
            state.validate_persisted(),
            Err(StoreError::DataCorruption(_))
        ));
    }

    #[test]
    fn persisted_validation_rejects_commit_ledger_key_or_order_tampering() {
        let run_id = RunId::new("ledger-tamper-run").expect("run id is valid");
        let first_identity =
            WorkflowReplayIdentity::new("workflow:first").expect("identity is valid");
        let second_identity =
            WorkflowReplayIdentity::new("workflow:second").expect("identity is valid");
        let first_key = IdempotencyKey::new("first-key").expect("key is valid");
        let second_key = IdempotencyKey::new("second-key").expect("key is valid");
        let mut state = DurableRunState::new(run_id);
        state.workflow_replay_identity = Some(second_identity.clone());
        state.record_commit(
            first_key,
            vec![DurableMutation::RecordWorkflowReplayIdentity(
                first_identity,
            )],
            StoreRevision(1),
        );
        state.record_commit(
            second_key,
            vec![DurableMutation::RecordWorkflowReplayIdentity(
                second_identity,
            )],
            StoreRevision(2),
        );
        state
            .validate_persisted()
            .expect("baseline ledger is valid");

        let mut key_tampered = state.clone();
        key_tampered.commit_ledger[0].idempotency_key =
            IdempotencyKey::new("tampered-key").expect("key is valid");
        assert!(matches!(
            key_tampered.validate_persisted(),
            Err(StoreError::DataCorruption(_))
        ));

        state.commit_ledger.swap(0, 1);
        assert!(matches!(
            state.validate_persisted(),
            Err(StoreError::DataCorruption(_))
        ));
    }
}

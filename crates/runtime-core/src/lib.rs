//! A synchronous, deterministic coordinator for the Kernis runtime structures.
//!
//! [`WorkflowGraph`] remains the authority for topology and completion,
//! [`workflow_recovery::DurableStore`] remains the persistence authority for
//! external-effect facts, while [`DurableJournal`] is the compatibility view,
//! [`capability_graph::Scope`] remains the authority for capability lifetime
//! and replacement. This crate owns coordination and disposable observations
//! only.

use capability_graph::{
    CapabilityContext, CapabilityHandle, CapabilityRegistry, EntryId, Generation,
    ReactiveCapabilityRuntime, Scope, ScopeError,
};
use execution_stream::{
    CoalescingBuffer, KeyedStreamItem, LosslessBuffer, LossyBuffer, PushError, SequenceError,
    StreamItem, StreamSequencer,
};
use kernis_core::Id;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use workflow_graph::{
    CompletionRecord as WorkflowCompletionRecord, MutationBatch, WorkflowGraph, WorkflowGraphError,
    WorkflowMutationRecord,
};
use workflow_recovery::{
    AttemptAdmission, AttemptId, CapabilityReplayIdentity, CommitRequest, CompletionRecord,
    DispatchRecord, DurableJournal, DurableMutation, DurableRunState, DurableStore, EffectIntent,
    EffectSemantics, IdempotencyKey, InMemoryDurableStore, KnownEffectOutcome, OperationId,
    OutcomeRecord, RecoveredEffectState, RecoveryAction, RecoveryDecision, StoreError,
    StoreInvariant, StoreRevision, WorkflowReplayIdentity, classify_recovery,
};

pub use workflow_recovery::RunId;

/// A capability handle pinned for the complete lifetime of one task attempt.
#[derive(Clone, Debug)]
pub struct CapabilityPin {
    /// Logical capability identity.
    pub capability_id: Id,
    /// Generation observed when the pin was created.
    pub generation: Generation,
    /// Exact published entry observed when the pin was created.
    pub entry_id: EntryId,
    /// Stable logical identity used for durable replay validation.
    pub replay_identity: CapabilityReplayIdentity,
    handle: CapabilityHandle,
}

impl CapabilityPin {
    /// Returns the exact handle retained by this pin.
    #[must_use]
    pub fn handle(&self) -> &CapabilityHandle {
        &self.handle
    }
}

/// One task execution attempt and its immutable capability snapshot.
#[derive(Clone, Debug)]
pub struct TaskAttempt {
    /// Workflow execution containing this attempt.
    pub run_id: RunId,
    /// Logical workflow task being attempted.
    pub task_id: Id,
    /// Unique transport/execution attempt identity.
    pub attempt_id: AttemptId,
    /// Logical effect owned by this attempt, when the task has one.
    pub operation_id: Option<OperationId>,
    /// Exact capability entries retained by this attempt.
    pub capability_pins: Vec<CapabilityPin>,
}

impl TaskAttempt {
    /// Looks up one pinned capability by logical identity.
    #[must_use]
    pub fn capability(&self, capability_id: &Id) -> Option<&CapabilityPin> {
        self.capability_pins
            .iter()
            .find(|pin| &pin.capability_id == capability_id)
    }
}

/// Runtime configuration for one logical external effect.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectSpec {
    /// Stable logical operation identity.
    pub operation_id: OperationId,
    /// Whether retrying this operation is externally safe.
    pub semantics: EffectSemantics,
}

/// Runtime inputs for a task; workflow topology and completion remain in the graph.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TaskConfig {
    required_capabilities: BTreeMap<Id, Option<String>>,
    effect: Option<EffectSpec>,
}

impl TaskConfig {
    /// Creates an empty task configuration.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            required_capabilities: BTreeMap::new(),
            effect: None,
        }
    }

    /// Adds one capability requirement in deterministic order.
    #[must_use]
    pub fn require_capability(mut self, capability_id: Id) -> Self {
        self.required_capabilities
            .entry(capability_id)
            .or_insert(None);
        self
    }

    /// Adds a capability requirement with a stable provider/configuration
    /// identity for restart validation.
    #[must_use]
    pub fn require_capability_with_identity(
        mut self,
        capability_id: Id,
        definition_identity: impl Into<String>,
    ) -> Self {
        self.required_capabilities
            .insert(capability_id, Some(definition_identity.into()));
        self
    }

    /// Assigns the one effect owned by this task.
    #[must_use]
    pub fn with_effect(mut self, operation_id: OperationId, semantics: EffectSemantics) -> Self {
        self.effect = Some(EffectSpec {
            operation_id,
            semantics,
        });
        self
    }

    /// Returns required capabilities in deterministic order.
    pub fn required_capabilities(&self) -> impl Iterator<Item = &Id> {
        self.required_capabilities.keys()
    }

    /// Returns the configured effect, if any.
    #[must_use]
    pub const fn effect(&self) -> Option<&EffectSpec> {
        self.effect.as_ref()
    }
}

/// Non-authoritative observations emitted by the Runtime Core.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeEvent {
    /// A task attempt acquired its capability pins.
    TaskStarted {
        /// Workflow execution identity.
        run_id: RunId,
        /// Task identity.
        task_id: Id,
        /// Attempt identity.
        attempt_id: AttemptId,
    },
    /// A disposable task progress observation.
    TaskProgress {
        /// Task identity.
        task_id: Id,
        /// Progress percentage represented by this observation.
        progress: u8,
    },
    /// A task completion observation after the workflow fact was recorded.
    TaskCompleted {
        /// Workflow execution identity.
        run_id: RunId,
        /// Task identity.
        task_id: Id,
        /// Attempt identity responsible for the completion.
        attempt_id: AttemptId,
    },
    /// A disposable diagnostic observation.
    Telemetry {
        /// Diagnostic payload.
        message: String,
    },
}

/// Result of one deterministic scheduler step.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StepResult {
    /// A task completed during this step.
    Completed {
        /// Completed task identity.
        task_id: Id,
        /// Attempt that produced the completion fact.
        attempt_id: AttemptId,
    },
    /// An effect task is prepared and awaits an explicit dispatch/outcome.
    EffectPending {
        /// Task whose effect is pending.
        task_id: Id,
        /// Attempt that pinned capabilities for the pending work.
        attempt_id: AttemptId,
        /// Logical effect identity.
        operation_id: OperationId,
    },
    /// A ready task cannot advance without a recovery decision or observation.
    Blocked {
        /// Task that caused the scheduler to stop, if one exists.
        task_id: Id,
        /// Effect identity requiring attention, if one exists.
        operation_id: Option<OperationId>,
        /// Durable recovery action currently indicated.
        action: RecoveryAction,
    },
    /// No non-cancelled task is ready.
    Idle,
}

/// Result of cancelling a task.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Cancellation {
    /// The task had not been dispatched and is now prevented from starting.
    NotDispatched {
        /// Cancelled task identity.
        task_id: Id,
    },
    /// An external dispatch already exists; cancellation cannot erase it.
    AlreadyDispatched {
        /// Task whose dispatch remains authoritative.
        task_id: Id,
        /// Logical effect identity.
        operation_id: OperationId,
        /// Dispatch attempt identity.
        attempt_id: AttemptId,
    },
}

/// Errors returned by the synchronous runtime coordinator.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeError {
    /// Workflow topology or completion rejected a mutation.
    Workflow(WorkflowGraphError),
    /// Capability admission or lookup rejected an attempt.
    Capability(ScopeError),
    /// A durable effect fact was invalid.
    Journal(workflow_recovery::JournalError),
    /// A durable store rejected a compare-and-swap or invariant check.
    Store(StoreError),
    /// A runtime observation stream exhausted its sequence.
    Stream(SequenceError),
    /// A requested task does not exist in the workflow authority.
    UnknownTask(Id),
    /// A configured capability was not visible when an attempt started.
    MissingCapability {
        /// Task that could not start.
        task_id: Id,
        /// Capability that was not visible.
        capability_id: Id,
    },
    /// Restart supplied a capability configuration different from the one
    /// durably admitted for an attempt.
    CapabilityReplayMismatch {
        /// Task whose admitted capability cannot be reconstructed.
        task_id: Id,
        /// Capability whose logical configuration differs.
        capability_id: Id,
    },
    /// The supplied topology/configuration does not match the durable run.
    WorkflowReplayIdentityMismatch {
        /// Identity computed from the supplied workflow and configuration.
        expected: WorkflowReplayIdentity,
        /// Identity recorded by the durable run, if present.
        actual: Option<WorkflowReplayIdentity>,
    },
    /// A run must be started or restored from topology without local
    /// completion facts; completions must come from the durable authority.
    PrecompletedWorkflow(Id),
    /// The deterministic attempt identity allocator is exhausted.
    AttemptIdExhausted,
    /// A cancelled task cannot be dispatched before its first dispatch.
    CancelledBeforeDispatch(Id),
    /// A cancelled task already has an external dispatch, so no retry is issued.
    CancelledAfterDispatch {
        /// Cancelled task identity.
        task_id: Id,
        /// Existing dispatch identity.
        operation_id: OperationId,
    },
    /// A non-idempotent unknown outcome needs reconciliation.
    ReconciliationRequired(OperationId),
    /// An operation already has a known outcome.
    OutcomeAlreadyKnown(OperationId),
    /// A coalescing progress stream is full for a new semantic key.
    ProgressBackpressure,
    /// The lossless lifecycle stream is full and retains the rejected item.
    ExecutionBackpressure {
        /// Lifecycle item that can be retried after draining the stream.
        item: StreamItem<RuntimeEvent>,
    },
}

impl From<WorkflowGraphError> for RuntimeError {
    fn from(error: WorkflowGraphError) -> Self {
        Self::Workflow(error)
    }
}

impl From<ScopeError> for RuntimeError {
    fn from(error: ScopeError) -> Self {
        Self::Capability(error)
    }
}

impl From<workflow_recovery::JournalError> for RuntimeError {
    fn from(error: workflow_recovery::JournalError) -> Self {
        Self::Journal(error)
    }
}

impl From<StoreError> for RuntimeError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<SequenceError> for RuntimeError {
    fn from(error: SequenceError) -> Self {
        Self::Stream(error)
    }
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Workflow(error) => write!(f, "workflow error: {error}"),
            Self::Capability(error) => write!(f, "capability error: {error}"),
            Self::Journal(error) => write!(f, "journal error: {error}"),
            Self::Store(error) => write!(f, "durable store error: {error}"),
            Self::Stream(error) => write!(f, "stream error: {error}"),
            Self::UnknownTask(task_id) => write!(f, "unknown task: {task_id}"),
            Self::MissingCapability {
                task_id,
                capability_id,
            } => write!(
                f,
                "task {task_id} requires unavailable capability {capability_id}"
            ),
            Self::CapabilityReplayMismatch {
                task_id,
                capability_id,
            } => write!(
                f,
                "task {task_id} capability {capability_id} does not match its durable replay identity"
            ),
            Self::WorkflowReplayIdentityMismatch { expected, actual } => write!(
                f,
                "workflow replay identity mismatch: expected {expected}, durable state has "
            )
            .and_then(|_| match actual {
                Some(actual) => write!(f, "{actual}"),
                None => f.write_str("none"),
            }),
            Self::PrecompletedWorkflow(task_id) => write!(
                f,
                "workflow supplied with non-durable completion fact for task {task_id}"
            ),
            Self::AttemptIdExhausted => f.write_str("task attempt identity exhausted"),
            Self::CancelledBeforeDispatch(task_id) => {
                write!(f, "task {task_id} was cancelled before dispatch")
            }
            Self::CancelledAfterDispatch {
                task_id,
                operation_id,
            } => write!(
                f,
                "task {task_id} was cancelled after dispatch of {operation_id}"
            ),
            Self::ReconciliationRequired(operation_id) => {
                write!(f, "effect {operation_id} requires reconciliation")
            }
            Self::OutcomeAlreadyKnown(operation_id) => {
                write!(f, "effect {operation_id} already has a known outcome")
            }
            Self::ProgressBackpressure => f.write_str("progress stream is backpressured"),
            Self::ExecutionBackpressure { .. } => f.write_str("execution stream is backpressured"),
        }
    }
}

impl std::error::Error for RuntimeError {}

/// Deterministic single-process Runtime Core.
pub struct Runtime<S = InMemoryDurableStore>
where
    S: DurableStore,
{
    run_id: RunId,
    workflow: WorkflowGraph,
    capability_runtime: ReactiveCapabilityRuntime,
    journal: DurableJournal,
    store: S,
    store_revision: StoreRevision,
    task_configs: BTreeMap<Id, TaskConfig>,
    attempts: Vec<TaskAttempt>,
    pending_started: Option<(TaskAttempt, StreamItem<RuntimeEvent>)>,
    cancelled: BTreeSet<Id>,
    next_attempt_number: u64,
    execution_sequencer: StreamSequencer,
    progress_sequencer: StreamSequencer,
    telemetry_sequencer: StreamSequencer,
    execution_events: LosslessBuffer<RuntimeEvent>,
    progress_events: CoalescingBuffer<Id, RuntimeEvent>,
    telemetry_events: LossyBuffer<RuntimeEvent>,
}

impl Runtime<InMemoryDurableStore> {
    /// Starts a deterministic run over an existing workflow graph and scope.
    ///
    /// Task configurations are runtime inputs. They do not become workflow
    /// topology or completion facts.
    pub fn start_run<I>(
        run_id: RunId,
        workflow: WorkflowGraph,
        scope: Scope,
        task_configs: I,
    ) -> Result<Self, RuntimeError>
    where
        I: IntoIterator<Item = (Id, TaskConfig)>,
    {
        Self::start_run_with_store(
            run_id,
            workflow,
            scope,
            task_configs,
            InMemoryDurableStore::new(),
        )
    }
}

impl<S> Runtime<S>
where
    S: DurableStore,
{
    /// Starts a deterministic run using the caller-supplied durable store.
    ///
    /// The runtime takes ownership of the store and creates the run exactly
    /// once before using the returned revision as its compare-and-swap head.
    pub fn start_run_with_store<I>(
        run_id: RunId,
        workflow: WorkflowGraph,
        scope: Scope,
        task_configs: I,
        mut store: S,
    ) -> Result<Self, RuntimeError>
    where
        I: IntoIterator<Item = (Id, TaskConfig)>,
    {
        ensure_unfinished_workflow(&workflow)?;
        let task_configs = collect_task_configs(&workflow, task_configs)?;
        let replay_identity = workflow_replay_identity(&workflow, &task_configs)?;
        let created_revision = store.create_run(run_id.clone())?;
        let store_revision = store
            .commit(CommitRequest::single(
                run_id.clone(),
                created_revision,
                IdempotencyKey::new("workflow-replay-identity")?,
                DurableMutation::RecordWorkflowReplayIdentity(replay_identity),
            ))?
            .revision;
        Self::build(
            run_id,
            workflow,
            scope,
            task_configs,
            DurableJournal::new(),
            store,
            store_revision,
        )
    }

    /// Restores a fresh process-local runtime from durable facts and supplied
    /// workflow/capability configuration.
    ///
    /// The store view is detached before runtime objects are created.  No
    /// stream, fiber, capability handle, or pending lifecycle observation is
    /// reused from the previous process.
    pub fn restore_run<I>(
        run_id: RunId,
        workflow: WorkflowGraph,
        scope: Scope,
        task_configs: I,
        store: S,
    ) -> Result<Self, RuntimeError>
    where
        I: IntoIterator<Item = (Id, TaskConfig)>,
    {
        ensure_unfinished_workflow(&workflow)?;
        let task_configs = collect_task_configs(&workflow, task_configs)?;
        let state = store.load_run(&run_id)?;
        let expected_identity = workflow_replay_identity(&workflow, &task_configs)?;
        if state.workflow_replay_identity() != Some(&expected_identity) {
            return Err(RuntimeError::WorkflowReplayIdentityMismatch {
                expected: expected_identity,
                actual: state.workflow_replay_identity().cloned(),
            });
        }
        let workflow = replay_workflow(&workflow, state.completion_history())?;
        let mut runtime = Self::build(
            run_id,
            workflow,
            scope,
            task_configs,
            DurableJournal::from_durable_state(&state)?,
            store,
            state.revision(),
        )?;
        for cancellation in state.cancellations() {
            runtime.cancelled.insert(cancellation.task_id.clone());
        }
        for admission in state.attempts() {
            let capability_pins = runtime
                .capture_capability_pins(&admission.task_id, Some(&admission.capabilities))?;
            runtime.attempts.push(TaskAttempt {
                run_id: admission.run_id.clone(),
                task_id: admission.task_id.clone(),
                attempt_id: admission.attempt_id.clone(),
                operation_id: admission.operation_id.clone(),
                capability_pins,
            });
        }
        runtime.next_attempt_number = state.attempts().count() as u64 + 1;
        Ok(runtime)
    }

    fn build(
        run_id: RunId,
        workflow: WorkflowGraph,
        scope: Scope,
        task_configs: BTreeMap<Id, TaskConfig>,
        journal: DurableJournal,
        store: S,
        store_revision: StoreRevision,
    ) -> Result<Self, RuntimeError> {
        Ok(Self {
            run_id,
            workflow,
            capability_runtime: ReactiveCapabilityRuntime::new(CapabilityContext::from_scope(
                scope,
            )),
            journal,
            store,
            store_revision,
            task_configs,
            attempts: Vec::new(),
            pending_started: None,
            cancelled: BTreeSet::new(),
            next_attempt_number: 1,
            execution_sequencer: StreamSequencer::new(stream_id("runtime-events")),
            progress_sequencer: StreamSequencer::new(stream_id("runtime-progress")),
            telemetry_sequencer: StreamSequencer::new(stream_id("runtime-telemetry")),
            execution_events: LosslessBuffer::new(128).expect("runtime event capacity is nonzero"),
            progress_events: CoalescingBuffer::new(8)
                .expect("runtime progress capacity is nonzero"),
            telemetry_events: LossyBuffer::new(1).expect("runtime telemetry capacity is nonzero"),
        })
    }

    /// Returns the run identity.
    #[must_use]
    pub const fn run_id(&self) -> &RunId {
        &self.run_id
    }

    /// Returns the authoritative workflow aggregate.
    #[must_use]
    pub const fn workflow(&self) -> &WorkflowGraph {
        &self.workflow
    }

    /// Returns the materialized durable effect journal compatibility view.
    ///
    /// Durable mutations are committed to [`Self::store`] before this view is
    /// replaced.  Recovery policy still consumes this view, never stream or
    /// fiber state.
    #[must_use]
    pub const fn journal(&self) -> &DurableJournal {
        &self.journal
    }

    /// Returns the caller-owned durable store implementation.
    #[must_use]
    pub const fn store(&self) -> &S {
        &self.store
    }

    /// Returns the current detached durable run view.
    pub fn durable_state(&self) -> Result<DurableRunState, RuntimeError> {
        Ok(self.store.load_run(&self.run_id)?)
    }

    /// Returns the capability scope used for future resolutions.
    #[must_use]
    pub fn scope(&self) -> &Scope {
        self.capability_runtime.scope()
    }

    /// Returns the explicit Cordis-derived context view used for capability
    /// runtime coordination. It does not become an authority for workflow or
    /// durable effect facts.
    #[must_use]
    pub fn capability_context(&self) -> &CapabilityContext {
        self.capability_runtime.context()
    }

    /// Returns the process-local plugin runtime registry.
    #[must_use]
    pub fn capability_registry(&self) -> &CapabilityRegistry {
        self.capability_runtime.registry()
    }

    /// Returns the runtime-owned reactive capability coordinator.
    #[must_use]
    pub const fn capability_runtime(&self) -> &ReactiveCapabilityRuntime {
        &self.capability_runtime
    }

    /// Returns all attempts in deterministic creation order.
    #[must_use]
    pub fn attempts(&self) -> &[TaskAttempt] {
        &self.attempts
    }

    /// Adds or replaces runtime inputs for an unfinished task.
    pub fn configure_task(&mut self, task_id: Id, config: TaskConfig) -> Result<(), RuntimeError> {
        if self.workflow.task(&task_id).is_none() {
            return Err(RuntimeError::UnknownTask(task_id));
        }
        let mut candidate_configs = self.task_configs.clone();
        candidate_configs.insert(task_id, config);
        let replay_identity = workflow_replay_identity(&self.workflow, &candidate_configs)?;
        self.commit_workflow_identity(replay_identity)?;
        self.task_configs = candidate_configs;
        Ok(())
    }

    /// Applies an atomic workflow mutation through the graph authority.
    pub fn apply_workflow_mutation<B>(
        &mut self,
        expected_revision: kernis_core::Revision,
        batch: B,
    ) -> Result<WorkflowMutationRecord, RuntimeError>
    where
        B: Into<MutationBatch>,
    {
        let mut candidate = self.workflow.clone();
        let record = candidate.apply_batch(expected_revision, batch)?;
        let replay_identity = workflow_replay_identity(&candidate, &self.task_configs)?;
        self.commit_workflow_identity(replay_identity)?;
        self.workflow = candidate;
        Ok(record)
    }

    /// Persists an effect intent before any external dispatch.
    pub fn record_effect_intent(
        &mut self,
        task_id: Id,
        operation_id: OperationId,
        semantics: EffectSemantics,
    ) -> Result<(), RuntimeError> {
        if self.workflow.task(&task_id).is_none() {
            return Err(RuntimeError::UnknownTask(task_id));
        }
        if self.workflow.is_completed(&task_id) {
            return Err(RuntimeError::Workflow(
                WorkflowGraphError::TaskAlreadyCompleted(task_id),
            ));
        }
        let intent = EffectIntent {
            task_id,
            operation_id,
            semantics,
        };
        let mut candidate = self.journal.clone();
        candidate.persist_intent(intent.clone())?;
        self.commit_durable(
            format!("intent:{}", intent.operation_id),
            vec![DurableMutation::RecordIntent(intent)],
        )?;
        self.journal = candidate;
        Ok(())
    }

    /// Dispatches an effect, allocating a new attempt for an idempotent retry.
    pub fn dispatch_effect(
        &mut self,
        operation_id: &OperationId,
    ) -> Result<AttemptId, RuntimeError> {
        let intent = self.journal.intent(operation_id)?.clone();
        let state = self.journal.state(operation_id);
        if self.cancelled.contains(&intent.task_id) {
            return match state {
                RecoveredEffectState::OutcomeUnknown => Err(RuntimeError::CancelledAfterDispatch {
                    task_id: intent.task_id,
                    operation_id: operation_id.clone(),
                }),
                _ => Err(RuntimeError::CancelledBeforeDispatch(intent.task_id)),
            };
        }

        let attempt_id = match state {
            RecoveredEffectState::Prepared => self
                .pending_attempt(&intent.task_id, operation_id)
                .map_or_else(
                || self.begin_attempt(&intent.task_id, Some(operation_id.clone())),
                Ok,
            )?,
            RecoveredEffectState::OutcomeUnknown => {
                if intent.semantics == EffectSemantics::NonIdempotent {
                    return Err(RuntimeError::ReconciliationRequired(operation_id.clone()));
                }
                self.begin_attempt(&intent.task_id, Some(operation_id.clone()))?
            }
            RecoveredEffectState::OutcomeKnown(_) => {
                return Err(RuntimeError::OutcomeAlreadyKnown(operation_id.clone()));
            }
            RecoveredEffectState::NotPrepared => unreachable!("intent lookup proved prepared"),
        };

        let dispatch = DispatchRecord {
            operation_id: operation_id.clone(),
            attempt_id: attempt_id.clone(),
        };
        let mut candidate = self.journal.clone();
        candidate.persist_dispatch(dispatch.clone())?;
        self.commit_durable(
            format!("dispatch:{}", attempt_id),
            vec![DurableMutation::RecordDispatch(dispatch)],
        )?;
        self.journal = candidate;
        Ok(attempt_id)
    }

    /// Records a known effect outcome in the durable journal.
    ///
    /// Workflow completion is applied by [`Self::recover`] or the next
    /// scheduler step, which preserves the explicit recovery boundary for a
    /// known success.
    pub fn record_effect_outcome(
        &mut self,
        operation_id: &OperationId,
        attempt_id: AttemptId,
        outcome: KnownEffectOutcome,
    ) -> Result<(), RuntimeError> {
        let outcome = OutcomeRecord {
            operation_id: operation_id.clone(),
            attempt_id: attempt_id.clone(),
            outcome,
        };
        let mut candidate = self.journal.clone();
        candidate.persist_outcome(outcome.clone())?;
        self.commit_durable(
            format!("outcome:{}", attempt_id),
            vec![DurableMutation::RecordOutcome(outcome)],
        )?;
        self.journal = candidate;
        Ok(())
    }

    /// Classifies recovery from workflow and journal authorities only.
    ///
    /// A known success is applied to the workflow completion authority without
    /// re-executing the external effect. Retry and reconciliation decisions are
    /// returned to the caller for explicit action.
    pub fn recover(
        &mut self,
        operation_id: &OperationId,
    ) -> Result<RecoveryDecision, RuntimeError> {
        let decision = classify_recovery(&self.workflow, &self.journal, operation_id)?;
        if decision.action == RecoveryAction::CompleteWithoutReexecution {
            let intent = self.journal.intent(operation_id)?.clone();
            let attempt_id = self
                .journal
                .known_outcome(operation_id)
                .expect("complete decision requires a known outcome")
                .attempt_id
                .clone();
            if !self.workflow.is_completed(&intent.task_id) {
                self.complete_task(&intent.task_id, &attempt_id)?;
            }
        }
        Ok(decision)
    }

    /// Prevents a task from starting when it has not yet been dispatched.
    pub fn cancel_task(&mut self, task_id: &Id) -> Result<Cancellation, RuntimeError> {
        if self.workflow.task(task_id).is_none() {
            return Err(RuntimeError::UnknownTask(task_id.clone()));
        }
        let operation_id = self.journal.operation_for_task(task_id).cloned();
        let attempt_id = operation_id
            .as_ref()
            .and_then(|operation_id| self.attempt_for_task(task_id, Some(operation_id)));
        let already_dispatched = operation_id
            .as_ref()
            .and_then(|operation_id| self.journal.latest_dispatch(operation_id).cloned());
        self.commit_durable(
            format!("cancel:{}", task_id),
            vec![DurableMutation::RecordCancellation(
                workflow_recovery::CancellationRecord {
                    run_id: self.run_id.clone(),
                    task_id: task_id.clone(),
                    operation_id: operation_id.clone(),
                    attempt_id,
                },
            )],
        )?;
        self.cancelled.insert(task_id.clone());
        if let (Some(operation_id), Some(dispatch)) = (operation_id, already_dispatched) {
            return Ok(Cancellation::AlreadyDispatched {
                task_id: task_id.clone(),
                operation_id,
                attempt_id: dispatch.attempt_id,
            });
        }
        Ok(Cancellation::NotDispatched {
            task_id: task_id.clone(),
        })
    }

    /// Executes deterministic ready work until no further automatic step is possible.
    pub fn run_until_blocked(&mut self) -> Result<Vec<StepResult>, RuntimeError> {
        let mut results = Vec::new();
        loop {
            let result = self.step()?;
            let stop = !matches!(result, StepResult::Completed { .. });
            results.push(result);
            if stop {
                return Ok(results);
            }
        }
    }

    /// Executes the lexicographically first ready, non-cancelled task.
    pub fn step(&mut self) -> Result<StepResult, RuntimeError> {
        if let Some((_, item)) = &self.pending_started {
            return Err(RuntimeError::ExecutionBackpressure { item: item.clone() });
        }
        for task_id in self.workflow.ready_tasks() {
            if self.cancelled.contains(&task_id) {
                continue;
            }
            return self.step_task(task_id);
        }
        Ok(StepResult::Idle)
    }

    /// Emits a coalescible progress observation.
    pub fn emit_progress(&mut self, task_id: &Id, progress: u8) -> Result<(), RuntimeError> {
        let item = self.progress_sequencer.emit(RuntimeEvent::TaskProgress {
            task_id: task_id.clone(),
            progress,
        })?;
        self.progress_events
            .try_push(KeyedStreamItem {
                key: task_id.clone(),
                item,
            })
            .map_err(|PushError::Backpressure(_)| RuntimeError::ProgressBackpressure)
    }

    /// Emits a lossy telemetry observation.
    pub fn emit_telemetry(&mut self, message: impl Into<String>) -> Result<(), RuntimeError> {
        let item = self.telemetry_sequencer.emit(RuntimeEvent::Telemetry {
            message: message.into(),
        })?;
        self.telemetry_events.push(item);
        Ok(())
    }

    /// Drains lossless task lifecycle observations.
    pub fn drain_execution_events(&mut self) -> Vec<StreamItem<RuntimeEvent>> {
        drain_lossless(&mut self.execution_events)
    }

    /// Retries a lifecycle item returned by [`RuntimeError::ExecutionBackpressure`].
    pub fn retry_execution_event(
        &mut self,
        item: StreamItem<RuntimeEvent>,
    ) -> Result<(), RuntimeError> {
        if let Some((attempt, pending_item)) = self.pending_started.take() {
            let retry_item = pending_item.clone();
            return match self.enqueue_execution(pending_item) {
                Ok(()) => {
                    self.attempts.push(attempt);
                    Ok(())
                }
                Err(error) => {
                    self.pending_started = Some((attempt, retry_item));
                    Err(error)
                }
            };
        }
        self.enqueue_execution(item)
    }

    /// Drains retained progress observations.
    pub fn drain_progress_events(&mut self) -> Vec<KeyedStreamItem<Id, RuntimeEvent>> {
        drain_coalescing(&mut self.progress_events)
    }

    /// Drains retained telemetry observations.
    pub fn drain_telemetry_events(&mut self) -> Vec<StreamItem<RuntimeEvent>> {
        drain_lossy(&mut self.telemetry_events)
    }

    fn step_task(&mut self, task_id: Id) -> Result<StepResult, RuntimeError> {
        let operation = self.ensure_configured_effect(&task_id)?;
        let Some((operation_id, _semantics)) = operation else {
            let attempt_id = self
                .attempt_for_task(&task_id, None)
                .map_or_else(|| self.begin_attempt(&task_id, None), Ok)?;
            self.complete_task(&task_id, &attempt_id)?;
            return Ok(StepResult::Completed {
                task_id,
                attempt_id,
            });
        };

        match self.journal.state(&operation_id) {
            RecoveredEffectState::Prepared => {
                let attempt_id = self.pending_attempt(&task_id, &operation_id).map_or_else(
                    || self.begin_attempt(&task_id, Some(operation_id.clone())),
                    Ok,
                )?;
                Ok(StepResult::EffectPending {
                    task_id,
                    attempt_id,
                    operation_id,
                })
            }
            RecoveredEffectState::OutcomeUnknown => {
                let decision = classify_recovery(&self.workflow, &self.journal, &operation_id)?;
                Ok(StepResult::Blocked {
                    task_id,
                    operation_id: Some(operation_id),
                    action: decision.action,
                })
            }
            RecoveredEffectState::OutcomeKnown(KnownEffectOutcome::Succeeded) => {
                let attempt_id = self
                    .journal
                    .known_outcome(&operation_id)
                    .expect("known success has outcome")
                    .attempt_id
                    .clone();
                self.complete_task(&task_id, &attempt_id)?;
                Ok(StepResult::Completed {
                    task_id,
                    attempt_id,
                })
            }
            RecoveredEffectState::OutcomeKnown(KnownEffectOutcome::Failed) => {
                Ok(StepResult::Blocked {
                    task_id,
                    operation_id: Some(operation_id),
                    action: RecoveryAction::ObserveFailure,
                })
            }
            RecoveredEffectState::NotPrepared => unreachable!("configured effect is persisted"),
        }
    }

    fn ensure_configured_effect(
        &mut self,
        task_id: &Id,
    ) -> Result<Option<(OperationId, EffectSemantics)>, RuntimeError> {
        if let Some(operation_id) = self.journal.operation_for_task(task_id).cloned() {
            let semantics = self.journal.intent(&operation_id)?.semantics;
            return Ok(Some((operation_id, semantics)));
        }
        let Some(effect) = self
            .task_configs
            .get(task_id)
            .and_then(TaskConfig::effect)
            .cloned()
        else {
            return Ok(None);
        };
        self.record_effect_intent(
            task_id.clone(),
            effect.operation_id.clone(),
            effect.semantics,
        )?;
        Ok(Some((effect.operation_id, effect.semantics)))
    }

    fn begin_attempt(
        &mut self,
        task_id: &Id,
        operation_id: Option<OperationId>,
    ) -> Result<AttemptId, RuntimeError> {
        let capability_pins = self.capture_capability_pins(task_id, None)?;

        let attempt_number = self.next_attempt_number;
        let attempt_id = AttemptId::new(format!("{}-attempt-{attempt_number}", self.run_id))
            .expect("generated attempt identity is non-empty");
        let attempt = TaskAttempt {
            run_id: self.run_id.clone(),
            task_id: task_id.clone(),
            attempt_id: attempt_id.clone(),
            operation_id,
            capability_pins,
        };
        self.commit_durable(
            format!("admit:{}", attempt.attempt_id),
            vec![DurableMutation::AdmitAttempt(AttemptAdmission {
                run_id: attempt.run_id.clone(),
                task_id: attempt.task_id.clone(),
                attempt_id: attempt.attempt_id.clone(),
                operation_id: attempt.operation_id.clone(),
                capabilities: attempt
                    .capability_pins
                    .iter()
                    .map(|pin| pin.replay_identity.clone())
                    .collect(),
            })],
        )?;
        self.next_attempt_number = self
            .next_attempt_number
            .checked_add(1)
            .ok_or(RuntimeError::AttemptIdExhausted)?;
        let item = self.execution_sequencer.emit(RuntimeEvent::TaskStarted {
            run_id: self.run_id.clone(),
            task_id: task_id.clone(),
            attempt_id: attempt_id.clone(),
        })?;
        match self.enqueue_execution(item) {
            Ok(()) => {
                self.attempts.push(attempt);
                Ok(attempt_id)
            }
            Err(RuntimeError::ExecutionBackpressure { item }) => {
                self.pending_started = Some((attempt, item.clone()));
                Err(RuntimeError::ExecutionBackpressure { item })
            }
            Err(error) => Err(error),
        }
    }

    fn capture_capability_pins(
        &self,
        task_id: &Id,
        expected: Option<&[CapabilityReplayIdentity]>,
    ) -> Result<Vec<CapabilityPin>, RuntimeError> {
        let required_capabilities = self
            .task_configs
            .get(task_id)
            .map(|config| {
                config
                    .required_capabilities
                    .iter()
                    .map(|(capability_id, identity)| (capability_id.clone(), identity.clone()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut capability_pins = Vec::with_capacity(required_capabilities.len());
        for (capability_id, configured_identity) in required_capabilities {
            let handle = self
                .capability_runtime
                .context()
                .get(&capability_id)
                .ok_or_else(|| RuntimeError::MissingCapability {
                    task_id: task_id.clone(),
                    capability_id: capability_id.clone(),
                })?;
            if let Some(configured_identity) = configured_identity.as_deref() {
                if configured_identity != handle.replay_identity() {
                    return Err(RuntimeError::CapabilityReplayMismatch {
                        task_id: task_id.clone(),
                        capability_id: capability_id.clone(),
                    });
                }
            }
            let replay_identity =
                CapabilityReplayIdentity::new(capability_id.clone(), handle.replay_identity());
            capability_pins.push(CapabilityPin {
                capability_id,
                generation: handle.generation(),
                entry_id: handle.entry_id(),
                replay_identity,
                handle,
            });
        }
        if let Some(expected) = expected {
            let expected = expected
                .iter()
                .map(|identity| (identity.capability_id().clone(), identity))
                .collect::<BTreeMap<_, _>>();
            if expected.len() != capability_pins.len()
                || capability_pins.iter().any(|pin| {
                    expected
                        .get(&pin.capability_id)
                        .is_none_or(|identity| *identity != &pin.replay_identity)
                })
            {
                let capability_id = capability_pins
                    .first()
                    .map(|pin| pin.capability_id.clone())
                    .or_else(|| expected.keys().next().cloned())
                    .unwrap_or_else(|| Id::new("durable-capability-mismatch").expect("static id"));
                return Err(RuntimeError::CapabilityReplayMismatch {
                    task_id: task_id.clone(),
                    capability_id,
                });
            }
        }
        Ok(capability_pins)
    }

    fn pending_attempt(&self, task_id: &Id, operation_id: &OperationId) -> Option<AttemptId> {
        self.attempt_for_task(task_id, Some(operation_id))
    }

    fn attempt_for_task(
        &self,
        task_id: &Id,
        operation_id: Option<&OperationId>,
    ) -> Option<AttemptId> {
        self.pending_started
            .as_ref()
            .filter(|(attempt, _)| {
                &attempt.task_id == task_id && attempt.operation_id.as_ref() == operation_id
            })
            .map(|(attempt, _)| attempt.attempt_id.clone())
            .or_else(|| {
                self.attempts
                    .iter()
                    .rev()
                    .find(|attempt| {
                        &attempt.task_id == task_id && attempt.operation_id.as_ref() == operation_id
                    })
                    .map(|attempt| attempt.attempt_id.clone())
            })
    }

    fn complete_task(&mut self, task_id: &Id, attempt_id: &AttemptId) -> Result<(), RuntimeError> {
        let mut candidate = self.workflow.clone();
        candidate.complete(task_id)?;
        self.commit_durable(
            format!("completion:{task_id}:{attempt_id}"),
            vec![DurableMutation::RecordCompletion(CompletionRecord {
                task_id: task_id.clone(),
                attempt_id: attempt_id.clone(),
            })],
        )?;
        self.workflow = candidate;
        self.emit_completed(task_id.clone(), attempt_id.clone())
    }

    fn commit_workflow_identity(
        &mut self,
        replay_identity: WorkflowReplayIdentity,
    ) -> Result<StoreRevision, RuntimeError> {
        self.commit_durable(
            format!(
                "workflow-identity:{}:{replay_identity}",
                self.store_revision.get()
            ),
            vec![DurableMutation::RecordWorkflowReplayIdentity(
                replay_identity,
            )],
        )
    }

    fn emit_completed(&mut self, task_id: Id, attempt_id: AttemptId) -> Result<(), RuntimeError> {
        let item = self.execution_sequencer.emit(RuntimeEvent::TaskCompleted {
            run_id: self.run_id.clone(),
            task_id,
            attempt_id,
        })?;
        self.enqueue_execution(item)
    }

    fn enqueue_execution(&mut self, item: StreamItem<RuntimeEvent>) -> Result<(), RuntimeError> {
        self.execution_events
            .try_push(item)
            .map_err(|PushError::Backpressure(item)| RuntimeError::ExecutionBackpressure { item })
    }

    fn commit_durable(
        &mut self,
        idempotency_key: String,
        mutations: Vec<DurableMutation>,
    ) -> Result<StoreRevision, RuntimeError> {
        let result = self.store.commit(CommitRequest {
            run_id: self.run_id.clone(),
            expected_revision: self.store_revision,
            idempotency_key: IdempotencyKey::new(idempotency_key)?,
            mutations,
        })?;
        self.store_revision = result.revision;
        Ok(result.revision)
    }
}

fn ensure_unfinished_workflow(workflow: &WorkflowGraph) -> Result<(), RuntimeError> {
    if let Some(completion) = workflow.completion_log().first() {
        return Err(RuntimeError::PrecompletedWorkflow(
            completion.task_id.clone(),
        ));
    }
    Ok(())
}

fn collect_task_configs<I>(
    workflow: &WorkflowGraph,
    task_configs: I,
) -> Result<BTreeMap<Id, TaskConfig>, RuntimeError>
where
    I: IntoIterator<Item = (Id, TaskConfig)>,
{
    let mut configs = BTreeMap::new();
    for (task_id, config) in task_configs {
        if workflow.task(&task_id).is_none() {
            return Err(RuntimeError::UnknownTask(task_id));
        }
        configs.insert(task_id, config);
    }
    Ok(configs)
}

fn workflow_replay_identity(
    workflow: &WorkflowGraph,
    task_configs: &BTreeMap<Id, TaskConfig>,
) -> Result<WorkflowReplayIdentity, StoreError> {
    let mut canonical = String::from("kernis-workflow-replay-v1");
    append_identity_part(&mut canonical, "tasks");
    for task in workflow.tasks() {
        append_identity_part(&mut canonical, "task");
        append_identity_part(&mut canonical, task.id.as_str());
        append_identity_part(&mut canonical, &task.label);
        if let Some(config) = task_configs.get(&task.id) {
            append_identity_part(&mut canonical, "config");
            for (capability_id, definition_identity) in &config.required_capabilities {
                append_identity_part(&mut canonical, "capability");
                append_identity_part(&mut canonical, capability_id.as_str());
                match definition_identity {
                    Some(definition_identity) => {
                        append_identity_part(&mut canonical, "specified");
                        append_identity_part(&mut canonical, definition_identity);
                    }
                    None => append_identity_part(&mut canonical, "unspecified"),
                }
            }
            if let Some(effect) = &config.effect {
                append_identity_part(&mut canonical, "effect");
                append_identity_part(&mut canonical, effect.operation_id.as_str());
                append_identity_part(
                    &mut canonical,
                    match effect.semantics {
                        EffectSemantics::Idempotent => "idempotent",
                        EffectSemantics::NonIdempotent => "non-idempotent",
                    },
                );
            } else {
                append_identity_part(&mut canonical, "no-effect");
            }
        } else {
            append_identity_part(&mut canonical, "config");
            append_identity_part(&mut canonical, "no-effect");
        }
    }
    append_identity_part(&mut canonical, "edges");
    for (task_id, dependency_id) in workflow.edges() {
        append_identity_part(&mut canonical, task_id.as_str());
        append_identity_part(&mut canonical, dependency_id.as_str());
    }
    WorkflowReplayIdentity::new(canonical)
}

fn append_identity_part(canonical: &mut String, value: &str) {
    canonical.push(':');
    canonical.push_str(&value.len().to_string());
    canonical.push(':');
    canonical.push_str(value);
}

fn replay_workflow(
    supplied: &WorkflowGraph,
    durable_completions: &[CompletionRecord],
) -> Result<WorkflowGraph, RuntimeError> {
    let completions = durable_completions
        .iter()
        .map(|completion| WorkflowCompletionRecord {
            task_id: completion.task_id.clone(),
        })
        .collect::<Vec<_>>();
    WorkflowGraph::replay_with_facts(supplied.mutation_log(), &completions).map_err(|error| {
        let invariant = match error {
            WorkflowGraphError::UnknownTask(task_id) => {
                StoreInvariant::CompletionUnknownTask { task_id }
            }
            WorkflowGraphError::IncompletePrerequisite { task, dependency } => {
                StoreInvariant::CompletionPrerequisiteMismatch {
                    task_id: task,
                    dependency,
                }
            }
            WorkflowGraphError::TaskAlreadyCompleted(task_id) => {
                StoreInvariant::CompletionReplayRejected {
                    task_id: Some(task_id),
                    reason: "task completion was replayed twice".to_owned(),
                }
            }
            other => StoreInvariant::CompletionReplayRejected {
                task_id: None,
                reason: other.to_string(),
            },
        };
        RuntimeError::Store(StoreError::InvariantViolation(invariant))
    })
}

fn stream_id(value: &str) -> Id {
    Id::new(value).expect("static stream identity is non-empty")
}

fn drain_lossless<T>(buffer: &mut LosslessBuffer<T>) -> Vec<StreamItem<T>> {
    let mut items = Vec::new();
    while let Some(item) = buffer.pop() {
        items.push(item);
    }
    items
}

fn drain_coalescing<K: Eq, T>(buffer: &mut CoalescingBuffer<K, T>) -> Vec<KeyedStreamItem<K, T>> {
    let mut items = Vec::new();
    while let Some(item) = buffer.pop() {
        items.push(item);
    }
    items
}

fn drain_lossy<T>(buffer: &mut LossyBuffer<T>) -> Vec<StreamItem<T>> {
    let mut items = Vec::new();
    while let Some(item) = buffer.pop() {
        items.push(item);
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    fn admitted(run_id: &RunId, attempt: &str) -> AttemptAdmission {
        AttemptAdmission {
            run_id: run_id.clone(),
            task_id: Id::new("task").expect("test id is valid"),
            attempt_id: AttemptId::new(attempt).expect("test attempt is valid"),
            operation_id: None,
            capabilities: Vec::new(),
        }
    }

    #[test]
    fn runtime_durable_head_does_not_regress_on_old_replay() {
        let run_id = RunId::new("runtime-replay").expect("test run is valid");
        let mut runtime = Runtime::start_run(
            run_id.clone(),
            WorkflowGraph::default(),
            Scope::root(),
            std::iter::empty::<(Id, TaskConfig)>(),
        )
        .expect("runtime starts");
        let first = DurableMutation::AdmitAttempt(admitted(&run_id, "attempt-1"));
        let first_revision = runtime
            .commit_durable("first".to_owned(), vec![first.clone()])
            .expect("first commit");
        let newer_revision = runtime
            .commit_durable(
                "newer".to_owned(),
                vec![DurableMutation::AdmitAttempt(admitted(
                    &run_id,
                    "attempt-2",
                ))],
            )
            .expect("newer commit");
        let replay_revision = runtime
            .commit_durable("first".to_owned(), vec![first])
            .expect("replay commit");

        assert_eq!(first_revision.get() + 1, newer_revision.get());
        assert_eq!(replay_revision, newer_revision);
        runtime
            .commit_durable(
                "after-replay".to_owned(),
                vec![DurableMutation::AdmitAttempt(admitted(
                    &run_id,
                    "attempt-3",
                ))],
            )
            .expect("next mutation uses the live replay head");
    }
}

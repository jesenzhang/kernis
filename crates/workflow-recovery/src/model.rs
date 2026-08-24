//! Typed facts and decisions for the crash/recovery boundary.

use kernis_core::{Id, InvalidId};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Stable identity of one logical external side effect.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct OperationId(Id);

impl OperationId {
    /// Creates a non-empty operation identity.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidId`] when the value is empty or only whitespace.
    pub fn new(value: impl Into<String>) -> Result<Self, InvalidId> {
        Id::new(value).map(Self)
    }

    /// Returns the operation identity as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Display for OperationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Identity of one transport or execution attempt for an operation.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct AttemptId(Id);

impl AttemptId {
    /// Creates a non-empty attempt identity.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidId`] when the value is empty or only whitespace.
    pub fn new(value: impl Into<String>) -> Result<Self, InvalidId> {
        Id::new(value).map(Self)
    }

    /// Returns the attempt identity as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Display for AttemptId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Declares whether repeating one logical operation is externally safe.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum EffectSemantics {
    /// Repeated calls with the same operation identity are deduplicated.
    Idempotent,
    /// Repeated calls may create another externally visible side effect.
    NonIdempotent,
}

/// Durable admission record declaring the logical effect to be performed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EffectIntent {
    /// Workflow task that owns the effect.
    pub task_id: Id,
    /// Stable identity reused by every retry of this logical effect.
    pub operation_id: OperationId,
    /// External retry semantics for the operation.
    pub semantics: EffectSemantics,
}

/// Durable boundary record proving that an external call was dispatched.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DispatchRecord {
    /// Logical operation being dispatched.
    pub operation_id: OperationId,
    /// Transport attempt used for this dispatch.
    pub attempt_id: AttemptId,
}

/// Result known by the local runtime after observing or recording the external world.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum KnownEffectOutcome {
    /// The external operation succeeded.
    Succeeded,
    /// The external operation failed.
    Failed,
}

/// Durable checkpoint of a known external result for one dispatch attempt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct OutcomeRecord {
    /// Logical operation whose result is recorded.
    pub operation_id: OperationId,
    /// Attempt that produced the recorded result.
    pub attempt_id: AttemptId,
    /// Result known by the local runtime.
    pub outcome: KnownEffectOutcome,
}

/// Local knowledge state derived from durable effect facts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveredEffectState {
    /// No durable intent exists for the operation.
    NotPrepared,
    /// Intent exists, but no dispatch boundary exists.
    Prepared,
    /// A known external outcome has been checkpointed.
    OutcomeKnown(KnownEffectOutcome),
    /// Dispatch exists, but the local runtime has no known outcome.
    OutcomeUnknown,
}

/// Action selected by deterministic recovery classification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryAction {
    /// Dispatch the prepared operation for the first time.
    Execute,
    /// Retry the same logical operation with a new transport attempt.
    RetrySameOperation,
    /// Record workflow completion without invoking the external effect again.
    CompleteWithoutReexecution,
    /// Stop automatic execution and require external reconciliation.
    Reconcile,
    /// No recovery work remains.
    NoAction,
    /// Observe a known failure without inventing a retry policy.
    ObserveFailure,
    /// Refuse to act because durable facts contradict each other.
    InvariantViolation,
}

/// Structured explanation for a recovery action.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecoveryReason {
    /// Intent is durable but dispatch has not started.
    PreparedNotDispatched {
        /// Logical operation that is safe to execute.
        operation_id: OperationId,
    },
    /// An idempotent operation may be retried with its same operation identity.
    IdempotentOutcomeUnknown {
        /// Logical operation whose outcome is unknown locally.
        operation_id: OperationId,
        /// Latest dispatch attempt with no known outcome.
        attempt_id: AttemptId,
    },
    /// A non-idempotent unknown outcome must be reconciled before execution.
    NonIdempotentOutcomeUnknown {
        /// Logical operation whose outcome is unknown locally.
        operation_id: OperationId,
        /// Latest dispatch attempt with no known outcome.
        attempt_id: AttemptId,
    },
    /// A known success can advance the workflow without another external call.
    KnownSuccess {
        /// Logical operation that succeeded.
        operation_id: OperationId,
        /// Attempt with the known success.
        attempt_id: AttemptId,
    },
    /// A known failure is not equivalent to an unknown outcome.
    KnownFailure {
        /// Logical operation that failed.
        operation_id: OperationId,
        /// Attempt with the known failure.
        attempt_id: AttemptId,
    },
    /// The workflow completion fact already exists after a known success.
    WorkflowAlreadyCompleted {
        /// Logical operation already reflected in workflow facts.
        operation_id: OperationId,
        /// Task whose completion fact is already durable.
        task_id: Id,
    },
    /// Workflow and effect facts cannot safely be reconciled automatically.
    WorkflowStateMismatch {
        /// Logical operation with contradictory local facts.
        operation_id: OperationId,
        /// Task whose workflow fact conflicts with the effect state.
        task_id: Id,
        /// Effect state observed in the durable journal.
        state: RecoveredEffectState,
    },
    /// No intent exists, so there is no admissible effect to execute.
    NotPrepared {
        /// Operation requested by the recovery query.
        operation_id: OperationId,
    },
}

/// Deterministic recovery result derived from a workflow and durable journal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveryDecision {
    /// Action selected by the recovery policy.
    pub action: RecoveryAction,
    /// Durable facts that explain the selected action.
    pub reason: RecoveryReason,
}

impl fmt::Display for RecoveryAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Self::Execute => "execute",
            Self::RetrySameOperation => "retry-same-operation",
            Self::CompleteWithoutReexecution => "complete-without-reexecution",
            Self::Reconcile => "reconcile",
            Self::NoAction => "no-action",
            Self::ObserveFailure => "observe-failure",
            Self::InvariantViolation => "invariant-violation",
        };
        f.write_str(label)
    }
}

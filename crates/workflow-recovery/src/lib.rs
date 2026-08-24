//! Crash/recovery boundary experiment for external workflow effects.
//!
//! This crate models durable local facts in memory. It deliberately does not
//! claim filesystem, database, power-loss, or distributed-transaction
//! durability.

mod durable;
mod journal;
mod model;
mod recovery;

pub use durable::{
    AttemptAdmission, CancellationRecord, CapabilityReplayIdentity, CommitRequest, CommitResult,
    CompletionRecord, DurableMutation, DurableRunState, DurableStore, IdempotencyKey,
    InMemoryDurableStore, RunId, StoreError, StoreErrorKind, StoreInvariant, StoreRevision,
    WorkflowReplayIdentity,
};
pub use journal::{DurableJournal, JournalError, JournalInvariant};
pub use model::{
    AttemptId, DispatchRecord, EffectIntent, EffectSemantics, KnownEffectOutcome, OperationId,
    OutcomeRecord, RecoveredEffectState, RecoveryAction, RecoveryDecision, RecoveryReason,
};
pub use recovery::classify_recovery;

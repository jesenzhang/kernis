//! Crash/recovery boundary experiment for external workflow effects.
//!
//! The crate exposes a deterministic in-memory conformance adapter and one
//! embedded physical adapter. Neither adapter persists process-local runtime
//! objects or claims unproven power-loss or distributed-transaction semantics.

mod durable;
mod journal;
mod model;
mod physical;
mod recovery;

pub use durable::{
    AttemptAdmission, CancellationRecord, CapabilityReplayIdentity, CommitLedgerEntry,
    CommitRequest, CommitResult, CompletionRecord, DurableMutation, DurableRunState, DurableStore,
    IdempotencyKey, InMemoryDurableStore, RunId, StoreError, StoreErrorKind, StoreInvariant,
    StoreRevision, WorkflowReplayIdentity,
};
pub use journal::{DurableJournal, JournalError, JournalInvariant};
pub use model::{
    AttemptId, DispatchRecord, EffectIntent, EffectSemantics, KnownEffectOutcome, OperationId,
    OutcomeRecord, RecoveredEffectState, RecoveryAction, RecoveryDecision, RecoveryReason,
};
pub use physical::FileDurableStore;
pub use recovery::classify_recovery;

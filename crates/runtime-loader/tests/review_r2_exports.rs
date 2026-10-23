//! The standing export-closure proof for the host entry crate (review finding
//! R2-04, ADR 0007 §2).
//!
//! # What this file proves, and what the fixture proves
//!
//! Every item below is *named* through `runtime_loader` alone. Naming is the
//! closure rule for values a supported host must bind: a value it stores, names
//! in a helper signature, or returns from its own function has to be reachable
//! here. A payload the host only matches by inference may stay at the subsystem
//! tier (ADR 0007 §2), which is why this list is a vocabulary regression rather
//! than the whole closure proof. This file fails to
//! compile the moment one of these re-exports is dropped or renamed.
//!
//! The stronger, structural proof lives in `fixtures/r2-host-fixture`, a
//! separate crate whose only dependency is `runtime-loader`, whose every
//! intermediate value carries an explicit type annotation (so no name can be
//! completed by inference), and which is checked on stable and on the MSRV.
//! Both are required: this one pins the vocabulary list in the reviewed tree,
//! that one proves a host can actually live inside it.
//!
//! # Proven gaps, closed by the R2 repair
//!
//! The R2 review proved five host-facing names were missing from the umbrella
//! (`FactoryRegistry`, `RUN_DEFINITION_FORMAT`, `RuntimeAssembly`,
//! `RecoveryReason`, `StoreInvariant`). The repair's own audit then found four
//! more in the same category — `RUN_DEFINITION_FORMAT`'s legacy sibling
//! `LEGACY_RUN_DEFINITION_FORMAT_V1` (ADR 0008 makes it a host-visible fact: a
//! durable identity carrying it is refused, and a host must recognize that
//! prefix), `StoreRevision` (bound from `DurableRunState::revision`),
//! `CapabilityConflictReason` (an error payload it must match), and
//! `InvalidId` (returned by `Id::new`, the host's entry point for every
//! identifier) — together with the R2-02/R2-03 lifecycle vocabulary
//! (`DriverOwner`, `CompositionReleaseOutcome`, `CompositionReleaseRejection`,
//! and the `CompositionReleaseFuture` that `dispose_after_driver` returns).
//! All are additive re-exports along core → composition → loader.
//!
//! # Deliberate exclusions (documented, not accidental)
//!
//! * Inspection accessors (`Runtime::scope`, `Runtime::capability_registry`,
//!   `Runtime::capability_context`) return subsystem-tier types by reference;
//!   a supported host consumes them by method-call inference and never names
//!   them, so `Scope`, `CapabilityRegistry`, `CapabilityContext`, and the
//!   error types of their deep APIs stay at the subsystem tier.
//! * `TaskConfig`/`EffectSpec` belong to the legacy direct-construction path,
//!   which the inventory classifies as experimental/compatibility for direct
//!   subsystem users, not to the R2 host entry path.
//! * `DurableMutation`, `CommitRequest`, `CommitResult`, `CommitLedgerEntry`,
//!   `IdempotencyKey`, `DurableJournal`, `classify_recovery`,
//!   `AttemptAdmission`, `EffectIntent`, and `CancellationRecord` are the
//!   store-authoring and conformance vocabulary. A supported R2 host selects
//!   `InMemoryDurableStore` or `FileDurableStore` (both nameable) and stays
//!   generic over the `DurableStore` trait itself; writing a new store is
//!   documented as subsystem work in the inventory.
//!
//! Every decision above is recorded with its rationale in
//! `docs/runtime/K6-supported-api-inventory.md`.

use runtime_loader::{
    CapabilityConflictReason, CapabilityContribution, CapabilityFactoryFn, CapabilityFiber,
    CapabilityId, CapabilityOwnership, CompositionReleaseFuture, CompositionReleaseOutcome,
    CompositionReleaseRejection, DriverOwner, DurableRunState, EntryId, FactoryRegistry,
    Generation, Id, InvalidId, LEGACY_RUN_DEFINITION_FORMAT_V1, LoaderError,
    ModuleRegistrationFactory, RUN_DEFINITION_FORMAT, RecoveryDecision, RecoveryReason,
    RollbackFailure, RollbackReport, RunDefinition, RunId, Runtime, RuntimeAssembly, RuntimeError,
    RuntimeHandle, StoreError, StoreErrorKind, StoreInvariant, StoreRevision,
};

/// Umbrella names the vocabulary regression does not otherwise exercise.
///
/// The closure fixture is a whitelist, so a re-export that nothing names could
/// be deleted with every proof staying green. Listing the remaining names in one
/// alias closes that hole: `tests/export_coverage.rs` fails if any `pub use`
/// name of the umbrella is absent from the closure evidence, and deleting one
/// here is what makes the export visible as covered.
///
/// `CapabilityId`, `Generation` and `EntryId` are here for a different reason:
/// the third review (2026-10-22, B1) showed a host that binds
/// `CapabilityPin::generation` or `CapabilityHandle::id()` has to name them, so
/// they were promoted to the host tier additively rather than documented away.
#[allow(dead_code)]
type UmbrellaNamesWithoutAnExercisingCall = (
    CapabilityContribution,
    CapabilityOwnership,
    CapabilityFactoryFn,
    ModuleRegistrationFactory,
    CapabilityFiber,
    CapabilityId,
    Generation,
    EntryId,
);

/// The five gaps the review proved, plus the ones the repair's own audit added,
/// in one import-closure statement. `RuntimeAssembly` also has to be nameable
/// in a return position, which is what the second signature is for.
#[allow(dead_code)]
fn proven_gaps_are_nameable(
    definition: &RunDefinition,
    registry: &FactoryRegistry,
    error: &StoreError,
    decision: &RecoveryDecision,
    revision: StoreRevision,
) -> Result<RuntimeAssembly, RuntimeError> {
    let _formats: [&'static str; 2] = [RUN_DEFINITION_FORMAT, LEGACY_RUN_DEFINITION_FORMAT_V1];
    let _registry: &FactoryRegistry = registry;
    let _identity: String = definition
        .identity()
        .map(|identity| identity.as_str().to_owned())
        .unwrap_or_default();
    let _kind: StoreErrorKind = error.kind();
    let _invariant: Option<&StoreInvariant> = match error {
        StoreError::InvariantViolation(invariant) => Some(invariant),
        _ => None,
    };
    let _reason: &RecoveryReason = &decision.reason;
    let _revision: StoreRevision = revision;
    Err(RuntimeError::AttemptIdExhausted)
}

/// The R2-02/R2-03 release vocabulary is nameable, and the driver-owner token
/// is the only way a host binds an exit to its own composition.
#[allow(dead_code)]
async fn release_vocabulary_is_nameable(
    mut composition: runtime_loader::CompositionHandle,
    exit: runtime_loader::DriverExit<runtime_loader::InMemoryDurableStore>,
    handle: &RuntimeHandle,
) -> Result<RollbackReport, CompositionReleaseRejection> {
    let owner: DriverOwner = handle.owner();
    if !owner.same_driver_as(exit.owner()) {
        return Err(CompositionReleaseRejection::ForeignDriverExit {
            exit: Box::new(exit),
        });
    }
    // The release future is a nameable type, not only an inferable one.
    let release: CompositionReleaseFuture<'_, runtime_loader::InMemoryDurableStore> =
        composition.dispose_after_driver(exit);
    match release.await {
        CompositionReleaseOutcome::Released(shutdown) => Ok(shutdown.rollback),
        CompositionReleaseOutcome::Rejected(rejection) => Err(rejection),
    }
}

/// Failures collected by a completed release are inspectable per resource.
#[allow(dead_code)]
fn rollback_failures_are_nameable(report: &RollbackReport) -> usize {
    report
        .failures
        .iter()
        .map(|failure: &RollbackFailure| failure.module_id.to_string().len())
        .sum()
}

/// Durable facts and identifier construction keep their own typed failures.
#[allow(dead_code)]
fn durable_and_identifier_vocabulary(state: &DurableRunState) -> Result<RunId, InvalidId> {
    let _revision: StoreRevision = state.revision();
    let _completed: bool = state.is_completed(&Id::new("prepare")?);
    let run: RunId = RunId::new("review-r2-exports")?;
    Ok(run)
}

/// Loader failures stay distinct from runtime failures at the boundary.
#[allow(dead_code)]
fn phase_failures_stay_distinct(loader: &LoaderError, runtime: &RuntimeError) -> bool {
    format!("{loader}").is_empty() || format!("{runtime}").is_empty()
}

/// The capability-slot conflict taxonomy is matchable at the host boundary.
#[allow(dead_code)]
fn slot_conflict_is_matchable(reason: &CapabilityConflictReason) -> &'static str {
    match reason {
        CapabilityConflictReason::UnownedRequirement => "unowned-requirement",
        CapabilityConflictReason::RequiredOnReactiveSlot { .. } => "required-on-reactive-slot",
        CapabilityConflictReason::IdentityMismatch { .. } => "identity-mismatch",
    }
}

/// A host may be generic over the store seam without naming `workflow-recovery`.
#[allow(dead_code)]
fn store_seam_is_generic<S: runtime_loader::DurableStore>(runtime: &Runtime<S>) -> bool {
    let _state: Result<DurableRunState, RuntimeError> = runtime.durable_state();
    true
}

#[test]
fn host_entry_names_the_supported_vocabulary() {
    // The import block above is the assertion; this test only keeps the file
    // in the test inventory so a dropped re-export is a failing gate rather
    // than a silently skipped compile.
    let id: Id = Id::new("review-r2-exports").expect("ids in this fixture are valid");
    assert_eq!(id.to_string(), "review-r2-exports");
}

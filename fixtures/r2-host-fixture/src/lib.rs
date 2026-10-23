//! Compile-only proof that `runtime-loader` alone names the supported R2 host
//! vocabulary (review finding R2-04).
//!
//! This crate declares exactly one dependency: `runtime-loader`. It is not a
//! member of the Kernis workspace, so nothing here can reach a subsystem crate
//! through workspace unification, and no name is filled in by inference — every
//! type a supported host path returns, accepts, or lets the host match on is
//! written out. That is the difference from
//! `crates/runtime-loader/examples/`, which compiles partly through inference
//! and therefore cannot prove nameability.
//!
//! Nothing here runs; `cargo check` is the assertion. Coverage follows the
//! tiered inventory in `docs/runtime/K6-supported-api-inventory.md`:
//!
//! * loader resolution, composition planning, activation, and cold restore;
//! * definition identity, including the v2 write format and the frozen legacy
//!   v1 tag (ADR 0008) and the typed fail-closed restore failure;
//! * the K3 driver lifecycle with the driver-owner token and the owner-bound
//!   orderly release (ADR 0009);
//! * the best-effort owner-loss release and its cleanup report;
//! * durable facts, recovery classification, and store error taxonomy;
//! * the typed failure of every phase, kept phase-distinct.

#![allow(dead_code, missing_docs)]
// These functions exist to name types in one place; a parameter list is the
// cheapest way to write an explicit type for each of them, so the usual
// arity lint does not apply to a compile-only vocabulary proof.
#![allow(clippy::too_many_arguments)]

use runtime_loader::{
    lifecycle_hook, ActivationStage, AttemptId, Cancellation, CapabilityConflictReason,
    CapabilityDeclaration, CapabilityDefinition, CapabilityHandle, CapabilityId, CapabilityPin,
    CapabilityReplayIdentity, CapabilityRequirement, CapabilitySlot, CapabilityValue, CatalogEntry,
    CleanupResource, CompletionRecord, CompositionBuilder, CompositionDriverShutdown,
    CompositionError, CompositionHandle, CompositionPlan, CompositionReleaseFuture,
    CompositionReleaseOutcome, CompositionReleaseRejection, CompositionShutdownFailure,
    ConfigRequirement, ConstructionStage, DefinitionError, DefinitionIdentity, DispatchRecord,
    DriveResult, DriverError, DriverExit, DriverFuture, DriverOwner, DriverOwnerState,
    DurableRunState, DurableStore, EffectDispatchError, EffectDispatchFuture,
    EffectDispatchRequest, EffectDispatcher, EffectSemantics, EntryId, FactoryConflictReason,
    FactoryRegistry, FactoryResolutionError, FiberState, FileDurableStore, Generation, HookFuture,
    HostConfig, Id, InMemoryDurableStore, IncompatibleEntryReason, InvalidId,
    InvalidReferenceReason, JournalError, KeyedStreamItem, KnownEffectOutcome,
    LegacyMutationOperation, LifecycleHook, LoaderError, ModuleCatalog, ModuleDefinition,
    ModuleFactoryError, ModuleReference, ModuleRegistration, ModuleVersion, OperationId,
    OutcomeRecord, OwnerLossReleaseError, PluginConfig, PluginConflictReason, PluginDefinition,
    PluginFactory, PluginLoadContext, PluginRuntime, ReactiveCapabilityDeclaration,
    ReconstructionError, RecoveredEffectState, ResolvedDependencies, ResolvedModules,
    RollbackFailure, RollbackReport, RunDefinition, RunId, Runtime, RuntimeAssembly, RuntimeDriver,
    RuntimeError, RuntimeEvent, RuntimeHandle, RuntimeLoader, ScopeError, ScopedEffect,
    SequenceError, ShutdownStatus, StartupFailure, StepResult, StoreError, StoreErrorKind,
    StoreInvariant, StoreRevision, StreamItem, TaskAttempt, TaskDefinition, WorkflowGraphError,
    WorkflowReplayIdentity, LEGACY_RUN_DEFINITION_FORMAT_V1, RUN_DEFINITION_FORMAT,
};
use std::{collections::BTreeMap, sync::Arc};

/// The host implements exactly one trait to drive effects; its request and
/// future types are named rather than inferred.
struct HostDispatcher;

impl EffectDispatcher for HostDispatcher {
    fn dispatch(&mut self, request: EffectDispatchRequest) -> EffectDispatchFuture {
        let operation: OperationId = request.operation_id.clone();
        let semantics: EffectSemantics = request.semantics;
        let _ = (operation, semantics);
        Box::pin(async move { Ok(KnownEffectOutcome::Succeeded) })
    }
}

/// What the host declares and what the catalog holds.
pub fn resolution(
    reference: ModuleReference,
    version: &ModuleVersion,
) -> Result<(CatalogEntry, String), InvalidId> {
    let module: Id = Id::new("module")?;
    let capability: Id = Id::new("store")?;
    let task: Id = Id::new("prepare")?;
    let entry: CatalogEntry = CatalogEntry::new(reference.clone(), move || {
        let declaration: CapabilityDeclaration =
            CapabilityDeclaration::new(capability.clone(), "provider", "v1");
        let definition: ModuleDefinition = ModuleDefinition::new(module.clone())
            .with_declarative_capability(declaration)
            .with_task(TaskDefinition::new(task.clone(), "prepare"));
        Ok(ModuleRegistration::new(definition))
    })
    .depends_on(reference.clone());
    Ok((entry, format!("{}@{}", reference.id(), version.as_str())))
}

/// Resolution is its own phase with its own failure type.
pub fn resolve(
    loader: &RuntimeLoader<'_>,
    requested: Vec<ModuleReference>,
) -> Result<ResolvedModules, LoaderError> {
    loader.resolve(requested)
}

/// Composition planning is a separate phase with a separate failure type; both
/// planning routes are named.
pub fn compose(modules: ResolvedModules) -> Result<CompositionPlan, CompositionError> {
    modules.compose()
}

pub fn compose_via_builder(modules: ResolvedModules) -> Result<CompositionPlan, CompositionError> {
    let builder: CompositionBuilder = modules.into_composition_builder()?;
    builder.build()
}

/// The plan's vocabulary, including definition identity with both format tags:
/// the current write format and the frozen historical one (ADR 0008).
pub fn plan_vocabulary(
    plan: &CompositionPlan,
) -> Result<(WorkflowReplayIdentity, Vec<Id>, usize), DefinitionError> {
    let definition: &RunDefinition = plan.definition();
    let identity: WorkflowReplayIdentity = definition.identity()?;
    assert!(
        identity.as_str().starts_with(RUN_DEFINITION_FORMAT),
        "a supported definition is identified in the current write format"
    );
    assert!(
        !identity
            .as_str()
            .starts_with(LEGACY_RUN_DEFINITION_FORMAT_V1),
        "the kernel never writes the ambiguous historical format"
    );
    let order: Vec<Id> = plan.module_order().to_vec();
    let slots: &BTreeMap<Id, CapabilitySlot> = plan.slots();
    Ok((identity, order, slots.len()))
}

/// The declarative seam: the host names the factory registry and the
/// requirement vocabulary it fills.
pub fn declarative_registry(
    capability: Id,
    identity: &str,
) -> Result<FactoryRegistry, FactoryResolutionError> {
    let requirement: CapabilityRequirement =
        CapabilityRequirement::new(capability.clone(), DefinitionIdentity::from(identity));
    drop(requirement);
    let mut factories: FactoryRegistry = FactoryRegistry::new();
    factories.register(capability, identity, |_| {
        Ok(CapabilityValue::from_value("fixture".to_owned()))
    })?;
    Ok(factories)
}

/// Starting by definition, with the failure passed through the classifier that
/// names the new compatibility variant.
pub fn declarative_lifecycle(
    run: RunId,
    definition: &RunDefinition,
    factories: &FactoryRegistry,
) -> Result<Runtime<InMemoryDurableStore>, RuntimeError> {
    let started: Runtime<InMemoryDurableStore> =
        Runtime::start_from_definition(run, definition.clone(), factories)
            .map_err(classify_runtime_error)?;
    Ok(started)
}

/// Every `RuntimeError` shape a supported host must react to by name, matched
/// rather than stringified.
pub fn classify_runtime_error(error: RuntimeError) -> RuntimeError {
    match error {
        RuntimeError::LegacyDefinitionIdentityUnsupported {
            durable_identity,
            supported_format,
        } => {
            let _legacy_prefix: &'static str = LEGACY_RUN_DEFINITION_FORMAT_V1;
            let _durable: &str = durable_identity.as_str();
            RuntimeError::LegacyDefinitionIdentityUnsupported {
                durable_identity,
                supported_format,
            }
        }
        RuntimeError::DefinitionMismatch { .. }
        | RuntimeError::MissingCapability { .. }
        | RuntimeError::CapabilityReplayMismatch { .. }
        | RuntimeError::WorkflowReplayIdentityMismatch { .. }
        | RuntimeError::AttemptIdExhausted
        | RuntimeError::CancelledAfterDispatch { .. } => error,
        other => other,
    }
}

/// Activation and cold restore, with the assembly named on both paths.
pub async fn activate(
    plan: CompositionPlan,
    run: RunId,
    config: &HostConfig,
) -> Result<RuntimeAssembly<InMemoryDurableStore>, StartupFailure> {
    plan.start(run, config).await
}

pub async fn restore(
    plan: CompositionPlan,
    run: RunId,
    config: &HostConfig,
    store: FileDurableStore,
) -> Result<RuntimeAssembly<FileDurableStore>, StartupFailure> {
    plan.restore(run, config, store).await
}

/// A lifecycle failure is either a driver rejection or a refused release; both
/// are umbrella types, and a refused exit stays available to its owner.
pub enum LifecycleFailure {
    Driver(DriverError),
    Refused(CompositionReleaseRejection<InMemoryDurableStore>),
}

/// The canonical orderly lifecycle, fully named: driver, handle, the
/// driver-owner token the handle is bound to, the exit that token must
/// recognize, and the owner-bound release outcome (ADR 0009).
pub async fn orderly_lifecycle(
    assembly: RuntimeAssembly<InMemoryDurableStore>,
) -> Result<CompositionDriverShutdown<InMemoryDurableStore>, LifecycleFailure> {
    let (driver, handle, mut composition): (
        RuntimeDriver<InMemoryDurableStore, HostDispatcher>,
        RuntimeHandle,
        CompositionHandle,
    ) = assembly.into_driver(HostDispatcher);
    let owner: DriverOwner = handle.owner();
    let state: DriverOwnerState = handle.owner_state();
    if !matches!(state, DriverOwnerState::Running) {
        return Err(LifecycleFailure::Driver(DriverError::ShuttingDown));
    }
    let driven: DriveResult = handle.drive().await.map_err(LifecycleFailure::Driver)?;
    let step: StepResult = match driven {
        DriveResult::Step(step) => step,
        DriveResult::EffectCompleted { .. }
        | DriveResult::EffectUnknown { .. }
        | DriveResult::Backpressured { .. } => return Ok(no_release()),
    };
    drop(step);
    let progress: Vec<KeyedStreamItem<Id, RuntimeEvent>> = handle
        .drain_progress_events()
        .await
        .map_err(LifecycleFailure::Driver)?;
    drop(progress);
    let telemetry: DriverFuture<Vec<StreamItem<RuntimeEvent>>> = handle.drain_telemetry_events();
    drop(telemetry);
    let status: ShutdownStatus = handle.shutdown().await.map_err(LifecycleFailure::Driver)?;
    drop(status);
    let exit: DriverExit<InMemoryDurableStore> = driver.run().await;
    assert!(
        owner.same_driver_as(exit.owner()),
        "the exit was produced by the driver this handle commands"
    );
    {
        // The runtime rides back on its own exit and stays usable until the
        // release that owns it runs.
        let runtime: &Runtime<InMemoryDurableStore> = exit.runtime();
        let _run: &RunId = runtime.run_id();
    }
    let release: CompositionReleaseFuture<'_, InMemoryDurableStore> =
        composition.dispose_after_driver(exit);
    match release.await {
        CompositionReleaseOutcome::Released(shutdown) => Ok(*shutdown),
        CompositionReleaseOutcome::Rejected(rejection) => {
            // A refusal keeps its own variant and its own exit: the host hands
            // the rejection (with the exit inside it) back to the composition
            // that produced the exit.
            Err(LifecycleFailure::Refused(rejection))
        }
    }
}

/// Only reachable from the non-`Step` branches above; declared so the release
/// aggregate type stays named on every path.
fn no_release() -> CompositionDriverShutdown<InMemoryDurableStore> {
    unreachable!("compile-only fixture")
}

/// The best-effort owner-loss release and the cleanup report it must tell the
/// truth in.
pub async fn owner_loss_release(
    composition: &mut CompositionHandle,
) -> Result<RollbackReport, OwnerLossReleaseError> {
    let report: RollbackReport = composition.release_after_owner_loss().await?;
    let failures: &Vec<RollbackFailure> = &report.failures;
    for failure in failures {
        match failure.resource {
            CleanupResource::DisposeHook
            | CleanupResource::Fiber { .. }
            | CleanupResource::PluginRegistration { .. } => {}
        }
        let _module: &Id = &failure.module_id;
    }
    Ok(report)
}

/// Durable facts a host reads back, named through the umbrella.
pub fn durable_facts(state: &DurableRunState, operation: &OperationId) -> (StoreRevision, usize) {
    let revision: StoreRevision = state.revision();
    let _identity: Option<&WorkflowReplayIdentity> = state.workflow_replay_identity();
    let completions: &[CompletionRecord] = state.completion_history();
    let dispatches: usize = state.dispatches(operation).count();
    (revision, completions.len() + dispatches)
}

/// Recovery classification: both fields of the decision are named, and the
/// reason is matched rather than stringified.
pub fn classify_recovery(decision: &runtime_loader::RecoveryDecision) -> &'static str {
    let action: &runtime_loader::RecoveryAction = &decision.action;
    let reason: &'static str = match &decision.reason {
        runtime_loader::RecoveryReason::PreparedNotDispatched { .. } => "prepared-not-dispatched",
        runtime_loader::RecoveryReason::IdempotentOutcomeUnknown { .. } => {
            "idempotent-outcome-unknown"
        }
        _ => "other",
    };
    if matches!(action, runtime_loader::RecoveryAction::NoAction) {
        "no-action"
    } else {
        reason
    }
}

/// Store failures are classified by kind and by the invariant they carry.
pub fn classify_store_error(error: &StoreError) -> (StoreErrorKind, Option<&StoreInvariant>) {
    let invariant: Option<&StoreInvariant> = match error {
        StoreError::InvariantViolation(invariant) => Some(invariant),
        _ => None,
    };
    (error.kind(), invariant)
}

/// Composition failures keep their own structured reasons per phase.
pub fn classify_composition_error(error: &CompositionError) -> &'static str {
    match error {
        CompositionError::CapabilityDefinitionConflict { reason, .. } => match reason {
            CapabilityConflictReason::UnownedRequirement => "unowned-requirement",
            CapabilityConflictReason::RequiredOnReactiveSlot { .. } => "required-on-reactive-slot",
            CapabilityConflictReason::IdentityMismatch { .. } => "identity-mismatch",
        },
        CompositionError::CapabilityPluginConflict { reason, .. } => match reason {
            PluginConflictReason::DuplicateRegistration { .. } => "duplicate-registration",
            PluginConflictReason::SlotAlreadyClaimed { .. } => "slot-already-claimed",
            _ => "plugin-conflict",
        },
        CompositionError::FactoryConflict { reason, .. } => match reason {
            FactoryConflictReason::UndeclaredSlot => "undeclared-slot",
            FactoryConflictReason::FactoryOnReactiveSlot { .. } => "factory-on-reactive-slot",
            FactoryConflictReason::IdentityMismatch { .. } => "identity-mismatch",
            _ => "factory-conflict",
        },
        CompositionError::ActivationFailed { stage, .. } => match stage {
            ActivationStage::PluginRegistration { .. } => "plugin-registration",
            ActivationStage::FiberInstantiate { .. } => "fiber-instantiate",
            ActivationStage::FiberStart { .. } => "fiber-start",
            _ => "activation",
        },
        CompositionError::RuntimeConstructionFailed { stage, .. } => match stage {
            ConstructionStage::Start => "runtime-start",
            ConstructionStage::Restore => "runtime-restore",
        },
        _ => "composition",
    }
}

/// Every phase keeps its own failure type; a host can name all of them.
pub fn error_taxonomy(
    reconstruction: &ReconstructionError,
    startup: &StartupFailure,
    shutdown: &CompositionShutdownFailure,
    factory: &FactoryResolutionError,
    journal: &JournalError,
    graph: &WorkflowGraphError,
    scope: &ScopeError,
    sequence: &SequenceError,
    definition: &DefinitionError,
    loader: &LoaderError,
    reference: &InvalidReferenceReason,
    incompatible: &IncompatibleEntryReason,
    module_factory: &ModuleFactoryError,
    legacy: &LegacyMutationOperation,
) -> String {
    let cause: &CompositionError = &startup.cause;
    format!(
        "{reconstruction} / {cause} / {factory} / {journal} / {graph} / {scope} / {sequence} / \
         {definition} / {loader} / {reference} / {incompatible} / {module_factory} / \
         {legacy:?} / {} / {}",
        shutdown.rollback.is_success(),
        startup.rollback.is_success()
    )
}

/// Plugin and lifecycle authoring vocabulary.
pub fn authoring(
    capability: CapabilityDefinition,
    plugin_id: Id,
    factory: PluginFactory,
) -> (Arc<PluginRuntime>, LifecycleHook) {
    let definition: PluginDefinition = PluginDefinition::new(plugin_id, capability, factory);
    let runtime: Arc<PluginRuntime> = PluginRuntime::new(definition);
    let hook: LifecycleHook = lifecycle_hook(|| Box::pin(async { Ok(()) }));
    let future: HookFuture = hook();
    drop(future);
    (runtime, hook)
}

/// The effect seam a plugin registers cleanup through.
pub fn effect_seam(context: &PluginLoadContext) -> ScopedEffect {
    let label: String = context.config().to_owned();
    ScopedEffect::sync(label, || Ok(()))
}

/// The remaining pass-through vocabulary a supported host may bind.
pub fn pass_through(
    pin: &CapabilityPin,
    attempt: &TaskAttempt,
    cancellation: &Cancellation,
    handle: &CapabilityHandle,
    replay: &CapabilityReplayIdentity,
    dependencies: &ResolvedDependencies,
    config: &PluginConfig,
    fiber: &FiberState,
    requirement: &ConfigRequirement,
    reactive: &ReactiveCapabilityDeclaration,
    slot: &CapabilitySlot,
    value: &CapabilityValue,
    error: &EffectDispatchError,
    state: &RecoveredEffectState,
    outcome: &OutcomeRecord,
    dispatch: &DispatchRecord,
    attempt_id: &AttemptId,
    catalog: &ModuleCatalog,
) -> bool {
    // Naming these types is the assertion. Deep inspection of them is the
    // documented subsystem tier, so no accessor is called here — except the
    // coordinate types a host that records an observation has to bind, which
    // are host tier since the third review (2026-10-22, B1).
    let _generation: &Generation = &pin.generation;
    let _entry: &EntryId = &pin.entry_id;
    let _capability: &CapabilityId = handle.id();
    let _ = (
        pin,
        attempt,
        cancellation,
        handle,
        replay,
        dependencies,
        config,
        fiber,
        requirement,
        reactive,
        slot,
        value,
        error,
        state,
        outcome,
        dispatch,
        attempt_id,
        catalog,
    );
    true
}
/// The store seam stays nameable: a host may be generic over `DurableStore`
/// without importing `workflow-recovery`.
pub fn store_is_nameable<S: DurableStore>(store: &S, memory: &InMemoryDurableStore) -> bool {
    let _ = (store, memory);
    true
}

/// The legacy compatibility seam is still nameable, including the operation the
/// runtime refuses for declarative runs.
pub fn legacy_operation_label(operation: &LegacyMutationOperation) -> String {
    format!("{operation:?}")
}

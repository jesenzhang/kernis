//! K4 runtime and plugin composition API.
//!
//! This crate is a composition layer, not a second runtime. It composes
//! host-provided [`ModuleRegistration`]s into one deterministic
//! [`CompositionPlan`], merges the stable [`ModuleDefinition`] contributions
//! into a single K2 [`RunDefinition`] and [`FactoryRegistry`], activates the
//! result through the existing K2/K3/M2-C machinery, and owns exactly the
//! resources activation acquired so rollback and shutdown release them in
//! reverse order: `dispose` hooks, started fibers, and plugin runtime
//! registrations through the capability registry. Cleanup only claims
//! completion while it holds that registry authority — after orderly driver
//! shutdown through [`CompositionHandle::dispose_after_driver`], or with the
//! live runtime on the driverless paths.
//!
//! # Two-plane model
//!
//! [`ModuleDefinition`] carries stable, serializable semantics only.
//! [`ModuleRegistration`] carries the process-local executable objects
//! (factories, plugin runtimes, lifecycle hooks) and is never serialized or
//! written to a durable store.
//!
//! # One capability slot, one declared ownership path
//!
//! Every capability slot a module declares is owned either declaratively
//! (contributes to the merged [`RunDefinition`] and requires a factory) or
//! reactively (published by a registered plugin fiber, contributes nothing
//! to the durable definition, cannot be required by a task). There is no
//! placeholder factory, dummy value, or string-kind escape hatch.
//!
//! # Deterministic, side-effect-free planning
//!
//! [`CompositionBuilder::build`] performs full validation and deterministic
//! planning before any activation: registration order changes never change
//! the activation order, the merged definition, or composition behavior.

mod assembly;
mod config;
mod definition;
mod error;
mod plan;
mod registration;

pub use assembly::{CompositionDriverShutdown, CompositionHandle, RuntimeAssembly};
pub use config::HostConfig;
pub use definition::{
    CapabilityContribution, CapabilityOwnership, ConfigRequirement, ModuleDefinition,
    ReactiveCapabilityDeclaration,
};
pub use error::{
    ActivationStage, CapabilityConflictReason, CleanupResource, CompositionError,
    CompositionShutdownFailure, ConstructionStage, FactoryConflictReason, PluginConflictReason,
    RollbackFailure, RollbackReport, StartupFailure,
};
pub use plan::{CapabilitySlot, CompositionBuilder, CompositionPlan};
pub use registration::{
    CapabilityFactoryFn, HookFuture, LifecycleHook, ModuleRegistration, lifecycle_hook,
};

// Upstream types the host passes through or receives from the composition
// API, re-exported so a host depends on this crate alone for composition.
pub use capability_graph::{
    CapabilityDefinition, CapabilityFiber, CapabilityValue, FiberState, PluginConfig,
    PluginDefinition, PluginFactory, PluginLoadContext, PluginRuntime, ResolvedDependencies,
    ScopedEffect,
};
pub use kernis_core::Id;
pub use runtime_core::{
    AttemptId, Cancellation, CapabilityDeclaration, CapabilityHandle, CapabilityPin,
    CapabilityReplayIdentity, CapabilityRequirement, DefinitionError, DefinitionIdentity,
    DriveResult, DriverError, DriverExit, DriverFuture, DurableRunState, EffectDispatchError,
    EffectDispatchFuture, EffectDispatchRequest, EffectDispatcher, FactoryResolutionError,
    JournalError, KeyedStreamItem, LegacyMutationOperation, ReconstructionError, RecoveryDecision,
    RunDefinition, RunId, Runtime, RuntimeDriver, RuntimeError, RuntimeEvent, RuntimeHandle,
    ScopeError, SequenceError, ShutdownStatus, StepResult, StoreError, StreamItem, TaskAttempt,
    TaskDefinition, WorkflowGraphError, WorkflowReplayIdentity,
};
pub use workflow_recovery::{
    DurableStore, EffectSemantics, FileDurableStore, InMemoryDurableStore, KnownEffectOutcome,
    OperationId,
};

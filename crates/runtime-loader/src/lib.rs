//! K5 minimal loader boundary — and, as of K6, the supported host entry
//! for the KERNIS R2 kernel.
//!
//! As the host entry (ADR 0007) this crate re-exports the complete
//! supported host vocabulary: a host can declare, resolve, compose,
//! activate, drive, recover, restore, and shut down depending on this
//! crate alone. The tiered surface inventory lives in
//! `docs/runtime/K6-supported-api-inventory.md`; the canonical full
//! lifecycle — including a genuine cold restart — is
//! [`examples/r2_host.rs`](https://github.com/jesenzhang/kernis/blob/main/crates/runtime-loader/examples/r2_host.rs).
//!
//! As the loader, this crate is a thin declarative layer between host configuration and K4
//! composition — not a plugin system. K4 already owns module composition,
//! activation, and rollback; this crate adds exactly one thing: a host can
//! declare *which* logical modules it needs and let the loader resolve the
//! reference dependency closure into fresh K4 [`ModuleRegistration`]s,
//! instead of hand-wiring every registration itself.
//!
//! ```text
//! Declarative ModuleReferences
//!         ↓  RuntimeLoader::resolve (this crate)
//! ResolvedModules (fresh process-local registrations)
//!         ↓  ResolvedModules::compose (handoff to K4)
//! CompositionPlan (K4 planning)
//!         ↓  start / start_with_store / restore (K4 activation)
//! RuntimeAssembly
//! ```
//!
//! # Three visible phases
//!
//! * **Resolution** ([`RuntimeLoader::resolve`]) — synchronous,
//!   side-effect-free with respect to the Runtime. Catalog lookup,
//!   deterministic dependency closure, cycle classification, and fresh
//!   construction of ordinary process-local values. Failures are
//!   [`LoaderError`]s and produce no Runtime-owned effect.
//! * **Composition** ([`ResolvedModules::compose`]) — hands the resolved
//!   registrations to K4's [`CompositionBuilder`]. Failures are K4
//!   [`CompositionError`]s, never rewritten loader errors.
//! * **Activation** — K4's `start`/`restore`, unchanged, owned by K4.
//!   Failures are [`StartupFailure`]s.
//!
//! The three phases stay distinguishable by type: [`LoaderError`] vs
//! [`CompositionError`] vs [`StartupFailure`]. Nothing collapses them into a
//! `load_and_run`.
//!
//! # What this crate is not
//!
//! It is not a dynamic plugin loader. The default loader is static,
//! in-process, explicitly registered, and trusted. There is no filesystem
//! discovery, directory scanning, dynamic library loading, WASM, network
//! download, package manager, marketplace, watcher, or hot reload — and the
//! [`ModuleCatalog`] is an explicit host-provided value, never a global
//! registry or ambient inventory. The catalog contains executable Rust
//! factories already linked into the host, so catalog registration is
//! trusted host authority; this crate does not sandbox executable code and
//! does not claim plugin isolation. See
//! `docs/architecture/0006-k5-minimal-loader-boundary.md`.
//!
//! # Vocabulary
//!
//! The loader vocabulary is deliberately *not* another plugin concept:
//! [`ModuleReference`] / [`ModuleVersion`] name what the host declares,
//! [`CatalogEntry`] / [`ModuleCatalog`] name what the host supplies,
//! [`RuntimeLoader`] performs resolution, and [`ResolvedModules`] is the
//! activated-later result. `PluginDefinition`/`PluginRuntime` remain the
//! capability-graph-owned lifecycle vocabulary and are not reused here.

mod catalog;
mod error;
mod reference;
mod resolve;

pub use catalog::{CatalogEntry, ModuleCatalog, ModuleRegistrationFactory};
pub use error::{IncompatibleEntryReason, InvalidReferenceReason, LoaderError, ModuleFactoryError};
pub use reference::{ModuleReference, ModuleVersion};
pub use resolve::{ResolvedModules, RuntimeLoader};

// Upstream types the host passes through or receives from the loader and
// composition boundaries, re-exported so a host can declare, resolve,
// compose, and activate depending on this crate alone.
pub use kernis_core::Id;
pub use runtime_composition::{
    ActivationStage, CapabilityContribution, CapabilityFactoryFn, CapabilityOwnership,
    CapabilitySlot, CleanupResource, CompositionBuilder, CompositionDriverShutdown,
    CompositionError, CompositionHandle, CompositionPlan, CompositionShutdownFailure,
    ConfigRequirement, ConstructionStage, FactoryConflictReason, HookFuture, HostConfig,
    LifecycleHook, ModuleDefinition, ModuleRegistration, OwnerLossReleaseError,
    PluginConflictReason, ReactiveCapabilityDeclaration, RollbackFailure, RollbackReport,
    StartupFailure, lifecycle_hook,
};
pub use runtime_composition::{
    AttemptId, Cancellation, CapabilityDeclaration, CapabilityHandle, CapabilityPin,
    CapabilityReplayIdentity, CapabilityRequirement, CompletionRecord, DefinitionError,
    DefinitionIdentity, DispatchRecord, DriveResult, DriverError, DriverExit, DriverFuture,
    DriverOwnerState, DurableRunState, EffectDispatchError, EffectDispatchFuture,
    EffectDispatchRequest, EffectDispatcher, FactoryResolutionError, JournalError, KeyedStreamItem,
    LegacyMutationOperation, OutcomeRecord, ReconstructionError, RecoveredEffectState,
    RecoveryAction, RecoveryDecision, RunDefinition, RunId, Runtime, RuntimeDriver, RuntimeError,
    RuntimeEvent, RuntimeHandle, ScopeError, SequenceError, ShutdownStatus, StepResult, StoreError,
    StoreErrorKind, StreamItem, TaskAttempt, TaskDefinition, WorkflowGraphError,
    WorkflowReplayIdentity,
};
pub use runtime_composition::{
    CapabilityDefinition, CapabilityFiber, CapabilityValue, FiberState, PluginConfig,
    PluginDefinition, PluginFactory, PluginLoadContext, PluginRuntime, ResolvedDependencies,
    ScopedEffect,
};
pub use runtime_composition::{
    DurableStore, EffectSemantics, FileDurableStore, InMemoryDurableStore, KnownEffectOutcome,
    OperationId,
};

//! Typed composition failures for planning, activation, and rollback.

use kernis_core::Id;
use runtime_core::{DefinitionError, DefinitionIdentity, RuntimeError};
use std::error::Error;
use std::fmt;

/// One cleanup step that failed while composing or disposing owned resources.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RollbackFailure {
    /// Module whose owned resource failed cleanup.
    pub module_id: Id,
    /// Owned resource class that failed.
    pub resource: CleanupResource,
    /// Disposer-provided failure description.
    pub reason: String,
}

/// Class of a composition-owned resource during cleanup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CleanupResource {
    /// The module `dispose` lifecycle hook.
    DisposeHook,
    /// One instantiated capability fiber owned through a plugin.
    Fiber {
        /// Logical plugin identity of the failed fiber.
        plugin_id: Id,
    },
    /// One composition-owned plugin runtime registration that failed to
    /// unregister through the capability registry.
    PluginRegistration {
        /// Logical plugin identity of the failed registration.
        plugin_id: Id,
    },
}

/// Structured result of a reverse-order rollback or shutdown sweep.
///
/// Cleanup continues after an individual failure, so `failures` may contain
/// multiple entries and `cleaned` lists the modules whose owned cleanup
/// completed without error.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RollbackReport {
    /// Module ids whose owned cleanup completed, in cleanup (reverse
    /// activation) order.
    pub cleaned: Vec<Id>,
    /// Every cleanup failure observed, in cleanup order.
    pub failures: Vec<RollbackFailure>,
}

impl RollbackReport {
    /// Returns whether cleanup completed with no failures.
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.failures.is_empty()
    }
}

/// Why a capability requirement or declaration does not agree with the
/// composition slot table.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CapabilityConflictReason {
    /// A task or declaration requires a capability no module owns.
    UnownedRequirement,
    /// A task requires a capability whose slot is reactively owned.
    RequiredOnReactiveSlot {
        /// Module that owns the reactive slot.
        module_id: Id,
    },
    /// The required definition identity differs from the declared slot.
    IdentityMismatch {
        /// Identity carried by the requirement.
        required: DefinitionIdentity,
        /// Identity declared by the slot.
        declared: DefinitionIdentity,
    },
}

/// Why a factory contribution does not match the composition contract.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FactoryConflictReason {
    /// A factory targets a capability no module declares.
    UndeclaredSlot,
    /// A factory targets a reactively owned slot.
    FactoryOnReactiveSlot {
        /// Module that owns the reactive slot.
        module_id: Id,
    },
    /// The factory's definition identity differs from the declared slot.
    IdentityMismatch {
        /// Identity declared by the slot.
        declared: DefinitionIdentity,
    },
    /// Two modules provide a factory for the same capability and identity.
    Duplicate {
        /// Module whose contribution collided.
        module_id: Id,
        /// Module that provided the factory first.
        previous_module_id: Id,
    },
    /// A declaratively owned slot has no factory contribution.
    Missing,
    /// The factory's definition identity is empty.
    InvalidDefinitionIdentity,
}

/// Why a plugin contribution does not match the composition contract.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PluginConflictReason {
    /// Two modules register the same plugin identity.
    DuplicateRegistration {
        /// Module whose registration collided.
        module_id: Id,
        /// Module that registered the plugin first.
        previous_module_id: Id,
    },
    /// Two plugins claim the same reactive capability slot.
    SlotAlreadyClaimed {
        /// Module whose plugin collided.
        module_id: Id,
        /// Module whose plugin claimed the slot first.
        previous_module_id: Id,
    },
    /// A plugin publishes a capability no module declares.
    UnownedPublication,
    /// A plugin publishes a capability that a module owns declaratively.
    StaticSlotCollision {
        /// Module that owns the declarative slot.
        module_id: Id,
    },
    /// The plugin's capability definition disagrees with the reactive slot.
    DefinitionMismatch {
        /// Human-readable mismatch description.
        detail: String,
    },
}

/// Stage of module activation that failed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ActivationStage {
    /// Registering the module's plugin runtime failed.
    PluginRegistration {
        /// Plugin whose registration failed.
        plugin_id: Id,
        /// Registry-provided failure description.
        reason: String,
    },
    /// Instantiating a fiber for the module's plugin failed.
    FiberInstantiate {
        /// Plugin whose fiber instantiation failed.
        plugin_id: Id,
        /// Coordinator-provided failure description.
        reason: String,
    },
    /// Starting an instantiated fiber failed.
    FiberStart {
        /// Plugin whose fiber start failed.
        plugin_id: Id,
        /// Fiber-provided failure description.
        reason: String,
    },
    /// The module `activate` lifecycle hook failed.
    ActivateHook {
        /// Hook-provided failure description.
        reason: String,
    },
}

/// Which Runtime construction path failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConstructionStage {
    /// `Runtime::start_from_definition*` failed.
    Start,
    /// `Runtime::restore_from_definition` failed.
    Restore,
}

/// Typed failure of composition planning, activation, or rollback.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CompositionError {
    /// The same module identity was registered twice.
    DuplicateModule {
        /// Module identity registered twice.
        module_id: Id,
    },
    /// A module depends on a module that is not registered.
    MissingModuleDependency {
        /// Module that declared the dependency.
        module_id: Id,
        /// Dependency that is not registered.
        dependency_id: Id,
    },
    /// The module dependency graph contains a cycle.
    ModuleDependencyCycle {
        /// Deterministic cycle path in module dependency order.
        cycle: Vec<Id>,
    },
    /// Two modules contribute the same task identity.
    DuplicateTaskContribution {
        /// Task identity contributed twice.
        task_id: Id,
        /// Module whose contribution collided.
        module_id: Id,
        /// Module that contributed the task first.
        previous_module_id: Id,
    },
    /// Two modules claim ownership of the same capability slot.
    DuplicateCapabilityOwnership {
        /// Capability identity claimed twice.
        capability_id: Id,
        /// Module whose claim collided.
        module_id: Id,
        /// Module that claimed the capability first.
        previous_module_id: Id,
    },
    /// A capability requirement or declaration disagrees with a slot.
    CapabilityDefinitionConflict {
        /// Capability whose usage conflicts.
        capability_id: Id,
        /// Structured conflict classification.
        reason: CapabilityConflictReason,
    },
    /// A reactively owned capability slot has no plugin contribution.
    MissingCapabilityPlugin {
        /// Reactive slot without a plugin.
        capability_id: Id,
        /// Module that owns the slot.
        module_id: Id,
    },
    /// A plugin contribution conflicts with the slot ownership table.
    CapabilityPluginConflict {
        /// Plugin whose contribution conflicts.
        plugin_id: Id,
        /// Structured conflict classification.
        reason: PluginConflictReason,
    },
    /// Two modules require the same configuration key with different
    /// requiredness.
    ConfigurationConflict {
        /// Configuration key required inconsistently.
        key: Id,
        /// Module whose requirement collided.
        module_id: Id,
        /// Module that declared the key first.
        previous_module_id: Id,
    },
    /// The host did not provide a required configuration key.
    MissingConfiguration {
        /// Module that requires the key.
        module_id: Id,
        /// Configuration key the host omitted.
        key: Id,
    },
    /// A factory contribution conflicts with the slot ownership table.
    FactoryConflict {
        /// Capability the factory targets.
        capability_id: Id,
        /// Definition identity the factory targets.
        definition_identity: DefinitionIdentity,
        /// Structured conflict classification.
        reason: FactoryConflictReason,
    },
    /// The merged contributions do not form a valid K2 `RunDefinition`.
    InvalidMergedDefinition {
        /// K2 validation failure for the merged definition.
        source: DefinitionError,
    },
    /// A module activation step failed after validation succeeded.
    ActivationFailed {
        /// Module whose activation failed.
        module_id: Id,
        /// Failed activation stage.
        stage: ActivationStage,
    },
    /// The final reactive reconciliation did not reach a stable boundary.
    ReconciliationFailed {
        /// Reconciliation-provided failure description.
        reason: String,
    },
    /// A cleanup sweep collected one or more failures.
    RollbackFailed {
        /// Every cleanup failure observed, in cleanup order.
        failures: Vec<RollbackFailure>,
    },
    /// The underlying K2 Runtime construction or restore failed.
    RuntimeConstructionFailed {
        /// Which construction path failed.
        stage: ConstructionStage,
        /// Runtime-core failure wrapped as the cause.
        source: RuntimeError,
    },
}

impl fmt::Display for CompositionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateModule { module_id } => {
                write!(f, "module {module_id} is registered twice")
            }
            Self::MissingModuleDependency {
                module_id,
                dependency_id,
            } => write!(
                f,
                "module {module_id} depends on unregistered {dependency_id}"
            ),
            Self::ModuleDependencyCycle { cycle } => {
                write!(f, "module dependency cycle: ")?;
                for (index, id) in cycle.iter().enumerate() {
                    if index > 0 {
                        write!(f, " -> ")?;
                    }
                    write!(f, "{id}")?;
                }
                Ok(())
            }
            Self::DuplicateTaskContribution {
                task_id,
                module_id,
                previous_module_id,
            } => write!(
                f,
                "task {task_id} is contributed by both {previous_module_id} and {module_id}"
            ),
            Self::DuplicateCapabilityOwnership {
                capability_id,
                module_id,
                previous_module_id,
            } => write!(
                f,
                "capability {capability_id} is owned by both {previous_module_id} and {module_id}"
            ),
            Self::CapabilityDefinitionConflict {
                capability_id,
                reason,
            } => write!(f, "capability {capability_id} conflicts: {reason}"),
            Self::MissingCapabilityPlugin {
                capability_id,
                module_id,
            } => write!(
                f,
                "reactive capability {capability_id} owned by {module_id} has no plugin"
            ),
            Self::CapabilityPluginConflict { plugin_id, reason } => {
                write!(f, "plugin {plugin_id} conflicts: {reason}")
            }
            Self::ConfigurationConflict {
                key,
                module_id,
                previous_module_id,
            } => write!(
                f,
                "configuration key {key} is required inconsistently by {previous_module_id} and {module_id}"
            ),
            Self::MissingConfiguration { module_id, key } => {
                write!(
                    f,
                    "host is missing required configuration {key} for module {module_id}"
                )
            }
            Self::FactoryConflict {
                capability_id,
                definition_identity,
                reason,
            } => write!(
                f,
                "factory for {capability_id}@{definition_identity} conflicts: {reason}"
            ),
            Self::InvalidMergedDefinition { source } => {
                write!(f, "merged RunDefinition is invalid: {source}")
            }
            Self::ActivationFailed { module_id, stage } => {
                write!(f, "activation of module {module_id} failed: {stage}")
            }
            Self::ReconciliationFailed { reason } => {
                write!(f, "reactive reconciliation failed to stabilize: {reason}")
            }
            Self::RollbackFailed { failures } => write!(
                f,
                "rollback collected {} cleanup failure(s): {}",
                failures.len(),
                failures
                    .iter()
                    .map(|failure| format!("{}: {failure}", failure.module_id))
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
            Self::RuntimeConstructionFailed { stage, source } => {
                write!(f, "runtime {stage:?} construction failed: {source}")
            }
        }
    }
}

impl fmt::Display for CapabilityConflictReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnownedRequirement => write!(f, "required capability has no owner"),
            Self::RequiredOnReactiveSlot { module_id } => {
                write!(f, "required capability is reactively owned by {module_id}")
            }
            Self::IdentityMismatch { required, declared } => write!(
                f,
                "required definition identity {required} differs from declared {declared}"
            ),
        }
    }
}

impl fmt::Display for FactoryConflictReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UndeclaredSlot => write!(f, "factory targets an undeclared capability slot"),
            Self::FactoryOnReactiveSlot { module_id } => {
                write!(f, "factory targets a slot reactively owned by {module_id}")
            }
            Self::IdentityMismatch { declared } => {
                write!(
                    f,
                    "factory identity differs from the declared slot {declared}"
                )
            }
            Self::Duplicate {
                module_id,
                previous_module_id,
            } => write!(
                f,
                "factory is provided by both {previous_module_id} and {module_id}"
            ),
            Self::Missing => write!(f, "declaratively owned slot has no factory contribution"),
            Self::InvalidDefinitionIdentity => write!(f, "factory definition identity is empty"),
        }
    }
}

impl fmt::Display for PluginConflictReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateRegistration {
                module_id,
                previous_module_id,
            } => write!(
                f,
                "plugin is registered by both {previous_module_id} and {module_id}"
            ),
            Self::SlotAlreadyClaimed {
                module_id,
                previous_module_id,
            } => write!(
                f,
                "reactive slot is claimed by plugins of both {previous_module_id} and {module_id}"
            ),
            Self::UnownedPublication => write!(f, "plugin publishes an unowned capability"),
            Self::StaticSlotCollision { module_id } => write!(
                f,
                "plugin publishes a capability declaratively owned by {module_id}"
            ),
            Self::DefinitionMismatch { detail } => {
                write!(
                    f,
                    "plugin capability differs from the reactive slot: {detail}"
                )
            }
        }
    }
}

impl fmt::Display for ActivationStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PluginRegistration { plugin_id, reason } => {
                write!(f, "plugin {plugin_id} registration failed: {reason}")
            }
            Self::FiberInstantiate { plugin_id, reason } => {
                write!(f, "fiber instantiation for {plugin_id} failed: {reason}")
            }
            Self::FiberStart { plugin_id, reason } => {
                write!(f, "fiber start for {plugin_id} failed: {reason}")
            }
            Self::ActivateHook { reason } => write!(f, "activate hook failed: {reason}"),
        }
    }
}

impl fmt::Display for RollbackFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.resource {
            CleanupResource::DisposeHook => {
                write!(f, "dispose hook failed: {}", self.reason)
            }
            CleanupResource::Fiber { plugin_id } => {
                write!(f, "fiber {plugin_id} disposal failed: {}", self.reason)
            }
            CleanupResource::PluginRegistration { plugin_id } => write!(
                f,
                "plugin {plugin_id} registration unregistration failed: {}",
                self.reason
            ),
        }
    }
}

impl Error for CompositionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidMergedDefinition { source } => Some(source),
            Self::RuntimeConstructionFailed { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Activation-phase failure: the cause plus the owned-resource rollback
/// report collected while unwinding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartupFailure {
    /// The activation failure that triggered the rollback.
    ///
    /// Boxed to keep the composition result's `Err` variant small; the full
    /// typed cause remains inspectable through this handle.
    pub cause: Box<CompositionError>,
    /// Result of the reverse-order cleanup sweep.
    pub rollback: RollbackReport,
}

impl fmt::Display for StartupFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.cause)?;
        if !self.rollback.failures.is_empty() {
            write!(
                f,
                "; rollback: {}",
                CompositionError::RollbackFailed {
                    failures: self.rollback.failures.clone()
                }
            )?;
        }
        Ok(())
    }
}

impl Error for StartupFailure {}

/// Failure of an otherwise successful composition shutdown sweep.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompositionShutdownFailure {
    /// Every cleanup failure observed during shutdown.
    pub rollback: RollbackReport,
}

impl fmt::Display for CompositionShutdownFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "composition shutdown collected {} cleanup failure(s)",
            self.rollback.failures.len()
        )
    }
}

impl Error for CompositionShutdownFailure {}

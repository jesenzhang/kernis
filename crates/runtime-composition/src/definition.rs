//! Stable, process-independent module declarations.
//!
//! A [`ModuleDefinition`] carries only stable composition semantics. It is
//! cold-reconstruction material and must never own a future, closure,
//! `Scope`, capability handle, fiber, runtime handle, mutex-owned object, or
//! effect disposer. Process-local executable pieces belong to
//! [`ModuleRegistration`](crate::ModuleRegistration).

use kernis_core::Id;
use runtime_core::{CapabilityDeclaration, DefinitionIdentity, TaskDefinition};
use serde::{Deserialize, Serialize};

/// Stable configuration demand of one module.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ConfigRequirement {
    /// Stable configuration key.
    pub key: Id,
    /// Whether the host must provide the key before activation.
    pub required: bool,
}

/// Stable declaration of a capability slot published by a reactive plugin
/// fiber rather than by a K2 factory.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReactiveCapabilityDeclaration {
    /// Capability identity the owning plugin fiber publishes.
    pub capability_id: Id,
    /// Capability kind the plugin's capability definition must declare.
    pub kind: String,
    /// Stable definition identity the plugin's replay identity must match.
    pub definition_identity: DefinitionIdentity,
}

/// The single declared ownership path of one capability slot.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum CapabilityOwnership {
    /// The slot participates in the merged K2 `RunDefinition` and is
    /// constructed by a merged `FactoryRegistry` entry during Runtime
    /// construction or restore. It may be required by tasks.
    Declarative,
    /// The slot is published for the lifetime of a registered plugin fiber
    /// into the Runtime's reactive capability scope. It contributes nothing
    /// to the durable definition identity and cannot be required by a task.
    Reactive,
}

/// One module's contribution to a capability slot.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum CapabilityContribution {
    /// A declaratively owned slot carrying the K2 declaration.
    Declarative(CapabilityDeclaration),
    /// A reactively owned slot carrying the expected stable shape of the
    /// owning plugin's published capability.
    Reactive(ReactiveCapabilityDeclaration),
}

impl CapabilityContribution {
    /// Returns the declared capability identity.
    #[must_use]
    pub fn capability_id(&self) -> &Id {
        match self {
            Self::Declarative(declaration) => &declaration.id,
            Self::Reactive(declaration) => &declaration.capability_id,
        }
    }

    /// Returns the declared definition identity.
    #[must_use]
    pub fn definition_identity(&self) -> &DefinitionIdentity {
        match self {
            Self::Declarative(declaration) => &declaration.definition_identity,
            Self::Reactive(declaration) => &declaration.definition_identity,
        }
    }

    /// Returns the declared capability kind.
    #[must_use]
    pub fn kind(&self) -> &str {
        match self {
            Self::Declarative(declaration) => &declaration.kind,
            Self::Reactive(declaration) => &declaration.kind,
        }
    }

    /// Returns the declared composition ownership plane.
    #[must_use]
    pub fn ownership(&self) -> CapabilityOwnership {
        match self {
            Self::Declarative(_) => CapabilityOwnership::Declarative,
            Self::Reactive(_) => CapabilityOwnership::Reactive,
        }
    }
}

/// Stable declaration of one composition module.
///
/// The field content is limited to stable semantics: identity, module
/// dependencies, task contributions, capability slot contributions, and
/// configuration requirements.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModuleDefinition {
    /// Stable module identity.
    pub id: Id,
    /// Identities of modules that must activate before this one.
    pub dependencies: Vec<Id>,
    /// Workflow task contributions merged into the K2 `RunDefinition`.
    pub tasks: Vec<TaskDefinition>,
    /// Capability slot contributions owned by this module.
    pub capabilities: Vec<CapabilityContribution>,
    /// Configuration this module demands from the host.
    pub config_requirements: Vec<ConfigRequirement>,
}

impl ModuleDefinition {
    /// Creates a module declaration without dependencies or contributions.
    #[must_use]
    pub fn new(id: Id) -> Self {
        Self {
            id,
            dependencies: Vec::new(),
            tasks: Vec::new(),
            capabilities: Vec::new(),
            config_requirements: Vec::new(),
        }
    }

    /// Adds one module dependency that must activate first.
    #[must_use]
    pub fn depends_on(mut self, dependency_id: Id) -> Self {
        self.dependencies.push(dependency_id);
        self
    }

    /// Adds one workflow task contribution.
    #[must_use]
    pub fn with_task(mut self, task: TaskDefinition) -> Self {
        self.tasks.push(task);
        self
    }

    /// Adds one declaratively owned capability slot.
    #[must_use]
    pub fn with_declarative_capability(mut self, declaration: CapabilityDeclaration) -> Self {
        self.capabilities
            .push(CapabilityContribution::Declarative(declaration));
        self
    }

    /// Adds one reactively owned capability slot.
    #[must_use]
    pub fn with_reactive_capability(
        mut self,
        capability_id: Id,
        kind: impl Into<String>,
        definition_identity: impl Into<DefinitionIdentity>,
    ) -> Self {
        self.capabilities.push(CapabilityContribution::Reactive(
            ReactiveCapabilityDeclaration {
                capability_id,
                kind: kind.into(),
                definition_identity: definition_identity.into(),
            },
        ));
        self
    }

    /// Adds one capability contribution.
    #[must_use]
    pub fn with_capability(mut self, contribution: CapabilityContribution) -> Self {
        self.capabilities.push(contribution);
        self
    }

    /// Declares that the host must provide a configuration key.
    #[must_use]
    pub fn requiring_config(mut self, key: Id) -> Self {
        self.config_requirements.push(ConfigRequirement {
            key,
            required: true,
        });
        self
    }

    /// Declares an optional configuration key.
    #[must_use]
    pub fn with_optional_config(mut self, key: Id) -> Self {
        self.config_requirements.push(ConfigRequirement {
            key,
            required: false,
        });
        self
    }
}

//! Stable declarative runtime inputs and process-local factory resolution.
//!
//! The types in this module describe the execution semantics needed to
//! reconstruct a runtime. They deliberately contain no scope, capability
//! handle, fiber, future, disposer, or other process-local value.

use capability_graph::{
    CapabilityDefinition, CapabilityGraph, CapabilityGraphError, CapabilityValue,
    ResolvedDependencies,
};
use kernis_core::Id;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;
use workflow_graph::{Task, WorkflowGraph, WorkflowGraphError, WorkflowMutation};
use workflow_recovery::{EffectSemantics, OperationId, WorkflowReplayIdentity};

/// Canonical format/version prefix for declarative run definitions.
pub const RUN_DEFINITION_FORMAT: &str = "kernis-run-definition-v1";

/// Stable identity of one capability definition understood by a factory.
///
/// This is a logical provider/configuration identity, not a process-local
/// capability entry identity. Generations, entry ids, handles, and cleanup
/// state are intentionally excluded.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct DefinitionIdentity(String);

impl DefinitionIdentity {
    /// Creates a non-empty stable definition identity.
    pub fn new(value: impl Into<String>) -> Result<Self, DefinitionError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(DefinitionError::InvalidDefinition(
                "definition identity must not be empty".to_owned(),
            ));
        }
        Ok(Self(value))
    }

    /// Returns the stable identity as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for DefinitionIdentity {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<String> for DefinitionIdentity {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl<'de> Deserialize<'de> for DefinitionIdentity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for DefinitionIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One stable capability requirement made by a task or capability.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct CapabilityRequirement {
    /// Logical capability slot being required.
    pub capability_id: Id,
    /// Stable definition identity that must be supplied for the slot.
    pub definition_identity: DefinitionIdentity,
}

impl CapabilityRequirement {
    /// Creates a capability requirement.
    #[must_use]
    pub fn new(capability_id: Id, definition_identity: impl Into<DefinitionIdentity>) -> Self {
        Self {
            capability_id,
            definition_identity: definition_identity.into(),
        }
    }
}

/// Stable declaration of a capability that can be built during
/// reconstruction.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CapabilityDeclaration {
    /// Logical capability slot published into the reconstructed root scope.
    pub id: Id,
    /// Capability kind used by the existing capability-graph authority.
    pub kind: String,
    /// Stable provider/configuration identity for this declaration.
    pub definition_identity: DefinitionIdentity,
    /// Stable capability dependencies needed before this declaration can be
    /// constructed.
    pub dependencies: Vec<CapabilityRequirement>,
}

impl CapabilityDeclaration {
    /// Creates a capability declaration without dependencies.
    #[must_use]
    pub fn new(
        id: Id,
        kind: impl Into<String>,
        definition_identity: impl Into<DefinitionIdentity>,
    ) -> Self {
        Self {
            id,
            kind: kind.into(),
            definition_identity: definition_identity.into(),
            dependencies: Vec::new(),
        }
    }

    /// Adds one stable dependency declaration.
    #[must_use]
    pub fn depends_on(mut self, requirement: CapabilityRequirement) -> Self {
        self.dependencies.push(requirement);
        self
    }

    pub(crate) fn implicit(requirement: &CapabilityRequirement) -> Self {
        Self::new(
            requirement.capability_id.clone(),
            "capability",
            requirement.definition_identity.clone(),
        )
    }
}

/// Stable declaration of one workflow task and its runtime inputs.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TaskDefinition {
    /// Stable logical task identity.
    pub id: Id,
    /// Human-readable display label. It is not replay semantics.
    pub label: String,
    /// Logical task identities that must complete first.
    pub dependencies: Vec<Id>,
    /// Capabilities required when an attempt is admitted.
    pub required_capabilities: Vec<CapabilityRequirement>,
    /// Optional external effect owned by the task.
    pub effect: Option<crate::EffectSpec>,
}

impl TaskDefinition {
    /// Creates a task declaration without dependencies or runtime inputs.
    #[must_use]
    pub fn new(id: Id, label: impl Into<String>) -> Self {
        Self {
            id,
            label: label.into(),
            dependencies: Vec::new(),
            required_capabilities: Vec::new(),
            effect: None,
        }
    }

    /// Adds one prerequisite task identity.
    #[must_use]
    pub fn depends_on(mut self, dependency_id: Id) -> Self {
        self.dependencies.push(dependency_id);
        self
    }

    /// Adds one required capability.
    #[must_use]
    pub fn require_capability(mut self, requirement: CapabilityRequirement) -> Self {
        self.required_capabilities.push(requirement);
        self
    }

    /// Assigns the external effect owned by this task.
    #[must_use]
    pub fn with_effect(mut self, operation_id: OperationId, semantics: EffectSemantics) -> Self {
        self.effect = Some(crate::EffectSpec {
            operation_id,
            semantics,
        });
        self
    }
}

/// Stable declarative input from which a fresh runtime can be constructed.
///
/// The vectors are intentionally convenient for serialization and authoring;
/// validation canonicalizes their logical contents before computing identity.
/// A leaf capability referenced by a task is implicitly declared with the
/// default kind `capability`. Use [`Self::with_capability`] to provide an
/// explicit kind or capability dependency topology.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct RunDefinition {
    /// Declared workflow tasks.
    pub tasks: Vec<TaskDefinition>,
    /// Explicit capability declarations.
    pub capabilities: Vec<CapabilityDeclaration>,
}

impl RunDefinition {
    /// Creates an empty run definition.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            tasks: Vec::new(),
            capabilities: Vec::new(),
        }
    }

    /// Returns declared tasks in authoring order.
    #[must_use]
    pub fn tasks(&self) -> &[TaskDefinition] {
        &self.tasks
    }

    /// Returns explicit capability declarations in authoring order.
    #[must_use]
    pub fn capabilities(&self) -> &[CapabilityDeclaration] {
        &self.capabilities
    }

    /// Returns this definition with one task appended.
    #[must_use]
    pub fn with_task(mut self, task: TaskDefinition) -> Self {
        self.tasks.push(task);
        self
    }

    /// Returns this definition with one capability declaration appended.
    #[must_use]
    pub fn with_capability(mut self, capability: CapabilityDeclaration) -> Self {
        self.capabilities.push(capability);
        self
    }

    /// Appends one task declaration in place.
    pub fn add_task(&mut self, task: TaskDefinition) {
        self.tasks.push(task);
    }

    /// Appends one capability declaration in place.
    pub fn add_capability(&mut self, capability: CapabilityDeclaration) {
        self.capabilities.push(capability);
    }

    /// Validates all logical identities, topology, and stable requirements.
    pub fn validate(&self) -> Result<(), DefinitionError> {
        self.validated().map(|_| ())
    }

    /// Computes the versioned canonical replay identity.
    pub fn identity(&self) -> Result<WorkflowReplayIdentity, DefinitionError> {
        Ok(self.validated()?.identity)
    }

    /// Alias for [`Self::identity`] emphasizing canonicalization.
    pub fn canonical_identity(&self) -> Result<WorkflowReplayIdentity, DefinitionError> {
        self.identity()
    }

    pub(crate) fn validated(&self) -> Result<ValidatedRunDefinition, DefinitionError> {
        let tasks = unique_tasks(&self.tasks)?;
        let mut capabilities = unique_capabilities(&self.capabilities)?;

        validate_tasks(&tasks)?;
        validate_capabilities(&capabilities)?;
        materialize_implicit_capabilities(&tasks, &mut capabilities)?;
        validate_capabilities(&capabilities)?;
        validate_capability_graph(&capabilities)?;
        validate_workflow(&tasks)?;

        let identity = canonical_identity(&tasks, &capabilities)?;
        Ok(ValidatedRunDefinition {
            tasks,
            capabilities,
            identity,
        })
    }
}

/// Typed rejection of a declarative definition before runtime execution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DefinitionError {
    /// A task logical identity appeared more than once.
    DuplicateTask(Id),
    /// A task configuration was supplied more than once for one task.
    DuplicateTaskConfiguration(Id),
    /// A capability logical identity appeared more than once.
    DuplicateCapability(Id),
    /// A task declared the same prerequisite more than once.
    DuplicateTaskDependency {
        /// Task declaring the duplicate prerequisite.
        task_id: Id,
        /// Repeated prerequisite identity.
        dependency_id: Id,
    },
    /// A capability declared the same dependency more than once.
    DuplicateCapabilityDependency {
        /// Capability declaring the duplicate dependency.
        capability_id: Id,
        /// Repeated dependency identity.
        dependency_id: Id,
    },
    /// A task declared the same capability slot more than once.
    DuplicateCapabilityRequirement {
        /// Task declaring the duplicate requirement.
        task_id: Id,
        /// Repeated capability slot identity.
        capability_id: Id,
    },
    /// A task prerequisite referenced no declared task.
    UnknownTask(Id),
    /// A capability requirement identity disagreed with its declaration.
    CapabilityIdentityMismatch {
        /// Logical capability slot whose identities disagree.
        capability_id: Id,
        /// Identity declared by the run definition.
        expected: DefinitionIdentity,
        /// Identity used by the requirement.
        actual: DefinitionIdentity,
    },
    /// Two tasks claimed the same logical external operation.
    DuplicateOperation(OperationId),
    /// The workflow graph authority rejected the materialized topology.
    Workflow(WorkflowGraphError),
    /// The capability graph authority rejected the materialized topology.
    CapabilityGraph(CapabilityGraphError),
    /// A stable declaration field was invalid.
    InvalidDefinition(String),
}

impl fmt::Display for DefinitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateTask(id) => write!(f, "duplicate task identity: {id}"),
            Self::DuplicateTaskConfiguration(id) => {
                write!(f, "duplicate task configuration: {id}")
            }
            Self::DuplicateCapability(id) => write!(f, "duplicate capability identity: {id}"),
            Self::DuplicateTaskDependency {
                task_id,
                dependency_id,
            } => write!(
                f,
                "task {task_id} declares duplicate dependency {dependency_id}"
            ),
            Self::DuplicateCapabilityDependency {
                capability_id,
                dependency_id,
            } => write!(
                f,
                "capability {capability_id} declares duplicate dependency {dependency_id}"
            ),
            Self::DuplicateCapabilityRequirement {
                task_id,
                capability_id,
            } => write!(
                f,
                "task {task_id} declares capability {capability_id} more than once"
            ),
            Self::UnknownTask(id) => write!(f, "unknown task in definition: {id}"),
            Self::CapabilityIdentityMismatch {
                capability_id,
                expected,
                actual,
            } => write!(
                f,
                "capability {capability_id} identity mismatch: declaration {expected}, requirement {actual}"
            ),
            Self::DuplicateOperation(operation_id) => {
                write!(f, "duplicate logical operation identity: {operation_id}")
            }
            Self::Workflow(error) => write!(f, "workflow definition error: {error}"),
            Self::CapabilityGraph(error) => write!(f, "capability definition error: {error}"),
            Self::InvalidDefinition(reason) => write!(f, "invalid run definition: {reason}"),
        }
    }
}

impl std::error::Error for DefinitionError {}

/// Typed failure while resolving a process-local capability factory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FactoryResolutionError {
    /// A factory was registered twice for one stable capability identity.
    DuplicateDefinitionIdentity {
        /// Capability slot claimed by both registrations.
        capability_id: Id,
        /// Stable identity claimed by both registrations.
        definition_identity: DefinitionIdentity,
    },
    /// A factory registration used an empty stable identity.
    InvalidDefinitionIdentity(String),
    /// The current process has no factory for a declared capability.
    MissingFactory {
        /// Capability slot that cannot be reconstructed.
        capability_id: Id,
        /// Stable identity requested by the definition.
        definition_identity: DefinitionIdentity,
    },
    /// A registered factory rejected construction of a fresh capability.
    ConstructionFailed {
        /// Capability slot whose factory failed.
        capability_id: Id,
        /// Stable identity being constructed.
        definition_identity: DefinitionIdentity,
        /// Factory-provided failure reason.
        reason: String,
    },
}

impl fmt::Display for FactoryResolutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateDefinitionIdentity {
                capability_id,
                definition_identity,
            } => write!(
                f,
                "factory already registered for capability {capability_id} definition {definition_identity}"
            ),
            Self::InvalidDefinitionIdentity(identity) => {
                write!(f, "factory definition identity is empty: {identity:?}")
            }
            Self::MissingFactory {
                capability_id,
                definition_identity,
            } => write!(
                f,
                "missing factory for capability {capability_id} definition {definition_identity}"
            ),
            Self::ConstructionFailed {
                capability_id,
                definition_identity,
                reason,
            } => write!(
                f,
                "factory for capability {capability_id} definition {definition_identity} failed: {reason}"
            ),
        }
    }
}

impl std::error::Error for FactoryResolutionError {}

type CapabilityFactory =
    Arc<dyn Fn(&ResolvedDependencies) -> Result<CapabilityValue, String> + Send + Sync>;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct FactoryKey {
    capability_id: Id,
    definition_identity: DefinitionIdentity,
}

/// Process-local mapping from stable capability identities to constructors.
///
/// The registry is deliberately not a durable authority and is not part of
/// [`RunDefinition`]. A fresh process creates a new registry and registers
/// constructors for the same stable identities before reconstruction.
#[derive(Clone, Default)]
pub struct FactoryRegistry {
    factories: BTreeMap<FactoryKey, CapabilityFactory>,
}

impl fmt::Debug for FactoryRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FactoryRegistry")
            .field("registered", &self.factories.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl FactoryRegistry {
    /// Creates an empty process-local factory registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers one constructor for a capability and stable definition.
    pub fn register<I, F>(
        &mut self,
        capability_id: Id,
        definition_identity: I,
        factory: F,
    ) -> Result<(), FactoryResolutionError>
    where
        I: Into<DefinitionIdentity>,
        F: Fn(&ResolvedDependencies) -> Result<CapabilityValue, String> + Send + Sync + 'static,
    {
        let definition_identity = definition_identity.into();
        if definition_identity.as_str().trim().is_empty() {
            return Err(FactoryResolutionError::InvalidDefinitionIdentity(
                definition_identity.as_str().to_owned(),
            ));
        }
        let key = FactoryKey {
            capability_id,
            definition_identity,
        };
        if self.factories.contains_key(&key) {
            return Err(FactoryResolutionError::DuplicateDefinitionIdentity {
                capability_id: key.capability_id,
                definition_identity: key.definition_identity,
            });
        }
        self.factories.insert(key, Arc::new(factory));
        Ok(())
    }

    /// Returns the number of registered stable factory identities.
    #[must_use]
    pub fn len(&self) -> usize {
        self.factories.len()
    }

    /// Returns whether no process-local factories are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.factories.is_empty()
    }

    /// Returns whether a factory is registered for one requirement.
    #[must_use]
    pub fn contains(&self, capability_id: &Id, definition_identity: &DefinitionIdentity) -> bool {
        self.factories.contains_key(&FactoryKey {
            capability_id: capability_id.clone(),
            definition_identity: definition_identity.clone(),
        })
    }

    pub(crate) fn validate(
        &self,
        definition: &ValidatedRunDefinition,
    ) -> Result<(), FactoryResolutionError> {
        for capability in definition.capabilities.values() {
            if !self.contains(&capability.id, &capability.definition_identity) {
                return Err(FactoryResolutionError::MissingFactory {
                    capability_id: capability.id.clone(),
                    definition_identity: capability.definition_identity.clone(),
                });
            }
        }
        Ok(())
    }

    pub(crate) fn factory_for(
        &self,
        capability_id: &Id,
        definition_identity: &DefinitionIdentity,
    ) -> Option<CapabilityFactory> {
        self.factories
            .get(&FactoryKey {
                capability_id: capability_id.clone(),
                definition_identity: definition_identity.clone(),
            })
            .cloned()
    }
}

/// Definition after duplicate checks and canonical identity validation.
pub(crate) struct ValidatedRunDefinition {
    pub(crate) tasks: BTreeMap<Id, TaskDefinition>,
    pub(crate) capabilities: BTreeMap<Id, CapabilityDeclaration>,
    pub(crate) identity: WorkflowReplayIdentity,
}

fn unique_tasks(
    task_definitions: &[TaskDefinition],
) -> Result<BTreeMap<Id, TaskDefinition>, DefinitionError> {
    let mut tasks = BTreeMap::new();
    for task in task_definitions {
        if tasks.insert(task.id.clone(), task.clone()).is_some() {
            return Err(DefinitionError::DuplicateTask(task.id.clone()));
        }
    }
    Ok(tasks)
}

fn unique_capabilities(
    declarations: &[CapabilityDeclaration],
) -> Result<BTreeMap<Id, CapabilityDeclaration>, DefinitionError> {
    let mut capabilities = BTreeMap::new();
    for capability in declarations {
        if capabilities
            .insert(capability.id.clone(), capability.clone())
            .is_some()
        {
            return Err(DefinitionError::DuplicateCapability(capability.id.clone()));
        }
    }
    Ok(capabilities)
}

fn validate_tasks(tasks: &BTreeMap<Id, TaskDefinition>) -> Result<(), DefinitionError> {
    let mut operations = BTreeSet::new();
    for task in tasks.values() {
        let mut dependencies = BTreeSet::new();
        for dependency_id in &task.dependencies {
            if !dependencies.insert(dependency_id.clone()) {
                return Err(DefinitionError::DuplicateTaskDependency {
                    task_id: task.id.clone(),
                    dependency_id: dependency_id.clone(),
                });
            }
            if !tasks.contains_key(dependency_id) {
                return Err(DefinitionError::UnknownTask(dependency_id.clone()));
            }
        }

        let mut requirements = BTreeSet::new();
        for requirement in &task.required_capabilities {
            validate_identity(&requirement.definition_identity)?;
            if !requirements.insert(requirement.capability_id.clone()) {
                return Err(DefinitionError::DuplicateCapabilityRequirement {
                    task_id: task.id.clone(),
                    capability_id: requirement.capability_id.clone(),
                });
            }
        }

        if let Some(effect) = &task.effect {
            if !operations.insert(effect.operation_id.clone()) {
                return Err(DefinitionError::DuplicateOperation(
                    effect.operation_id.clone(),
                ));
            }
        }
    }
    Ok(())
}

fn validate_capabilities(
    capabilities: &BTreeMap<Id, CapabilityDeclaration>,
) -> Result<(), DefinitionError> {
    for capability in capabilities.values() {
        validate_identity(&capability.definition_identity)?;
        let mut dependencies = BTreeSet::new();
        for requirement in &capability.dependencies {
            validate_identity(&requirement.definition_identity)?;
            if !dependencies.insert(requirement.capability_id.clone()) {
                return Err(DefinitionError::DuplicateCapabilityDependency {
                    capability_id: capability.id.clone(),
                    dependency_id: requirement.capability_id.clone(),
                });
            }
        }
    }
    Ok(())
}

fn validate_identity(identity: &DefinitionIdentity) -> Result<(), DefinitionError> {
    if identity.as_str().trim().is_empty() {
        return Err(DefinitionError::InvalidDefinition(
            "definition identity must not be empty".to_owned(),
        ));
    }
    Ok(())
}

fn materialize_implicit_capabilities(
    tasks: &BTreeMap<Id, TaskDefinition>,
    capabilities: &mut BTreeMap<Id, CapabilityDeclaration>,
) -> Result<(), DefinitionError> {
    let mut requirements = tasks
        .values()
        .flat_map(|task| task.required_capabilities.iter())
        .cloned()
        .collect::<Vec<_>>();
    requirements.extend(
        capabilities
            .values()
            .flat_map(|capability| capability.dependencies.iter())
            .cloned(),
    );

    for requirement in requirements {
        match capabilities.get(&requirement.capability_id) {
            Some(capability)
                if capability.definition_identity != requirement.definition_identity =>
            {
                return Err(DefinitionError::CapabilityIdentityMismatch {
                    capability_id: requirement.capability_id,
                    expected: capability.definition_identity.clone(),
                    actual: requirement.definition_identity,
                });
            }
            Some(_) => {}
            None => {
                capabilities.insert(
                    requirement.capability_id.clone(),
                    CapabilityDeclaration::implicit(&requirement),
                );
            }
        }
    }
    Ok(())
}

fn validate_workflow(tasks: &BTreeMap<Id, TaskDefinition>) -> Result<(), DefinitionError> {
    let mut mutations = Vec::with_capacity(
        tasks.len()
            + tasks
                .values()
                .map(|task| task.dependencies.len())
                .sum::<usize>(),
    );
    for task in tasks.values() {
        mutations.push(WorkflowMutation::AddTask {
            task: Task {
                id: task.id.clone(),
                label: task.label.clone(),
            },
        });
    }
    for task in tasks.values() {
        for dependency_id in &task.dependencies {
            mutations.push(WorkflowMutation::AddDependency {
                task_id: task.id.clone(),
                dependency_id: dependency_id.clone(),
            });
        }
    }
    if mutations.is_empty() {
        return Ok(());
    }
    let mut workflow = WorkflowGraph::default();
    workflow
        .apply_batch(workflow.revision(), mutations)
        .map(|_| ())
        .map_err(DefinitionError::Workflow)
}

fn validate_capability_graph(
    capabilities: &BTreeMap<Id, CapabilityDeclaration>,
) -> Result<(), DefinitionError> {
    let mut graph = CapabilityGraph::default();
    for capability in capabilities.values() {
        let mut definition =
            CapabilityDefinition::new(capability.id.clone(), capability.kind.clone())
                .with_replay_identity(capability.definition_identity.as_str().to_owned());
        for dependency in &capability.dependencies {
            definition.add_dependency(dependency.capability_id.clone());
        }
        graph.insert(definition);
    }
    graph
        .resolve()
        .map(|_| ())
        .map_err(DefinitionError::CapabilityGraph)
}

fn canonical_identity(
    tasks: &BTreeMap<Id, TaskDefinition>,
    capabilities: &BTreeMap<Id, CapabilityDeclaration>,
) -> Result<WorkflowReplayIdentity, DefinitionError> {
    let mut canonical = String::from(RUN_DEFINITION_FORMAT);

    append_identity_part(&mut canonical, "capabilities");
    for capability in capabilities.values() {
        append_identity_part(&mut canonical, "capability");
        append_identity_part(&mut canonical, capability.id.as_str());
        append_identity_part(&mut canonical, "kind");
        append_identity_part(&mut canonical, &capability.kind);
        append_identity_part(&mut canonical, "definition");
        append_identity_part(&mut canonical, capability.definition_identity.as_str());
        append_identity_part(&mut canonical, "dependencies");
        let mut dependencies = capability.dependencies.iter().collect::<Vec<_>>();
        dependencies.sort_by(|left, right| left.capability_id.cmp(&right.capability_id));
        for dependency in dependencies {
            append_identity_part(&mut canonical, dependency.capability_id.as_str());
            append_identity_part(&mut canonical, dependency.definition_identity.as_str());
        }
    }

    append_identity_part(&mut canonical, "tasks");
    for task in tasks.values() {
        append_identity_part(&mut canonical, "task");
        append_identity_part(&mut canonical, task.id.as_str());
        append_identity_part(&mut canonical, "dependencies");
        let mut dependencies = task.dependencies.iter().collect::<Vec<_>>();
        dependencies.sort();
        for dependency in dependencies {
            append_identity_part(&mut canonical, dependency.as_str());
        }

        append_identity_part(&mut canonical, "capabilities");
        let mut requirements = task.required_capabilities.iter().collect::<Vec<_>>();
        requirements.sort_by(|left, right| left.capability_id.cmp(&right.capability_id));
        for requirement in requirements {
            append_identity_part(&mut canonical, requirement.capability_id.as_str());
            append_identity_part(&mut canonical, requirement.definition_identity.as_str());
        }

        append_identity_part(&mut canonical, "effect");
        if let Some(effect) = &task.effect {
            append_identity_part(&mut canonical, "present");
            append_identity_part(&mut canonical, effect.operation_id.as_str());
            append_identity_part(
                &mut canonical,
                match effect.semantics {
                    EffectSemantics::Idempotent => "idempotent",
                    EffectSemantics::NonIdempotent => "non-idempotent",
                },
            );
        } else {
            append_identity_part(&mut canonical, "absent");
        }
    }

    WorkflowReplayIdentity::new(canonical)
        .map_err(|error| DefinitionError::InvalidDefinition(error.to_string()))
}

fn append_identity_part(canonical: &mut String, value: &str) {
    canonical.push(':');
    canonical.push_str(&value.len().to_string());
    canonical.push(':');
    canonical.push_str(value);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(value: &str) -> Id {
        Id::new(value).expect("test id is valid")
    }

    fn requirement(capability: &str, identity: &str) -> CapabilityRequirement {
        CapabilityRequirement::new(id(capability), identity)
    }

    fn task(value: &str) -> TaskDefinition {
        TaskDefinition::new(id(value), value)
    }

    #[test]
    fn identity_ignores_display_label_and_authoring_order() {
        let reordered = RunDefinition::new()
            .with_capability(CapabilityDeclaration::new(
                id("service"),
                "service",
                "service-v1",
            ))
            .with_task(
                TaskDefinition::new(id("source"), "renamed display label")
                    .require_capability(requirement("service", "service-v1")),
            );
        let original = RunDefinition::new()
            .with_task(
                TaskDefinition::new(id("source"), "original display label")
                    .require_capability(requirement("service", "service-v1")),
            )
            .with_capability(CapabilityDeclaration::new(
                id("service"),
                "service",
                "service-v1",
            ));
        assert_eq!(
            original.identity().expect("original validates"),
            reordered.identity().expect("reordered validates")
        );
    }

    #[test]
    fn duplicate_task_and_requirement_fail_before_materialization() {
        let duplicate_task = RunDefinition::new()
            .with_task(task("same"))
            .with_task(task("same"));
        assert_eq!(
            duplicate_task.validate(),
            Err(DefinitionError::DuplicateTask(id("same")))
        );

        let duplicate_requirement = RunDefinition::new().with_task(
            task("consumer")
                .require_capability(requirement("service", "service-v1"))
                .require_capability(requirement("service", "service-v1")),
        );
        assert_eq!(
            duplicate_requirement.validate(),
            Err(DefinitionError::DuplicateCapabilityRequirement {
                task_id: id("consumer"),
                capability_id: id("service"),
            })
        );
    }

    #[test]
    fn capability_factory_registration_rejects_duplicate_key() {
        let mut registry = FactoryRegistry::new();
        registry
            .register(id("service"), "service-v1", |_| {
                Ok(CapabilityValue::from_value(()))
            })
            .expect("first factory registers");
        assert!(matches!(
            registry.register(id("service"), "service-v1", |_| {
                Ok(CapabilityValue::from_value(()))
            }),
            Err(FactoryResolutionError::DuplicateDefinitionIdentity { .. })
        ));
    }
}

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
///
/// The v2 canonical representation is structurally unambiguous: every group and
/// enum field carries a structural tag, every collection carries an element
/// count and an explicit end tag, and every scalar value — including the scalars
/// that follow a group tag by position — is a length-delimited data token that
/// cannot imitate structure. `docs/architecture/0008-run-definition-identity-v2.md`
/// specifies the grammar, the field order, and the rules this encoding is tested
/// against.
pub const RUN_DEFINITION_FORMAT: &str = "kernis-run-definition-v2";

/// Historical format tag of the K2 `kernis-run-definition-v1` canonicalization.
///
/// This tag is retained for diagnosis and for the migration contract in
/// [`ADR 0008`](https://github.com/jesenzhang/kernis/blob/main/docs/architecture/0008-run-definition-identity-v2.md):
/// the kernel never writes a v1 identity, and a durable v1 declarative
/// identity is refused fail-closed because its bytes do not determine the
/// definition that produced them.
pub const LEGACY_RUN_DEFINITION_FORMAT_V1: &str = "kernis-run-definition-v1";

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

impl std::error::Error for DefinitionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Workflow(error) => Some(error),
            Self::CapabilityGraph(error) => Some(error),
            _ => None,
        }
    }
}

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

    push_tag(&mut canonical, "capabilities");
    push_count(&mut canonical, capabilities.len());
    for capability in capabilities.values() {
        push_tag(&mut canonical, "capability");
        push_value(&mut canonical, capability.id.as_str());
        push_value(&mut canonical, &capability.kind);
        push_value(&mut canonical, capability.definition_identity.as_str());
        push_requirements(&mut canonical, capability.dependencies.iter());
    }
    push_tag(&mut canonical, "end");

    push_tag(&mut canonical, "tasks");
    push_count(&mut canonical, tasks.len());
    for task in tasks.values() {
        push_tag(&mut canonical, "task");
        push_value(&mut canonical, task.id.as_str());

        let mut dependencies = task.dependencies.iter().collect::<Vec<_>>();
        dependencies.sort();
        push_tag(&mut canonical, "depends");
        push_count(&mut canonical, dependencies.len());
        for dependency in dependencies {
            push_value(&mut canonical, dependency.as_str());
        }
        push_tag(&mut canonical, "end");

        push_requirements(&mut canonical, task.required_capabilities.iter());

        push_tag(&mut canonical, "effect");
        if let Some(effect) = &task.effect {
            push_tag(&mut canonical, "present");
            push_value(&mut canonical, effect.operation_id.as_str());
            // The v2 tag is `non_idempotent`, unlike the frozen v1 bytes'
            // `non-idempotent`: v2 renames this tag on purpose, and the v1
            // spelling stays pinned by `legacy_v1_identity_bytes_stay_frozen`
            // so a future migration reproduces the historical bytes instead
            // of "aligning" one grammar to the other.
            push_tag(
                &mut canonical,
                match effect.semantics {
                    EffectSemantics::Idempotent => "idempotent",
                    EffectSemantics::NonIdempotent => "non_idempotent",
                },
            );
        } else {
            push_tag(&mut canonical, "absent");
        }
    }
    push_tag(&mut canonical, "end");

    WorkflowReplayIdentity::new(canonical)
        .map_err(|error| DefinitionError::InvalidDefinition(error.to_string()))
}

/// Appends one structural tag. Structural tokens always begin with `#` while
/// data tokens always begin with `%`, so user data can never imitate structure
/// or shift a field boundary. `%` is used rather than `@` because `@` is the
/// host-facing module-reference separator (`module@version`) and a durable
/// identity must stay lexically distinct from loader reference syntax.
fn push_tag(canonical: &mut String, tag: &str) {
    canonical.push('#');
    canonical.push_str(tag);
}

/// Appends the element count of one collection, terminated by `]`.
fn push_count(canonical: &mut String, count: usize) {
    canonical.push('[');
    canonical.push_str(&count.to_string());
    canonical.push(']');
}

/// Appends one length-delimited data token: `%<byte length>:<bytes>`.
fn push_value(canonical: &mut String, value: &str) {
    canonical.push('%');
    canonical.push_str(&value.len().to_string());
    canonical.push(':');
    canonical.push_str(value);
}

/// Appends one tagged, counted, identity-ordered capability requirement list.
fn push_requirements<'a, I>(canonical: &mut String, requirements: I)
where
    I: Iterator<Item = &'a CapabilityRequirement>,
{
    let mut requirements = requirements.collect::<Vec<_>>();
    requirements.sort_by(|left, right| left.capability_id.cmp(&right.capability_id));
    push_tag(canonical, "requires");
    push_count(canonical, requirements.len());
    for requirement in requirements {
        push_value(canonical, requirement.capability_id.as_str());
        push_value(canonical, requirement.definition_identity.as_str());
    }
    push_tag(canonical, "end");
}

/// The frozen `kernis-run-definition-v1` canonicalization, retained as the
/// historical interpretation recorded by ADR 0008.
///
/// Nothing in the kernel writes this format: the v1 grammar is not injective, so
/// a durable v1 declarative identity is refused fail-closed on restore. The
/// golden-byte tests below pin the exact v1 output, so a future authorized
/// migration implementation must reproduce the historical bytes instead of
/// re-deriving them.
#[cfg(test)]
fn legacy_canonical_identity_v1(
    tasks: &BTreeMap<Id, TaskDefinition>,
    capabilities: &BTreeMap<Id, CapabilityDeclaration>,
) -> String {
    let mut canonical = String::from(LEGACY_RUN_DEFINITION_FORMAT_V1);

    append_legacy_part(&mut canonical, "capabilities");
    for capability in capabilities.values() {
        append_legacy_part(&mut canonical, "capability");
        append_legacy_part(&mut canonical, capability.id.as_str());
        append_legacy_part(&mut canonical, "kind");
        append_legacy_part(&mut canonical, &capability.kind);
        append_legacy_part(&mut canonical, "definition");
        append_legacy_part(&mut canonical, capability.definition_identity.as_str());
        append_legacy_part(&mut canonical, "dependencies");
        let mut dependencies = capability.dependencies.iter().collect::<Vec<_>>();
        dependencies.sort_by(|left, right| left.capability_id.cmp(&right.capability_id));
        for dependency in dependencies {
            append_legacy_part(&mut canonical, dependency.capability_id.as_str());
            append_legacy_part(&mut canonical, dependency.definition_identity.as_str());
        }
    }

    append_legacy_part(&mut canonical, "tasks");
    for task in tasks.values() {
        append_legacy_part(&mut canonical, "task");
        append_legacy_part(&mut canonical, task.id.as_str());
        append_legacy_part(&mut canonical, "dependencies");
        let mut dependencies = task.dependencies.iter().collect::<Vec<_>>();
        dependencies.sort();
        for dependency in dependencies {
            append_legacy_part(&mut canonical, dependency.as_str());
        }

        append_legacy_part(&mut canonical, "capabilities");
        let mut requirements = task.required_capabilities.iter().collect::<Vec<_>>();
        requirements.sort_by(|left, right| left.capability_id.cmp(&right.capability_id));
        for requirement in requirements {
            append_legacy_part(&mut canonical, requirement.capability_id.as_str());
            append_legacy_part(&mut canonical, requirement.definition_identity.as_str());
        }

        append_legacy_part(&mut canonical, "effect");
        if let Some(effect) = &task.effect {
            append_legacy_part(&mut canonical, "present");
            append_legacy_part(&mut canonical, effect.operation_id.as_str());
            append_legacy_part(
                &mut canonical,
                match effect.semantics {
                    EffectSemantics::Idempotent => "idempotent",
                    EffectSemantics::NonIdempotent => "non-idempotent",
                },
            );
        } else {
            append_legacy_part(&mut canonical, "absent");
        }
    }

    canonical
}

#[cfg(test)]
fn append_legacy_part(canonical: &mut String, value: &str) {
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

    fn canonical_v2(definition: &RunDefinition) -> String {
        definition
            .validated()
            .expect("test definition validates")
            .identity
            .as_str()
            .to_owned()
    }

    fn canonical_v1(definition: &RunDefinition) -> String {
        let validated = definition.validated().expect("test definition validates");
        legacy_canonical_identity_v1(&validated.tasks, &validated.capabilities)
    }

    /// Golden bytes for the grammar recorded in
    /// `docs/architecture/0008-run-definition-identity-v2.md`. The expectations are
    /// written from that grammar by hand, so the canonicalizer is pinned to the
    /// specification instead of to whatever it happened to emit.
    #[test]
    fn identity_v2_matches_the_documented_grammar_bytes() {
        let minimal = RunDefinition::new()
            .with_capability(CapabilityDeclaration::new(id("store"), "service", "v1"))
            .with_task(TaskDefinition::new(id("load"), "display label"));
        assert_eq!(
            canonical_v2(&minimal),
            "kernis-run-definition-v2#capabilities[1]#capability%5:store%7:service%2:v1\
             #requires[0]#end#end#tasks[1]#task%4:load#depends[0]#end#requires[0]#end\
             #effect#absent#end"
        );

        let shaped = RunDefinition::new()
            .with_capability(
                CapabilityDeclaration::new(id("store"), "provider", "store-v1")
                    .depends_on(requirement("clock", "clock-v1")),
            )
            .with_task(task("read"))
            .with_task(
                TaskDefinition::new(id("write"), "w")
                    .depends_on(id("read"))
                    .require_capability(requirement("store", "store-v1"))
                    .with_effect(
                        OperationId::new("op-1").expect("test operation id is valid"),
                        EffectSemantics::NonIdempotent,
                    ),
            );
        assert_eq!(
            canonical_v2(&shaped),
            "kernis-run-definition-v2#capabilities[2]#capability%5:clock%10:capability\
             %8:clock-v1#requires[0]#end#capability%5:store%8:provider%8:store-v1\
             #requires[1]%5:clock%8:clock-v1#end#end#tasks[2]#task%4:read#depends[0]#end\
             #requires[0]#end#effect#absent#task%5:write#depends[1]%4:read#end#requires[1]\
             %5:store%8:store-v1#end#effect#present%4:op-1#non_idempotent#end"
        );
    }

    /// The historical `kernis-run-definition-v1` bytes stay frozen: any future
    /// authorized migration must reproduce them (ADR 0008 §2 and §4).
    #[test]
    fn legacy_v1_identity_bytes_stay_frozen() {
        let minimal = RunDefinition::new()
            .with_capability(CapabilityDeclaration::new(id("store"), "service", "v1"))
            .with_task(TaskDefinition::new(id("load"), "display label"));
        assert_eq!(
            canonical_v1(&minimal),
            "kernis-run-definition-v1:12:capabilities:10:capability:5:store:4:kind:7:service\
             :10:definition:2:v1:12:dependencies:5:tasks:4:task:4:load:12:dependencies\
             :12:capabilities:6:effect:6:absent"
        );

        let shaped = RunDefinition::new()
            .with_capability(
                CapabilityDeclaration::new(id("store"), "provider", "store-v1")
                    .depends_on(requirement("clock", "clock-v1")),
            )
            .with_task(task("read"))
            .with_task(
                TaskDefinition::new(id("write"), "w")
                    .depends_on(id("read"))
                    .require_capability(requirement("store", "store-v1"))
                    .with_effect(
                        OperationId::new("op-1").expect("test operation id is valid"),
                        EffectSemantics::NonIdempotent,
                    ),
            );
        assert_eq!(
            canonical_v1(&shaped),
            "kernis-run-definition-v1:12:capabilities:10:capability:5:clock:4:kind\
             :10:capability:10:definition:8:clock-v1:12:dependencies:10:capability:5:store\
             :4:kind:8:provider:10:definition:8:store-v1:12:dependencies:5:clock:8:clock-v1\
             :5:tasks:4:task:4:read:12:dependencies:12:capabilities:6:effect:6:absent:4:task\
             :5:write:12:dependencies:4:read:12:capabilities:5:store:8:store-v1:6:effect\
             :7:present:4:op-1:14:non-idempotent"
        );
    }

    /// The exact structural collisions the v1 format allowed (review R2-01) are
    /// separated by v2. The v1 equality is asserted on purpose: it is a documented
    /// historical fact a migration must reason about, not something to hide.
    #[test]
    fn identity_v2_separates_every_v1_structural_collision() {
        let base = RunDefinition::new()
            .with_capability(CapabilityDeclaration::new(
                id("x"),
                "service",
                "capabilities",
            ))
            .with_task(task("capabilities"))
            .with_task(task("x"));
        let target = |variant: u8| {
            let task = TaskDefinition::new(id("z"), "target");
            let task = match variant {
                0 => task.require_capability(requirement("x", "capabilities")),
                1 => task.depends_on(id("capabilities")).depends_on(id("x")),
                2 => task
                    .depends_on(id("capabilities"))
                    .require_capability(requirement("x", "capabilities")),
                3 => task
                    .depends_on(id("x"))
                    .require_capability(requirement("capabilities", "capabilities")),
                4 => task
                    .depends_on(id("capabilities"))
                    .depends_on(id("x"))
                    .require_capability(requirement("x", "capabilities")),
                _ => task.depends_on(id("capabilities")).depends_on(id("x")),
            };
            base.clone().with_task(task)
        };

        let mut v2 = Vec::new();
        for variant in 0..5 {
            let definition = target(variant);
            definition.validate().expect("variant validates");
            v2.push(canonical_v2(&definition));
        }
        for (left_index, left) in v2.iter().enumerate() {
            for right in v2.iter().skip(left_index + 1) {
                assert_ne!(left, right, "distinct semantics must not share an identity");
            }
        }

        // Documented v1 collision: prerequisites and capability requirements were
        // indistinguishable, so variant 0 and variant 1 hashed to the same bytes.
        assert_eq!(canonical_v1(&target(0)), canonical_v1(&target(1)));
        assert_ne!(v2[0], v2[1]);
    }

    /// Data is framed by **byte** length and can never imitate structure.
    ///
    /// The goldens pin multibyte lengths (a `chars()`-based length would emit
    /// `%2:任务`), and stripping every length-delimited token out of an
    /// adversarial value leaves one constant skeleton — the direct statement of
    /// "user data cannot move a field boundary", which value-by-value
    /// distinctness alone does not express.
    #[test]
    fn identity_v2_frames_data_by_byte_length_and_keeps_the_skeleton_constant() {
        let multibyte = RunDefinition::new()
            .with_capability(CapabilityDeclaration::new(id("store"), "任务", "🔐"))
            .with_task(TaskDefinition::new(id("load"), "display label"));
        assert_eq!(
            canonical_v2(&multibyte),
            "kernis-run-definition-v2#capabilities[1]#capability%5:store%6:任务%4:🔐\
             #requires[0]#end#end#tasks[1]#task%4:load#depends[0]#end#requires[0]#end\
             #effect#absent#end",
            "a data token declares its UTF-8 byte length, not its character count"
        );

        let wider = RunDefinition::new()
            .with_capability(CapabilityDeclaration::new(id("store"), "任务🔐", "v1"))
            .with_task(TaskDefinition::new(id("load"), "display label"));
        assert_eq!(
            canonical_v2(&wider),
            "kernis-run-definition-v2#capabilities[1]#capability%5:store%10:任务🔐%2:v1\
             #requires[0]#end#end#tasks[1]#task%4:load#depends[0]#end#requires[0]#end\
             #effect#absent#end",
            "任务 is six bytes and the lock emoji is four, so that kind token is ten long"
        );

        /// Removes every `%<byte length>:<bytes>` token, leaving the frame.
        fn skeleton(identity: &str) -> String {
            let mut rest = identity;
            let mut frame = String::new();
            while let Some(offset) = rest.find('%') {
                frame.push_str(&rest[..offset]);
                let header = &rest[offset + 1..];
                let colon = header
                    .find(':')
                    .expect("a data token carries a length header");
                let length: usize = header[..colon]
                    .parse()
                    .expect("a data token length is a decimal byte count");
                let value = &header[colon + 1..];
                assert!(
                    value.is_char_boundary(length),
                    "the declared {length} bytes do not land on a character boundary"
                );
                rest = &value[length..];
            }
            frame.push_str(rest);
            frame
        }

        let shaped = |value: &str| {
            RunDefinition::new()
                .with_capability(CapabilityDeclaration::new(id("capabilities"), value, value))
                .with_task(
                    TaskDefinition::new(id("tasks"), value)
                        .depends_on(id("effect"))
                        .require_capability(requirement("capabilities", value)),
                )
                .with_task(task("effect"))
        };
        let frame = skeleton(&canonical_v2(&shaped("plain")));
        assert!(!frame.contains('%'), "the frame itself carries no data");

        // The identifiers are framed too. A canonicalizer that concatenated an
        // id raw would leave its text in the frame, so renaming the capability
        // (the requirement follows it) must not move a structural boundary.
        let renamed = |capability: &str| {
            RunDefinition::new()
                .with_capability(CapabilityDeclaration::new(id(capability), "plain", "plain"))
                .with_task(
                    TaskDefinition::new(id("tasks"), "plain")
                        .depends_on(id("effect"))
                        .require_capability(requirement(capability, "plain")),
                )
                .with_task(task("effect"))
        };
        for capability in ["capabilities", "cap", "capabilities-long"] {
            assert_eq!(
                skeleton(&canonical_v2(&renamed(capability))),
                frame,
                "the capability id {capability:?} moved a structural boundary"
            );
        }

        for value in [
            "任务",
            "🔐",
            "#end",
            "%5:store",
            "#tasks[0]#end",
            "#requires[1]",
            "[0]",
            "]",
            "0:1:2",
            "#capability%0:#end",
        ] {
            let identity = canonical_v2(&shaped(value));
            assert!(
                identity.contains(&format!("%{}:{value}", value.len())),
                "{value:?} is framed by its byte length in {identity}"
            );
            assert_eq!(
                skeleton(&identity),
                frame,
                "{value:?} moved a structural boundary: {identity}"
            );
        }
    }

    /// Strings that imitate the v1 framing, the v2 framing, or a v2 count are
    /// ordinary data: they never move a field boundary.
    #[test]
    fn identity_v2_keeps_structural_tokens_and_user_data_disjoint() {
        let adversarial = [
            "capabilities",
            "dependencies",
            "tasks",
            "effect",
            "present",
            "absent",
            "12:capabilities",
            "0:1:2",
            "@0:",
            "@5:store",
            "%0:",
            "%5:store",
            "#end",
            "#requires[1]",
            "#capability",
            "requires[0]#end",
            ":",
            "#",
            "@",
            "%",
            "任务",
            "🔐",
            "0",
        ];

        let definition_for = |identity: &str| {
            RunDefinition::new()
                .with_capability(CapabilityDeclaration::new(
                    id("capabilities"),
                    identity,
                    identity,
                ))
                .with_task(
                    TaskDefinition::new(id("tasks"), identity)
                        .depends_on(id("effect"))
                        .require_capability(requirement("capabilities", identity)),
                )
                .with_task(task("effect"))
        };

        let mut seen = Vec::new();
        for identity in adversarial {
            let definition = definition_for(identity);
            definition
                .validate()
                .unwrap_or_else(|error| panic!("definition for {identity:?} validates: {error}"));
            let identity = canonical_v2(&definition);
            assert!(
                !seen.contains(&identity),
                "{identity:?} reused the identity of an earlier adversarial variant"
            );
            seen.push(identity);
        }

        // Display labels stay excluded from the identity.
        let mut labeled = definition_for("capabilities").tasks.clone();
        labeled[0].label = "a different display label".to_owned();
        let labeled = RunDefinition {
            tasks: labeled,
            ..definition_for("capabilities")
        };
        assert_eq!(
            canonical_v2(&labeled),
            canonical_v2(&definition_for("capabilities"))
        );
    }

    #[test]
    fn identity_v2_keeps_the_k2_equivalence_and_sensitivity_rules() {
        let authored = RunDefinition::new()
            .with_task(
                TaskDefinition::new(id("second"), "second")
                    .depends_on(id("first"))
                    .require_capability(requirement("clock", "clock-v1")),
            )
            .with_task(task("first"))
            .with_capability(CapabilityDeclaration::new(
                id("clock"),
                "service",
                "clock-v1",
            ));
        let reordered = RunDefinition::new()
            .with_capability(CapabilityDeclaration::new(
                id("clock"),
                "service",
                "clock-v1",
            ))
            .with_task(task("first"))
            .with_task(
                TaskDefinition::new(id("second"), "renamed")
                    .require_capability(requirement("clock", "clock-v1"))
                    .depends_on(id("first")),
            );
        assert_eq!(canonical_v2(&authored), canonical_v2(&reordered));

        let mut variants = vec![authored.clone()];
        variants.push({
            let mut definition = authored.clone();
            definition.tasks[0].dependencies.clear();
            definition
        });
        variants.push({
            let mut definition = authored.clone();
            definition.tasks[0].required_capabilities.clear();
            definition
        });
        variants.push({
            let mut definition = authored.clone();
            definition.capabilities[0].kind = "provider".to_owned();
            definition
        });
        variants.push({
            let mut definition = authored.clone();
            definition.tasks[0].effect = Some(crate::EffectSpec {
                operation_id: OperationId::new("op").expect("test operation id is valid"),
                semantics: EffectSemantics::Idempotent,
            });
            definition
        });
        variants.push({
            let mut definition = authored.clone();
            definition.tasks[0].effect = Some(crate::EffectSpec {
                operation_id: OperationId::new("op").expect("test operation id is valid"),
                semantics: EffectSemantics::NonIdempotent,
            });
            definition
        });

        let identities: Vec<String> = variants
            .iter()
            .map(|definition| {
                definition.validate().expect("variant validates");
                canonical_v2(definition)
            })
            .collect();
        for (left_index, left) in identities.iter().enumerate() {
            for right in identities.iter().skip(left_index + 1) {
                assert_ne!(left, right, "each semantic variant needs its own identity");
            }
        }
    }

    /// The parts of the grammar the sensitivity list above does not vary: a
    /// capability's own dependency edge, an empty collection, and an empty
    /// scalar field.
    #[test]
    fn identity_v2_is_sensitive_to_capability_edges_and_empty_fields() {
        let plain = RunDefinition::new()
            .with_capability(CapabilityDeclaration::new(
                id("clock"),
                "service",
                "clock-v1",
            ))
            .with_capability(CapabilityDeclaration::new(
                id("store"),
                "service",
                "store-v1",
            ))
            .with_task(task("read"));
        let with_edge = RunDefinition::new()
            .with_capability(
                CapabilityDeclaration::new(id("clock"), "service", "clock-v1")
                    .depends_on(requirement("store", "store-v1")),
            )
            .with_capability(CapabilityDeclaration::new(
                id("store"),
                "service",
                "store-v1",
            ))
            .with_task(task("read"));
        assert_ne!(
            canonical_v2(&plain),
            canonical_v2(&with_edge),
            "a capability dependency edge is admission-relevant, so it is identity-bearing"
        );

        let no_capabilities = RunDefinition::new().with_task(task("read"));
        let no_capabilities_identity = canonical_v2(&no_capabilities);
        assert!(
            no_capabilities_identity.contains("#capabilities[0]"),
            "an empty collection is framed as a zero count: {no_capabilities_identity}"
        );
        assert_ne!(
            no_capabilities_identity,
            canonical_v2(&plain),
            "no capability declarations is not the same run as some"
        );

        let empty_kind = RunDefinition::new()
            .with_capability(CapabilityDeclaration::new(id("store"), "", "store-v1"))
            .with_task(task("read"));
        let empty_kind_identity = canonical_v2(&empty_kind);
        assert!(
            empty_kind_identity.contains("%0:"),
            "an empty field is a zero-length token, never an omitted one: \
             {empty_kind_identity}"
        );
    }
}

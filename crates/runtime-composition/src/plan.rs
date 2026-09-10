//! Side-effect-free composition planning and deterministic activation.

use crate::assembly::{ActivatedModule, RuntimeAssembly, rollback_modules};
use crate::config::HostConfig;
use crate::definition::{CapabilityContribution, CapabilityOwnership};
use crate::error::{
    ActivationStage, CapabilityConflictReason, CompositionError, ConstructionStage,
    FactoryConflictReason, PluginConflictReason, RollbackReport, StartupFailure,
};
use crate::registration::{ModuleRegistration, PluginContribution};
use kernis_core::Id;
use runtime_core::{
    CapabilityDeclaration, DefinitionError, DefinitionIdentity, FactoryRegistry, RunDefinition,
    RunId, Runtime, RuntimeError,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use workflow_recovery::{DurableStore, InMemoryDurableStore};

/// Composition-visible owner record for one capability slot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapabilitySlot {
    /// Module that owns the slot.
    pub module_id: Id,
    /// Declared ownership plane.
    pub ownership: CapabilityOwnership,
    /// Stable definition identity of the slot.
    pub definition_identity: DefinitionIdentity,
    /// Declared capability kind.
    pub kind: String,
}

/// Host-facing composition builder.
///
/// [`Self::register`] accepts process-local
/// [`ModuleRegistration`]s and rejects duplicate module identity
/// immediately. [`Self::build`] performs all validation and deterministic
/// planning, acquires no resource, and returns a [`CompositionPlan`] that
/// can be activated later.
#[derive(Default)]
pub struct CompositionBuilder {
    modules: BTreeMap<Id, ModuleRegistration>,
}

impl CompositionBuilder {
    /// Creates an empty composition builder.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            modules: BTreeMap::new(),
        }
    }

    /// Registers one module, rejecting a duplicate module identity before
    /// any planning work happens.
    pub fn register(self, registration: ModuleRegistration) -> Result<Self, CompositionError> {
        let module_id = registration.definition().id.clone();
        let Self { mut modules } = self;
        if modules.contains_key(&module_id) {
            return Err(CompositionError::DuplicateModule { module_id });
        }
        modules.insert(module_id, registration);
        Ok(Self { modules })
    }

    /// Validates the complete module graph and contribution ownership, then
    /// produces the deterministic composition plan without side effects.
    pub fn build(self) -> Result<CompositionPlan, CompositionError> {
        let Self { modules } = self;
        validate_dependencies(&modules)?;
        let order = deterministic_order(&modules)?;
        let slots = validate_capability_slots(&modules)?;
        validate_task_contributions(&modules)?;
        validate_requirements(&modules, &slots)?;
        let factories = validate_factories(&modules, &slots)?;
        validate_plugins(&modules, &slots)?;
        validate_configuration(&modules)?;
        let definition = merge_definition(&modules, &order, &slots)?;
        Ok(CompositionPlan {
            modules,
            order,
            definition,
            factories,
            slots,
        })
    }
}

/// Validated, deterministic composition plan.
///
/// The plan merges the stable module contributions into one K2
/// [`RunDefinition`] and the process-local factories into one
/// [`FactoryRegistry`]. Activation constructs the Runtime through K2,
/// registers the composition-owned plugin runtimes, instantiates and starts
/// fibers in deterministic module order, runs the module `activate` hooks,
/// and reaches the stable reactive boundary.
pub struct CompositionPlan {
    modules: BTreeMap<Id, ModuleRegistration>,
    order: Vec<Id>,
    definition: RunDefinition,
    factories: FactoryRegistry,
    slots: BTreeMap<Id, CapabilitySlot>,
}

impl CompositionPlan {
    /// Returns the merged stable run definition.
    #[must_use]
    pub fn definition(&self) -> &RunDefinition {
        &self.definition
    }

    /// Returns the deterministic activation order.
    #[must_use]
    pub fn module_order(&self) -> &[Id] {
        &self.order
    }

    /// Returns the composition-visible capability slot table.
    #[must_use]
    pub fn slots(&self) -> &BTreeMap<Id, CapabilitySlot> {
        &self.slots
    }

    /// Starts a fresh run through K2 and activates the composition.
    pub async fn start(
        self,
        run_id: RunId,
        config: &HostConfig,
    ) -> Result<RuntimeAssembly<InMemoryDurableStore>, StartupFailure> {
        self.start_with_store(run_id, config, InMemoryDurableStore::new())
            .await
    }

    /// Starts a fresh run through K2 on a caller-provided durable store and
    /// activates the composition.
    pub async fn start_with_store<S>(
        self,
        run_id: RunId,
        config: &HostConfig,
        store: S,
    ) -> Result<RuntimeAssembly<S>, StartupFailure>
    where
        S: DurableStore,
    {
        self.activate(run_id, config, store, ConstructionStage::Start)
            .await
    }

    /// Restores an existing durable run through K2 and activates the
    /// composition with fresh process-local registrations.
    pub async fn restore<S>(
        self,
        run_id: RunId,
        config: &HostConfig,
        store: S,
    ) -> Result<RuntimeAssembly<S>, StartupFailure>
    where
        S: DurableStore,
    {
        self.activate(run_id, config, store, ConstructionStage::Restore)
            .await
    }

    async fn activate<S>(
        self,
        run_id: RunId,
        config: &HostConfig,
        store: S,
        stage: ConstructionStage,
    ) -> Result<RuntimeAssembly<S>, StartupFailure>
    where
        S: DurableStore,
    {
        for (module_id, registration) in &self.modules {
            for requirement in &registration.definition().config_requirements {
                if requirement.required && !config.contains(&requirement.key) {
                    return Err(StartupFailure {
                        cause: CompositionError::MissingConfiguration {
                            module_id: module_id.clone(),
                            key: requirement.key.clone(),
                        },
                        rollback: RollbackReport::default(),
                    });
                }
            }
        }

        let runtime = match stage {
            ConstructionStage::Start => Runtime::start_from_definition_with_store(
                run_id.clone(),
                self.definition.clone(),
                &self.factories,
                store,
            ),
            ConstructionStage::Restore => Runtime::restore_from_definition(
                run_id.clone(),
                self.definition.clone(),
                &self.factories,
                store,
            ),
        }
        .map_err(|source: RuntimeError| StartupFailure {
            cause: CompositionError::RuntimeConstructionFailed { stage, source },
            rollback: RollbackReport::default(),
        })?;

        let mut activated = Vec::new();
        if let Some(cause) = self.activate_modules(&runtime, &mut activated).await {
            let rollback = rollback_modules(&mut activated).await;
            drop(runtime);
            return Err(StartupFailure { cause, rollback });
        }

        Ok(RuntimeAssembly::new(
            runtime,
            self.order.clone(),
            activated,
            config.clone(),
        ))
    }

    async fn activate_modules<S>(
        &self,
        runtime: &Runtime<S>,
        activated: &mut Vec<ActivatedModule>,
    ) -> Option<CompositionError>
    where
        S: DurableStore,
    {
        for module_id in &self.order {
            let registration = &self.modules[module_id];
            activated.push(ActivatedModule::new(
                module_id.clone(),
                registration.on_dispose.clone(),
            ));
            let index = activated.len() - 1;

            for contribution in &registration.plugins {
                let plugin_id = contribution.plugin.id().clone();
                if let Err(source) = runtime
                    .capability_registry()
                    .register(Arc::clone(&contribution.plugin))
                {
                    return Some(CompositionError::ActivationFailed {
                        module_id: module_id.clone(),
                        stage: ActivationStage::PluginRegistration {
                            plugin_id,
                            reason: source.to_string(),
                        },
                    });
                }
                activated[index].plugins.push(plugin_id);
            }

            let mut fibers = Vec::with_capacity(registration.plugins.len());
            for contribution in &registration.plugins {
                let plugin_id = contribution.plugin.id().clone();
                match runtime
                    .capability_runtime()
                    .instantiate(&plugin_id, contribution.config.clone())
                {
                    Ok(fiber) => fibers.push((plugin_id, fiber)),
                    Err(source) => {
                        return Some(CompositionError::ActivationFailed {
                            module_id: module_id.clone(),
                            stage: ActivationStage::FiberInstantiate {
                                plugin_id,
                                reason: source.to_string(),
                            },
                        });
                    }
                }
            }

            for (plugin_id, fiber) in fibers {
                if let Err(source) = fiber.start().await {
                    return Some(CompositionError::ActivationFailed {
                        module_id: module_id.clone(),
                        stage: ActivationStage::FiberStart {
                            plugin_id: plugin_id.clone(),
                            reason: source.to_string(),
                        },
                    });
                }
                activated[index].fibers.push((plugin_id, fiber));
            }

            if let Some(hook) = &registration.on_activate {
                if let Err(reason) = hook().await {
                    return Some(CompositionError::ActivationFailed {
                        module_id: module_id.clone(),
                        stage: ActivationStage::ActivateHook { reason },
                    });
                }
                activated[index].hook_armed = true;
            }
        }

        let report = runtime.capability_runtime().reconcile().await;
        if !report.is_success() {
            let reason = report
                .errors
                .first()
                .map(|error| format!("{:?}", error.error))
                .or_else(|| {
                    report
                        .cleanup_errors
                        .first()
                        .map(|failure| format!("{:?}", failure.errors))
                })
                .unwrap_or_default();
            return Some(CompositionError::ReconciliationFailed { reason });
        }
        None
    }
}

fn validate_dependencies(
    modules: &BTreeMap<Id, ModuleRegistration>,
) -> Result<(), CompositionError> {
    for module_id in modules.keys() {
        for dependency_id in &modules[module_id].definition().dependencies {
            if !modules.contains_key(dependency_id) {
                return Err(CompositionError::MissingModuleDependency {
                    module_id: module_id.clone(),
                    dependency_id: dependency_id.clone(),
                });
            }
        }
    }
    Ok(())
}

/// Kahn topological order over the module dependency graph with the stable
/// module identity as the deterministic tie-breaker at every level.
fn deterministic_order(
    modules: &BTreeMap<Id, ModuleRegistration>,
) -> Result<Vec<Id>, CompositionError> {
    let dependencies = dependency_sets(modules);
    let mut dependents: BTreeMap<&Id, BTreeSet<Id>> = BTreeMap::new();
    let mut indegree: BTreeMap<&Id, usize> = BTreeMap::new();
    for module_id in modules.keys() {
        indegree.insert(module_id, 0);
    }
    for (module_id, deps) in &dependencies {
        for dependency_id in deps {
            *indegree.entry(module_id).or_insert(0) += 1;
            dependents
                .entry(dependency_id)
                .or_default()
                .insert((*module_id).clone());
        }
    }
    let mut ready: BTreeSet<Id> = indegree
        .iter()
        .filter(|(_, count)| **count == 0)
        .map(|(module_id, _)| (*module_id).clone())
        .collect();
    let mut order = Vec::with_capacity(modules.len());
    while let Some(module_id) = ready.iter().next().cloned() {
        ready.remove(&module_id);
        order.push(module_id.clone());
        if let Some(dependents_of_module) = dependents.get(&module_id) {
            for dependent in dependents_of_module {
                let counter = indegree
                    .get_mut(dependent)
                    .expect("dependents are registered modules");
                *counter -= 1;
                if *counter == 0 {
                    ready.insert(dependent.clone());
                }
            }
        }
    }
    if order.len() == modules.len() {
        return Ok(order);
    }
    let remaining: BTreeSet<Id> = modules
        .keys()
        .filter(|module_id| !order.contains(module_id))
        .cloned()
        .collect();
    let mut path: Vec<Id> = Vec::new();
    let mut positions: BTreeMap<Id, usize> = BTreeMap::new();
    let mut cursor = remaining
        .iter()
        .next()
        .expect("a non-empty cycle set always exists")
        .clone();
    loop {
        if let Some(position) = positions.get(&cursor) {
            let mut cycle = path[*position..].to_vec();
            cycle.push(cursor.clone());
            return Err(CompositionError::ModuleDependencyCycle { cycle });
        }
        positions.insert(cursor.clone(), path.len());
        path.push(cursor.clone());
        cursor = dependencies
            .get(&&cursor)
            .expect("cycle members are registered modules")
            .iter()
            .find(|dependency_id| remaining.contains(dependency_id))
            .expect("a cycle member always retains a remaining dependency")
            .clone();
    }
}

fn dependency_sets(modules: &BTreeMap<Id, ModuleRegistration>) -> BTreeMap<&Id, BTreeSet<Id>> {
    modules
        .iter()
        .map(|(module_id, registration)| {
            (
                module_id,
                registration
                    .definition()
                    .dependencies
                    .iter()
                    .cloned()
                    .collect(),
            )
        })
        .collect()
}

fn validate_task_contributions(
    modules: &BTreeMap<Id, ModuleRegistration>,
) -> Result<(), CompositionError> {
    let mut owners: BTreeMap<&Id, &Id> = BTreeMap::new();
    for (module_id, registration) in modules {
        for task in &registration.definition().tasks {
            if let Some(previous_module_id) = owners.insert(&task.id, module_id) {
                return Err(CompositionError::DuplicateTaskContribution {
                    task_id: task.id.clone(),
                    module_id: module_id.clone(),
                    previous_module_id: previous_module_id.clone(),
                });
            }
        }
    }
    Ok(())
}

fn validate_capability_slots(
    modules: &BTreeMap<Id, ModuleRegistration>,
) -> Result<BTreeMap<Id, CapabilitySlot>, CompositionError> {
    let mut slots: BTreeMap<Id, CapabilitySlot> = BTreeMap::new();
    for (module_id, registration) in modules {
        for contribution in &registration.definition().capabilities {
            let slot = CapabilitySlot {
                module_id: module_id.clone(),
                ownership: contribution.ownership(),
                definition_identity: contribution.definition_identity().clone(),
                kind: contribution.kind().to_owned(),
            };
            if let Some(previous) = slots.insert(contribution.capability_id().clone(), slot) {
                return Err(CompositionError::DuplicateCapabilityOwnership {
                    capability_id: contribution.capability_id().clone(),
                    module_id: module_id.clone(),
                    previous_module_id: previous.module_id,
                });
            }
        }
    }
    Ok(slots)
}

fn validate_requirements(
    modules: &BTreeMap<Id, ModuleRegistration>,
    slots: &BTreeMap<Id, CapabilitySlot>,
) -> Result<(), CompositionError> {
    for registration in modules.values() {
        for task in &registration.definition().tasks {
            for requirement in &task.required_capabilities {
                validate_requirement(
                    &requirement.capability_id,
                    &requirement.definition_identity,
                    slots,
                )?;
            }
        }
        for contribution in &registration.definition().capabilities {
            if let CapabilityContribution::Declarative(declaration) = contribution {
                for dependency in &declaration.dependencies {
                    validate_requirement(
                        &dependency.capability_id,
                        &dependency.definition_identity,
                        slots,
                    )?;
                }
            }
        }
    }
    Ok(())
}

fn validate_requirement(
    capability_id: &Id,
    required_identity: &DefinitionIdentity,
    slots: &BTreeMap<Id, CapabilitySlot>,
) -> Result<(), CompositionError> {
    let Some(slot) = slots.get(capability_id) else {
        return Err(CompositionError::CapabilityDefinitionConflict {
            capability_id: capability_id.clone(),
            reason: CapabilityConflictReason::UnownedRequirement,
        });
    };
    if slot.ownership == CapabilityOwnership::Reactive {
        return Err(CompositionError::CapabilityDefinitionConflict {
            capability_id: capability_id.clone(),
            reason: CapabilityConflictReason::RequiredOnReactiveSlot {
                module_id: slot.module_id.clone(),
            },
        });
    }
    if &slot.definition_identity != required_identity {
        return Err(CompositionError::CapabilityDefinitionConflict {
            capability_id: capability_id.clone(),
            reason: CapabilityConflictReason::IdentityMismatch {
                required: required_identity.clone(),
                declared: slot.definition_identity.clone(),
            },
        });
    }
    Ok(())
}

fn validate_factories(
    modules: &BTreeMap<Id, ModuleRegistration>,
    slots: &BTreeMap<Id, CapabilitySlot>,
) -> Result<FactoryRegistry, CompositionError> {
    let mut factories = FactoryRegistry::new();
    for (module_id, registration) in modules {
        for contribution in &registration.factories {
            let capability_id = &contribution.capability_id;
            let definition_identity = &contribution.definition_identity;
            if definition_identity.as_str().trim().is_empty() {
                return Err(CompositionError::FactoryConflict {
                    capability_id: capability_id.clone(),
                    definition_identity: definition_identity.clone(),
                    reason: FactoryConflictReason::InvalidDefinitionIdentity,
                });
            }
            match slots.get(capability_id) {
                None => {
                    return Err(CompositionError::FactoryConflict {
                        capability_id: capability_id.clone(),
                        definition_identity: definition_identity.clone(),
                        reason: FactoryConflictReason::UndeclaredSlot,
                    });
                }
                Some(slot) if slot.ownership == CapabilityOwnership::Reactive => {
                    return Err(CompositionError::FactoryConflict {
                        capability_id: capability_id.clone(),
                        definition_identity: definition_identity.clone(),
                        reason: FactoryConflictReason::FactoryOnReactiveSlot {
                            module_id: slot.module_id.clone(),
                        },
                    });
                }
                Some(slot) if &slot.definition_identity != definition_identity => {
                    return Err(CompositionError::FactoryConflict {
                        capability_id: capability_id.clone(),
                        definition_identity: definition_identity.clone(),
                        reason: FactoryConflictReason::IdentityMismatch {
                            declared: slot.definition_identity.clone(),
                        },
                    });
                }
                Some(_) => {}
            }
            let factory = Arc::clone(&contribution.factory);
            if factories
                .register(
                    capability_id.clone(),
                    definition_identity.clone(),
                    move |dependencies| factory(dependencies),
                )
                .is_err()
            {
                return Err(CompositionError::FactoryConflict {
                    capability_id: capability_id.clone(),
                    definition_identity: definition_identity.clone(),
                    reason: FactoryConflictReason::Duplicate {
                        module_id: module_id.clone(),
                        previous_module_id: first_factory_owner(
                            modules,
                            capability_id,
                            definition_identity,
                        ),
                    },
                });
            }
        }
    }
    for (capability_id, slot) in slots {
        if slot.ownership == CapabilityOwnership::Declarative
            && !factories.contains(capability_id, &slot.definition_identity)
        {
            return Err(CompositionError::FactoryConflict {
                capability_id: capability_id.clone(),
                definition_identity: slot.definition_identity.clone(),
                reason: FactoryConflictReason::Missing,
            });
        }
    }
    Ok(factories)
}

/// Returns the first module in stable Id order that contributes a factory
/// for this exact capability identity. Because the scan proceeds in module
/// Id order, this is the previous owner for a cross-module duplicate and the
/// module itself for a duplicate contributed twice by one module.
fn first_factory_owner(
    modules: &BTreeMap<Id, ModuleRegistration>,
    capability_id: &Id,
    definition_identity: &DefinitionIdentity,
) -> Id {
    modules
        .iter()
        .find(|(_, registration)| {
            registration.factories.iter().any(|contribution| {
                &contribution.capability_id == capability_id
                    && &contribution.definition_identity == definition_identity
            })
        })
        .map(|(module_id, _)| module_id.clone())
        .expect("a duplicate factory always has an owning module")
}

fn validate_plugins(
    modules: &BTreeMap<Id, ModuleRegistration>,
    slots: &BTreeMap<Id, CapabilitySlot>,
) -> Result<(), CompositionError> {
    let mut plugin_owners: BTreeSet<Id> = BTreeSet::new();
    let mut reactive_claims: BTreeMap<Id, Id> = BTreeMap::new();
    for (module_id, registration) in modules {
        for contribution in &registration.plugins {
            let PluginContribution { plugin, .. } = contribution;
            let plugin_id = plugin.id().clone();
            if !plugin_owners.insert(plugin_id.clone()) {
                return Err(CompositionError::CapabilityPluginConflict {
                    plugin_id: plugin_id.clone(),
                    reason: PluginConflictReason::DuplicateRegistration {
                        module_id: module_id.clone(),
                        previous_module_id: first_plugin_owner(modules, &plugin_id),
                    },
                });
            }
            let capability = plugin.definition().capability();
            let capability_id = &capability.id;
            for dependency in &capability.dependencies {
                if !slots.contains_key(&dependency.id) {
                    return Err(CompositionError::CapabilityDefinitionConflict {
                        capability_id: dependency.id.clone(),
                        reason: CapabilityConflictReason::UnownedRequirement,
                    });
                }
            }
            match slots.get(capability_id) {
                None => {
                    return Err(CompositionError::CapabilityPluginConflict {
                        plugin_id,
                        reason: PluginConflictReason::UnownedPublication,
                    });
                }
                Some(slot) if slot.ownership == CapabilityOwnership::Declarative => {
                    return Err(CompositionError::CapabilityPluginConflict {
                        plugin_id,
                        reason: PluginConflictReason::StaticSlotCollision {
                            module_id: slot.module_id.clone(),
                        },
                    });
                }
                Some(slot) => {
                    if let Some(previous_module_id) =
                        reactive_claims.insert(capability_id.clone(), module_id.clone())
                    {
                        return Err(CompositionError::CapabilityPluginConflict {
                            plugin_id,
                            reason: PluginConflictReason::SlotAlreadyClaimed {
                                module_id: module_id.clone(),
                                previous_module_id,
                            },
                        });
                    }
                    if capability.kind != slot.kind {
                        return Err(CompositionError::CapabilityPluginConflict {
                            plugin_id,
                            reason: PluginConflictReason::DefinitionMismatch {
                                detail: format!(
                                    "plugin kind {} differs from slot kind {}",
                                    capability.kind, slot.kind
                                ),
                            },
                        });
                    }
                    if capability.replay_identity != slot.definition_identity.as_str() {
                        return Err(CompositionError::CapabilityPluginConflict {
                            plugin_id,
                            reason: PluginConflictReason::DefinitionMismatch {
                                detail: format!(
                                    "plugin replay identity {} differs from slot identity {}",
                                    capability.replay_identity,
                                    slot.definition_identity.as_str()
                                ),
                            },
                        });
                    }
                }
            }
        }
    }
    for (capability_id, slot) in slots {
        if slot.ownership == CapabilityOwnership::Reactive
            && !reactive_claims.contains_key(capability_id)
        {
            return Err(CompositionError::MissingCapabilityPlugin {
                capability_id: capability_id.clone(),
                module_id: slot.module_id.clone(),
            });
        }
    }
    Ok(())
}

/// Returns the first module in stable Id order contributing this plugin
/// identity, which is the previous owner for a cross-module duplicate and
/// the module itself when one module contributes the plugin twice.
fn first_plugin_owner(modules: &BTreeMap<Id, ModuleRegistration>, plugin_id: &Id) -> Id {
    modules
        .iter()
        .find(|(_, registration)| {
            registration
                .plugins
                .iter()
                .any(|contribution| contribution.plugin.id() == plugin_id)
        })
        .map(|(module_id, _)| module_id.clone())
        .expect("a duplicate plugin always has an owning module")
}

fn validate_configuration(
    modules: &BTreeMap<Id, ModuleRegistration>,
) -> Result<(), CompositionError> {
    let mut requirements: BTreeMap<&Id, (&Id, bool)> = BTreeMap::new();
    for (module_id, registration) in modules {
        for requirement in &registration.definition().config_requirements {
            if let Some((previous_module_id, previous_required)) =
                requirements.insert(&requirement.key, (module_id, requirement.required))
            {
                if previous_required != requirement.required {
                    return Err(CompositionError::ConfigurationConflict {
                        key: requirement.key.clone(),
                        module_id: module_id.clone(),
                        previous_module_id: previous_module_id.clone(),
                    });
                }
            }
        }
    }
    Ok(())
}

fn merge_definition(
    modules: &BTreeMap<Id, ModuleRegistration>,
    order: &[Id],
    slots: &BTreeMap<Id, CapabilitySlot>,
) -> Result<RunDefinition, CompositionError> {
    let mut definition = RunDefinition::new();
    for module_id in order {
        for task in &modules[module_id].definition().tasks {
            definition.add_task(task.clone());
        }
    }
    let mut declarations: BTreeMap<&Id, CapabilityDeclaration> = BTreeMap::new();
    for registration in modules.values() {
        for contribution in &registration.definition().capabilities {
            if let CapabilityContribution::Declarative(declaration) = contribution {
                declarations.insert(&declaration.id, declaration.clone());
            }
        }
    }
    for capability_id in slots.keys() {
        if let Some(declaration) = declarations.get(capability_id) {
            definition.add_capability(declaration.clone());
        }
    }
    definition.validate().map_err(map_definition_error)?;
    Ok(definition)
}

fn map_definition_error(source: DefinitionError) -> CompositionError {
    CompositionError::InvalidMergedDefinition { source }
}

//! K4 acceptance scenario F: every contribution conflict fails before
//! activation with a typed error and zero side effects, plus configuration
//! conflict and missing-configuration behavior.

mod k4_common;

use k4_common::{Recorder, id, ok_plugin, run_id, value};
use runtime_composition::{
    CapabilityConflictReason, CapabilityDeclaration, CapabilityRequirement, CompositionBuilder,
    CompositionError, CompositionPlan, FactoryConflictReason, HostConfig, ModuleDefinition,
    ModuleRegistration, PluginConflictReason, TaskDefinition,
};

fn shared_slot() -> CapabilityDeclaration {
    CapabilityDeclaration::new(id("shared"), "provider", "shared-v1")
}

fn provider_module(factory: bool) -> ModuleRegistration {
    let registration = ModuleRegistration::new(
        ModuleDefinition::new(id("module-a")).with_declarative_capability(shared_slot()),
    );
    if factory {
        registration.factory(id("shared"), "shared-v1", |_| Ok(value("v")))
    } else {
        registration
    }
}

fn reactive_module() -> ModuleRegistration {
    ModuleRegistration::new(
        ModuleDefinition::new(id("module-b"))
            .depends_on(id("module-a"))
            .with_reactive_capability(id("reactive"), "service", "reactive-v1"),
    )
    .plugin(ok_plugin(
        "b-plugin",
        "reactive",
        "reactive-v1",
        "published",
    ))
}

fn task_module(task: TaskDefinition) -> ModuleRegistration {
    ModuleRegistration::new(ModuleDefinition::new(id("module-c")).with_task(task))
}

fn requires_shared(identity: &str) -> TaskDefinition {
    TaskDefinition::new(id("task"), "task")
        .require_capability(CapabilityRequirement::new(id("shared"), identity))
}

#[tokio::test(flavor = "current_thread")]
async fn duplicate_task_contribution_fails_before_activation() {
    let registry = Recorder::new();
    let error = CompositionBuilder::new()
        .register(provider_module(true))
        .expect("A registers")
        .register(
            ModuleRegistration::new(
                ModuleDefinition::new(id("module-b"))
                    .depends_on(id("module-a"))
                    .with_task(requires_shared("shared-v1")),
            )
            .on_activate(registry.hook("activate:module-b", None)),
        )
        .expect("B registers")
        .register(
            ModuleRegistration::new(
                ModuleDefinition::new(id("module-c"))
                    .depends_on(id("module-b"))
                    .with_task(requires_shared("shared-v1")),
            )
            .on_activate(registry.hook("activate:module-c", None)),
        )
        .expect("C registers")
        .build()
        .expect_err("one task identity may have only one owner");
    assert_eq!(
        error,
        CompositionError::DuplicateTaskContribution {
            task_id: id("task"),
            module_id: id("module-c"),
            previous_module_id: id("module-b"),
        }
    );
    assert!(registry.events().is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn duplicate_capability_ownership_fails_before_activation() {
    let registry = Recorder::new();
    let error = CompositionBuilder::new()
        .register(provider_module(true))
        .expect("A registers")
        .register(
            ModuleRegistration::new(
                ModuleDefinition::new(id("module-b"))
                    .depends_on(id("module-a"))
                    .with_declarative_capability(shared_slot()),
            )
            .factory(id("shared"), "shared-v1", |_| Ok(value("other")))
            .on_activate(registry.hook("activate:module-b", None)),
        )
        .expect("B registers")
        .build()
        .expect_err("one capability slot may have only one owner");
    assert_eq!(
        error,
        CompositionError::DuplicateCapabilityOwnership {
            capability_id: id("shared"),
            module_id: id("module-b"),
            previous_module_id: id("module-a"),
        }
    );
    assert!(registry.events().is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn conflicting_factory_registration_fails_before_activation() {
    let error = CompositionBuilder::new()
        .register(provider_module(true))
        .expect("A registers")
        .register(
            ModuleRegistration::new(
                ModuleDefinition::new(id("module-b")).depends_on(id("module-a")),
            )
            .factory(id("shared"), "shared-v1", |_| Ok(value("other"))),
        )
        .expect("B registers")
        .build()
        .expect_err("two factories for one slot identity conflict");
    assert_eq!(
        error,
        CompositionError::FactoryConflict {
            capability_id: id("shared"),
            definition_identity: "shared-v1".into(),
            reason: FactoryConflictReason::Duplicate {
                module_id: id("module-b"),
                previous_module_id: id("module-a"),
            },
        }
    );
}

#[tokio::test(flavor = "current_thread")]
async fn factory_must_match_its_slot() {
    // Factory on a reactively owned slot.
    let error = CompositionBuilder::new()
        .register(reactive_module())
        .expect("B registers")
        .register(
            ModuleRegistration::new(ModuleDefinition::new(id("module-a"))).factory(
                id("reactive"),
                "reactive-v1",
                |_| Ok(value("no")),
            ),
        )
        .expect("A registers")
        .build()
        .expect_err("reactive slots forbid factories");
    assert_eq!(
        error,
        CompositionError::FactoryConflict {
            capability_id: id("reactive"),
            definition_identity: "reactive-v1".into(),
            reason: FactoryConflictReason::FactoryOnReactiveSlot {
                module_id: id("module-b")
            },
        }
    );

    // Factory for an undeclared slot.
    let error = CompositionBuilder::new()
        .register(provider_module(true))
        .expect("A registers")
        .register(
            ModuleRegistration::new(ModuleDefinition::new(id("module-b"))).factory(
                id("ghost"),
                "ghost-v1",
                |_| Ok(value("no")),
            ),
        )
        .expect("B registers")
        .build()
        .expect_err("factories require declared slots");
    assert_eq!(
        error,
        CompositionError::FactoryConflict {
            capability_id: id("ghost"),
            definition_identity: "ghost-v1".into(),
            reason: FactoryConflictReason::UndeclaredSlot,
        }
    );

    // Factory definition identity differs from the slot.
    let error = CompositionBuilder::new()
        .register(provider_module(true))
        .expect("A registers")
        .register(
            ModuleRegistration::new(ModuleDefinition::new(id("module-b"))).factory(
                id("shared"),
                "shared-v9",
                |_| Ok(value("no")),
            ),
        )
        .expect("B registers")
        .build()
        .expect_err("factory identity must match the slot");
    assert_eq!(
        error,
        CompositionError::FactoryConflict {
            capability_id: id("shared"),
            definition_identity: "shared-v9".into(),
            reason: FactoryConflictReason::IdentityMismatch {
                declared: "shared-v1".into(),
            },
        }
    );

    // Declaratively owned slot without a factory.
    let error = CompositionBuilder::new()
        .register(provider_module(false))
        .expect("A registers")
        .build()
        .expect_err("declarative slots require factories");
    assert_eq!(
        error,
        CompositionError::FactoryConflict {
            capability_id: id("shared"),
            definition_identity: "shared-v1".into(),
            reason: FactoryConflictReason::Missing,
        }
    );
}

#[tokio::test(flavor = "current_thread")]
async fn requirements_must_resolve_to_one_declarative_slot() {
    let registry = Recorder::new();

    // Requirement for an unowned capability.
    let error = CompositionBuilder::new()
        .register(task_module(requires_shared("shared-v1")))
        .expect("C registers")
        .build()
        .expect_err("requirements need an owner");
    assert!(matches!(
        error,
        CompositionError::CapabilityDefinitionConflict {
            capability_id: ref capability,
            reason: CapabilityConflictReason::UnownedRequirement,
        } if capability == &id("shared")
    ));

    // Requirement for a reactively owned slot.
    let error = CompositionBuilder::new()
        .register(reactive_module())
        .expect("B registers")
        .register(provider_module(true))
        .expect("A registers")
        .register(task_module(
            TaskDefinition::new(id("task"), "task")
                .require_capability(CapabilityRequirement::new(id("reactive"), "reactive-v1")),
        ))
        .expect("C registers")
        .build()
        .expect_err("tasks cannot require reactive slots");
    assert!(matches!(
        error,
        CompositionError::CapabilityDefinitionConflict {
            capability_id: ref capability,
            reason: CapabilityConflictReason::RequiredOnReactiveSlot { ref module_id },
        } if capability == &id("reactive") && module_id == &id("module-b")
    ));

    // Requirement identity differs from the declared slot.
    let error = CompositionBuilder::new()
        .register(provider_module(true))
        .expect("A registers")
        .register(task_module(requires_shared("shared-v2")))
        .expect("C registers")
        .build()
        .expect_err("requirement identities must match");
    assert!(matches!(
        error,
        CompositionError::CapabilityDefinitionConflict {
            capability_id: ref capability,
            reason: CapabilityConflictReason::IdentityMismatch { .. },
        } if capability == &id("shared")
    ));
    assert!(registry.events().is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn plugin_contributions_must_match_the_slot_table() {
    // Plugin publishes an unowned capability.
    let error = CompositionBuilder::new()
        .register(
            ModuleRegistration::new(ModuleDefinition::new(id("module-b"))).plugin(ok_plugin(
                "b-plugin",
                "ghost",
                "ghost-v1",
                "published",
            )),
        )
        .expect("B registers")
        .build()
        .expect_err("plugin publications need declared slots");
    assert!(matches!(
        error,
        CompositionError::CapabilityPluginConflict {
            plugin_id: ref plugin,
            reason: PluginConflictReason::UnownedPublication,
        } if plugin == &id("b-plugin")
    ));

    // Plugin collides with a declaratively owned slot.
    let error = CompositionBuilder::new()
        .register(provider_module(true))
        .expect("A registers")
        .register(
            ModuleRegistration::new(ModuleDefinition::new(id("module-b"))).plugin(ok_plugin(
                "b-plugin",
                "shared",
                "shared-v1",
                "published",
            )),
        )
        .expect("B registers")
        .build()
        .expect_err("a plugin cannot collide with a declarative slot");
    assert!(matches!(
        error,
        CompositionError::CapabilityPluginConflict {
            reason: PluginConflictReason::StaticSlotCollision { ref module_id },
            ..
        } if module_id == &id("module-a")
    ));

    // Two plugins claim the same reactive slot.
    let error = CompositionBuilder::new()
        .register(reactive_module_defining(
            "reactive",
            "service",
            "reactive-v1",
        ))
        .expect("B registers")
        .register(
            ModuleRegistration::new(ModuleDefinition::new(id("module-c"))).plugin(ok_plugin(
                "c-plugin",
                "reactive",
                "reactive-v1",
                "other",
            )),
        )
        .expect("C registers")
        .build()
        .expect_err("one reactive slot has one owning plugin");
    assert!(matches!(
        error,
        CompositionError::CapabilityPluginConflict {
            reason: PluginConflictReason::SlotAlreadyClaimed { .. },
            ..
        }
    ));

    // A reactive slot without a plugin.
    let error = CompositionBuilder::new()
        .register(ModuleRegistration::new(
            ModuleDefinition::new(id("module-b")).with_reactive_capability(
                id("reactive"),
                "service",
                "reactive-v1",
            ),
        ))
        .expect("B registers")
        .build()
        .expect_err("reactive slots need an owning plugin");
    assert_eq!(
        error,
        CompositionError::MissingCapabilityPlugin {
            capability_id: id("reactive"),
            module_id: id("module-b"),
        }
    );

    // Plugin capability definition differs from the slot declaration.
    let error = CompositionBuilder::new()
        .register(reactive_module_defining(
            "reactive",
            "provider",
            "reactive-v1",
        ))
        .expect("B registers")
        .build()
        .expect_err("plugin capability definitions must match the slot");
    assert!(matches!(
        error,
        CompositionError::CapabilityPluginConflict {
            reason: PluginConflictReason::DefinitionMismatch { .. },
            ..
        }
    ));

    // A duplicate plugin identity across modules.
    let error = CompositionBuilder::new()
        .register(reactive_module_defining(
            "reactive",
            "service",
            "reactive-v1",
        ))
        .expect("B registers")
        .register(
            ModuleRegistration::new(ModuleDefinition::new(id("module-c"))).plugin(ok_plugin(
                "b-plugin",
                "reactive",
                "reactive-v1",
                "other",
            )),
        )
        .expect("C registers")
        .build()
        .expect_err("one plugin identity may register once");
    assert!(matches!(
        error,
        CompositionError::CapabilityPluginConflict {
            reason: PluginConflictReason::DuplicateRegistration { .. },
            ..
        }
    ));
}

fn reactive_module_defining(capability: &str, kind: &str, identity: &str) -> ModuleRegistration {
    ModuleRegistration::new(
        ModuleDefinition::new(id("module-b")).with_reactive_capability(
            id(capability),
            kind,
            identity,
        ),
    )
    .plugin(ok_plugin("b-plugin", capability, identity, "published"))
}

#[tokio::test(flavor = "current_thread")]
async fn plugin_dependency_on_an_unowned_capability_fails() {
    let error = CompositionBuilder::new()
        .register(
            ModuleRegistration::new(
                ModuleDefinition::new(id("module-b")).with_reactive_capability(
                    id("reactive"),
                    "service",
                    "reactive-v1",
                ),
            )
            .plugin(runtime_composition::PluginRuntime::new(
                runtime_composition::PluginDefinition::new(
                    id("b-plugin"),
                    k4_common::service_definition("reactive", "reactive-v1")
                        .depends_on(id("ghost")),
                    k4_common::plugin_factory(|_| async { Ok(value("x")) }),
                ),
            )),
        )
        .expect("B registers")
        .build()
        .expect_err("plugin dependencies need owners");
    assert!(matches!(
        error,
        CompositionError::CapabilityDefinitionConflict {
            capability_id: ref capability,
            reason: CapabilityConflictReason::UnownedRequirement,
        } if capability == &id("ghost")
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn configuration_conflict_fails_before_activation() {
    let error = CompositionBuilder::new()
        .register(ModuleRegistration::new(
            ModuleDefinition::new(id("module-a")).requiring_config(id("endpoint")),
        ))
        .expect("A registers")
        .register(ModuleRegistration::new(
            ModuleDefinition::new(id("module-b")).with_optional_config(id("endpoint")),
        ))
        .expect("B registers")
        .build()
        .expect_err("one key cannot be required and optional at once");
    assert_eq!(
        error,
        CompositionError::ConfigurationConflict {
            key: id("endpoint"),
            module_id: id("module-b"),
            previous_module_id: id("module-a"),
        }
    );
}

#[tokio::test(flavor = "current_thread")]
async fn missing_required_configuration_fails_activation_without_side_effects() {
    let registry = Recorder::new();
    let plan: CompositionPlan = CompositionBuilder::new()
        .register(
            ModuleRegistration::new(
                ModuleDefinition::new(id("module-a"))
                    .with_declarative_capability(shared_slot())
                    .requiring_config(id("endpoint")),
            )
            .factory(id("shared"), "shared-v1", |_| Ok(value("v")))
            .on_activate(registry.hook("activate:module-a", None)),
        )
        .expect("A registers")
        .build()
        .expect("composition validates");
    let failure = plan
        .start(run_id("k4-config"), &HostConfig::new())
        .await
        .expect_err("required configuration must be host-provided");
    assert_eq!(
        failure.cause,
        CompositionError::MissingConfiguration {
            module_id: id("module-a"),
            key: id("endpoint"),
        }
    );
    assert!(failure.rollback.is_success());
    assert!(registry.events().is_empty());

    // With the key provided, activation succeeds.
    let plan: CompositionPlan = CompositionBuilder::new()
        .register(
            ModuleRegistration::new(
                ModuleDefinition::new(id("module-a"))
                    .with_declarative_capability(shared_slot())
                    .requiring_config(id("endpoint")),
            )
            .factory(id("shared"), "shared-v1", |_| Ok(value("v")))
            .on_activate(registry.hook("activate:module-a", None))
            .on_dispose(registry.hook("dispose:module-a", None)),
        )
        .expect("A registers")
        .build()
        .expect("composition validates");
    let config = HostConfig::new().provide(id("endpoint"), "https://example.invalid");
    let assembly = plan
        .start(run_id("k4-config-ok"), &config)
        .await
        .expect("configuration unlocks activation");
    assert_eq!(
        assembly.host_config().get(&id("endpoint")),
        Some("https://example.invalid")
    );
    assembly
        .shutdown()
        .await
        .expect("shutdown releases the module");
    assert_eq!(registry.count("activate:module-a"), 1);
    assert_eq!(registry.count("dispose:module-a"), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn every_planning_failure_produces_zero_activation() {
    let registry = Recorder::new();
    let attempts: Vec<Result<CompositionPlan, CompositionError>> = vec![
        CompositionBuilder::new()
            .register(module_a_duplicate_task())
            .expect("registers")
            .build(),
        CompositionBuilder::new()
            .register(ModuleRegistration::new(
                ModuleDefinition::new(id("lonely")).depends_on(id("absent")),
            ))
            .expect("registers")
            .build(),
        CompositionBuilder::new()
            .register(ModuleRegistration::new(
                ModuleDefinition::new(id("self-cycle")).depends_on(id("self-cycle")),
            ))
            .expect("registers")
            .build(),
    ];
    for attempt in attempts {
        assert!(
            attempt.is_err(),
            "each crafted composition must fail planning"
        );
    }
    assert!(
        registry.events().is_empty(),
        "planning failures never activate anything"
    );
}

fn module_a_duplicate_task() -> ModuleRegistration {
    ModuleRegistration::new(
        ModuleDefinition::new(id("dup-a"))
            .with_task(requires_shared("shared-v1"))
            .with_task(requires_shared("shared-v1")),
    )
}

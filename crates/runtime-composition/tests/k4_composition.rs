//! K4 acceptance scenarios A-E: successful composition, registration-order
//! independence, duplicate module, missing dependency, and deterministic
//! cycles.

mod k4_common;

use k4_common::{Recorder, SuccessDispatcher, id, operation, plugin_factory, run_id, value};
use runtime_composition::{
    CapabilityDeclaration, CapabilityRequirement, CompositionBuilder, CompositionError,
    DriveResult, EffectSemantics, HostConfig, KnownEffectOutcome, ModuleDefinition,
    ModuleRegistration, PluginDefinition, PluginRuntime, RunDefinition, ShutdownStatus, StepResult,
    TaskDefinition,
};

fn shared_declaration() -> CapabilityDeclaration {
    CapabilityDeclaration::new(id("shared"), "provider", "shared-v1")
}

fn driven_task() -> TaskDefinition {
    TaskDefinition::new(id("task"), "task")
        .require_capability(CapabilityRequirement::new(id("shared"), "shared-v1"))
        .with_effect(operation("operation"), EffectSemantics::Idempotent)
}

fn b_plugin() -> std::sync::Arc<PluginRuntime> {
    PluginRuntime::new(PluginDefinition::new(
        id("b-plugin"),
        k4_common::service_definition("reactive", "reactive-v1").depends_on(id("shared")),
        plugin_factory(|context| async move {
            let shared = context
                .dependencies()
                .get(&id("shared"))
                .expect("shared dependency resolves for the reactive plugin");
            Ok(value(
                shared
                    .downcast_ref::<String>()
                    .expect("shared publishes a string"),
            ))
        }),
    ))
}

fn module_a(registry: &Recorder) -> ModuleRegistration {
    ModuleRegistration::new(
        ModuleDefinition::new(id("module-a")).with_declarative_capability(shared_declaration()),
    )
    .factory(id("shared"), "shared-v1", |_| Ok(value("shared-value")))
    .on_activate(registry.hook("activate:module-a", None))
    .on_dispose(registry.hook("dispose:module-a", None))
}

fn module_b(registry: &Recorder) -> ModuleRegistration {
    ModuleRegistration::new(
        ModuleDefinition::new(id("module-b"))
            .depends_on(id("module-a"))
            .with_reactive_capability(id("reactive"), "service", "reactive-v1"),
    )
    .plugin(b_plugin())
    .on_activate(registry.hook("activate:module-b", None))
    .on_dispose(registry.hook("dispose:module-b", None))
}

fn module_c() -> ModuleRegistration {
    ModuleRegistration::new(
        ModuleDefinition::new(id("module-c"))
            .depends_on(id("module-b"))
            .with_task(driven_task()),
    )
}

fn standard_plan(registry: &Recorder) -> runtime_composition::CompositionPlan {
    CompositionBuilder::new()
        .register(module_a(registry))
        .expect("module A registers")
        .register(module_b(registry))
        .expect("module B registers")
        .register(module_c())
        .expect("module C registers")
        .build()
        .expect("standard composition validates")
}

#[tokio::test(flavor = "current_thread")]
async fn scenario_a_composes_starts_drives_and_cleans_exactly_once() {
    let registry = Recorder::new();
    let plan = standard_plan(&registry);
    assert_eq!(
        plan.module_order(),
        [id("module-a"), id("module-b"), id("module-c")]
    );

    let assembly = plan
        .start(run_id("k4-a"), &HostConfig::new())
        .await
        .expect("composition activates through K2");
    assert_eq!(
        assembly.module_order(),
        [id("module-a"), id("module-b"), id("module-c")]
    );
    assert!(
        assembly.runtime().scope().get(&id("reactive")).is_some(),
        "the reactive plugin slot is published at the stable boundary"
    );
    assert!(
        assembly
            .runtime()
            .capability_registry()
            .contains(&id("b-plugin"))
    );

    let dispatcher = SuccessDispatcher::new();
    let (driver, handle, composition) = assembly.into_driver(dispatcher.clone());
    let join = tokio::spawn(driver.run());
    let first = handle.drive().await.expect("effect dispatch succeeds");
    assert!(matches!(
        first,
        DriveResult::EffectCompleted {
            outcome: KnownEffectOutcome::Succeeded,
            ..
        }
    ));
    assert_eq!(dispatcher.request_count(), 1);
    assert!(matches!(
        handle.drive().await.expect("attempt completion succeeds"),
        DriveResult::Step(StepResult::Completed { ref task_id, .. })
            if task_id == &id("task")
    ));
    assert_eq!(
        handle.shutdown().await.expect("driver shuts down cleanly"),
        ShutdownStatus::Clean
    );
    let exit = join.await.expect("driver task joins");

    let report = composition
        .dispose()
        .await
        .expect("composition releases exactly once");
    drop(exit);
    assert_eq!(
        report.cleaned,
        vec![id("module-c"), id("module-b"), id("module-a")]
    );
    assert!(report.is_success());
    assert_eq!(registry.count("activate:module-a"), 1);
    assert_eq!(registry.count("activate:module-b"), 1);
    assert_eq!(registry.count("dispose:module-a"), 1);
    assert_eq!(registry.count("dispose:module-b"), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn scenario_b_registration_order_changes_nothing() {
    let mut orders = Vec::new();
    let mut identities = Vec::new();
    let mut slot_tables = Vec::new();
    let mut activation_events = Vec::new();
    let mut cleaned_tables = Vec::new();

    for (index, permutation) in [[0usize, 1, 2], [2, 0, 1], [1, 2, 0]]
        .into_iter()
        .enumerate()
    {
        let registry = Recorder::new();
        let mut builder = CompositionBuilder::new();
        for slot in permutation {
            builder = match slot {
                0 => builder.register(module_a(&registry)).expect("A registers"),
                1 => builder.register(module_b(&registry)).expect("B registers"),
                _ => builder.register(module_c()).expect("C registers"),
            };
        }
        let plan = builder.build().expect("composition validates");
        orders.push(plan.module_order().to_vec());
        identities.push(
            plan.definition()
                .identity()
                .expect("merged definition has a stable identity"),
        );
        slot_tables.push(plan.slots().clone());

        let assembly = plan
            .start(run_id(&format!("k4-b-{index}")), &HostConfig::new())
            .await
            .expect("composition activates");
        assert!(assembly.runtime().scope().get(&id("reactive")).is_some());
        let report = assembly
            .shutdown()
            .await
            .expect("assembly shuts down cleanly");
        activation_events.push(registry.events());
        cleaned_tables.push(report.cleaned);
    }

    assert_eq!(
        orders[0],
        vec![id("module-a"), id("module-b"), id("module-c")]
    );
    assert_eq!(orders, vec![orders[0].clone(); 3]);
    assert_eq!(identities, vec![identities[0].clone(); 3]);
    assert_eq!(slot_tables, vec![slot_tables[0].clone(); 3]);
    assert_eq!(cleaned_tables, vec![cleaned_tables[0].clone(); 3]);
    let expected_activation = vec![
        "activate:module-a".to_owned(),
        "activate:module-b".to_owned(),
        "dispose:module-b".to_owned(),
        "dispose:module-a".to_owned(),
    ];
    assert_eq!(activation_events, vec![expected_activation; 3]);
}

#[tokio::test(flavor = "current_thread")]
async fn scenario_c_duplicate_module_fails_before_activation() {
    let registry = Recorder::new();
    let builder = CompositionBuilder::new()
        .register(module_a(&registry))
        .expect("module A registers once");
    let error = builder
        .register(module_a(&registry))
        .expect_err("the same module identity cannot register twice");
    assert_eq!(
        error,
        CompositionError::DuplicateModule {
            module_id: id("module-a")
        }
    );
    assert!(
        registry.events().is_empty(),
        "registration performs no activation"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn scenario_d_missing_dependency_fails_before_activation() {
    let registry = Recorder::new();
    let error = CompositionBuilder::new()
        .register(module_b(&registry))
        .expect("module B registers")
        .build()
        .expect_err("a dependency must exist");
    assert_eq!(
        error,
        CompositionError::MissingModuleDependency {
            module_id: id("module-b"),
            dependency_id: id("module-a"),
        }
    );
    assert!(
        registry.events().is_empty(),
        "planning performs no activation"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn scenario_e_cycle_fails_deterministically() {
    fn cycle_module(name: &str, dependency: &str) -> ModuleRegistration {
        ModuleRegistration::new(ModuleDefinition::new(id(name)).depends_on(id(dependency)))
    }
    fn cycle_error(order_low_to_high: bool) -> CompositionError {
        let builder = CompositionBuilder::new();
        let modules: [(&str, &str); 3] = [
            ("cycle-a", "cycle-b"),
            ("cycle-b", "cycle-c"),
            ("cycle-c", "cycle-a"),
        ];
        let builder = if order_low_to_high {
            modules
                .into_iter()
                .fold(builder, |builder, (name, dependency)| {
                    builder
                        .register(cycle_module(name, dependency))
                        .expect("cycle module registers")
                })
        } else {
            modules
                .into_iter()
                .rev()
                .fold(builder, |builder, (name, dependency)| {
                    builder
                        .register(cycle_module(name, dependency))
                        .expect("cycle module registers")
                })
        };
        builder.build().expect_err("a dependency cycle must fail")
    }

    let expected = CompositionError::ModuleDependencyCycle {
        cycle: vec![id("cycle-a"), id("cycle-b"), id("cycle-c"), id("cycle-a")],
    };
    let forward = cycle_error(true);
    let reverse = cycle_error(false);
    assert_eq!(forward, expected);
    assert_eq!(
        forward, reverse,
        "the reported cycle must not depend on registration order"
    );
}

#[test]
fn merged_definition_carries_only_declarative_contributions() {
    let registry = Recorder::new();
    let plan = standard_plan(&registry);
    let definition: &RunDefinition = plan.definition();
    assert!(definition.tasks().iter().any(|task| task.id == id("task")));
    assert!(
        definition
            .capabilities()
            .iter()
            .any(|capability| capability.id == id("shared"))
    );
    assert!(
        !definition
            .capabilities()
            .iter()
            .any(|capability| capability.id == id("reactive")),
        "a reactively owned slot contributes nothing to the durable definition"
    );
}

//! K4 acceptance scenarios H-J: K2 cold reconstruction compatibility, K3
//! driver compatibility (including owner-loss), and M2-C reactive
//! replacement exact pinning through a composed assembly.

mod k4_common;

use k4_common::{
    Recorder, SuccessDispatcher, TempStore, id, ok_plugin, operation, plugin_factory, run_id, value,
};
use runtime_composition::{
    CapabilityDeclaration, CapabilityRequirement, CompositionBuilder, DriveResult, DriverError,
    EffectSemantics, FileDurableStore, HostConfig, KnownEffectOutcome, ModuleDefinition,
    ModuleRegistration, PluginDefinition, PluginRuntime, StepResult,
};

fn provider_slot() -> CapabilityDeclaration {
    CapabilityDeclaration::new(id("shared"), "provider", "shared-v1")
}

fn provider_registration(registry: &Recorder) -> ModuleRegistration {
    ModuleRegistration::new(
        ModuleDefinition::new(id("module-a")).with_declarative_capability(provider_slot()),
    )
    .factory(id("shared"), "shared-v1", |_| Ok(value("factory-value")))
    .on_activate(registry.hook("activate:module-a", None))
    .on_dispose(registry.hook("dispose:module-a", None))
}

fn reactive_registration(registry: &Recorder) -> ModuleRegistration {
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
    .on_activate(registry.hook("activate:module-b", None))
    .on_dispose(registry.hook("dispose:module-b", None))
}

fn admission_plan(registry: &Recorder) -> runtime_composition::CompositionPlan {
    let task = TaskDefinition::new(id("task"), "task")
        .require_capability(CapabilityRequirement::new(id("shared"), "shared-v1"));
    CompositionBuilder::new()
        .register(provider_registration(registry))
        .expect("provider module registers")
        .register(reactive_registration(registry))
        .expect("reactive module registers")
        .register(ModuleRegistration::new(
            ModuleDefinition::new(id("module-task"))
                .depends_on(id("module-a"))
                .with_task(task),
        ))
        .expect("task module registers")
        .build()
        .expect("admission composition validates")
}

use runtime_composition::TaskDefinition;

#[tokio::test(flavor = "current_thread")]
async fn scenario_h_cold_reconstruction_survives_a_process_replacement() {
    let temp = TempStore::new("h");
    let run = run_id("k4-h");

    let recorder_a = Recorder::new();
    let plan_a = admission_plan(&recorder_a);
    let store = FileDurableStore::open(temp.path()).expect("durable store opens");
    let mut assembly = plan_a
        .start_with_store(run.clone(), &HostConfig::new(), store)
        .await
        .expect("composition starts on the durable store");
    assert!(matches!(
        assembly
            .runtime_mut()
            .step()
            .expect("attempt admits through the composed definition"),
        StepResult::Completed { ref task_id, .. } if task_id == &id("task")
    ));
    assembly
        .shutdown()
        .await
        .expect("process A releases the composition");
    assert_eq!(recorder_a.count("dispose:module-a"), 1);
    assert_eq!(recorder_a.count("dispose:module-b"), 1);

    let recorder_b = Recorder::new();
    let plan_b = admission_plan(&recorder_b);
    let reopened = FileDurableStore::open(temp.path()).expect("durable store reopens");
    let restored = plan_b
        .restore(run, &HostConfig::new(), reopened)
        .await
        .expect("K4 restore reconstructs the durable run with fresh registrations");
    assert!(
        restored
            .runtime()
            .capability_registry()
            .contains(&id("b-plugin")),
        "the restored process registers its own plugin runtimes"
    );
    assert!(restored.runtime().scope().get(&id("reactive")).is_some());
    let report = restored
        .shutdown()
        .await
        .expect("restored composition releases cleanly");
    assert_eq!(
        report.cleaned,
        vec![id("module-task"), id("module-b"), id("module-a")]
    );
    assert_eq!(recorder_b.count("dispose:module-a"), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn scenario_i_driver_drive_dispatch_shutdown_keeps_the_k3_contract() {
    let registry = Recorder::new();
    let task = TaskDefinition::new(id("task"), "task")
        .require_capability(CapabilityRequirement::new(id("shared"), "shared-v1"))
        .with_effect(operation("operation"), EffectSemantics::Idempotent);
    let plan = CompositionBuilder::new()
        .register(provider_registration(&registry))
        .expect("provider module registers")
        .register(reactive_registration(&registry))
        .expect("reactive module registers")
        .register(ModuleRegistration::new(
            ModuleDefinition::new(id("module-task"))
                .depends_on(id("module-a"))
                .with_task(task),
        ))
        .expect("task module registers")
        .build()
        .expect("driver composition validates");
    let assembly = plan
        .start(run_id("k4-i"), &HostConfig::new())
        .await
        .expect("composition activates");

    let dispatcher = SuccessDispatcher::new();
    let (driver, handle, composition) = assembly.into_driver(dispatcher.clone());
    let join = tokio::spawn(driver.run());
    assert!(matches!(
        handle.drive().await.expect("driver dispatch succeeds"),
        DriveResult::EffectCompleted {
            outcome: KnownEffectOutcome::Succeeded,
            ..
        }
    ));
    assert_eq!(dispatcher.request_count(), 1);
    assert!(matches!(
        handle.drive().await.expect("driver completion succeeds"),
        DriveResult::Step(StepResult::Completed { ref task_id, .. })
            if task_id == &id("task")
    ));
    assert_eq!(
        handle.shutdown().await.expect("clean shutdown"),
        runtime_composition::ShutdownStatus::Clean
    );
    let exit = join.await.expect("driver task joins");
    assert_eq!(exit.runtime().attempts().len(), 1);
    assert!(
        exit.runtime()
            .capability_registry()
            .contains(&id("b-plugin")),
        "the composition-owned plugin registration is still live while the \
         driver path owns the runtime"
    );
    assert_eq!(
        registry.count("dispose:"),
        0,
        "the waiting composition handle disposed nothing early; orderly \
         cleanup is only available through the driver exit"
    );

    let outcome = composition.dispose_after_driver(exit).await;
    assert_eq!(
        outcome.shutdown_status,
        runtime_composition::ShutdownStatus::Clean,
        "the K3 final shutdown classification is preserved"
    );
    let report = outcome.rollback;
    assert_eq!(
        report.cleaned,
        vec![id("module-task"), id("module-b"), id("module-a")]
    );
    assert!(report.is_success());
    assert_eq!(registry.count("dispose:module-a"), 1);
    assert_eq!(registry.count("dispose:module-b"), 1);
    assert!(
        !outcome
            .runtime
            .capability_registry()
            .contains(&id("b-plugin")),
        "orderly composition cleanup unregisters the composition-owned plugin"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn scenario_i_owner_loss_resolves_commands_and_keeps_release_independent() {
    let registry = Recorder::new();
    let plan = CompositionBuilder::new()
        .register(
            ModuleRegistration::new(ModuleDefinition::new(id("solo")))
                .on_activate(registry.hook("activate:solo", None))
                .on_dispose(registry.hook("dispose:solo", None)),
        )
        .expect("solo registers")
        .build()
        .expect("solo composition validates");
    let assembly = plan
        .start(run_id("k4-i-owner"), &HostConfig::new())
        .await
        .expect("composition activates");
    let (driver, handle, composition) = assembly.into_driver(SuccessDispatcher::new());
    drop(driver);
    assert!(matches!(
        handle.drive().await,
        Err(DriverError::OwnerDropped)
    ));
    assert!(matches!(
        handle.shutdown().await,
        Err(DriverError::OwnerDropped)
    ));

    let report = composition.release_after_owner_loss().await;
    assert_eq!(report.cleaned, vec![id("solo")]);
    assert!(report.is_success());
    assert_eq!(registry.count("dispose:solo"), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn scenario_j_reactive_replacement_pins_exact_entries_per_attempt() {
    let provider = id("provider");
    let provider_registration = ModuleRegistration::new(
        ModuleDefinition::new(id("provider-module")).with_declarative_capability(
            CapabilityDeclaration::new(provider.clone(), "provider", "provider-v1"),
        ),
    )
    .factory(provider.clone(), "provider-v1", |_| Ok(value("V1")));
    let task_module = ModuleRegistration::new(
        ModuleDefinition::new(id("consumer-module"))
            .with_task(
                TaskDefinition::new(id("attempt-a"), "attempt-a").require_capability(
                    CapabilityRequirement::new(provider.clone(), "provider-v1"),
                ),
            )
            .with_task(
                TaskDefinition::new(id("attempt-b"), "attempt-b")
                    .depends_on(id("attempt-a"))
                    .require_capability(CapabilityRequirement::new(
                        provider.clone(),
                        "provider-v1",
                    )),
            ),
    );
    let plan = CompositionBuilder::new()
        .register(provider_registration)
        .expect("provider registers")
        .register(task_module)
        .expect("consumers register")
        .build()
        .expect("pinning composition validates");
    let mut assembly = plan
        .start(run_id("k4-j"), &HostConfig::new())
        .await
        .expect("composition activates");

    assert!(matches!(
        assembly.runtime_mut().step().expect("attempt A admits"),
        StepResult::Completed { ref task_id, .. } if task_id == &id("attempt-a")
    ));
    let pin_a = assembly.runtime().attempts()[0]
        .capability(&provider)
        .expect("attempt A pins the provider")
        .clone();
    assert_eq!(
        pin_a.handle().downcast_ref::<String>(),
        Some(&"V1".to_owned())
    );

    let (_replacement, report) = assembly
        .runtime()
        .capability_runtime()
        .replace_and_reconcile(
            k4_common::provider_definition("provider", "provider-v1"),
            pin_a.generation,
            |_| Ok(value("V2")),
        )
        .await
        .expect("in-place provider replacement reaches the stable boundary");
    assert!(report.is_success());

    assert!(matches!(
        assembly.runtime_mut().step().expect("attempt B admits"),
        StepResult::Completed { ref task_id, .. } if task_id == &id("attempt-b")
    ));
    let pin_b = assembly.runtime().attempts()[1]
        .capability(&provider)
        .expect("attempt B pins the provider")
        .clone();
    assert_eq!(
        pin_b.handle().downcast_ref::<String>(),
        Some(&"V2".to_owned())
    );
    assert_ne!(pin_b.entry_id, pin_a.entry_id);
    assert_ne!(pin_b.generation, pin_a.generation);
    assert_eq!(pin_b.replay_identity.definition_identity(), "provider-v1");
    assert_eq!(
        pin_a.handle().downcast_ref::<String>(),
        Some(&"V1".to_owned()),
        "the old attempt keeps its exact pinned entry after replacement"
    );

    assembly
        .shutdown()
        .await
        .expect("composition releases after the replacement boundary");
}

#[tokio::test(flavor = "current_thread")]
async fn reactive_plugin_can_consume_a_declarative_slot() {
    let registry = Recorder::new();
    let consumer = ModuleRegistration::new(
        ModuleDefinition::new(id("module-b"))
            .depends_on(id("module-a"))
            .with_reactive_capability(id("derived"), "service", "derived-v1"),
    )
    .plugin(PluginRuntime::new(PluginDefinition::new(
        id("derived-plugin"),
        k4_common::service_definition("derived", "derived-v1").depends_on(id("shared")),
        plugin_factory(|context| async move {
            let shared = context
                .dependencies()
                .get(&id("shared"))
                .expect("the declarative slot resolves for the plugin");
            Ok(value(
                shared
                    .downcast_ref::<String>()
                    .expect("factory value is a string"),
            ))
        }),
    )))
    .on_activate(registry.hook("activate:module-b", None))
    .on_dispose(registry.hook("dispose:module-b", None));
    let plan = CompositionBuilder::new()
        .register(provider_registration(&registry))
        .expect("provider registers")
        .register(consumer)
        .expect("consumer registers")
        .build()
        .expect("cross-plane composition validates");
    let assembly = plan
        .start(run_id("k4-cross"), &HostConfig::new())
        .await
        .expect("cross-plane composition activates");
    let derived = assembly
        .runtime()
        .scope()
        .get(&id("derived"))
        .expect("the reactive plugin published its slot");
    assert_eq!(
        derived.downcast_ref::<String>(),
        Some(&"factory-value".to_owned()),
        "the reactive fiber observed the declaratively owned factory value"
    );
    assembly
        .shutdown()
        .await
        .expect("cross-plane release is clean");
}

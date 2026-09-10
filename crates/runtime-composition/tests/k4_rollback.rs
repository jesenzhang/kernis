//! K4 acceptance scenario G: partial startup rollback. The third module
//! fails during activation; cleanup must run in reverse activation order,
//! release each owned disposer exactly once, continue after one cleanup
//! failure, and never fake-dispose resources a module never acquired.

mod k4_common;

use k4_common::{Recorder, id, ok_plugin, run_id};
use runtime_composition::{
    ActivationStage, CleanupResource, CompositionBuilder, CompositionError, HostConfig,
    ModuleDefinition, ModuleRegistration, RollbackFailure,
};

#[tokio::test(flavor = "current_thread")]
async fn scenario_g_partial_startup_rolls_back_in_reverse_order() {
    let registry = Recorder::new();

    let module_a = ModuleRegistration::new(ModuleDefinition::new(id("module-a")))
        .on_activate(registry.hook("activate:module-a", None))
        .on_dispose(registry.hook("dispose:module-a", None));
    let module_b = ModuleRegistration::new(
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
    .on_dispose(registry.hook("dispose:module-b", Some("b cleanup failed")));
    let module_c =
        ModuleRegistration::new(ModuleDefinition::new(id("module-c")).depends_on(id("module-b")))
            .on_activate(registry.hook("activate:module-c", Some("c activation failed")))
            .on_dispose(registry.hook("dispose:module-c", None));
    let module_d = ModuleRegistration::new(
        ModuleDefinition::new(id("module-d"))
            .depends_on(id("module-c"))
            .with_reactive_capability(id("d-reactive"), "service", "d-reactive-v1"),
    )
    .plugin(ok_plugin(
        "d-plugin",
        "d-reactive",
        "d-reactive-v1",
        "published",
    ))
    .on_activate(registry.hook("activate:module-d", None))
    .on_dispose(registry.hook("dispose:module-d", None));

    let plan = CompositionBuilder::new()
        .register(module_a)
        .expect("A registers")
        .register(module_b)
        .expect("B registers")
        .register(module_c)
        .expect("C registers")
        .register(module_d)
        .expect("D registers")
        .build()
        .expect("composition validates");
    assert_eq!(
        plan.module_order(),
        [
            id("module-a"),
            id("module-b"),
            id("module-c"),
            id("module-d")
        ]
    );

    let failure = plan
        .start(run_id("k4-g"), &HostConfig::new())
        .await
        .expect_err("module C fails activation");

    assert_eq!(
        *failure.cause,
        CompositionError::ActivationFailed {
            module_id: id("module-c"),
            stage: ActivationStage::ActivateHook {
                reason: "c activation failed".to_owned(),
            },
        }
    );

    // The failing module's un-armed disposer and the never-activated module
    // were not fake-disposed; module B's owned fiber cleanup continued after
    // its own failing hook and module A was still released.
    assert_eq!(
        failure.rollback.failures,
        vec![RollbackFailure {
            module_id: id("module-b"),
            resource: CleanupResource::DisposeHook,
            reason: "b cleanup failed".to_owned(),
        }]
    );
    assert_eq!(
        failure.rollback.cleaned,
        vec![id("module-c"), id("module-a")]
    );
    assert!(!failure.rollback.is_success());

    assert_eq!(
        registry.events(),
        vec![
            "activate:module-a".to_owned(),
            "activate:module-b".to_owned(),
            "activate:module-c".to_owned(),
            "dispose:module-b".to_owned(),
            "dispose:module-a".to_owned(),
        ],
        "module C's activate hook ran once and failed; rollback then runs \
         B then A, once per armed disposer, and cleanup continues after the \
         failed B hook"
    );
    assert_eq!(registry.count("activate:module-c"), 1);
    assert_eq!(registry.count("dispose:module-c"), 0);
    assert_eq!(registry.count("activate:module-d"), 0);
    assert_eq!(registry.count("dispose:module-d"), 0);
}

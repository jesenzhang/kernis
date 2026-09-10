//! K4 composition cleanup ownership regressions.
//!
//! * A — driverless `RuntimeAssembly::shutdown` releases hooks, fibers, and
//!   plugin registrations with live registry authority.
//! * B — startup rollback after a partial activation explicitly unregisters
//!   already-registered plugins through the live registry instead of hiding
//!   the missing cleanup behind the runtime drop.
//! * D — the public driver-path API cannot fake orderly composition cleanup
//!   while the driver still owns the runtime.
//! * E — after `OwnerDropped` the composition performs best-effort local
//!   release and reports the lost registry authority instead of claiming
//!   orderly completion.
//!
//! Scenario C (orderly driver shutdown recovering registry authority
//! through `DriverExit`) is proven in `k4_compat::scenario_i` and
//! `k4_composition::scenario_a`.

mod k4_common;

use k4_common::{
    Recorder, SuccessDispatcher, id, plugin_factory, run_id, service_definition, value,
};
use runtime_composition::{
    ActivationStage, CleanupResource, CompositionBuilder, CompositionError, DriverError,
    HostConfig, LifecycleHook, ModuleDefinition, ModuleRegistration, PluginDefinition,
    PluginRuntime, RollbackFailure, ScopedEffect, lifecycle_hook,
};
use std::sync::Arc;
use std::sync::Mutex;

/// Strong-count observer installed as a module `dispose` hook. The hook
/// captures only a `Weak`, so observing the count never changes it.
#[derive(Clone, Default)]
struct StrongCounts {
    values: Arc<Mutex<Vec<usize>>>,
}

impl StrongCounts {
    fn pusher(&self, plugin: &Arc<PluginRuntime>) -> LifecycleHook {
        let values = Arc::clone(&self.values);
        let plugin = Arc::downgrade(plugin);
        lifecycle_hook(move || {
            let values = Arc::clone(&values);
            let plugin = plugin.clone();
            async move {
                values
                    .lock()
                    .expect("counts lock is healthy")
                    .push(plugin.strong_count());
                Ok(())
            }
        })
    }

    fn values(&self) -> Vec<usize> {
        self.values.lock().expect("counts lock is healthy").clone()
    }
}

/// One `dispose` hook that records the plugin's strong count and a recorder
/// marker in one call.
fn observing_hook(
    recorder: &Recorder,
    counts: &StrongCounts,
    plugin: &Arc<PluginRuntime>,
    marker: &str,
) -> LifecycleHook {
    let marker_hook = recorder.hook(marker, None);
    let count_hook = counts.pusher(plugin);
    lifecycle_hook(move || {
        let marker_hook = Arc::clone(&marker_hook);
        let count_hook = Arc::clone(&count_hook);
        async move {
            count_hook().await?;
            marker_hook().await
        }
    })
}

/// A plugin whose fiber registers one owned effect, so fiber disposal runs
/// exactly one observable disposer.
fn tracked_plugin(
    plugin: &str,
    capability: &str,
    identity: &str,
    events: &Arc<Mutex<Vec<String>>>,
    dispose_failure: Option<&str>,
) -> Arc<PluginRuntime> {
    let effect_events = Arc::clone(events);
    let dispose_failure = dispose_failure.map(str::to_owned);
    PluginRuntime::new(PluginDefinition::new(
        id(plugin),
        service_definition(capability, identity),
        plugin_factory(move |context| {
            let effect_events = Arc::clone(&effect_events);
            let dispose_failure = dispose_failure.clone();
            async move {
                context
                    .effect(ScopedEffect::sync("tracked-disposal", move || {
                        effect_events
                            .lock()
                            .expect("effect events lock is healthy")
                            .push("fiber-effect".to_owned());
                        match dispose_failure {
                            Some(reason) => Err(reason),
                            None => Ok(()),
                        }
                    }))
                    .expect("the fiber accepts the tracked cleanup effect");
                Ok(value("published"))
            }
        }),
    ))
}

fn event_count(events: &Arc<Mutex<Vec<String>>>, marker: &str) -> usize {
    events
        .lock()
        .expect("effect events lock is healthy")
        .iter()
        .filter(|event| event.contains(marker))
        .count()
}

/// The audit module activates first and cleans last, so its `dispose` hook
/// observes the plugin strong count after the plugin module's full cleanup
/// (hook → fibers → registration) while the runtime is still alive.
fn audit_registration(
    recorder: &Recorder,
    counts: &StrongCounts,
    plugin: &Arc<PluginRuntime>,
) -> ModuleRegistration {
    ModuleRegistration::new(ModuleDefinition::new(id("audit-module")))
        .on_activate(recorder.hook("activate:audit", None))
        .on_dispose(observing_hook(recorder, counts, plugin, "dispose:audit"))
}

fn plugin_registration(
    plugin: &Arc<PluginRuntime>,
    recorder: &Recorder,
    counts: &StrongCounts,
) -> ModuleRegistration {
    ModuleRegistration::new(
        ModuleDefinition::new(id("plugin-module"))
            .depends_on(id("audit-module"))
            .with_reactive_capability(id("reactive"), "service", "reactive-v1"),
    )
    .plugin(Arc::clone(plugin))
    .on_activate(recorder.hook("activate:plugin", None))
    .on_dispose(observing_hook(recorder, counts, plugin, "dispose:plugin"))
}

/// Scenario A: driverless normal shutdown. Hooks, fibers, and the plugin
/// registration are released exactly once, and the registration is gone
/// before the runtime is dropped — the strong-count delta is observed from
/// inside the sweep, not inferred from the runtime drop.
#[tokio::test(flavor = "current_thread")]
async fn driverless_shutdown_unregisters_plugin_registrations_with_live_authority() {
    let recorder = Recorder::new();
    let events = Arc::new(Mutex::new(Vec::new()));
    let counts = StrongCounts::default();
    let plugin = tracked_plugin("owned-plugin", "reactive", "reactive-v1", &events, None);

    let plan = CompositionBuilder::new()
        .register(audit_registration(&recorder, &counts, &plugin))
        .expect("audit module registers")
        .register(plugin_registration(&plugin, &recorder, &counts))
        .expect("plugin module registers")
        .build()
        .expect("composition validates");
    let assembly = plan
        .start(run_id("k4-cleanup-a"), &HostConfig::new())
        .await
        .expect("composition activates");
    assert!(
        assembly
            .runtime()
            .capability_registry()
            .contains(&id("owned-plugin")),
        "activation registered the composition-owned plugin"
    );

    let report = assembly
        .shutdown()
        .await
        .expect("driverless shutdown reports success");
    assert!(report.is_success());
    assert_eq!(
        report.cleaned,
        vec![id("plugin-module"), id("audit-module")],
        "cleanup ran in reverse activation order"
    );

    assert_eq!(recorder.count("dispose:audit"), 1);
    assert_eq!(recorder.count("dispose:plugin"), 1);
    assert_eq!(
        event_count(&events, "fiber-effect"),
        1,
        "the started fiber disposed exactly once"
    );
    assert_eq!(plugin.fiber_count(), 0);

    // The plugin hook ran before the registration step and the audit hook
    // ran after it, both while the runtime was still alive. Exactly one
    // strong reference disappeared between them: the registry entry.
    let observed = counts.values();
    assert_eq!(observed.len(), 2, "both observer hooks ran exactly once");
    assert_eq!(
        observed[0],
        observed[1] + 1,
        "the sweep unregistered the plugin through the live registry, not \
         by dropping the runtime"
    );
}

/// Scenario B: startup fails after a plugin was registered. The rollback
/// holds the live registry, unregisters the plugin even though the module's
/// own fiber disposal failed, continues with the remaining modules, and
/// reports every failure without hiding anything behind the runtime drop.
#[tokio::test(flavor = "current_thread")]
async fn startup_rollback_unregisters_registrations_after_a_fiber_cleanup_failure() {
    let recorder = Recorder::new();
    let events = Arc::new(Mutex::new(Vec::new()));
    let counts = StrongCounts::default();
    let plugin = tracked_plugin(
        "owned-plugin",
        "reactive",
        "reactive-v1",
        &events,
        Some("effect disposal failed"),
    );

    let failing_module = ModuleRegistration::new(
        ModuleDefinition::new(id("bad-module")).depends_on(id("plugin-module")),
    )
    .on_activate(recorder.hook("activate:bad", Some("bad activation")));

    let plan = CompositionBuilder::new()
        .register(audit_registration(&recorder, &counts, &plugin))
        .expect("audit module registers")
        .register(plugin_registration(&plugin, &recorder, &counts))
        .expect("plugin module registers")
        .register(failing_module)
        .expect("failing module registers")
        .build()
        .expect("composition validates");
    let failure = plan
        .start(run_id("k4-cleanup-b"), &HostConfig::new())
        .await
        .expect_err("the third module fails activation");

    assert_eq!(
        *failure.cause,
        CompositionError::ActivationFailed {
            module_id: id("bad-module"),
            stage: ActivationStage::ActivateHook {
                reason: "bad activation".to_owned(),
            },
        }
    );

    // The module's fiber disposal failed, yet its plugin registration was
    // still unregistered and the remaining modules were cleaned: no
    // PluginRegistration failure was collected, and cleanup continued.
    assert_eq!(
        failure.rollback.failures,
        vec![RollbackFailure {
            module_id: id("plugin-module"),
            resource: CleanupResource::Fiber {
                plugin_id: id("owned-plugin"),
            },
            reason: "fiber cleanup failed with 1 error(s)".to_owned(),
        }],
        "the only failure is the failed fiber disposal; the registration \
         step succeeded through the live registry"
    );
    assert_eq!(
        failure.rollback.cleaned,
        vec![id("bad-module"), id("audit-module")]
    );
    assert!(!failure.rollback.is_success());

    assert_eq!(
        recorder.events(),
        vec![
            "activate:audit".to_owned(),
            "activate:plugin".to_owned(),
            "activate:bad".to_owned(),
            "dispose:plugin".to_owned(),
            "dispose:audit".to_owned(),
        ],
        "previously activated modules roll back in reverse order and the \
         never-armed failing module is not fake-disposed"
    );
    assert_eq!(
        event_count(&events, "fiber-effect"),
        1,
        "the failed disposer ran once and cleanup did not retry it"
    );
    assert_eq!(plugin.fiber_count(), 0);

    // The strong-count delta was observed strictly inside the rollback,
    // before the runtime drop: the audit hook ran while the runtime and its
    // registry were still alive.
    let observed = counts.values();
    assert_eq!(observed.len(), 2, "both observer hooks ran exactly once");
    assert_eq!(
        observed[0],
        observed[1] + 1,
        "rollback unregistered the already-registered plugin through the \
         live registry authority"
    );
}

/// Scenario D: while the driver owns the runtime, the composition handle
/// exposes no release path at all. Orderly completion literally requires
/// the `DriverExit`, which only exists after K3 shutdown released the
/// runtime, so "driver still owns Runtime + composition independently
/// disposed" is not expressible through the public API.
#[tokio::test(flavor = "current_thread")]
async fn no_composition_release_is_possible_while_the_driver_owns_the_runtime() {
    let recorder = Recorder::new();
    let events = Arc::new(Mutex::new(Vec::new()));
    let counts = StrongCounts::default();
    let plugin = tracked_plugin("owned-plugin", "reactive", "reactive-v1", &events, None);

    let plan = CompositionBuilder::new()
        .register(audit_registration(&recorder, &counts, &plugin))
        .expect("audit module registers")
        .register(plugin_registration(&plugin, &recorder, &counts))
        .expect("plugin module registers")
        .build()
        .expect("composition validates");
    let assembly = plan
        .start(run_id("k4-cleanup-d"), &HostConfig::new())
        .await
        .expect("composition activates");

    let (driver, handle, composition) = assembly.into_driver(SuccessDispatcher::new());
    let join = tokio::spawn(driver.run());

    // The driver still owns and services the runtime; the composition only
    // offers read access at this point and disposed nothing.
    handle
        .drive()
        .await
        .expect("the driver still services commands while the composition handle waits");
    assert_eq!(
        composition.module_order(),
        [id("audit-module"), id("plugin-module")],
        "the only handle capability while the driver runs is read access"
    );
    assert_eq!(recorder.count("dispose:"), 0);
    assert_eq!(event_count(&events, "fiber-effect"), 0);

    // Orderly shutdown: only the returned DriverExit unlocks composition
    // cleanup, and the registration is verifiably still live until then.
    assert_eq!(
        handle.shutdown().await.expect("driver shuts down cleanly"),
        runtime_composition::ShutdownStatus::Clean
    );
    let exit = join.await.expect("driver task joins");
    assert!(
        exit.runtime()
            .capability_registry()
            .contains(&id("owned-plugin")),
        "the waiting composition could not have unregistered anything"
    );

    let outcome = composition.dispose_after_driver(exit).await;
    assert!(outcome.rollback.is_success());
    assert!(
        !outcome
            .runtime
            .capability_registry()
            .contains(&id("owned-plugin"))
    );
    assert_eq!(recorder.count("dispose:audit"), 1);
    assert_eq!(recorder.count("dispose:plugin"), 1);
    assert_eq!(event_count(&events, "fiber-effect"), 1);
    let observed = counts.values();
    assert_eq!(observed.len(), 2);
    assert_eq!(observed[0], observed[1] + 1);
}

/// Scenario E: after the driver owner is lost, the Runtime dropped with the
/// registry. `release_after_owner_loss` still releases the composition's
/// process-local handles exactly once, and reports the lost registry
/// authority as a structured `PluginRegistration` failure instead of
/// claiming orderly composition cleanup.
#[tokio::test(flavor = "current_thread")]
async fn owner_loss_release_disposes_locally_once_and_reports_lost_authority() {
    let recorder = Recorder::new();
    let events = Arc::new(Mutex::new(Vec::new()));
    let plugin = tracked_plugin("owned-plugin", "reactive", "reactive-v1", &events, None);

    let plan = CompositionBuilder::new()
        .register(
            ModuleRegistration::new(
                ModuleDefinition::new(id("solo-module")).with_reactive_capability(
                    id("reactive"),
                    "service",
                    "reactive-v1",
                ),
            )
            .plugin(Arc::clone(&plugin))
            .on_activate(recorder.hook("activate:solo", None))
            .on_dispose(recorder.hook("dispose:solo", None)),
        )
        .expect("solo module registers")
        .build()
        .expect("composition validates");
    let assembly = plan
        .start(run_id("k4-cleanup-e"), &HostConfig::new())
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
    assert!(
        !report.is_success(),
        "the owner-loss path must never claim orderly completion"
    );
    assert!(report.cleaned.is_empty());
    assert_eq!(
        report.failures,
        vec![RollbackFailure {
            module_id: id("solo-module"),
            resource: CleanupResource::PluginRegistration {
                plugin_id: id("owned-plugin"),
            },
            reason: "the capability registry authority was released with the \
                     runtime owner, so the registration could not be unregistered"
                .to_owned(),
        }],
        "every outstanding composition-owned registration is reported"
    );

    // The process-local handles the composition still holds are released
    // best-effort exactly once, with no double dispose.
    assert_eq!(recorder.count("dispose:solo"), 1);
    assert_eq!(
        event_count(&events, "fiber-effect"),
        1,
        "the fiber disposed exactly once after the owner loss"
    );
    assert_eq!(plugin.fiber_count(), 0);
}

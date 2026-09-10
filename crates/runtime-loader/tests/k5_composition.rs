//! K5 acceptance scenarios L and N-P: resolution always constructs fresh
//! process-local registrations (the catalog is not a runtime cache), and the
//! loader boundary preserves K4 rollback semantics, K2 cold reconstruction
//! through a durable store, and the K3 driver contract. A loader-resolved
//! composition behaves exactly like a directly composed one on every
//! downstream authority.

mod k5_common;

use k5_common::{
    Recorder, SuccessDispatcher, TempStore, id, ok_factory, ok_plugin, operation, reference,
    run_id, value,
};
use runtime_loader::{
    ActivationStage, CapabilityDeclaration, CapabilityRequirement, CatalogEntry, CompositionError,
    DriveResult, EffectSemantics, FileDurableStore, HostConfig, KnownEffectOutcome, ModuleCatalog,
    ModuleDefinition, ModuleRegistration, PluginRuntime, RuntimeLoader, ShutdownStatus, StepResult,
    TaskDefinition,
};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

fn provider_entry(registry: &Recorder) -> CatalogEntry {
    let recorder = registry.clone();
    CatalogEntry::new(
        reference("module-a", "1"),
        ok_factory(move || {
            ModuleRegistration::new(
                ModuleDefinition::new(id("module-a")).with_declarative_capability(
                    CapabilityDeclaration::new(id("shared"), "provider", "shared-v1"),
                ),
            )
            .factory(id("shared"), "shared-v1", |_| Ok(value("shared-value")))
            .on_activate(recorder.hook("activate:module-a", None))
            .on_dispose(recorder.hook("dispose:module-a", None))
        }),
    )
}

fn reactive_entry(registry: &Recorder) -> CatalogEntry {
    let recorder = registry.clone();
    CatalogEntry::new(
        reference("module-b", "1"),
        ok_factory(move || {
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
            .on_activate(recorder.hook("activate:module-b", None))
            .on_dispose(recorder.hook("dispose:module-b", None))
        }),
    )
    .depends_on(reference("module-a", "1"))
}

/// Scenario L: every resolution invokes the entry factories again and hands
/// out fresh registrations — the catalog never caches live registrations or
/// plugin instances, so two resolutions are two independent lifecycles.
#[tokio::test(flavor = "current_thread")]
async fn scenario_l_every_resolution_constructs_fresh_registrations() {
    let registry = Recorder::new();
    let constructions = Arc::new(AtomicUsize::new(0));
    let plugin_ptrs = Arc::new(Mutex::new(Vec::new()));
    // Keep every constructed instance alive alongside its logged address:
    // a pointer only proves "not the same instance" while both allocations
    // are simultaneously live, because a fully dropped block may be handed
    // straight back by the allocator on the next construction.
    let keepalive: Arc<Mutex<Vec<Arc<PluginRuntime>>>> = Arc::new(Mutex::new(Vec::new()));

    let recorder = registry.clone();
    let counter = Arc::clone(&constructions);
    let ptrs = Arc::clone(&plugin_ptrs);
    let alive = Arc::clone(&keepalive);
    let reactive = CatalogEntry::new(reference("module-b", "1"), move || {
        let number = counter.fetch_add(1, Ordering::SeqCst) + 1;
        let plugin = ok_plugin("b-plugin", "reactive", "reactive-v1", "published");
        ptrs.lock()
            .expect("pointer log lock is healthy")
            .push(Arc::as_ptr(&plugin) as usize);
        alive
            .lock()
            .expect("keepalive lock is healthy")
            .push(Arc::clone(&plugin));
        let activation = recorder.hook(&format!("activate:module-b#{number}"), None);
        let disposal = recorder.hook(&format!("dispose:module-b#{number}"), None);
        Ok(ModuleRegistration::new(
            ModuleDefinition::new(id("module-b"))
                .depends_on(id("module-a"))
                .with_reactive_capability(id("reactive"), "service", "reactive-v1"),
        )
        .plugin(plugin)
        .on_activate(activation)
        .on_dispose(disposal))
    })
    .depends_on(reference("module-a", "1"));
    let catalog = ModuleCatalog::new()
        .register(provider_entry(&registry))
        .expect("module-a@1 registers")
        .register(reactive)
        .expect("module-b@1 registers");

    for attempt in 1..=2 {
        let resolved = RuntimeLoader::new(&catalog)
            .resolve([reference("module-b", "1")])
            .expect("resolution succeeds");
        assert_eq!(
            constructions.load(Ordering::SeqCst),
            attempt,
            "each resolution invokes the entry factory exactly once more"
        );
        let plan = resolved.compose().expect("composition validates");
        let assembly = plan
            .start(run_id(&format!("k5-l-{attempt}")), &HostConfig::new())
            .await
            .expect("each resolution activates independently");
        assert!(
            assembly
                .runtime()
                .capability_registry()
                .contains(&id("b-plugin"))
        );
        assembly
            .shutdown()
            .await
            .expect("each lifecycle releases independently");
    }

    assert_eq!(constructions.load(Ordering::SeqCst), 2);
    let logged = plugin_ptrs
        .lock()
        .expect("pointer log lock is healthy")
        .clone();
    assert_eq!(logged.len(), 2);
    assert_eq!(
        keepalive.lock().expect("keepalive lock is healthy").len(),
        2,
        "both instances are alive, so the logged addresses are live allocations"
    );
    assert_ne!(
        logged[0], logged[1],
        "two resolutions must not share a plugin instance"
    );
    assert!(
        registry
            .events()
            .contains(&"activate:module-b#1".to_owned())
    );
    assert!(
        registry
            .events()
            .contains(&"activate:module-b#2".to_owned())
    );
    assert_eq!(registry.count("dispose:module-b#1"), 1);
    assert_eq!(registry.count("dispose:module-b#2"), 1);
}

/// Scenario N: an activation failure inside a loader-resolved composition
/// stays a K4 `StartupFailure` — the loader never rewrites composition
/// errors into loader errors — and K4 rollback semantics are preserved.
#[tokio::test(flavor = "current_thread")]
async fn scenario_n_k4_rollback_semantics_survive_the_loader_boundary() {
    let registry = Recorder::new();
    let failing_recorder = registry.clone();
    let catalog = ModuleCatalog::new()
        .register(provider_entry(&registry))
        .expect("module-a@1 registers")
        .register(reactive_entry(&registry))
        .expect("module-b@1 registers")
        .register(
            CatalogEntry::new(
                reference("module-c", "1"),
                ok_factory(move || {
                    ModuleRegistration::new(
                        ModuleDefinition::new(id("module-c")).depends_on(id("module-b")),
                    )
                    .on_activate(
                        failing_recorder.hook("activate:module-c", Some("c activation failed")),
                    )
                    .on_dispose(failing_recorder.hook("dispose:module-c", None))
                }),
            )
            .depends_on(reference("module-b", "1")),
        )
        .expect("module-c@1 registers");

    let resolved = RuntimeLoader::new(&catalog)
        .resolve([reference("module-c", "1")])
        .expect("the closure resolves");
    let plan = resolved.compose().expect("composition validates");

    let failure = plan
        .start(run_id("k5-n"), &HostConfig::new())
        .await
        .expect_err("module C fails activation");

    // The failure keeps its K4 identity: this is a composition startup
    // failure, not something the loader renamed.
    assert_eq!(
        *failure.cause,
        CompositionError::ActivationFailed {
            module_id: id("module-c"),
            stage: ActivationStage::ActivateHook {
                reason: "c activation failed".to_owned(),
            },
        }
    );
    // Every successfully activated module released all owned cleanup —
    // including module B's plugin unregistration, which is only reported
    // clean when the registry removal itself succeeded.
    assert!(
        failure.rollback.failures.is_empty(),
        "cleanup failures: {:?}",
        failure.rollback.failures
    );
    assert_eq!(
        failure.rollback.cleaned,
        vec![id("module-c"), id("module-b"), id("module-a")]
    );
    assert_eq!(
        registry.events(),
        vec![
            "activate:module-a".to_owned(),
            "activate:module-b".to_owned(),
            "activate:module-c".to_owned(),
            "dispose:module-b".to_owned(),
            "dispose:module-a".to_owned(),
        ],
        "rollback runs in reverse activation order and never fires the \
         un-armed disposer of the module that failed to activate"
    );
    assert_eq!(registry.count("dispose:module-c"), 0);
}

/// Scenario O: two independently constructed catalogs (two processes)
/// resolve to the same K2 durable identity, and the second process cold-
/// reconstructs a durable run from fresh loader-constructed registrations.
#[tokio::test(flavor = "current_thread")]
async fn scenario_o_cold_reconstruction_works_through_the_loader() {
    let temp = TempStore::new("o");
    let run = run_id("k5-o");

    let recorder_first = Recorder::new();
    let catalog_first = ModuleCatalog::new()
        .register(provider_entry(&recorder_first))
        .expect("module-a@1 registers")
        .register(reactive_entry(&recorder_first))
        .expect("module-b@1 registers");
    let plan_first = RuntimeLoader::new(&catalog_first)
        .resolve([reference("module-b", "1")])
        .expect("the first process resolves")
        .compose()
        .expect("the first process composes");
    let identity_first = plan_first
        .definition()
        .identity()
        .expect("the merged definition has a stable identity");
    let store = FileDurableStore::open(temp.path()).expect("durable store opens");
    let assembly = plan_first
        .start_with_store(run.clone(), &HostConfig::new(), store)
        .await
        .expect("the first process activates on the durable store");
    assert!(
        assembly
            .runtime()
            .capability_registry()
            .contains(&id("b-plugin")),
        "the first process registers its own plugin runtime"
    );
    let report = assembly
        .shutdown()
        .await
        .expect("the first process releases cleanly");
    assert_eq!(report.cleaned, vec![id("module-b"), id("module-a")]);

    // A second process: fresh catalog, fresh factories, fresh recorder.
    let recorder_second = Recorder::new();
    let catalog_second = ModuleCatalog::new()
        .register(provider_entry(&recorder_second))
        .expect("module-a@1 registers")
        .register(reactive_entry(&recorder_second))
        .expect("module-b@1 registers");
    let plan_second = RuntimeLoader::new(&catalog_second)
        .resolve([reference("module-b", "1")])
        .expect("the second process resolves")
        .compose()
        .expect("the second process composes");
    let identity_second = plan_second
        .definition()
        .identity()
        .expect("the merged definition has a stable identity");
    assert_eq!(
        identity_first, identity_second,
        "loader metadata (versions, catalog shape) must not touch K2 \
         durable identity"
    );
    assert!(
        !format!("{identity_first:?}").contains('@'),
        "loader reference syntax must not leak into the K2 identity"
    );

    let reopened = FileDurableStore::open(temp.path()).expect("durable store reopens");
    let restored = plan_second
        .restore(run, &HostConfig::new(), reopened)
        .await
        .expect("the second process cold-reconstructs the durable run");
    assert!(
        restored
            .runtime()
            .capability_registry()
            .contains(&id("b-plugin")),
        "the restored process registers its own plugin runtime"
    );
    assert!(restored.runtime().scope().get(&id("reactive")).is_some());
    let report = restored
        .shutdown()
        .await
        .expect("the restored composition releases cleanly");
    assert_eq!(report.cleaned, vec![id("module-b"), id("module-a")]);
    assert_eq!(recorder_second.count("activate:module-a"), 1);
    assert_eq!(recorder_second.count("dispose:module-a"), 1);
}

/// Scenario P: a loader-resolved composition keeps the full K3 driver
/// contract — drive, dispatch, orderly shutdown, and composition cleanup
/// only through the driver exit.
#[tokio::test(flavor = "current_thread")]
async fn scenario_p_driver_contract_is_kept_for_loader_resolved_compositions() {
    let registry = Recorder::new();
    let task_recorder = registry.clone();
    let catalog = ModuleCatalog::new()
        .register(provider_entry(&registry))
        .expect("module-a@1 registers")
        .register(reactive_entry(&registry))
        .expect("module-b@1 registers")
        .register(
            CatalogEntry::new(
                reference("module-c", "1"),
                ok_factory(move || {
                    ModuleRegistration::new(
                        ModuleDefinition::new(id("module-c"))
                            .depends_on(id("module-b"))
                            .with_task(
                                TaskDefinition::new(id("task"), "task")
                                    .require_capability(CapabilityRequirement::new(
                                        id("shared"),
                                        "shared-v1",
                                    ))
                                    .with_effect(
                                        operation("operation"),
                                        EffectSemantics::Idempotent,
                                    ),
                            ),
                    )
                    .on_activate(task_recorder.hook("activate:module-c", None))
                }),
            )
            .depends_on(reference("module-b", "1")),
        )
        .expect("module-c@1 registers");

    let plan = RuntimeLoader::new(&catalog)
        .resolve([reference("module-c", "1")])
        .expect("the closure resolves")
        .compose()
        .expect("composition validates");
    let assembly = plan
        .start(run_id("k5-p"), &HostConfig::new())
        .await
        .expect("the composition activates");

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
        ShutdownStatus::Clean
    );
    let exit = join.await.expect("driver task joins");
    assert_eq!(exit.runtime().attempts().len(), 1);
    assert!(
        exit.runtime()
            .capability_registry()
            .contains(&id("b-plugin")),
        "the composition-owned plugin registration stays live while the \
         driver owns the runtime"
    );
    assert_eq!(
        registry.count("dispose:"),
        0,
        "the composition handle disposed nothing early"
    );

    let outcome = composition.dispose_after_driver(exit).await;
    assert_eq!(outcome.shutdown_status, ShutdownStatus::Clean);
    assert!(outcome.rollback.is_success());
    assert_eq!(
        outcome.rollback.cleaned,
        vec![id("module-c"), id("module-b"), id("module-a")]
    );
    assert!(
        !outcome
            .runtime
            .capability_registry()
            .contains(&id("b-plugin")),
        "orderly composition cleanup left no owned plugin registration"
    );
}

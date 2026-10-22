//! Conformance tests for the explicit Cordis-derived runtime semantics.

use capability_graph::{
    CapabilityContext, CapabilityDefinition, CapabilityFiber, CapabilityId, CapabilityRegistry,
    CapabilityValue, EffectError, EffectScope, EffectStack, FiberError, FiberState,
    PluginDefinition, PluginFactory, PluginLoadContext, PluginRuntime, RegistryError, Scope,
    ScopedEffect,
};
use kernis_core::Id;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

fn id(value: &str) -> CapabilityId {
    Id::new(value).expect("test id is valid")
}

fn definition(value: &str) -> CapabilityDefinition {
    CapabilityDefinition::new(id(value), "service")
}

fn value(value: &str) -> CapabilityValue {
    CapabilityValue::from_value(value.to_owned())
}

fn factory<F, Fut>(function: F) -> PluginFactory
where
    F: Fn(PluginLoadContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<CapabilityValue, String>> + Send + 'static,
{
    Arc::new(move |context| Box::pin(function(context)))
}

fn runtime(capability: CapabilityDefinition, factory: PluginFactory) -> Arc<PluginRuntime> {
    PluginRuntime::new(PluginDefinition::new(id("plugin"), capability, factory))
}

fn create_fiber(
    runtime: Arc<PluginRuntime>,
    context: CapabilityContext,
    config: String,
) -> Arc<CapabilityFiber> {
    let registry = CapabilityRegistry::new();
    let id = runtime.id().clone();
    registry.register(runtime).expect("test runtime registers");
    registry
        .instantiate(&id, context, config)
        .expect("test fiber instantiates")
}

async fn start(fiber: &CapabilityFiber) {
    assert_eq!(
        fiber.start().await.expect("fiber starts"),
        FiberState::Active
    );
}

#[test]
fn context_has_parent_fallback_override_isolation_and_intercept_precedence() {
    let root = CapabilityContext::root();
    root.provide(definition("model"), |_| Ok(value("root")))
        .expect("root model");

    let child = root.child_context();
    assert_eq!(
        child
            .get(&id("model"))
            .expect("parent fallback")
            .downcast_ref::<String>()
            .map(String::as_str),
        Some("root")
    );
    child
        .provide(definition("model"), |_| Ok(value("child")))
        .expect("child override");
    assert_eq!(
        child
            .get(&id("model"))
            .expect("local override")
            .downcast_ref::<String>()
            .map(String::as_str),
        Some("child")
    );
    assert_eq!(
        root.get(&id("model"))
            .expect("root remains")
            .downcast_ref::<String>()
            .map(String::as_str),
        Some("root")
    );

    let isolated = root.isolate(id("model"));
    assert!(isolated.get(&id("model")).is_none());
    isolated
        .provide(definition("model"), |_| Ok(value("isolated")))
        .expect("isolated local provider");
    assert_eq!(
        isolated
            .get(&id("model"))
            .expect("isolated local")
            .downcast_ref::<String>()
            .map(String::as_str),
        Some("isolated")
    );

    let intercepted = root
        .intercept(id("plugin"), "parent".to_owned())
        .intercept(id("plugin"), "local".to_owned());
    assert_eq!(
        intercepted.intercepted_config(&id("plugin")),
        Some("local".to_owned())
    );
}

#[test]
fn context_from_scope_preserves_scope_parent_fallback() {
    let parent = Scope::root();
    parent
        .provide(definition("model"), |_| Ok(value("parent")))
        .expect("parent model");
    let child = parent.child();
    let context = CapabilityContext::from_scope(child);

    assert_eq!(
        context
            .get(&id("model"))
            .expect("scope parent fallback")
            .downcast_ref::<String>()
            .map(String::as_str),
        Some("parent")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn registry_has_one_runtime_and_multiple_fibers_with_exact_removal() {
    let registry = CapabilityRegistry::new();
    let runtime = runtime(
        definition("instance"),
        factory(|context| async move { Ok(value(context.config())) }),
    );
    registry
        .register(Arc::clone(&runtime))
        .expect("register once");
    assert!(
        registry.register(Arc::clone(&runtime)).is_err(),
        "duplicate identity fails closed"
    );

    let first_context = CapabilityContext::root().child_context();
    let second_context = CapabilityContext::root().child_context();
    let first = registry
        .instantiate(&id("plugin"), first_context.clone(), "one".to_owned())
        .expect("first fiber");
    let second = registry
        .instantiate(&id("plugin"), second_context.clone(), "two".to_owned())
        .expect("second fiber");
    assert_eq!(runtime.fiber_count(), 2);
    start(&first).await;
    start(&second).await;
    assert_eq!(runtime.fiber_count(), 2);

    registry
        .remove(&id("plugin"))
        .await
        .expect("remove runtime");
    assert_eq!(first.state(), FiberState::Disposed);
    assert_eq!(second.state(), FiberState::Disposed);
    assert_eq!(runtime.fiber_count(), 0);
    assert!(first_context.get(&id("instance")).is_none());
    assert!(second_context.get(&id("instance")).is_none());
    assert!(
        registry
            .instantiate(&id("plugin"), CapabilityContext::root(), "late".to_owned())
            .is_err()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn registry_removal_disposes_fiber_owned_by_runtime_after_caller_drop() {
    let disposed = Arc::new(AtomicUsize::new(0));
    let disposed_for_factory = Arc::clone(&disposed);
    let runtime = runtime(
        definition("instance"),
        factory(move |context| {
            let disposed = Arc::clone(&disposed_for_factory);
            async move {
                context
                    .effect(ScopedEffect::sync("cleanup", move || {
                        disposed.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    }))
                    .map_err(|error| error.to_string())?;
                Ok(value("instance"))
            }
        }),
    );
    let registry = CapabilityRegistry::new();
    registry
        .register(Arc::clone(&runtime))
        .expect("test runtime registers");
    let fiber = registry
        .instantiate(&id("plugin"), CapabilityContext::root(), String::new())
        .expect("fiber instantiates");
    start(&fiber).await;
    drop(fiber);

    assert_eq!(runtime.fiber_count(), 1);
    registry
        .remove(&id("plugin"))
        .await
        .expect("remove runtime");
    assert_eq!(disposed.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.fiber_count(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn isolated_context_hides_parent_dependency_from_fiber_until_local_override() {
    let root = CapabilityContext::root();
    root.provide(definition("model"), |_| Ok(value("parent")))
        .expect("parent model");
    let isolated = root.isolate(id("model"));
    let service = runtime(
        definition("service").depends_on(id("model")),
        factory(|context| async move {
            let model = context
                .dependencies()
                .get(&id("model"))
                .expect("local model")
                .downcast_ref::<String>()
                .expect("model value")
                .clone();
            Ok(value(&model))
        }),
    );
    let fiber = create_fiber(service, isolated.clone(), String::new());

    assert_eq!(
        fiber.start().await.expect("fiber remains pending"),
        FiberState::Pending
    );
    assert!(fiber.handle().await.is_none());

    isolated
        .provide(definition("model"), |_| Ok(value("local")))
        .expect("local model");
    start(&fiber).await;
    assert_eq!(
        fiber
            .handle()
            .await
            .expect("service handle")
            .downcast_ref::<String>()
            .map(String::as_str),
        Some("local")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn dependency_replacement_unloads_and_reloads_same_fiber() {
    let root = CapabilityContext::root();
    let model = root
        .provide(definition("model"), |_| Ok(value("v1")))
        .expect("model v1");
    let child = root.child_context();
    let loads = Arc::new(AtomicUsize::new(0));
    let loads_for_factory = Arc::clone(&loads);
    let primary_runtime = runtime(
        definition("service").depends_on(id("model")),
        factory(move |context| {
            let loads = Arc::clone(&loads_for_factory);
            async move {
                let count = loads.fetch_add(1, Ordering::SeqCst) + 1;
                let model = context
                    .dependencies()
                    .get(&id("model"))
                    .expect("model dependency")
                    .downcast_ref::<String>()
                    .expect("model value")
                    .clone();
                Ok(value(&format!("{model}-{count}")))
            }
        }),
    );
    let fiber = create_fiber(Arc::clone(&primary_runtime), child.clone(), String::new());
    start(&fiber).await;
    let first_epoch = fiber.dependency_epoch().await.expect("first epoch");
    assert_eq!(
        first_epoch.generation(&id("model")),
        Some(model.generation())
    );

    root.scope()
        .replace(definition("model"), model.generation(), |_| Ok(value("v2")))
        .expect("replace model");
    assert_eq!(fiber.start().await.expect("reload"), FiberState::Active);
    let second_epoch = fiber.dependency_epoch().await.expect("second epoch");
    assert_ne!(first_epoch, second_epoch);
    assert_eq!(loads.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn dependency_replacement_during_unload_uses_latest_epoch() {
    let root = CapabilityContext::root();
    let model = root
        .provide(definition("model"), |_| Ok(value("v1")))
        .expect("model v1");
    let child = root.child_context();
    let loads = Arc::new(AtomicUsize::new(0));
    let cleanup_started = Arc::new(Notify::new());
    let cleanup_release = Arc::new(Notify::new());
    let cleanups = Arc::new(AtomicUsize::new(0));
    let loads_for_factory = Arc::clone(&loads);
    let cleanup_started_for_factory = Arc::clone(&cleanup_started);
    let cleanup_release_for_factory = Arc::clone(&cleanup_release);
    let cleanups_for_factory = Arc::clone(&cleanups);
    let service = runtime(
        definition("service").depends_on(id("model")),
        factory(move |context| {
            let loads = Arc::clone(&loads_for_factory);
            let cleanup_started = Arc::clone(&cleanup_started_for_factory);
            let cleanup_release = Arc::clone(&cleanup_release_for_factory);
            let cleanups = Arc::clone(&cleanups_for_factory);
            async move {
                let load = loads.fetch_add(1, Ordering::SeqCst) + 1;
                let model = context
                    .dependencies()
                    .get(&id("model"))
                    .expect("model dependency")
                    .downcast_ref::<String>()
                    .expect("model value")
                    .clone();
                context
                    .effect(ScopedEffect::asynchronous("cleanup", move || async move {
                        if cleanups.fetch_add(1, Ordering::SeqCst) == 0 {
                            cleanup_started.notify_one();
                            cleanup_release.notified().await;
                        }
                        Ok(())
                    }))
                    .map_err(|error| error.to_string())?;
                Ok(value(&format!("{model}-{load}")))
            }
        }),
    );
    let fiber = create_fiber(service, child, String::new());
    start(&fiber).await;

    let model_v2 = root
        .scope()
        .replace(definition("model"), model.generation(), |_| Ok(value("v2")))
        .expect("model v2");
    let loading = {
        let fiber = Arc::clone(&fiber);
        tokio::spawn(async move { fiber.start().await })
    };
    cleanup_started.notified().await;
    let model_v3 = root
        .scope()
        .replace(definition("model"), model_v2.generation(), |_| {
            Ok(value("v3"))
        })
        .expect("model v3");
    cleanup_release.notify_one();

    assert_eq!(
        loading.await.expect("reload task").expect("reload"),
        FiberState::Active
    );
    assert_eq!(loads.load(Ordering::SeqCst), 2);
    assert_eq!(
        fiber
            .handle()
            .await
            .expect("service handle")
            .downcast_ref::<String>()
            .map(String::as_str),
        Some("v3-2")
    );
    assert_eq!(
        fiber
            .dependency_epoch()
            .await
            .expect("latest dependency epoch")
            .entry_id(&id("model")),
        Some(model_v3.entry_id())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn dependency_disappearance_unloads_to_pending() {
    let root = CapabilityContext::root();
    root.provide(definition("model"), |_| Ok(value("v1")))
        .expect("model");
    let fiber = create_fiber(
        runtime(
            definition("service").depends_on(id("model")),
            factory(|_| async { Ok(value("service")) }),
        ),
        root.child_context(),
        String::new(),
    );
    start(&fiber).await;

    root.teardown();

    assert_eq!(
        fiber.await_stable().await.expect("pending state"),
        FiberState::Pending
    );
    assert!(fiber.handle().await.is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn active_fiber_dispose_is_idempotent_and_cleans_once() {
    let disposed = Arc::new(AtomicUsize::new(0));
    let disposed_for_factory = Arc::clone(&disposed);
    let fiber = create_fiber(
        runtime(
            definition("service"),
            factory(move |context| {
                let disposed = Arc::clone(&disposed_for_factory);
                async move {
                    context
                        .effect(ScopedEffect::sync("cleanup", move || {
                            disposed.fetch_add(1, Ordering::SeqCst);
                            Ok(())
                        }))
                        .map_err(|error| error.to_string())?;
                    Ok(value("service"))
                }
            }),
        ),
        CapabilityContext::root(),
        String::new(),
    );

    assert_eq!(
        fiber.await_stable().await.expect("active state"),
        FiberState::Active
    );
    fiber.dispose().await.expect("first dispose");
    fiber.dispose().await.expect("second dispose");
    assert_eq!(fiber.state(), FiberState::Disposed);
    assert_eq!(disposed.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn stale_async_load_cannot_publish_and_dispose_during_load_is_final() {
    let root = CapabilityContext::root();
    let model = root
        .provide(definition("model"), |_| Ok(value("v1")))
        .expect("model v1");
    let child = root.child_context();
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let started_for_factory = Arc::clone(&started);
    let release_for_factory = Arc::clone(&release);
    let calls_for_factory = Arc::clone(&calls);
    let stale_runtime = runtime(
        definition("service").depends_on(id("model")),
        factory(move |context| {
            let started = Arc::clone(&started_for_factory);
            let release = Arc::clone(&release_for_factory);
            let calls = Arc::clone(&calls_for_factory);
            async move {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    started.notify_one();
                    release.notified().await;
                }
                Ok(value(context.config()))
            }
        }),
    );
    let fiber = create_fiber(Arc::clone(&stale_runtime), child, String::new());
    let loading = {
        let fiber = Arc::clone(&fiber);
        tokio::spawn(async move { fiber.start().await })
    };
    started.notified().await;
    root.scope()
        .replace(definition("model"), model.generation(), |_| Ok(value("v2")))
        .expect("replace while loading");
    release.notify_one();
    assert_eq!(
        loading.await.expect("load task").expect("reload"),
        FiberState::Active
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(
        fiber
            .handle()
            .await
            .expect("service handle")
            .entry_id()
            .get()
            > 0
    );

    let dispose_started = Arc::new(Notify::new());
    let dispose_release = Arc::new(Notify::new());
    let dispose_started_for_factory = Arc::clone(&dispose_started);
    let dispose_release_for_factory = Arc::clone(&dispose_release);
    let dispose_runtime = runtime(
        definition("other").depends_on(id("model")),
        factory(move |_| {
            let started = Arc::clone(&dispose_started_for_factory);
            let release = Arc::clone(&dispose_release_for_factory);
            async move {
                started.notify_one();
                release.notified().await;
                Ok(value("other"))
            }
        }),
    );
    let dispose_fiber = create_fiber(
        Arc::clone(&dispose_runtime),
        root.child_context(),
        String::new(),
    );
    let loading = {
        let fiber = Arc::clone(&dispose_fiber);
        tokio::spawn(async move { fiber.start().await })
    };
    dispose_started.notified().await;
    let disposing = {
        let fiber = Arc::clone(&dispose_fiber);
        tokio::spawn(async move { fiber.dispose().await })
    };
    dispose_release.notify_one();
    assert!(matches!(
        loading.await.expect("load task"),
        Err(FiberError::Disposed)
    ));
    disposing.await.expect("dispose task").expect("dispose");
    assert_eq!(dispose_fiber.state(), FiberState::Disposed);
}

#[tokio::test(flavor = "current_thread")]
async fn stale_async_load_error_retries_latest_epoch() {
    let root = CapabilityContext::root();
    let model = root
        .provide(definition("model"), |_| Ok(value("v1")))
        .expect("model v1");
    let child = root.child_context();
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let disposed = Arc::new(AtomicUsize::new(0));
    let started_for_factory = Arc::clone(&started);
    let release_for_factory = Arc::clone(&release);
    let calls_for_factory = Arc::clone(&calls);
    let disposed_for_factory = Arc::clone(&disposed);
    let service = runtime(
        definition("service").depends_on(id("model")),
        factory(move |context| {
            let started = Arc::clone(&started_for_factory);
            let release = Arc::clone(&release_for_factory);
            let calls = Arc::clone(&calls_for_factory);
            let disposed = Arc::clone(&disposed_for_factory);
            async move {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    let disposed_for_effect = Arc::clone(&disposed);
                    context
                        .effect(ScopedEffect::sync("stale cleanup", move || {
                            disposed_for_effect.fetch_add(1, Ordering::SeqCst);
                            Ok(())
                        }))
                        .map_err(|error| error.to_string())?;
                    started.notify_one();
                    release.notified().await;
                    Err("stale load failed".to_owned())
                } else {
                    let model = context
                        .dependencies()
                        .get(&id("model"))
                        .expect("model dependency")
                        .downcast_ref::<String>()
                        .expect("model value")
                        .clone();
                    Ok(value(&model))
                }
            }
        }),
    );
    let fiber = create_fiber(service, child, String::new());
    let loading = {
        let fiber = Arc::clone(&fiber);
        tokio::spawn(async move { fiber.start().await })
    };
    started.notified().await;
    root.scope()
        .replace(definition("model"), model.generation(), |_| Ok(value("v2")))
        .expect("replace while loading");
    release.notify_one();

    assert_eq!(
        loading
            .await
            .expect("load task")
            .expect("stale error retries latest epoch"),
        FiberState::Active
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(disposed.load(Ordering::SeqCst), 1);
    assert_eq!(fiber.error().await, None);
    assert_eq!(
        fiber
            .handle()
            .await
            .expect("service handle")
            .downcast_ref::<String>()
            .map(String::as_str),
        Some("v2")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn effect_stack_is_reverse_order_nested_and_failure_isolated() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut stack = EffectStack::new();
    for label in ["first", "second"] {
        let events = Arc::clone(&events);
        stack
            .push(ScopedEffect::sync(label, move || {
                events.lock().expect("events lock").push(label.to_owned());
                Ok(())
            }))
            .expect("effect registers");
    }
    let errors = stack.dispose_all().await;
    assert!(errors.is_empty());
    assert_eq!(
        *events.lock().expect("events lock"),
        vec!["second".to_owned(), "first".to_owned()]
    );
    assert!(matches!(
        stack.push(ScopedEffect::sync("late", || Ok(()))),
        Err(EffectError::Closed)
    ));

    let scope = EffectScope::new();
    let nested = scope.child("nested").expect("nested scope");
    nested
        .register(ScopedEffect::sync("bad", || Err("bad cleanup".to_owned())))
        .expect("nested effect");
    scope
        .register(ScopedEffect::sync("independent", || {
            Err("independent cleanup".to_owned())
        }))
        .expect("independent effect");
    let errors = scope.dispose_all().await;
    assert_eq!(
        errors.len(),
        2,
        "parent keeps both cleanup failures observable"
    );
    assert!(errors.iter().any(|error| matches!(
        error,
        EffectError::Disposer { label, reason }
            if label == "independent" && reason == "independent cleanup"
    )));
    assert_eq!(
        nested.dispose_all().await.len(),
        0,
        "nested was already drained"
    );

    let async_disposed = Arc::new(AtomicUsize::new(0));
    let async_disposed_for_cleanup = Arc::clone(&async_disposed);
    let async_scope = EffectScope::new();
    async_scope
        .register(ScopedEffect::asynchronous("async", move || async move {
            tokio::task::yield_now().await;
            async_disposed_for_cleanup.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }))
        .expect("async effect");
    assert!(async_scope.dispose_all().await.is_empty());
    assert_eq!(async_disposed.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn partial_initialization_cleans_registered_effects() {
    let context = CapabilityContext::root();
    let disposed = Arc::new(AtomicUsize::new(0));
    let disposed_for_factory = Arc::clone(&disposed);
    let runtime = runtime(
        definition("broken"),
        factory(move |context| {
            let disposed = Arc::clone(&disposed_for_factory);
            async move {
                let disposed_for_effect = Arc::clone(&disposed);
                context
                    .effect(ScopedEffect::sync("partial", move || {
                        disposed_for_effect.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    }))
                    .map_err(|error| error.to_string())?;
                Err("initialization failed".to_owned())
            }
        }),
    );
    let fiber = create_fiber(Arc::clone(&runtime), context, String::new());
    assert!(matches!(
        fiber.start().await,
        Err(FiberError::InitializationFailed { .. })
    ));
    assert_eq!(fiber.state(), FiberState::Failed);
    assert_eq!(disposed.load(Ordering::SeqCst), 1);
    assert!(fiber.context().get(&id("broken")).is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn fiber_effect_rejected_when_pending_or_disposed() {
    let runtime = runtime(
        definition("effect"),
        factory(|_| async { Ok(value("effect")) }),
    );
    let fiber = create_fiber(runtime, CapabilityContext::root(), String::new());

    assert_eq!(
        fiber.effect(ScopedEffect::sync("pending", || Ok(()))),
        Err(FiberError::InactiveEffect(FiberState::Pending))
    );
    start(&fiber).await;
    fiber.dispose().await.expect("dispose");
    assert_eq!(
        fiber.effect(ScopedEffect::sync("disposed", || Ok(()))),
        Err(FiberError::InactiveEffect(FiberState::Disposed))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn update_restarts_the_same_fiber_and_failed_restart_can_recover() {
    let configurable_runtime = runtime(
        definition("configurable"),
        factory(|context| async move { Ok(value(context.config())) }),
    );
    let fiber = create_fiber(
        Arc::clone(&configurable_runtime),
        CapabilityContext::root(),
        "v1".to_owned(),
    );
    start(&fiber).await;
    assert_eq!(
        fiber
            .handle()
            .await
            .expect("configurable handle")
            .downcast_ref::<String>()
            .map(String::as_str),
        Some("v1")
    );
    assert_eq!(
        fiber.update("v2".to_owned()).await.expect("update"),
        FiberState::Active
    );
    assert_eq!(
        fiber
            .handle()
            .await
            .expect("updated handle")
            .downcast_ref::<String>()
            .map(String::as_str),
        Some("v2")
    );

    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_factory = Arc::clone(&calls);
    let recovering_runtime = runtime(
        definition("recovering"),
        factory(move |_| {
            let calls = Arc::clone(&calls_for_factory);
            async move {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    Err("first load fails".to_owned())
                } else {
                    Ok(value("recovered"))
                }
            }
        }),
    );
    let recovering = create_fiber(
        Arc::clone(&recovering_runtime),
        CapabilityContext::root(),
        String::new(),
    );
    assert!(recovering.start().await.is_err());
    assert_eq!(recovering.state(), FiberState::Failed);
    assert_eq!(
        recovering.restart().await.expect("restart recovery"),
        FiberState::Active
    );
}

/// `remove` is now `take` + `dispose_taken`: the split must not drift.
///
/// The cancellation-safe composition sweep relies on both halves — `take`
/// removes the registration synchronously, before any await, so a dropped
/// caller can still see what is outstanding, and `dispose_taken` reports the
/// same cleanup failure the one-shot `remove` reports for the same fibers.
/// This pins that equivalence on the absent, successful, and failing cases
/// instead of trusting the refactor.
#[tokio::test(flavor = "current_thread")]
async fn take_plus_dispose_taken_matches_one_shot_removal() {
    let absent = CapabilityRegistry::new();
    assert!(
        absent.take(&id("plugin")).is_none(),
        "an absent take is the same no-op as an absent removal"
    );
    assert!(
        absent
            .remove(&id("plugin"))
            .await
            .expect("removing an absent registration never fails")
            .is_none()
    );

    let success = runtime(
        definition("instance"),
        factory(|context| async move {
            context
                .effect(ScopedEffect::sync("cleanup", || Ok(())))
                .map_err(|error| error.to_string())?;
            Ok(value("instance"))
        }),
    );
    let registry = CapabilityRegistry::new();
    registry
        .register(Arc::clone(&success))
        .expect("the test runtime registers");
    let fiber = registry
        .instantiate(&id("plugin"), CapabilityContext::root(), String::new())
        .expect("the fiber instantiates");
    start(&fiber).await;
    let taken = registry
        .take(&id("plugin"))
        .expect("the registration is taken");
    assert!(
        registry.take(&id("plugin")).is_none(),
        "the take is one-shot"
    );
    assert_eq!(
        success.fiber_count(),
        1,
        "taking the registration does not dispose the fibers"
    );
    CapabilityRegistry::dispose_taken(id("plugin"), &taken)
        .await
        .expect("the taken runtime disposes");
    assert_eq!(fiber.state(), FiberState::Disposed);
    assert_eq!(success.fiber_count(), 0);

    let failing = || {
        runtime(
            definition("instance"),
            factory(|context| async move {
                context
                    .effect(ScopedEffect::sync("cleanup", || {
                        Err("cleanup failed".to_owned())
                    }))
                    .map_err(|error| error.to_string())?;
                Ok(value("instance"))
            }),
        )
    };
    let split_runtime = failing();
    let split_registry = CapabilityRegistry::new();
    split_registry
        .register(Arc::clone(&split_runtime))
        .expect("the failing runtime registers");
    let split_fiber = split_registry
        .instantiate(&id("plugin"), CapabilityContext::root(), String::new())
        .expect("the failing fiber instantiates");
    start(&split_fiber).await;
    let split_taken = split_registry
        .take(&id("plugin"))
        .expect("the failing registration is taken");
    let split_error = match CapabilityRegistry::dispose_taken(id("plugin"), &split_taken).await {
        Ok(()) => panic!("a failing fiber cleanup is reported, not swallowed"),
        Err(error) => error,
    };

    let one_shot_runtime = failing();
    let one_shot_registry = CapabilityRegistry::new();
    one_shot_registry
        .register(Arc::clone(&one_shot_runtime))
        .expect("the one-shot runtime registers");
    let one_shot_fiber = one_shot_registry
        .instantiate(&id("plugin"), CapabilityContext::root(), String::new())
        .expect("the one-shot fiber instantiates");
    start(&one_shot_fiber).await;
    let one_shot_error = match one_shot_registry.remove(&id("plugin")).await {
        Ok(_) => panic!("a failing fiber cleanup is reported, not swallowed"),
        Err(error) => error,
    };

    assert!(matches!(
        split_error,
        RegistryError::Dispose { ref plugin, .. } if plugin == &id("plugin")
    ));
    assert_eq!(
        split_error.to_string(),
        one_shot_error.to_string(),
        "the split reports exactly the failure the one-shot removal reports"
    );
}

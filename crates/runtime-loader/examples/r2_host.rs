//! The canonical KERNIS R2 host: one supported lifecycle, start to finish,
//! across a cold restart.
//!
//! This is the single normal host lifecycle (K6, ADR 0007): resolve →
//! compose → activate → `RuntimeAssembly::into_driver` → `RuntimeDriver::run`
//! → `RuntimeHandle::drive`/`shutdown` → `DriverExit` →
//! `CompositionHandle::dispose_after_driver` → final runtime release.
//! Every type named here comes from `runtime_loader` alone — the supported
//! host-entry umbrella — never from a subsystem crate.
//!
//! The example then performs a *genuine cold restart*: phase two reopens the
//! same physical store but constructs a fresh catalog, loader,
//! registrations, composition plan, and runtime. Only stable references and
//! the physical store are reused. Durable facts are verified after the
//! restart, and the remaining work is continued — never redone.

use runtime_loader::{
    CapabilityDeclaration, CapabilityDefinition, CapabilityRequirement, CapabilityValue,
    CatalogEntry, DriveResult, EffectDispatchFuture, EffectDispatchRequest, EffectDispatcher,
    EffectSemantics, FileDurableStore, HostConfig, Id, KnownEffectOutcome, ModuleCatalog,
    ModuleDefinition, ModuleReference, ModuleRegistration, OperationId, PluginDefinition,
    PluginFactory, PluginRuntime, RunId, RuntimeLoader, ShutdownStatus, StepResult, TaskDefinition,
};
use std::{fs, process};

const RUN_ID: &str = "r2-canonical-run";
const PUBLISH_OP: &str = "publish-document";

fn id(value: &str) -> Id {
    Id::new(value).expect("example ids are valid")
}

fn reference(module: &str, version: &str) -> ModuleReference {
    ModuleReference::new(module, version).expect("example references are valid")
}

fn operation(value: &str) -> OperationId {
    OperationId::new(value).expect("example operation is valid")
}

/// Build the host's explicit catalog: every module the host is willing to
/// run, with its factory and declared dependency references. Both phases
/// build this from the same stable declarations; the cold restart reuses the
/// declarations, never the objects.
fn build_catalog() -> ModuleCatalog {
    let provider = CatalogEntry::new(reference("content-store", "1"), || {
        Ok(ModuleRegistration::new(
            ModuleDefinition::new(id("content-store")).with_declarative_capability(
                CapabilityDeclaration::new(id("document-store"), "provider", "v1"),
            ),
        )
        .factory(id("document-store"), "v1", |_| {
            Ok(CapabilityValue::from_value("in-memory-store".to_owned()))
        }))
    });
    let indexer = CatalogEntry::new(reference("indexer", "1"), || {
        let factory: PluginFactory = std::sync::Arc::new(|_| {
            Box::pin(async { Ok(CapabilityValue::from_value("index-ready".to_owned())) })
        });
        Ok(ModuleRegistration::new(
            ModuleDefinition::new(id("indexer"))
                .depends_on(id("content-store"))
                .with_reactive_capability(id("index-updates"), "service", "v1"),
        )
        .plugin(PluginRuntime::new(PluginDefinition::new(
            id("index-plugin"),
            CapabilityDefinition::new(id("index-updates"), "service").with_replay_identity("v1"),
            factory,
        ))))
    })
    .depends_on(reference("content-store", "1"));
    let publisher = CatalogEntry::new(reference("publisher", "1"), || {
        Ok(ModuleRegistration::new(
            ModuleDefinition::new(id("publisher"))
                .depends_on(id("indexer"))
                .with_task(TaskDefinition::new(id("prepare"), "prepare one document"))
                .with_task(
                    TaskDefinition::new(id("publish"), "publish one document")
                        .depends_on(id("prepare"))
                        .require_capability(CapabilityRequirement::new(id("document-store"), "v1"))
                        .with_effect(operation(PUBLISH_OP), EffectSemantics::Idempotent),
                ),
        ))
    })
    .depends_on(reference("indexer", "1"));

    ModuleCatalog::new()
        .register(provider)
        .expect("content-store@1 registers")
        .register(indexer)
        .expect("indexer@1 registers")
        .register(publisher)
        .expect("publisher@1 registers")
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let dir = std::env::temp_dir().join(format!(
        "kernis-r2-host-{}-{}",
        process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the unix epoch")
            .as_nanos()
    ));
    fs::create_dir_all(&dir).expect("the example store directory is created");
    let store_path = dir.join("run.redb");
    println!("physical store: {}", store_path.display());

    // ---- Phase 1: fresh start, orderly stop after the first task. ----
    println!("\nphase 1: catalog -> resolve -> compose -> activate -> drive");
    let catalog = build_catalog();
    let resolved = RuntimeLoader::new(&catalog)
        .resolve([reference("publisher", "1")])
        .expect("the requested closure resolves against the catalog");
    let plan = resolved.compose().expect("the resolved modules compose");
    let store = FileDurableStore::open(&store_path).expect("the physical store opens");
    let assembly = plan
        .start_with_store(
            RunId::new(RUN_ID).expect("example run id is valid"),
            &HostConfig::new(),
            store,
        )
        .await
        .expect("the composition activates");
    let (driver, handle, composition) = assembly.into_driver(ExampleDispatcher);
    let join = tokio::spawn(driver.run());
    let prepared = handle.drive().await.expect("driving succeeds");
    println!("  drive -> {prepared:?}");
    assert!(matches!(&prepared,
            DriveResult::Step(StepResult::Completed { task_id, .. }) if *task_id == id("prepare")));
    let status = handle.shutdown().await.expect("orderly shutdown");
    assert_eq!(status, ShutdownStatus::Clean);
    let exit = join.await.expect("the driver task joins");
    let outcome = composition.dispose_after_driver(exit).await;
    assert_eq!(outcome.shutdown_status, ShutdownStatus::Clean);
    println!(
        "  dispose cleaned: {}",
        outcome
            .rollback
            .cleaned
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    );
    // Dropping the outcome releases the final runtime and its store lock.
    drop(outcome);

    // ---- Phase 2: genuine cold restart. ----
    // Fresh catalog, fresh loader, fresh registrations, fresh plan, fresh
    // runtime. Only the stable references and the same physical store are
    // reused.
    println!("\nphase 2: cold restart with fresh objects against the same store");
    let catalog = build_catalog();
    let resolved = RuntimeLoader::new(&catalog)
        .resolve([reference("publisher", "1")])
        .expect("the same stable references resolve again");
    let plan = resolved.compose().expect("the plan composes identically");
    let store = FileDurableStore::open(&store_path).expect("the physical store reopens");
    let assembly = plan
        .restore(
            RunId::new(RUN_ID).expect("example run id is valid"),
            &HostConfig::new(),
            store,
        )
        .await
        .expect("the cold restore reconstructs the runtime");
    let (driver, handle, composition) = assembly.into_driver(ExampleDispatcher);
    let join = tokio::spawn(driver.run());
    let dispatched = handle.drive().await.expect("driving succeeds");
    println!("  drive -> {dispatched:?}");
    assert!(
        matches!(&dispatched, DriveResult::EffectCompleted { request, outcome }
            if request.operation_id == operation(PUBLISH_OP)
                && *outcome == KnownEffectOutcome::Succeeded)
    );
    let completed = handle.drive().await.expect("driving succeeds");
    println!("  drive -> {completed:?}");
    assert!(matches!(
        &completed,
        DriveResult::Step(StepResult::Completed { task_id, .. }) if *task_id == id("publish")
    ));
    let idle = handle.drive().await.expect("driving succeeds");
    println!("  drive -> {idle:?}");
    assert!(matches!(idle, DriveResult::Step(StepResult::Idle)));
    let status = handle.shutdown().await.expect("orderly shutdown");
    assert_eq!(status, ShutdownStatus::Clean);
    let exit = join.await.expect("the driver task joins");
    let outcome = composition.dispose_after_driver(exit).await;

    // Verify the durable facts exactly once, after the whole lifecycle.
    let state = outcome
        .runtime
        .durable_state()
        .expect("durable state reads");
    assert!(state.is_completed(&id("prepare")) && state.is_completed(&id("publish")));
    assert_eq!(state.completion_history().len(), 2);
    assert_eq!(
        state.dispatches(&operation(PUBLISH_OP)).count(),
        1,
        "completed work is never re-dispatched after a cold restart"
    );
    println!(
        "\nphase 2 verified: 2 completions, 1 effect dispatch in total, no duplicate work;\n\
         dispose cleaned: {}",
        outcome
            .rollback
            .cleaned
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    );
    drop(outcome);

    if let Err(error) = fs::remove_dir_all(&dir) {
        eprintln!("warning: could not clean {}: {error}", store_path.display());
    }
    println!("R2 host lifecycle complete.");
}

/// A dispatcher that acknowledges every effect as succeeded.
struct ExampleDispatcher;

impl EffectDispatcher for ExampleDispatcher {
    fn dispatch(&mut self, request: EffectDispatchRequest) -> EffectDispatchFuture {
        Box::pin(async move {
            println!(
                "  host dispatcher executed effect: {}",
                request.operation_id
            );
            Ok(KnownEffectOutcome::Succeeded)
        })
    }
}

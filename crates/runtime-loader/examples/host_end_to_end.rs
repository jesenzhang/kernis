//! A small host end-to-end through the K5 loader boundary.
//!
//! The host declares which logical modules it needs, resolves them against
//! an explicit in-process catalog, composes the resolved registrations
//! through K4, activates the runtime through K2, drives one task through
//! the K3 driver, and shuts down cleanly. This is an example, not a CLI:
//! there is no argument parsing, no config file, and no discovery — the
//! host code hands the loader its catalog and roots directly.

use runtime_loader::{
    CapabilityDeclaration, CapabilityDefinition, CapabilityRequirement, CapabilityValue,
    CatalogEntry, EffectDispatchFuture, EffectDispatchRequest, EffectDispatcher, EffectSemantics,
    HostConfig, Id, KnownEffectOutcome, ModuleCatalog, ModuleDefinition, ModuleReference,
    ModuleRegistration, PluginDefinition, PluginFactory, PluginRuntime, RunId, RuntimeLoader,
    ShutdownStatus, TaskDefinition,
};
use std::sync::Arc;

fn id(value: &str) -> Id {
    Id::new(value).expect("example ids are valid")
}

fn reference(module: &str, version: &str) -> ModuleReference {
    ModuleReference::new(module, version).expect("example references are valid")
}

/// Build the host's explicit catalog: every module the host is willing to
/// run, with its factory and declared dependency references.
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
        let factory: PluginFactory = Arc::new(|_| {
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
                .with_task(
                    TaskDefinition::new(id("publish"), "publish one document")
                        .require_capability(CapabilityRequirement::new(id("document-store"), "v1"))
                        .with_effect(
                            runtime_loader::OperationId::new("publish-document")
                                .expect("example operation is valid"),
                            EffectSemantics::Idempotent,
                        ),
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
    let catalog = build_catalog();
    println!("catalog holds {} entries:", catalog.len());
    for reference in catalog.references() {
        println!("  - {reference}");
    }

    // Phase 1: resolution. The host names only the module it needs; the
    // loader closes the dependency reference deterministically and fails
    // closed on anything it cannot honor.
    let resolved = RuntimeLoader::new(&catalog)
        .resolve([reference("publisher", "1")])
        .expect("the requested closure resolves against the catalog");
    println!(
        "resolved closure: {}",
        resolved
            .references()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" -> ")
    );

    // Phase 2: composition. K4 validates the merged plan without any
    // runtime-side effect.
    let plan = resolved.compose().expect("the resolved modules compose");
    println!(
        "composition order: {}",
        plan.module_order()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" -> ")
    );

    // Phase 3: activation through K2.
    let assembly = plan
        .start(
            RunId::new("example-run").expect("example run id is valid"),
            &HostConfig::new(),
        )
        .await
        .expect("the composition activates");
    println!(
        "runtime activated; index-plugin registered: {}",
        assembly
            .runtime()
            .capability_registry()
            .contains(&id("index-plugin"))
    );

    // K3 driving: one effect dispatch, one completed step.
    let (driver, handle, composition) = assembly.into_driver(ExampleDispatcher);
    let join = tokio::spawn(driver.run());
    let dispatched = handle.drive().await.expect("the effect dispatches");
    println!("first drive result: {dispatched:?}");
    let stepped = handle.drive().await.expect("the step completes");
    println!("second drive result: {stepped:?}");
    let status = handle.shutdown().await.expect("orderly shutdown");
    assert_eq!(status, ShutdownStatus::Clean);
    let exit = join.await.expect("the driver task joins");

    // Composition cleanup is available only through the driver exit.
    let outcome = composition.dispose_after_driver(exit).await;
    println!(
        "shutdown status: {:?}; cleaned modules: {}",
        outcome.shutdown_status,
        outcome
            .rollback
            .cleaned
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    );
}

/// A dispatcher that acknowledges every effect as succeeded.
struct ExampleDispatcher;

impl EffectDispatcher for ExampleDispatcher {
    fn dispatch(&mut self, request: EffectDispatchRequest) -> EffectDispatchFuture {
        Box::pin(async move {
            println!("host dispatcher executed effect: {}", request.operation_id);
            Ok(KnownEffectOutcome::Succeeded)
        })
    }
}

//! K5 acceptance scenarios I-M: reference cycles with a deterministic
//! path, incompatible registrations, dependency metadata disagreement, and
//! the guarantee that a resolution failure has zero activation effect.

mod k5_common;

use k5_common::{Recorder, id, ok_factory, reference, value};
use runtime_loader::{
    CapabilityDeclaration, CatalogEntry, IncompatibleEntryReason, LoaderError, ModuleCatalog,
    ModuleDefinition, ModuleFactoryError, ModuleRegistration, RuntimeLoader,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

fn counting_provider_entry(registry: &Recorder, calls: &Arc<AtomicUsize>) -> CatalogEntry {
    let recorder = registry.clone();
    let counter = Arc::clone(calls);
    CatalogEntry::new(reference("module-a", "1"), move || {
        counter.fetch_add(1, Ordering::SeqCst);
        Ok(ModuleRegistration::new(
            ModuleDefinition::new(id("module-a")).with_declarative_capability(
                CapabilityDeclaration::new(id("shared"), "provider", "shared-v1"),
            ),
        )
        .factory(id("shared"), "shared-v1", |_| Ok(value("shared-value")))
        .on_activate(recorder.hook("activate:module-a", None)))
    })
}

/// Scenario I: a reference cycle fails with a deterministic closure path
/// that is independent of root order.
#[tokio::test(flavor = "current_thread")]
async fn scenario_i_reference_cycle_reports_a_deterministic_path() {
    let registry = Recorder::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let catalog = ModuleCatalog::new()
        .register(
            CatalogEntry::new(reference("module-a", "1"), move || {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(ModuleRegistration::new(
                    ModuleDefinition::new(id("module-a")).depends_on(id("module-b")),
                ))
            })
            .depends_on(reference("module-b", "1")),
        )
        .expect("module-a@1 registers");
    let counter = Arc::clone(&calls);
    let catalog = catalog
        .register(
            CatalogEntry::new(reference("module-b", "1"), move || {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(ModuleRegistration::new(
                    ModuleDefinition::new(id("module-b")).depends_on(id("module-a")),
                ))
            })
            .depends_on(reference("module-a", "1")),
        )
        .expect("module-b@1 registers");

    let expected = LoaderError::DependencyCycle {
        path: vec![
            reference("module-a", "1"),
            reference("module-b", "1"),
            reference("module-a", "1"),
        ],
    };

    let failure = RuntimeLoader::new(&catalog)
        .resolve([reference("module-a", "1")])
        .expect_err("a self-referential closure cannot resolve");
    assert_eq!(failure, expected);

    // The same cycle seen from the other root reports the same path: the
    // diagnostic is a property of the graph, not of the request order.
    let failure = RuntimeLoader::new(&catalog)
        .resolve([reference("module-b", "1")])
        .expect_err("the cycle is detected from any member");
    assert_eq!(failure, expected);

    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "a cyclic closure never constructs any registration"
    );
    assert!(registry.events().is_empty());
}

/// Scenario J: a catalog entry whose factory produces an incompatible
/// registration — wrong identity, or a factory error — fails closed with a
/// typed loader error before any composition or activation.
#[tokio::test(flavor = "current_thread")]
async fn scenario_j_incompatible_registration_fails_closed() {
    let registry = Recorder::new();
    let recorder = registry.clone();
    let catalog = ModuleCatalog::new()
        .register(CatalogEntry::new(
            reference("module-a", "1"),
            ok_factory(move || {
                ModuleRegistration::new(ModuleDefinition::new(id("module-x")))
                    .on_activate(recorder.hook("activate:module-a", None))
            }),
        ))
        .expect("the mislabeled entry registers");

    let failure = RuntimeLoader::new(&catalog)
        .resolve([reference("module-a", "1")])
        .expect_err("the entry produced a different module identity");
    assert_eq!(
        failure,
        LoaderError::IncompatibleEntry {
            reference: reference("module-a", "1"),
            reason: IncompatibleEntryReason::ProducedIdentityMismatch {
                actual: id("module-x"),
            },
        }
    );
    assert!(
        registry.events().is_empty(),
        "an incompatible registration is never activated"
    );

    let catalog = ModuleCatalog::new()
        .register(CatalogEntry::new(reference("module-a", "1"), || {
            Err(ModuleFactoryError::new("the factory exploded"))
        }))
        .expect("the failing entry registers");

    let failure = RuntimeLoader::new(&catalog)
        .resolve([reference("module-a", "1")])
        .expect_err("a factory error is a typed loader failure");
    assert!(
        std::error::Error::source(&failure).is_some(),
        "the factory error is preserved as the error source"
    );
    match failure {
        LoaderError::RegistrationConstructionFailed {
            reference: failed,
            source,
        } => {
            assert_eq!(failed, reference("module-a", "1"));
            assert_eq!(source.reason(), "the factory exploded");
        }
        other => panic!("expected a construction failure, got {other:?}"),
    }
}

/// Scenario K: catalog-declared dependency metadata that disagrees with
/// the produced module definition fails closed instead of silently
/// trusting one side.
#[tokio::test(flavor = "current_thread")]
async fn scenario_k_dependency_metadata_disagreement_is_rejected() {
    let registry = Recorder::new();
    let provider_recorder = registry.clone();
    let service_recorder = registry.clone();
    let catalog = ModuleCatalog::new()
        .register(CatalogEntry::new(
            reference("module-a", "1"),
            ok_factory(move || {
                ModuleRegistration::new(ModuleDefinition::new(id("module-a")))
                    .on_activate(provider_recorder.hook("activate:module-a", None))
            }),
        ))
        .expect("module-a@1 registers")
        .register(
            CatalogEntry::new(
                reference("module-b", "1"),
                ok_factory(move || {
                    ModuleRegistration::new(
                        ModuleDefinition::new(id("module-b")).depends_on(id("module-z")),
                    )
                    .on_activate(service_recorder.hook("activate:module-b", None))
                }),
            )
            .depends_on(reference("module-a", "1")),
        )
        .expect("module-b@1 registers");

    let failure = RuntimeLoader::new(&catalog)
        .resolve([reference("module-b", "1")])
        .expect_err("declared and produced dependency metadata disagree");
    assert_eq!(
        failure,
        LoaderError::IncompatibleEntry {
            reference: reference("module-b", "1"),
            reason: IncompatibleEntryReason::DeclaredDependenciesDisagree {
                declared: vec![id("module-a")],
                produced: vec![id("module-z")],
            },
        }
    );
    assert!(
        registry.events().is_empty(),
        "the disagreement is caught before any module activates, even \
         though the already-constructed provider exists in the closure"
    );
}

/// Scenario M: every resolution failure class leaves no activation effect
/// — no lifecycle hook runs, and graph-level failures never even
/// construct a registration.
#[tokio::test(flavor = "current_thread")]
async fn scenario_m_resolution_failures_have_zero_activation_effect() {
    let registry = Recorder::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let catalog = ModuleCatalog::new()
        .register(counting_provider_entry(&registry, &calls))
        .expect("module-a@1 registers");
    let recorder = registry.clone();
    let catalog = catalog
        .register(
            CatalogEntry::new(
                reference("module-b", "1"),
                ok_factory(move || {
                    ModuleRegistration::new(
                        ModuleDefinition::new(id("module-b"))
                            .depends_on(id("module-a"))
                            .with_reactive_capability(id("reactive"), "service", "reactive-v1"),
                    )
                    .on_activate(recorder.hook("activate:module-b", None))
                }),
            )
            .depends_on(reference("module-a", "1")),
        )
        .expect("module-b@1 registers");

    // Unknown root: the closure cannot even be formed.
    let failure = RuntimeLoader::new(&catalog)
        .resolve([reference("ghost", "1")])
        .expect_err("the unknown root is not in the catalog");
    assert!(matches!(failure, LoaderError::MissingReference { .. }));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "no registration constructed"
    );

    // Missing transitive reference and a cycle: graph-level failures.
    let recorder = registry.clone();
    let cyclic = ModuleCatalog::new()
        .register(
            CatalogEntry::new(
                reference("module-a", "1"),
                ok_factory(move || {
                    ModuleRegistration::new(
                        ModuleDefinition::new(id("module-a")).depends_on(id("module-a")),
                    )
                    .on_activate(recorder.hook("activate:module-a", None))
                }),
            )
            .depends_on(reference("module-a", "1")),
        )
        .expect("the self-cyclic entry registers");
    let failure = RuntimeLoader::new(&cyclic)
        .resolve([reference("module-a", "1")])
        .expect_err("a self-cycle cannot resolve");
    assert!(matches!(failure, LoaderError::DependencyCycle { .. }));

    // Duplicate roots: the request itself is rejected.
    let failure = RuntimeLoader::new(&catalog)
        .resolve([reference("module-a", "1"), reference("module-a", "1")])
        .expect_err("duplicate roots are rejected");
    assert!(matches!(
        failure,
        LoaderError::DuplicateRootReference { .. }
    ));

    // Version mismatch: fail-closed on exact references.
    let failure = RuntimeLoader::new(&catalog)
        .resolve([reference("module-a", "2")])
        .expect_err("exact version matching never substitutes");
    assert!(matches!(failure, LoaderError::MissingReference { .. }));

    assert_eq!(
        registry.count("activate:"),
        0,
        "no resolution failure path may activate anything"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "graph-level failures never construct a registration"
    );
}

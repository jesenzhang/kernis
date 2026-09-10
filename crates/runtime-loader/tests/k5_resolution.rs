//! K5 acceptance scenarios A-H: deterministic resolve feeding K4, catalog
//! insertion-order independence, root-order independence, unknown root,
//! missing transitive reference, version mismatch fail-closed, duplicate
//! catalog entry, and duplicate root reference.

mod k5_common;

use k5_common::{Recorder, id, ok_factory, ok_plugin, operation, reference, run_id, value};
use runtime_loader::{
    CapabilityDeclaration, CapabilityRequirement, CatalogEntry, EffectSemantics, HostConfig,
    LoaderError, ModuleCatalog, ModuleDefinition, ModuleRegistration, RuntimeLoader,
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

fn provider_entry(registry: &Recorder) -> CatalogEntry {
    let recorder = registry.clone();
    CatalogEntry::new(
        reference("module-a", "1"),
        ok_factory(move || {
            ModuleRegistration::new(
                ModuleDefinition::new(id("module-a"))
                    .with_declarative_capability(shared_declaration()),
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

fn task_entry() -> CatalogEntry {
    CatalogEntry::new(
        reference("module-c", "1"),
        ok_factory(|| {
            ModuleRegistration::new(
                ModuleDefinition::new(id("module-c"))
                    .depends_on(id("module-b"))
                    .with_task(driven_task()),
            )
        }),
    )
    .depends_on(reference("module-b", "1"))
}

fn standard_catalog(registry: &Recorder) -> ModuleCatalog {
    ModuleCatalog::new()
        .register(provider_entry(registry))
        .expect("module-a@1 registers")
        .register(reactive_entry(registry))
        .expect("module-b@1 registers")
        .register(task_entry())
        .expect("module-c@1 registers")
}

/// Scenario A: root `module-c@1` resolves the full transitive closure in a
/// deterministic order and composes through K4 into an activated Runtime.
#[tokio::test(flavor = "current_thread")]
async fn scenario_a_references_resolve_transitively_and_compose_through_k4() {
    let registry = Recorder::new();
    let catalog = standard_catalog(&registry);

    let resolved = RuntimeLoader::new(&catalog)
        .resolve([reference("module-c", "1")])
        .expect("the root reference closure resolves");

    assert_eq!(
        resolved.references().cloned().collect::<Vec<_>>(),
        vec![
            reference("module-a", "1"),
            reference("module-b", "1"),
            reference("module-c", "1"),
        ],
        "the transitive closure is the full resolved set in stable reference order"
    );
    assert_eq!(resolved.registrations().count(), 3);

    let plan = resolved
        .compose()
        .expect("the resolved registrations compose through K4");
    assert_eq!(
        plan.module_order(),
        [id("module-a"), id("module-b"), id("module-c")]
    );

    let assembly = plan
        .start(run_id("k5-a"), &HostConfig::new())
        .await
        .expect("the composed plan activates through K2");
    assert!(
        assembly
            .runtime()
            .capability_registry()
            .contains(&id("b-plugin")),
        "the resolved reactive plugin is registered during activation"
    );

    let report = assembly
        .shutdown()
        .await
        .expect("the activated assembly shuts down cleanly");
    assert!(report.is_success());
    assert_eq!(registry.count("activate:module-a"), 1);
    assert_eq!(registry.count("activate:module-b"), 1);
    assert_eq!(registry.count("dispose:module-a"), 1);
    assert_eq!(registry.count("dispose:module-b"), 1);
}

/// Scenario B: catalog insertion order cannot change the resolved set, the
/// K4 module order, the merged K2 canonical identity, or activation
/// behavior.
#[tokio::test(flavor = "current_thread")]
async fn scenario_b_catalog_insertion_order_changes_nothing() {
    let mut resolved_sets = Vec::new();
    let mut orders = Vec::new();
    let mut identities = Vec::new();
    let mut activation_events = Vec::new();

    for (index, permutation) in [[0usize, 1, 2], [2, 0, 1], [1, 2, 0]]
        .into_iter()
        .enumerate()
    {
        let registry = Recorder::new();
        let entries = [
            provider_entry(&registry),
            reactive_entry(&registry),
            task_entry(),
        ];
        let mut catalog = ModuleCatalog::new();
        for slot in permutation {
            catalog = catalog
                .register(entries[slot].clone())
                .expect("entry registers");
        }

        let resolved = RuntimeLoader::new(&catalog)
            .resolve([reference("module-c", "1")])
            .expect("the closure resolves regardless of insertion order");
        resolved_sets.push(resolved.references().cloned().collect::<Vec<_>>());

        let plan = resolved.compose().expect("composition validates");
        orders.push(plan.module_order().to_vec());
        identities.push(
            plan.definition()
                .identity()
                .expect("the merged definition has a stable identity"),
        );

        let assembly = plan
            .start(run_id(&format!("k5-b-{index}")), &HostConfig::new())
            .await
            .expect("the composition activates");
        assembly
            .shutdown()
            .await
            .expect("the assembly shuts down cleanly");
        activation_events.push(registry.events());
    }

    assert_eq!(resolved_sets[0], resolved_sets[1]);
    assert_eq!(resolved_sets[1], resolved_sets[2]);
    assert_eq!(orders[0], orders[1]);
    assert_eq!(orders[1], orders[2]);
    assert_eq!(identities[0], identities[1]);
    assert_eq!(identities[1], identities[2]);
    assert_eq!(activation_events[0], activation_events[1]);
    assert_eq!(activation_events[1], activation_events[2]);
}

/// Scenario C: the root request order is irrelevant when the logical set of
/// roots is the same.
#[tokio::test(flavor = "current_thread")]
async fn scenario_c_root_order_changes_nothing() {
    let registry = Recorder::new();
    let catalog = standard_catalog(&registry)
        .register(
            CatalogEntry::new(
                reference("module-d", "1"),
                ok_factory(|| {
                    ModuleRegistration::new(
                        ModuleDefinition::new(id("module-d"))
                            .depends_on(id("module-a"))
                            .with_task(
                                TaskDefinition::new(id("second-task"), "second task")
                                    .require_capability(CapabilityRequirement::new(
                                        id("shared"),
                                        "shared-v1",
                                    )),
                            ),
                    )
                }),
            )
            .depends_on(reference("module-a", "1")),
        )
        .expect("module-d@1 registers");

    let mut reference_lists = Vec::new();
    let mut orders = Vec::new();
    let mut identities = Vec::new();
    for roots in [
        [reference("module-c", "1"), reference("module-d", "1")],
        [reference("module-d", "1"), reference("module-c", "1")],
    ] {
        let resolved = RuntimeLoader::new(&catalog)
            .resolve(roots)
            .expect("the same logical root set resolves");
        reference_lists.push(resolved.references().cloned().collect::<Vec<_>>());
        let plan = resolved.compose().expect("composition validates");
        orders.push(plan.module_order().to_vec());
        identities.push(
            plan.definition()
                .identity()
                .expect("the merged definition has a stable identity"),
        );
    }

    assert_eq!(
        reference_lists[0],
        vec![
            reference("module-a", "1"),
            reference("module-b", "1"),
            reference("module-c", "1"),
            reference("module-d", "1"),
        ]
    );
    assert_eq!(reference_lists[0], reference_lists[1]);
    assert_eq!(orders[0], orders[1]);
    assert_eq!(identities[0], identities[1]);
}

/// Scenario D: an unknown root fails with a typed missing-reference error
/// and no K4 activation.
#[tokio::test(flavor = "current_thread")]
async fn scenario_d_unknown_root_fails_before_any_activation() {
    let registry = Recorder::new();
    let catalog = standard_catalog(&registry);

    let failure = RuntimeLoader::new(&catalog)
        .resolve([reference("unknown", "1")])
        .expect_err("the unknown root is not in the catalog");

    assert_eq!(
        failure,
        LoaderError::MissingReference {
            requested: reference("unknown", "1"),
            required_by: None,
            path: vec![reference("unknown", "1")],
        }
    );
    assert!(
        registry.events().is_empty(),
        "a resolution failure never activates any module"
    );
}

/// Scenario E: a missing transitive dependency names both the requested
/// reference and the requesting parent.
#[tokio::test(flavor = "current_thread")]
async fn scenario_e_missing_transitive_dependency_names_the_requester() {
    let registry = Recorder::new();
    let catalog = ModuleCatalog::new()
        .register(
            CatalogEntry::new(
                reference("module-b", "1"),
                ok_factory(|| {
                    ModuleRegistration::new(
                        ModuleDefinition::new(id("module-b")).depends_on(id("module-a")),
                    )
                }),
            )
            .depends_on(reference("module-a", "2")),
        )
        .expect("module-b@1 registers")
        .register(CatalogEntry::new(
            reference("module-a", "1"),
            ok_factory(|| ModuleRegistration::new(ModuleDefinition::new(id("module-a")))),
        ))
        .expect("module-a@1 registers");

    let failure = RuntimeLoader::new(&catalog)
        .resolve([reference("module-b", "1")])
        .expect_err("module-a@2 is not in the catalog");

    assert_eq!(
        failure,
        LoaderError::MissingReference {
            requested: reference("module-a", "2"),
            required_by: Some(reference("module-b", "1")),
            path: vec![reference("module-b", "1"), reference("module-a", "2")],
        }
    );
    assert!(
        registry.events().is_empty(),
        "a resolution failure never activates any module"
    );
}

/// Scenario F: an exact version mismatch fails closed; the loader never
/// substitutes a different version of the same module identity.
#[tokio::test(flavor = "current_thread")]
async fn scenario_f_version_mismatch_fails_closed() {
    let catalog = ModuleCatalog::new()
        .register(CatalogEntry::new(
            reference("module-a", "1"),
            ok_factory(|| ModuleRegistration::new(ModuleDefinition::new(id("module-a")))),
        ))
        .expect("module-a@1 registers");

    let failure = RuntimeLoader::new(&catalog)
        .resolve([reference("module-a", "2")])
        .expect_err("A@2 must not silently resolve to A@1");

    assert_eq!(
        failure,
        LoaderError::MissingReference {
            requested: reference("module-a", "2"),
            required_by: None,
            path: vec![reference("module-a", "2")],
        }
    );

    let resolved = RuntimeLoader::new(&catalog)
        .resolve([reference("module-a", "1")])
        .expect("the exact registered version still resolves");
    assert_eq!(
        resolved.references().cloned().collect::<Vec<_>>(),
        vec![reference("module-a", "1")]
    );
}

/// Scenario G: registering the same exact reference twice fails with a
/// typed duplicate error instead of silently overwriting.
#[tokio::test(flavor = "current_thread")]
async fn scenario_g_duplicate_catalog_entry_is_rejected() {
    let catalog = ModuleCatalog::new()
        .register(CatalogEntry::new(
            reference("module-a", "1"),
            ok_factory(|| ModuleRegistration::new(ModuleDefinition::new(id("module-a")))),
        ))
        .expect("the first entry registers");

    let failure = catalog
        .register(CatalogEntry::new(
            reference("module-a", "1"),
            ok_factory(|| ModuleRegistration::new(ModuleDefinition::new(id("module-a")))),
        ))
        .expect_err("the same exact reference cannot be registered twice");

    assert_eq!(
        failure,
        LoaderError::DuplicateCatalogEntry {
            reference: reference("module-a", "1"),
        }
    );
}

/// Scenario H: a root reference requested twice fails closed instead of
/// silently deduplicating host configuration, and the detected duplicate
/// is permutation-independent.
#[tokio::test(flavor = "current_thread")]
async fn scenario_h_duplicate_root_reference_is_rejected() {
    let catalog = ModuleCatalog::new();

    for roots in [
        [reference("module-a", "1"), reference("module-a", "1")],
        [reference("module-d", "1"), reference("module-d", "1")],
    ] {
        let failure = RuntimeLoader::new(&catalog)
            .resolve(roots)
            .expect_err("duplicate roots are rejected explicitly");
        match failure {
            LoaderError::DuplicateRootReference {
                reference: duplicate,
            } => {
                assert!(
                    [reference("module-a", "1"), reference("module-d", "1")].contains(&duplicate),
                    "the reported duplicate is one of the requested references"
                );
            }
            other => panic!("expected a duplicate root error, got {other:?}"),
        }
    }

    // The same duplicate is reported identically regardless of input order.
    let first = RuntimeLoader::new(&catalog)
        .resolve([
            reference("module-d", "1"),
            reference("module-d", "1"),
            reference("module-a", "1"),
        ])
        .expect_err("duplicates are rejected");
    let second = RuntimeLoader::new(&catalog)
        .resolve([
            reference("module-a", "1"),
            reference("module-d", "1"),
            reference("module-d", "1"),
        ])
        .expect_err("duplicates are rejected");
    assert_eq!(
        format!("{first}"),
        format!("{second}"),
        "duplicate detection is permutation-independent"
    );
}

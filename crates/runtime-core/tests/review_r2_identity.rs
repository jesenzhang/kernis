#![allow(missing_docs)]
use kernis_core::Id;
use runtime_core::{CapabilityDeclaration, CapabilityRequirement, RunDefinition, TaskDefinition};
use runtime_core::{FactoryRegistry, RunId, Runtime, RuntimeError};
use capability_graph::CapabilityValue;

fn id(value: &str) -> Id { Id::new(value).unwrap() }

#[test]
fn review_semantically_distinct_definitions_must_not_share_identity() {
    let common = RunDefinition::new()
        .with_capability(CapabilityDeclaration::new(id("x"), "service", "capabilities"))
        .with_task(TaskDefinition::new(id("capabilities"), "prerequisite one"))
        .with_task(TaskDefinition::new(id("x"), "prerequisite two"));
    let requires_capability = common.clone().with_task(
        TaskDefinition::new(id("z"), "target")
            .require_capability(CapabilityRequirement::new(id("x"), "capabilities")));
    let requires_tasks = common.with_task(
        TaskDefinition::new(id("z"), "target")
            .depends_on(id("capabilities")).depends_on(id("x")));
    requires_capability.validate().unwrap();
    requires_tasks.validate().unwrap();
    let a = requires_capability.canonical_identity().unwrap();
    let b = requires_tasks.canonical_identity().unwrap();
    eprintln!("A={a}\nB={b}");
    let mut factories = FactoryRegistry::new();
    factories.register(id("x"), "capabilities", |_| Ok(CapabilityValue::from_value(()))).unwrap();
    let run_id = RunId::new("collision-run").unwrap();
    let original = Runtime::start_from_definition(run_id.clone(), requires_capability, &factories).unwrap();
    let restored = Runtime::restore_from_definition(run_id, requires_tasks, &factories, original.store().clone());
    eprintln!("restore under different semantics accepted={}", restored.is_ok());
    assert!(matches!(restored, Err(RuntimeError::DefinitionMismatch { .. })),
        "restore must reject a changed prerequisite/capability contract");
    assert_ne!(a, b, "capability requirement and task prerequisites have different semantics");
}

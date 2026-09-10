# ADR 0005: Runtime and plugin composition layer

Status: accepted for K4 implementation

## Context

K2 established the stable `RunDefinition` plus process-local `FactoryRegistry`
reconstruction boundary, and K3 added the executor-neutral
`RuntimeDriver`/`RuntimeHandle` host seam. A host that wants a complete Kernis
Runtime today must still manually order a long chain: merge module
contributions into one `RunDefinition`, merge factories into one
`FactoryRegistry`, construct or restore `Runtime`, register plugin runtimes,
instantiate and start fibers, run module lifecycle hooks, reach the reactive
stable boundary, and only then construct the driver. On partial failure the
host must roll back exactly the process-local resources the half-built
composition actually acquired, in reverse order, without touching
pre-existing or sibling resources.

The existing authorities are already complete: `capability-graph` owns
capability scope/fiber/plugin lifecycle, `workflow-graph` owns topology,
`workflow-recovery` owns durable facts, and `runtime-core` owns coordination.
What is missing is a host-facing assembly layer that owns *ordering,
contribution ownership, and startup transaction semantics* — not any new
authority.

## Decision

### A composition crate, not a second runtime

Add a new host-facing crate `runtime-composition` that depends on
`runtime-core` and `capability-graph`. It is a composition policy layer over
their existing public APIs. It does not implement a second
`CapabilityRegistry`, `PluginRuntime`, workflow graph, durable store,
execution stream, or Runtime. Assembly order, contribution ownership, and
startup rollback belong to this crate and to no other crate.

### `RuntimeModule` vocabulary, not a second plugin concept

The composition unit is a module: stable `ModuleDefinition` plus
process-local `ModuleRegistration`, assembled by `CompositionBuilder` into a
validated `CompositionPlan`, activated into a `RuntimeAssembly`. A module may
contribute zero or more capability-graph `PluginRuntime` instances, but it is
not one: `PluginDefinition`/`PluginRuntime` remain the capability lifecycle
unit owned by `capability-graph`, while a module is the composition
contribution/ownership unit. The names `PluginDefinition` and `PluginRuntime`
are therefore not reused for composition types.

### Two-plane module model, preserving the K2 split

`ModuleDefinition` carries only stable semantics: module id, module
dependencies, task contributions (`runtime-core::TaskDefinition`), capability
slot contributions, and configuration requirements. It contains no future,
closure, `Scope`, handle, fiber, disposer, or mutex-owned object, and is
cold-reconstruction material. `ModuleRegistration` wraps a definition with
the process-local objects — factories, `PluginRuntime` handles, and lifecycle
hooks — and is never serialized or written to the `DurableStore`. This is the
K2 stable-declaration/process-local-executable split lifted to the module
plane.

### One capability slot, one declared ownership path

Each capability slot declares exactly one typed composition owner:

- `Declarative`: the capability participates in the merged K2
  `RunDefinition`, is constructed by a merged `FactoryRegistry` entry during
  `Runtime` construction/restore, and may be required by tasks. Its identity
  is part of the K2 replay identity.
- `Reactive`: the capability is published for the lifetime of a registered
  `PluginRuntime` fiber into the Runtime's reactive capability scope. It
  contributes nothing to `RunDefinition` and cannot be required by a task.

The rule that makes this decidable before activation: every capability
required by any contributed task must resolve to exactly one `Declarative`
slot; a required-but-`Reactive` capability, an unowned required capability,
or a factory/plugin mismatch fails the plan with a typed error. This is why
K2's `CapabilityDeclaration` public contract needs no extension: the ownership
plane lives in the composition vocabulary, not in the durable definition
format. A slot with both a factory and a fiber owner, or a fiber capability
colliding with a declarative slot, is rejected as an ownership conflict.
Reactive replacement, pinning, reconciliation, and withdrawal remain entirely
capability-graph (M2-C) authority; K4 only drives those existing APIs.

### Deterministic planning, side-effect-free validation

`CompositionBuilder::build()` performs validation and deterministic planning
only: duplicate module identity, dependency existence, cycle detection,
cross-module contribution conflicts (task id, capability slot, factory,
plugin), configuration conflicts, then a Kahn topological order with the
stable module `Id` as the tie-breaker at each level, then merge into one
`RunDefinition` (K2 keeps validation and canonicalization) and one
`FactoryRegistry`, with full ownership re-verification. Nothing acquires a
resource until `start`/`restore`. Registration order must not change
activation order, the merged definition, or behavior.

### A startup transaction over genuinely owned resources

Activation proceeds in deterministic module order: register plugins,
instantiate and start fibers, run the module `activate` hook; finally one
`reconcile()` must reach the stable reactive boundary. K4 records exactly
what each module successfully acquired. On failure it rolls back completed
activations in reverse order — dispose hook, fiber disposal, plugin
unregistration — continuing after cleanup failures and collecting all of
them into a structured rollback report. It never disposes resources a failed
module never acquired, never removes pre-existing or sibling-owned
resources, and never fabricates a rollback of the K2 durable bootstrap: if
Runtime construction/restore itself fails, K4 owns nothing and reports the
typed construction failure. `RuntimeAssembly` then exposes `runtime()`,
`runtime_mut()`, `into_driver(dispatcher)` (which hands the Runtime to the
K3 driver and returns a `CompositionHandle` that disposes the
composition-owned resources after driver shutdown), and a synchronous
`shutdown()` for the driverless path.

### Lifecycle hooks stay executor-neutral

Modules may declare only `activate` and `dispose` hooks, returning boxed
`Send` futures resolving to a typed result, following the K3 rule: standard
library futures at the public seam, no Tokio type crossing it.

### Loader stays out

Filesystem discovery, dynamic libraries, WASM, remote plugins, package
management, and watchers are the K5 boundary. K4 accepts in-process typed
registrations only.

## Consequences

- Host code assembles a Runtime through
  `CompositionBuilder::register(..).build()?` then `plan.start(run_id, config)`
  or `plan.restore(run_id, config, store)`, and shuts down through the
  assembly without touching internal registration order.
- Module identity, ownership variants, error taxonomy, and hook types become
  long-lived public API decisions and require independent review.
- The reactive plane remains outside the durable definition identity, exactly
  as in M2-C2: two processes with the same stable definitions and fresh
  registrations restore the same durable run.
- Cold reconstruction keeps working because `CompositionPlan::definition()`
  is a real K2 `RunDefinition` and every declarative slot has a validated
  factory before K2 is invoked.

## Rejected alternatives

- *A second `CapabilityRegistry`/`PluginRuntime` inside composition*: it
  would duplicate lifecycle authority and re-create the exact ambiguity K4
  exists to remove.
- *Extending K2 `CapabilityDeclaration` with an ownership flag*: the durable
  identity format does not need the distinction; a reactive slot is simply
  absent from `RunDefinition`, so the K2 contract stays frozen.
- *Placeholder factories or string-kind markers for reactive capabilities*:
  they hide which object publishes which slot and defeat fail-closed
  validation; typed slot variants replace them.
- *Rollback by wiping the registry, tearing down the whole scope, or
  rebuilding the Runtime*: those destroy resources the composition does not
  own and contradict the ownership ledger this ADR establishes.
- *Moving composition policy into `runtime-core`*: Runtime Core is the
  coordination authority and must stay a synchronous deterministic seam;
  assembly policy is host-facing and belongs in its own crate.

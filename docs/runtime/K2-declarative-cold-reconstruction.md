# K2: Declarative Configuration and Cold Reconstruction

Status: In Progress / Candidate

Base: `1e8cf70` (`main`, K1 integrated)

K2 adds a stable declaration boundary around the existing deterministic
Runtime Core. The declaration is an input to reconstruction; it is not a new
workflow, capability, or durable authority.

## Stable model

`runtime-core` exposes:

- `RunDefinition`: serializable-friendly task and capability declarations;
- `TaskDefinition`: logical task id, prerequisite topology, capability
  requirements, and optional effect semantics;
- `CapabilityDeclaration`: stable capability id/kind/definition identity and
  optional capability dependencies;
- `CapabilityRequirement`: a logical capability id paired with its stable
  `DefinitionIdentity`;
- `DefinitionIdentity`: the stable provider/configuration key used by a
  process-local factory;
- `FactoryRegistry`: a process-local map from `(capability id, definition
  identity)` to a constructor returning a fresh `CapabilityValue`.

`RunDefinition` owns stable execution semantics only. It does not own a
`Scope`, capability value or handle, fiber, future, closure, disposer,
executor, queue, stream, mutex-protected object, or task handle. The factory
registry owns construction logic only and is never written to the
`DurableStore`.

Leaf capabilities referenced by a task are materialized with a default kind
when no explicit `CapabilityDeclaration` is supplied. Explicit declarations
are used when a capability kind or capability dependency topology matters.
Capability dependency ordering is resolved by the existing
`capability-graph::CapabilityGraph` authority.

## Cold reconstruction

The new APIs are:

```text
Runtime::start_from_definition(...)
Runtime::start_from_definition_with_store(...)
Runtime::restore_from_definition(...)
```

They validate the complete declaration, check every required factory, and
only then construct a new root `Scope`. The existing `WorkflowGraph`,
`ReactiveCapabilityRuntime`, execution buffers, journal view, and attempt
pins are newly allocated. Restore loads durable state, compares the
declaration identity with durable authority, rebuilds workflow completion
facts through `WorkflowGraph::replay_with_facts`, and rebuilds capability pins
from fresh scope entries.

The existing `start_run*` and `restore_run` live-object APIs remain available
for compatibility and continue to use the existing authorities directly.

## Canonical identity

The canonical representation is explicitly versioned as
`kernis-run-definition-v1` and uses length-prefixed textual fields. It is not
based on Rust `Debug` output or an incidental serializer format. Task and
capability declarations, prerequisite edges, capability requirements, and
effect configuration are sorted by logical identity before canonicalization.

Task display labels are excluded: changing a human-readable label does not
change replay identity. Changes to task identity, topology, capability
identity/kind/dependencies, operation identity, or effect semantics do change
the identity. Equivalent authoring order produces the same identity.

## Failure behavior

Definition validation rejects duplicate task/capability declarations,
duplicate task configuration inputs, duplicate requirements, invalid topology,
duplicate operation identity, and capability identity disagreement with typed
`DefinitionError` values. A missing factory returns typed
`FactoryResolutionError::MissingFactory` before store creation or task
execution. A durable identity mismatch returns `RuntimeError::DefinitionMismatch`
before factory construction. Durable backend failures remain wrapped by the
existing typed `RuntimeError::Store` boundary.

Definition identity transitions continue to use the existing durable revision
CAS and idempotency protocol. A stale identity writer cannot overwrite a
newer durable identity; no configuration migration framework is introduced.

## Non-goals

K2 does not add filesystem configuration loading, dynamic discovery, plugin
ABI/SDK or lifecycle, hot reload, WASM, dynamic libraries, MCP, distributed
execution, async executor redesign, service location, secret management, or
serialization of arbitrary Rust closures and runtime objects.

## Evidence

The K2 candidate test suite covers canonical ordering and label semantics,
typed duplicate rejection, missing-factory fail-closed behavior, dependency
ordered capability construction, definition mismatch before construction,
revision-CAS protection for an A-to-B-to-A transition, and a child-process
physical-store write followed by fresh declarative reconstruction and effect
recovery.

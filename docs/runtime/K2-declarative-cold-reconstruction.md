# K2: Declarative Configuration and Cold Reconstruction

Status: Implemented on integrated `main` / independent review pending

Base: `1e8cf70` (`main`, K1 integrated)

The K2 implementation range (`017e2af`, the review-feedback compatibility fix
`5fe0ea4`, and the closeout record `684ae84`) is integrated on `main`, and
`origin/main` matches. GitHub Actions CI passed on the current `main` HEAD
`ebfc7d3` (run 34446922406). No independent-review APPROVE is recorded for
K2, so K2 remains unmarked as Integrated.

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
for compatibility and continue to use the existing authorities directly. They
retain the exact main-branch `kernis-workflow-replay-v1` canonicalization,
including its task-label field and legacy field ordering. They are separate
from the K2 definition canonicalizer; K2 does not silently reinterpret an
existing K1 durable identity.

Runtime identity provenance is tracked internally. A runtime started or
restored through the declarative APIs is marked as declarative, and the legacy
`configure_task` and `apply_workflow_mutation` APIs return typed
`RuntimeError::DeclarativeMutationUnsupported` before changing workflow state,
task configuration, or durable identity. A complete definition-based
reconfiguration protocol is outside K2.

## K1 compatibility and migration

`restore_run` remains the compatibility path for K1/main durable runs whose
identity starts with `kernis-workflow-replay-v1`; a fixed physical fixture
proves that the exact legacy identity still restores after K2. Conversely,
`restore_from_definition` requires the exact `kernis-run-definition-v1`
identity produced by `RunDefinition`. There is no automatic migration or
fallback matching between the two formats.

## RunDefinition canonical identity

Only `RunDefinition` uses the K2 canonical representation. It is explicitly
versioned as `kernis-run-definition-v1` and uses length-prefixed textual
fields. It is not based on Rust `Debug` output or an incidental serializer
format. Task and
capability declarations, prerequisite edges, capability requirements, and
effect configuration are sorted by logical identity before canonicalization.

Task display labels are excluded: changing a human-readable label does not
change replay identity. Changes to task identity, topology, capability
identity/kind/dependencies, operation identity, or effect semantics do change
the identity. Equivalent authoring order produces the same identity.

## Failure behavior

Definition validation rejects duplicate task/capability declarations,
duplicate requirements, invalid topology, duplicate operation identity, and
capability identity disagreement with typed `DefinitionError` values. The
legacy live-object task-configuration collection retains main's existing
last-write-wins behavior; declarative definitions have one validated task
declaration per logical task and do not route through that collection. A
missing factory returns typed
`FactoryResolutionError::MissingFactory` before store creation or task
execution. A durable identity mismatch returns `RuntimeError::DefinitionMismatch`
before factory construction, and legacy mutators are rejected on declarative
runtimes before a durable commit. Durable backend failures remain wrapped by
the existing typed `RuntimeError::Store` boundary.

Definition identity transitions continue to use the existing durable revision
CAS and idempotency protocol. A stale identity writer cannot overwrite a
newer durable identity; no configuration migration framework is introduced.

## Non-goals

K2 does not add filesystem configuration loading, dynamic discovery, plugin
ABI/SDK or lifecycle, hot reload, WASM, dynamic libraries, MCP, distributed
execution, async executor redesign, service location, secret management, or
serialization of arbitrary Rust closures and runtime objects.

## Evidence

The K2 candidate test suite covers canonical ordering and RunDefinition label
semantics, typed declarative duplicate rejection, the fixed main-branch legacy
identity fixture, missing-factory fail-closed behavior, dependency-ordered
capability construction, definition mismatch before construction, typed
declarative provenance rejection for legacy mutators, separate legacy and
declarative durable-boundary CAS transitions, and a child-process
physical-store write followed by fresh declarative reconstruction and effect
recovery.

Repository status reconciliation (2026-09-10, K4 preflight): earlier wording
described K2 as a candidate awaiting the K3 integration precondition. The
repository truth is that the K2 range is integrated on `main` with CI green
on the current `main` HEAD; the review-feedback compatibility fix `5fe0ea4`
is recorded repository evidence. The formal independent review remains
pending, and this reconciliation records repository-provable state only.

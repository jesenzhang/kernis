# M2-B Handoff: Durable Runtime Extension

Status: M2-B0 complete; M2-B1 in-memory durable-store/restart slice
implemented; concrete physical persistence adapter not started; M2-C1
Integrated / Closed at `589827af0156fa0d3f25f5bb6f4044f2be61b527`

M2-A keeps Cordis-derived Context, Registry, Fiber, and Effect state
process-local. The next milestone must persist authoritative facts without
serializing the in-memory Fiber object graph.

## Data that must remain durable

- workflow revisions and completion facts;
- effect intent;
- dispatch records and latest-dispatch identity;
- known outcomes;
- `OperationId` and `AttemptId` lineage;
- the recovery identity required to distinguish late outcomes from the latest
  dispatch;
- capability configuration identity when replay needs to select the same
  logical capability configuration.

## Observations that are not authoritative

- token streams;
- stdout/progress observations;
- telemetry;
- UI observations;
- disposable Execution Stream buffer contents.

## Runtime boundary

Capability Fiber lifecycle is process-local runtime state by default. A
restart after a process crash reconstructs the required runtime from durable
configuration and authoritative workflow/effect facts; it does not deserialize
old Fiber pointers, disposers, mutexes, or async tasks.

M2-B must preserve M1's latest-dispatch authority, late-outcome rejection,
exact `AttemptId` identity, capability pinning for in-flight attempts, and the
separation between WorkflowGraph, DurableJournal, the Runtime-owned reactive
capability coordinator, and Execution Streams. Reactive replacement and
withdrawal are process-local lifecycle changes; they do not rewrite durable
operation, attempt, or replay/config identity.

## Runtime Kernel Store Port

Runtime is parameterized over the existing `DurableStore` contract. The
deterministic `InMemoryDurableStore` remains the default used by
`Runtime::start_run(...)`; callers may supply another synchronous store through
`Runtime::start_run_with_store(...)` and use the same store type for restore.
No physical persistence adapter exists yet, and this boundary does not claim
physical durability.

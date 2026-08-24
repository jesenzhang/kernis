# M2-B Handoff: Durable Runtime Extension

Status: M2-B0 complete; M2-B1 in-memory durable-store/restart slice and
completion/replay contract closure implemented; physical persistence is owned
by Stage 2 K1; M2-C1
Integrated / Closed at `589827af0156fa0d3f25f5bb6f4044f2be61b527`

M2-A keeps Cordis-derived Context, Registry, Fiber, and Effect state
process-local. The next milestone must persist authoritative facts without
serializing the in-memory Fiber object graph.

## Data that must remain durable

- typed workflow completion facts with attempt lineage;
- workflow replay identity for the supplied topology/configuration;
- supplied topology and mutation history remain `WorkflowGraph` inputs; the
  store does not duplicate scheduler semantics;
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

Completion has two explicit boundaries. `DurableStore` records a typed
completion fact tied to an admitted attempt; `WorkflowGraph` validates and
applies the fact's topology prerequisites. Runtime commits the durable fact
before replacing its local graph and emitting `TaskCompleted`. Restore first
validates the canonical workflow replay identity, then replays completion
facts in durable append order. A mismatch or invalid prerequisite fails closed
as an invariant error.

The replay identity is the current canonical identity of an evolving
workflow/configuration, not a topology revision. Approved runtime topology or
configuration changes update it through their own expected-revision durable
commit; transition-specific idempotency keys prevent an old identity update
from being replayed after an A-to-B-to-A transition.

`StoreError` keeps domain conflicts separate from backend-unavailable, I/O,
and persistent-data-corruption categories. Runtime preserves these categories
through `RuntimeError::Store`.

## Runtime Kernel Store Port

Runtime is parameterized over the existing `DurableStore` contract. The
deterministic `InMemoryDurableStore` remains the default used by
`Runtime::start_run(...)`; callers may supply another synchronous store through
`Runtime::start_run_with_store(...)` and use the same store type for restore.
The M2-B contract itself remains backend-neutral. Stage 2 K1 supplies the
`FileDurableStore` redb adapter and preserves the same completion idempotency,
expected-revision CAS, replay identity validation, and outcome-to-completion
crash-window semantics. Neither boundary claims unqualified fsync/power-loss
durability or persists a database/WAL copy of the runtime object graph.

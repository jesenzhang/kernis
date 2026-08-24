# M2-B Durable State Model

Status: M2-B0 complete; M2-B1 in-memory durable-store/restart slice and
completion/replay contract closure implemented; concrete physical persistence
adapter not started

This document defines the boundary between durable correctness state and
process-local runtime state. The default rule is:

> A Fiber object graph is reconstructable runtime state, not durable truth.

## Authority boundary

| Category | State | Owner / rule |
| --- | --- | --- |
| Durable | Run identity | Stable `RunId` for the workflow execution. |
| Supplied | Workflow topology and mutation history | `WorkflowGraph` owns the topology and revision; restore receives the unfinished graph and uses the durable replay identity to validate it. |
| Durable | Workflow completion facts | `DurableStore` retains typed task/attempt completion facts; `WorkflowGraph` remains the topology, prerequisite, and local completion authority when those facts are replayed. |
| Durable | Workflow replay identity | A canonical identity covers the supplied topology and runtime configuration; restore rejects a mismatch before creating process-local runtime objects. |
| Durable | Operation ownership | One `TaskId` owns one logical `OperationId`; duplicate ownership is rejected. |
| Durable | Effect intent | `DurableJournal` records the logical effect and its semantics before external dispatch. |
| Durable | Dispatch record | `DurableJournal` records the latest `OperationId`/`AttemptId` dispatch identity. |
| Durable | Outcome record | Outcome is tied to the exact `AttemptId`; late outcomes cannot replace the latest attempt. |
| Durable | Attempt identity | `AttemptId` is historical and never reused for a new external dispatch. |
| Durable | Cancellation fact | Cancellation is retained and interpreted against the latest dispatch. |
| Durable | Recovery classification inputs | Intent, dispatch, outcome, semantics, ownership, and revisions are sufficient inputs to classify recovery. |
| Durable | Capability replay/config identity when required | Persist the stable logical capability identity needed to validate a replay; the process-local pin continues to retain exact `Generation` and `EntryId`, but no live handle is durable. |
| Reconstructable | `CapabilityContext` and `ReactiveCapabilityRuntime` | Rebuild one fresh Runtime-owned coordinator from supplied capability configuration and scope topology. |
| Reconstructable | Registry runtime cache | Re-register runtime metadata in the fresh coordinator; registry membership is not the workflow authority. |
| Reconstructable | Fiber instances | Recreate from configuration and durable facts; do not deserialize old Fibers. |
| Reconstructable | Effect stacks | Recreate only for the current process epoch; old disposer closures are not replayed as objects. |
| Reconstructable | Capability handles | Re-resolve or reconcile against the pinned identity; a live pointer is never durable. |
| Reconstructable | Scheduler queues | Rebuild from ready workflow facts, pending operations, timers, and worker ownership. |
| Reconstructable | In-memory indexes | Rebuild from the authoritative store. |
| Non-authoritative observation | Model token deltas | May be lost or coalesced without changing correctness. |
| Non-authoritative observation | stdout/stderr and progress | Diagnostics only. |
| Non-authoritative observation | UI progress and telemetry | Diagnostics only. |
| Non-authoritative observation | Execution Stream buffers | Backpressure/loss does not change workflow facts, effect facts, Fiber authority, or capability identity. |

## What is reconstructed

Recovery has three separate jobs:

1. Reconstruct correctness state: supplied workflow topology/revision plus
   durable completion facts, operation ownership, intent, dispatch, outcomes,
   cancellation, retry classification, and timers.
2. Reconstruct runtime process objects: contexts, registry entries, Fibers,
   effect scopes, handles, queues, and worker-local indexes.
3. Reconstruct observations only when useful: stream consumers may resume from
   a validated stream identity/sequence, but observation replay is not required
   for workflow or effect correctness.

The second and third jobs must not be used to fill gaps in the first. In
particular, a Fiber restart is not a durable-effect retry, and a recovered
Execution Stream is not evidence that an effect completed.

## Durable transition boundary

The proposed first persistence boundary is a typed transactional commit:

```text
validate expected workflow revision and operation ownership
  -> append intent/cancellation/dispatch facts as one commit
  -> advance monotonic correctness revision
  -> commit
  -> perform external dispatch
  -> commit outcome with OperationId + AttemptId compare-and-set
  -> commit task completion with AttemptId lineage
  -> apply workflow completion locally
  -> emit TaskCompleted observation
```

The external effect is never performed before its intent/dispatch authority is
committed. If the process crashes after the dispatch commit but before the
outcome commit, the outcome is unknown. Recovery must classify that state from
the durable facts, not guess from a missing Fiber or stream event.

The replay identity is the current canonical identity for an evolving
workflow/configuration. Runtime topology/configuration changes update it in a
separate CAS-protected durable mutation whose idempotency key includes the
current store revision, so a previously used target identity cannot replay an
old transition.

If the outcome is committed but completion is not, restore replays the known
outcome and commits completion without another dispatch. If completion is
already committed, restore reconstructs the completed graph and the scheduler
returns `Idle` without appending another completion fact. Completion replay is
ordered by durable append order, so prerequisite violations fail closed.

The first backend should be an embedded transactional journal plus materialized
state/snapshot store. It should expose append-before-effect, idempotent append,
compare-and-swap on revision, and a monotonic revision. A distributed store,
provider SDK, WAL implementation, and SQL adapter are outside this in-memory
contract closure.

## Crash and cancellation decisions

### Pre-dispatch cancellation

The sequence is:

```text
attempt created
TaskStarted observed
intent persisted
dispatch not recorded
cancellation arrives
```

The attempt is durable once the attempt admission/intent boundary commits;
`TaskStarted` by itself is not the authority. Cancellation is durable at the
same correctness boundary. The cancelled attempt remains a historical fact and
is never reused. If the logical operation must later execute, recovery creates a
new `AttemptId` under the same `OperationId`; it does not mutate the cancelled
attempt into a live one.

This preserves the M1 rule that cancellation before dispatch prevents the
external call, while preserving enough history to explain why an observed
attempt did not dispatch.

### Operation and attempt identity

`OperationId` answers “which logical effect is this?” and stays stable across
retry, cancellation, and recovery. `AttemptId` answers “which dispatch attempt
produced this record?” and is unique per dispatch. `Capability Generation` and
`EntryId` answer “which exact capability implementation did this attempt pin?”
and are independent of recovery retry identity. A worker execution/lease ID,
when added, answers “which worker currently owns processing?” and must not be
used as either operation or attempt identity.

### Execution retry item

The current `retry_execution_event(item)` shape should evolve to
`retry_pending_execution_event()` or gain strict stream-id/event-id/sequence
validation. Transport replay retains the original lossless item identity;
external-effect retry creates a new `AttemptId`. The two operations must not be
merged under one generic retry API.

## Gate answers

1. Directly reusable mechanisms: durable intent before effect, explicit
   operation/attempt identities, event/journal replay, idempotent append,
   monotonic revisions, duplicate-event filtering, retry timer identity, and
   stale-worker fencing.
2. Rust adaptation required: typed commit requests, ownership of serialized
   state, explicit CAS errors, `Result`-based recovery classification,
   capability pin/config serialization, and separation of runtime objects from
   records.
3. Kernis gaps: a durable store, a durable timer/lease boundary,
   worker-execution identity, non-idempotent provider reconciliation protocol,
   and a validated retry-pending-execution API.
4. Minimum authoritative persistent state: `RunId`, supplied workflow replay
   identity, completion facts, task/operation ownership, intent, latest
   dispatch, outcomes, `AttemptId` lineage, cancellation, recovery inputs, and
   capability replay/config identity where needed. The store does not copy the
   workflow topology or scheduler semantics.
5. Fiber is not durable because it contains process-local handles, mutexes,
   async tasks, effect closures, and caches; only the configuration and facts
   required to reconstruct it are durable.
6. Execution Streams are not durable authority because they are bounded,
   lossy/coalescing observations and M1 already proves that stream loss must
   not mutate workflow/effect truth.
7. `OperationId` is stable logical effect identity; `AttemptId` is unique
   dispatch identity and changes on an external-effect retry.
8. Cancellation is persisted before it can authorize or prevent dispatch, in
   the same transactional authority boundary as intent/ownership; after
   dispatch it is appended without erasing the dispatch or outcome facts.
9. The first persistence version does not need general event sourcing. A
   typed journal/fact interface plus materialized state and monotonic revisions
   gives the required recovery semantics with a smaller proof surface.
10. The first backend should be an embedded transactional journal/snapshot
    store behind a trait; a concrete SQLite or embedded-log adapter is deferred.
11. The transaction boundary is expected-revision validation plus atomic
    intent/ownership/cancellation/dispatch fact commit; outcome CAS and
    completion-fact commits are subsequent idempotent boundaries, with local
    workflow application and `TaskCompleted` observation after the completion
    commit.
12. M2-B1 should first implement the minimal `DurableStore` interface and
    deterministic in-memory conformance backend, then crash-boundary tests,
    before selecting a concrete backend.

## M2-B1 implementation status

The M2-B1 slice is implemented with a synchronous typed `DurableStore` seam
and deterministic `InMemoryDurableStore` adapter. Its `StoreRevision` is
independent from workflow topology revision. Attempt admission is an
append-only fact before `TaskStarted`; dispatch and outcome facts retain exact
`AttemptId` lineage, and recovery authority follows the latest dispatch while
older outcomes remain history. Completion is an append-only fact tied to the
admitted attempt, with idempotent replay and prerequisite validation delegated
to `WorkflowGraph`. Cancellation is retained and conflicting cancellation
lineage is rejected without rewriting prior facts. Capability replay identity
is stored separately from process-local generation, `EntryId`, and live
handles. The store also exposes backend-neutral unavailable, I/O, and
corruption error categories.

Runtime restart tests reconstruct a new Runtime-owned reactive coordinator
from a cloned durable store view with fresh streams, registry state, fibers,
and capability handles. No physical persistence adapter is selected in this
milestone.

## M2-B1 follow-up boundaries

- Select a physical persistence adapter only in a later milestone.
- Keep provider reconciliation, worker leasing/fencing, timers, compaction,
  and distributed execution outside this slice.
- Preserve capability pinning at `TaskAttempt` admission and keep
  Fiber/streams outside the durable authority.

# ADR 0004: Explicit asynchronous execution boundary

Status: accepted for K3 implementation

## Context

Kernis Runtime Core already has a synchronous deterministic execution model.
`Runtime<S>` owns the coordination needed to advance a workflow, establish the
durable dispatch boundary, record known effect outcomes, and apply recovery
decisions. The durable store remains synchronous and compare-and-swap
protected. K3 needs an asynchronous host without moving workflow, capability,
stream, or durable authority into an executor-specific abstraction.

The seam must also preserve the append-before-effect rule: an external effect
may be called only after `Runtime::dispatch_effect` has durably committed the
dispatch and its exact `AttemptId`. An async wait must not retain a borrow or a
guard over `Runtime<S>`.

## Decision

Add an executor-neutral `RuntimeDriver<S, D>` module in `runtime-core`.

* One `RuntimeDriver` owns one `Runtime<S>` and one `EffectDispatcher`.
* Callers receive a clonable `RuntimeHandle`, not an `Arc<Mutex<Runtime<S>>>`.
  The handle submits typed wake, cancellation, recovery, observation-drain,
  explicit dispatch, and shutdown commands through a small waker-backed
  mailbox.
* Each command returns a standard-library `Future`. The implementation uses
  `std::future::Future`, `Waker`, and synchronization primitives only; Tokio or
  another executor is allowed in adapters and tests but no executor-specific
  type crosses the public runtime-core seam.
* The driver serializes commands and advances the existing synchronous
  `Runtime` at command boundaries. `drive()` is one deterministic scheduler
  step. If that step yields an effect, the driver durably dispatches it, awaits
  the dispatcher future, and records a known outcome before completing the
  command. Unknown external results remain unknown durable facts.
* `EffectDispatchRequest` contains the `RunId`, task id, logical
  `OperationId`, exact `AttemptId`, and `EffectSemantics`. The dispatcher has no
  authority to choose or replace an attempt identity. A driver invariant check
  rejects a mismatch before the external call.
* `EffectDispatcher` returns a boxed `Send` future with a typed
  `EffectDispatchError`. An error means the external result is unknown because
  the durable dispatch boundary has already been crossed; it is never silently
  converted into a known failure.
* The driver owns the await boundary. It copies the typed dispatch request,
  releases all `Runtime` borrows, awaits the dispatcher, and reacquires the
  owner only to append the known outcome. Cancellation commands queued during
  an in-flight dispatch are observed only after that dispatch resolves, so a
  cancellation cannot erase or race the external ownership fact.
* Shutdown is an explicit command. It admits no later commands after a
  successful shutdown, settles known outcomes without re-execution, reports
  prepared-but-not-dispatched work, reports idempotent unknown work as
  pending, reports non-idempotent unknown work as requiring reconciliation,
  and keeps a known failure explicitly observed rather than inventing a retry.
  A shutdown inspection/store failure leaves the driver alive so the caller
  can retry; ownership is released exactly once only on a successful shutdown
  response.
* Lossless execution-stream backpressure is retained inside the driver. A
  rejected lifecycle item is returned in the typed drive result and kept for
  `retry_execution_event` after the caller drains the stream. Durable facts and
  attempt identities therefore do not depend on observation-buffer capacity.

The existing synchronous Runtime APIs remain supported and remain the
reference semantics. The driver does not introduce automatic retry policy,
parallel actor ownership, async durable-store operations, plugin composition,
or capability lifecycle replacement.

## Consequences

The external interface is a deep ownership seam: callers learn one handle and
typed results while serialization, append-before-effect ordering, exact
attempt lineage, backpressure retention, and shutdown classification stay
local to the driver implementation. Concurrent wakeups are harmlessly
serialized; they do not create a second Runtime owner or duplicate a dispatch.

The async host is executor-neutral but still requires `S: Send` and
`EffectDispatcher: Send` when used as an owned asynchronous task. A dispatcher
adapter must choose how to classify provider failures and must preserve the
operation's idempotency semantics when handling an unknown result. Distributed
leases, remote workers, async store implementations, and provider-specific
reconciliation remain later milestones or adapter concerns.

## Rejected alternatives

* `Arc<Mutex<Runtime<S>>>`: makes every caller responsible for serialization,
  encourages guards to cross `.await`, and makes exact dispatch ownership
  difficult to prove.
* Tokio channels, `JoinHandle`, or `tokio::sync::Mutex` in the public seam:
  they would make the runtime contract depend on one executor and pull K3
  toward lifecycle semantics that are not required here.
* Automatically retrying unknown outcomes in the driver: this would change
  the existing explicit recovery policy and could duplicate a non-idempotent
  external side effect.
* Running multiple driver tasks over one Runtime: parallel ownership would
  require a new actor/scheduling contract and is outside K3.

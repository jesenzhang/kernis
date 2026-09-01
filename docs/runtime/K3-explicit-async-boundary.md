# K3: Explicit Asynchronous Execution Boundary

Status: In Progress / Candidate

Integrated K2 base: `684ae84a3da94472e4b2263a5c3bfd734574c96f` on `main`

K3 adds an executor-neutral host seam around the existing synchronous
`Runtime<S>`. It does not replace the deterministic Runtime model, make the
durable store asynchronous, or introduce a scheduler/actor authority.

## Interface

`runtime-core` exposes:

- `RuntimeDriver<S, D>`: the single owner of one synchronous `Runtime<S>` and
  one `EffectDispatcher`;
- `RuntimeHandle`: a cloneable `Send + Sync` command handle with awaitable
  `drive`/`wake`, explicit effect dispatch, recovery, cancellation, stream
  drains, and shutdown operations;
- `EffectDispatchRequest`: `RunId`, task id, logical `OperationId`, exact
  durable `AttemptId`, and `EffectSemantics`;
- `EffectDispatcher`: an adapter returning a standard-library `Future` that
  resolves to a known outcome or a typed unknown-outcome error;
- `DriveResult` and `ShutdownStatus`: typed results for known/unknown effect
  outcomes, lossless lifecycle backpressure, and shutdown classification.

The public seam never exposes `Arc<Mutex<Runtime<S>>>`, Tokio handles, Tokio
channels, or a runtime borrow across an external effect await. A dispatcher
error is conservative: because dispatch was durably committed first, it leaves
the operation in `OutcomeUnknown` and is not fabricated into a known failure.

## State and ordering

Commands are serialized by the one driver owner. A wakeup performs one
deterministic synchronous step. When that step prepares an effect, the driver
commits `dispatch_effect` and validates the exact `AttemptId` before invoking
the dispatcher. It records a known result only for that request's attempt.
Commands submitted during an in-flight effect remain queued until that effect
resolves, which makes cancellation an explicit barrier rather than a race
with external ownership.

Shutdown settles known successful outcomes without re-execution. It classifies
un-dispatched durable intents as `PendingDispatch`, idempotent unknown
outcomes as `PendingUnknown`, non-idempotent unknown outcomes as
`ReconciliationRequired`, and known failures as `ObservedFailure`. A
successful shutdown closes admission and releases the runtime exactly once; a
store or lifecycle-buffer failure leaves the owner alive for a drain/retry.

## Evidence

The K3 focused suite is `cargo test -p runtime-core --test k3_async
--all-features`. It proves:

- synchronous/async durable-fact equivalence;
- one dispatch for concurrent wakeups;
- cancellation before dispatch, behind an in-flight unknown outcome, and
  after a known outcome;
- explicit idempotent retry with the same logical operation and a new attempt;
- non-idempotent unknown-outcome reconciliation;
- prepared-work shutdown classification;
- lossless lifecycle backpressure pause/resume with durable attempt and event
  lineage preserved; and
- `FileDurableStore` restart with preserved `PendingDispatch`,
  `PendingUnknown`, `ReconciliationRequired`, and `ObservedFailure`
  classifications, plus no duplicate external call after a known outcome.

Focused compatibility evidence: the K3 suite has 18 passing tests; the K1
physical suite has 9; the K2 declarative suite has 14; and
`cargo test -p workflow-recovery --all-features` has 51. The workspace suite,
format, clippy, graph-lab, and final diff checks are recorded only after the
final checkpoint verification below.

## Candidate checkpoint verification

- `cargo fmt --all -- --check`: PASS
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`:
  PASS, 0 errors
- `cargo test --workspace --all-features`: PASS, 245 tests
- `cargo run -p graph-lab`: PASS
- `git diff --check 684ae84a3da94472e4b2263a5c3bfd734574c96f...HEAD`: PASS

The physical restart cases include prepared work, idempotent unknown work,
non-idempotent unknown work, and known failure; the known-success case also
proves that a reopened `FileDurableStore` does not call the external adapter a
second time. These are local candidate results, not CI or integration claims.

This document remains a candidate record until the final verification and
independent review are complete. K3 is not marked integrated by this branch.

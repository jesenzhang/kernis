# ADR 0009: Driver-exit owner binding and cancellation-safe composition cleanup

Status: proposed for the K6/R2 repair (Stage 2 / R2 closeout); it tightens the K4 lifecycle
contract in `docs/runtime/R2-KERNEL-CONTRACT.md` "Shutdown (the supported lifecycle)" and
`docs/architecture/0005-k4-runtime-composition.md`, and records a pre-1.0 breaking signature
change per `COMPATIBILITY.md` §1.

## Context

`RuntimeAssembly::into_driver` splits one activation into three objects: the
[`RuntimeDriver`] (owns the `Runtime`), the [`RuntimeHandle`] (typed commands), and the
[`CompositionHandle`] (composition-owned hooks, fibers, and plugin registrations). K4's rule
is that composition cleanup is legal only while the composition can name the authority it is
cleaning through: either the driver completed an orderly shutdown and returned its
`DriverExit` (registry still live), or the driver owner was genuinely lost and the registry
went with it (best-effort release, honestly reported).

The 2026-09-28 K6/R2 review found two holes in that rule
(`docs/reviews/2026-09-28-K6-R2-rereview.md` R2-02 and R2-03, with counterexamples and logs):

1. **The orderly path accepted any driver's exit.** `DriverExit` carried only
   `Runtime` + `ShutdownStatus`, and `CompositionHandle::dispose_after_driver` never checked
   where the exit came from. Feeding composition A a shutdown exit from an unrelated
   composition B disposed A's `dispose` hooks while A's driver was still `Running`, reported
   `rollback.is_success() == true`, and handed back B's runtime. With overlapping plugin ids
   the same path would also unregister from the wrong registry. The owner-loss guard added
   earlier proved the *state* of the exact driver, so the orderly entry point was the
   remaining way to make composition cleanup happen without the runtime owner's consent.
   `RunId` equality would not close this: two compositions can share a run id, and the
   reviewed counterexample only needs *some* exit of the right store type.
2. **Cancelling a cleanup future destroyed the ledger and lied about it.**
   `release_after_owner_loss` moved the whole owned-module ledger into a temporary future and
   `cleanup_modules` popped each module before awaiting its dispose hook. Dropping the future
   (host timeout, task cancellation, unwind) therefore discarded both the untouched modules
   and the partial report, and the retry answered
   `OwnerLossReleaseError::AlreadyReleased` - whose public meaning is "released once, nothing
   remains" - while the un-started modules had disposed nothing.

Both are the same underlying mistake: process-local authority and cleanup *progress* were
carried by values a caller could hand around or throw away, instead of by the one owner that
is supposed to hold them.

## Decision

### 1. `DriverExit` carries an unforgeable identity of the exact driver owner

- `DriverOwner` is a new opaque runtime-core type: a private clone of the same
  `CommandMailbox` handle the K3 driver-owner guard marks on drop/abort. It is the object
  identity of the mailbox that already is the single driver-owner truth (K3), so it introduces
  no second lifecycle authority and no process-wide owner registry.
- It has no public constructor, no `From`/`Deserialize`/textual form, and exposes exactly one
  operation, `DriverOwner::same_driver_as(&self, other) -> bool` (pointer identity). There is
  no bool/string "proof" a host could fabricate, and `RunId` is never used as instance
  identity.
- `RuntimeHandle::owner()` returns the token, `DriverExit::owner()` returns the exit's token,
  and `RuntimeAssembly::into_driver` binds the returned `CompositionHandle` to the token of the
  handle it returns. Because `CompositionHandle` is only ever produced by `into_driver`, that
  binding cannot be pointed at another driver.

### 2. Orderly cleanup validates the owner before touching anything

`dispose_after_driver` becomes:

```rust
pub async fn dispose_after_driver<S>(
    &mut self,
    exit: DriverExit<S>,
) -> CompositionReleaseOutcome<S>
where S: DurableStore + Send

pub enum CompositionReleaseOutcome<S = InMemoryDurableStore> {
    Released(CompositionDriverShutdown<S>),
    Rejected(DriverExitRejection<S>),
}

pub enum DriverExitRejection<S = InMemoryDurableStore> {
    /// The exit belongs to a different driver owner. Nothing was disposed,
    /// this handle's ledger is untouched, and the exit is returned intact.
    ForeignDriverExit { exit: DriverExit<S> },
    /// This handle's resources were already released exactly once. The exit
    /// is returned intact so the caller still holds its own shutdown fact.
    AlreadyReleased { exit: DriverExit<S> },
}
```

- The same-owner check happens before any hook, fiber disposal, or registry operation.
- A rejection consumes nothing: no hook runs, no fiber is disposed, no registration is
  unregistered, and `DriverExitRejection` hands the `DriverExit` back by value so the
  composition that actually owns it can still release through its own orderly path. That is
  why the outcome carries the exit instead of returning a plain error.
- Two already-`Shutdown` instances of the same store type with the same configuration and the
  same run id are still distinguished, because identity is the mailbox object, not any
  value inside it.

### 3. Cleanup progress lives in the handle, and an in-flight step is never restarted

- `CompositionHandle` keeps the cleanup ledger: the not-yet-started modules in reverse
  activation order, the partially cleaned current module (remaining fibers and remaining
  registrations), the accumulated `RollbackReport`, and the **in-flight step's own future**
  (`Pin<Box<dyn Future ... + Send>>`) for the two awaits that can block indefinitely - the
  armed `dispose` hook and fiber disposal. Both step futures own their subject, so they stay
  `'static` and survive the cancellation of the caller's future.
- The sweep is a hand-written resumable future that borrows the ledger and the authority and
  polls the parked in-flight future **in place**. Consequently: a module is removed from the
  ledger only after all of its owned steps completed; failures are recorded as they happen;
  and dropping the caller's future loses neither the remaining resources nor the facts already
  collected.
- `release_after_owner_loss` therefore never answers `AlreadyReleased` for work that is
  outstanding. `AlreadyReleased` now means exactly one thing: the entire cleanup flow for this
  handle completed once. A re-entry resumes the parked step instead of re-calling a started
  non-idempotent hook, and the eventual report keeps the earlier failures.
- Registry removals cannot be parked (`registry.remove` borrows the runtime's registry), so an
  interruption inside a removal is recorded as an interrupted step with its registration still
  outstanding, and is never retried blindly.
- Interruption of the orderly sweep also proves the registry authority is unreachable: the
  `Runtime` the sweep borrowed was dropped with the cancelled future. The ledger records that
  fact, and `release_after_owner_loss` accepts exactly two preconditions - the bound mailbox
  reports `OwnerDropped`, or this handle's own ledger records an interrupted orderly sweep.
  Only a real interruption can set the second condition, so this widens no authority: a foreign
  exit still cannot start a sweep, and a composition whose driver is alive still cannot
  best-effort-dispose while its runtime owns the registry. Registrations that can no longer be
  unregistered keep being reported as `CleanupResource::PluginRegistration` failures, so a
  report never claims orderly completion it could not perform.

### 4. Breaking-change and migration note (pre-1.0)

`dispose_after_driver` changes from `async fn(self, DriverExit<S>) ->
CompositionDriverShutdown<S>` to `async fn(&mut self, DriverExit<S>) ->
CompositionReleaseOutcome<S>`. Per `COMPATIBILITY.md` §1 this needs a migration note plus a
repository-wide search proving no supported path keeps needing the old form:

- Repository-wide call sites at base `d3818f5` (all updated in this repair, none kept on the
  old form): `crates/runtime-loader/examples/r2_host.rs` (2),
  `crates/runtime-loader/examples/host_end_to_end.rs` (1),
  `crates/runtime-loader/tests/k6_r2_end_to_end.rs` (4),
  `crates/runtime-loader/tests/k5_composition.rs` (1),
  `crates/runtime-composition/tests/k4_composition.rs` (1),
  `crates/runtime-composition/tests/k4_compat.rs` (1),
  `crates/runtime-composition/tests/k4_cleanup_ownership.rs` (3).
- Migration for a host: match the outcome. `Released(shutdown)` is the previous behavior
  unchanged; `Rejected(rejection)` means nothing was disposed and `rejection.into_exit()`
  returns the exit for its owning composition.
- Additive surface: `DriverOwner`, `RuntimeHandle::owner`, `DriverExit::owner`,
  `CompositionReleaseOutcome`, `DriverExitRejection`. Per ADR 0007 §2 all of them are threaded
  `runtime-core` -> `runtime-composition` -> `runtime-loader`.
- `OwnerLossReleaseError` keeps its variants; `AlreadyReleased` is re-documented to the
  completion-only meaning above, and `OrderlyShutdownCompleted` keeps its guidance except for
  the one new precondition recorded in the ledger.

## Consequences

- The supported lifecycle in `R2-KERNEL-CONTRACT.md` gains the binding step explicitly:
  `DriverExit -> dispose_after_driver` is only accepted by the composition whose `into_driver`
  produced that driver, and rejection is inert and reversible.
- Cancellation of a cleanup future becomes a recoverable event with a defined meaning instead
  of a silent loss: resources stay reachable, started steps stay started, and the report stays
  truthful.
- `CompositionHandle` is now stateful across awaits, so its documentation must state the
  re-entry rules; the ledger is private and nothing about it is serializable.
- Hosts that own a `RuntimeAssembly` and call `RuntimeAssembly::shutdown()` keep the
  pre-existing contract: that path consumes the assembly, so cancelling its future drops the
  runtime with the future and no ledger survives to resume. It is documented as a limitation,
  not silently redefined; the handle paths are the supported driver lifecycle.
- `capability-graph` is unchanged: fiber disposal is driven through the existing
  `CapabilityFiber::dispose`, wrapped by an owned future in the composition ledger, so no
  second lifecycle authority appears.

## Rejected alternatives

- **Compare `RunId`s.** A run id is a logical label, not instance identity; two compositions
  can share one, and the reviewed counterexample works with any store-typed exit.
- **A global registry of live driver owners** (id -> owner table). New process-wide mutable
  authority, new leak/ABA surface, and it would still need a non-forgeable handle.
- **A host-callable `verify_exit(&self, &DriverExit) -> bool` before disposal.** Check-then-act
  discipline that a caller can skip; the unsafe composition of the two calls remains
  expressible, which is the bug class under repair.
- **Exposing a numeric/token `u64` or `&'static str` owner id.** Forgeable, comparable to the
  wrong thing, and it invites hosts to persist or transmit it as authority.
- **Re-running the pending hook/fiber on retry and calling it exactly-once.** Restarts a
  started non-idempotent external effect for the sake of a tidy report.
- **Answering `AlreadyReleased` when only the ledger was moved.** Encodes "we lost the
  bookkeeping" as "nothing remains", which is the reviewed misreport.
- **Storing the whole cleanup future in the handle instead of the ledger.** The orderly sweep
  borrows the `Runtime`'s registry, so a storable future would need a self-referential or
  heap-pinned structure; parking the *step* futures keeps the ledger the single owner of the
  data and needs no `unsafe` (this workspace forbids it).
- **Making `RuntimeAssembly::shutdown(&mut self)` resumable.** `RuntimeAssembly`'s host-facing
  `runtime()`/`runtime_mut()` accessors promise a live runtime, so holding it in an `Option`
  to survive cancellation would weaken an inventory promise. Documented limitation instead.
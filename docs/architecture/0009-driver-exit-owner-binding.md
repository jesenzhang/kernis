# ADR 0009: Driver-exit owner binding and cancellation-safe composition cleanup

Status: accepted and implemented in the K6/R2 repair (Stage 2 / R2 closeout); it tightens the
K4 lifecycle contract in `docs/runtime/R2-KERNEL-CONTRACT.md` "Shutdown (the supported
lifecycle)" and `docs/architecture/0005-k4-runtime-composition.md`, and records a pre-1.0
breaking signature change per `COMPATIBILITY.md` §1. Section 5 records where the
implementation differs from the sketch above, as it now stands in the tree.

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
/// The future returned by `dispose_after_driver`, returned already
/// constructed: the acceptance it depends on was recorded at call time.
pub type CompositionReleaseFuture<'a, S = InMemoryDurableStore> =
    Pin<Box<dyn Future<Output = CompositionReleaseOutcome<S>> + Send + 'a>>;

pub fn dispose_after_driver<S>(
    &mut self,
    exit: DriverExit<S>,
) -> CompositionReleaseFuture<'_, S>
where S: DurableStore + Send + '_

pub enum CompositionReleaseOutcome<S = InMemoryDurableStore> {
    Released(Box<CompositionDriverShutdown<S>>),
    Rejected(CompositionReleaseRejection<S>),
}

pub enum CompositionReleaseRejection<S = InMemoryDurableStore> {
    /// The exit belongs to a different driver owner. Nothing was disposed,
    /// this handle's ledger is untouched, and the exit is returned intact.
    ForeignDriverExit { exit: Box<DriverExit<S>> },
    /// This handle's resources were already released exactly once. The exit
    /// is returned intact so the caller still holds its own shutdown fact.
    AlreadyReleased { exit: Box<DriverExit<S>> },
    /// An orderly release was accepted for this handle and never completed, so
    /// its registry authority left with the accepted exit and no sweep with
    /// full authority can run here again; the remaining best-effort cleanup
    /// continues through `release_after_owner_loss`. No exit is carried:
    /// acceptance consumed the one it was given.
    OrderlySweepInterrupted,
}
```

- The same-owner check happens before any hook, fiber disposal, or registry operation.
- A rejection consumes nothing: no hook runs, no fiber is disposed, no registration is
  unregistered, and `CompositionReleaseRejection` hands the `DriverExit` back by value so the
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
- The sweep runs off the ledger, borrowing it and the authority, and awaits the
  parked in-flight future **in place** (`await_parked`) instead of building a
  fresh one; the ledger already recorded, when the orderly release was accepted,
  that this handle may no longer promise a full-authority sweep. Consequently: a module
  is removed from the ledger only after all of its owned steps completed;
  failures are recorded as they happen; and dropping the caller's future loses
  neither the remaining resources nor the facts already collected. The ledger
  and the parked futures live on the handle, which is what survives the
  cancellation of the caller's future; `unsafe` code is forbidden in this
  workspace, so no self-referential resumable future is constructed.
- `release_after_owner_loss` therefore never answers `AlreadyReleased` for work that is
  outstanding. `AlreadyReleased` now means exactly one thing: the entire cleanup flow for this
  handle completed once. A re-entry resumes the parked step instead of re-calling a started
  non-idempotent hook, and the eventual report keeps the earlier failures.
- Registry removals are parkable, and the sweep parks them. The one-shot
  `CapabilityRegistry::remove` is now the composition of a synchronous `take` and a parkable
  `dispose_taken`; the sweep takes the registration synchronously (the entry leaves the registry
  with the take, which never awaits) and parks the taken runtime's disposal as its own in-flight
  step, exactly like a dispose hook or a fiber. A cancellation between the map removal and the
  disposal completing therefore loses neither the outstanding runtime nor the truth about what
  already happened: the resume completes the parked disposal instead of reporting a removal that
  already happened as impossible. `remove` keeps its one-shot behaviour and the same
  `RegistryError::Dispose` shape on top of the two halves.
- The step is load-bearing, not merely defensive. The composition's own activation records every
  plugin fiber in the module's fiber list, so the composition-created path never blocks at the
  registration. But the public registry accessor lets a subsystem host instantiate an additional
  fiber for an already-registered plugin, and that fiber belongs to no module's list: the
  registration disposal then really awaits its disposer. The third review reproduced exactly
  that (2026-10-22, M8): with an extra gated fiber installed through
  `RuntimeAssembly::runtime().capability_registry().instantiate(..)`, the same sequence passes at
  the repaired head and fails at `0ec8096` with "the registration could not be unregistered".
  Parking is therefore required, and the module-fiber path is only the narrower case that never
  blocks.
- Accepting the orderly release also proves the registry authority can become unreachable: the
  accepted call takes the `Runtime` the sweep cleans through, and the acceptance is recorded
  when `dispose_after_driver` is called with an exit this handle accepts, not when the returned
  future is first polled, so the fact is already on the ledger even if that future is dropped
  before its first poll.
  `release_after_owner_loss` accepts exactly two preconditions - the bound mailbox
  reports `OwnerDropped`, or this handle's own ledger records an accepted-but-unfinished
  orderly release.
  Only a real acceptance can set the second condition, so this widens no authority: a foreign
  exit still cannot start a sweep, and a composition whose driver is alive still cannot
  best-effort-dispose while its runtime owns the registry. Registrations that can no longer be
  unregistered keep being reported as `CleanupResource::PluginRegistration` failures, so a
  report never claims orderly completion it could not perform.
- Panics are outside the cancellation contract, and the contract says so. A panic while a
  `dispose` hook is being constructed leaves no future: the step stays in the ledger and a
  re-entry calls the hook again, never reporting the module clean. A panic while an already
  parked step is polled keeps that future in the ledger, so a later release re-polls it and the
  standard library panics immediately (`resumed after panicking`); the ledger never turns that
  into a completion. Both are host programming errors; only cancellation is recoverable.

### 4. Breaking-change and migration note (pre-1.0)

`dispose_after_driver` changes from `async fn(self, DriverExit<S>) ->
CompositionDriverShutdown<S>` to `fn(&mut self, DriverExit<S>) ->
CompositionReleaseFuture<'_, S>`, a boxed future of `CompositionReleaseOutcome<S>`. It is
deliberately not an `async fn`: the ownership check and the ledger marking described above are
synchronous, so they happen when the method is called and not at the future's first poll;
awaiting the returned value stays the same source syntax for every caller.
Per `COMPATIBILITY.md` §1 this needs a migration note plus a
repository-wide search proving no supported path keeps needing the old form:

- Repository-wide call sites at base `d3818f5` (all updated in this repair, none kept on the
  old form): `crates/runtime-loader/examples/r2_host.rs` (2),
  `crates/runtime-loader/examples/host_end_to_end.rs` (1),
  `crates/runtime-loader/tests/k6_r2_end_to_end.rs` (3),
  `crates/runtime-loader/tests/k5_composition.rs` (1),
  `crates/runtime-composition/tests/k4_composition.rs` (1),
  `crates/runtime-composition/tests/k4_compat.rs` (1),
  `crates/runtime-composition/tests/k4_cleanup_ownership.rs` (3).
- Migration for a host: match the outcome. `Released(shutdown)` is the previous behavior
  unchanged; `Rejected(rejection)` means nothing was disposed and `rejection.into_exit()`
  returns the exit for its owning composition. `into_exit()` returns `Option<DriverExit<S>>`:
  `Some` for the two exit-carrying variants, `None` for `OrderlySweepInterrupted`.
- Additive surface: `DriverOwner`, `RuntimeHandle::owner`, `DriverExit::owner`,
  `CompositionReleaseOutcome`, `CompositionReleaseRejection`, `CompositionReleaseFuture`.
  Per ADR 0007 §2 all of them are
  threaded `runtime-core` -> `runtime-composition` -> `runtime-loader`.
- Payload sizing: `CompositionReleaseOutcome` boxes the shutdown and
  `CompositionReleaseRejection` boxes each carried exit, following the existing
  `StartupFailure::cause` convention, so the host-facing enum stays pointer-sized per variant
  instead of moving a whole runtime (985 bytes at the time of writing) through every
  rejection; `into_released()`/`into_exit()` unbox at the boundary the host sees.
- Precedence inside `release_after_owner_loss`: the completed-ledger check runs **first**, so a
  handle that already finished its flow answers `AlreadyReleased` before the driver-state
  observation is consulted, and only afterwards does the interrupted-sweep precondition apply.
- `OwnerLossReleaseError` keeps its variants; `AlreadyReleased` is re-documented to the
  completion-only meaning above, and `OrderlyShutdownCompleted` keeps its guidance except for
  the one new precondition recorded in the ledger.
- `CapabilityRegistry::dispose_taken` takes only the runtime it disposes
  (`dispose_taken(&PluginRuntime)`); the earlier two-argument form existed only inside this
  unmerged branch (`fdc8e5e`), so no released version ever carried it. The current call sites
  are `semantic.rs` (inside `remove`), `assembly.rs` (`dispose_registration`) and
  `capability-graph/tests/semantic.rs`. The failing plugin id is derived from `runtime.id()`,
  which is the only key `register` ever inserts, so a caller cannot misattribute a failure.

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
- `capability-graph` gains one additive split and no second lifecycle authority: fiber disposal
  still runs through the existing `CapabilityFiber::dispose`, and the composition ledger wraps it
  in an owned future. The registry now offers `take` plus `dispose_taken`, with the one-shot
  `remove` kept as their composition so every existing caller and failure message is unchanged.

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

## Evidence

Owner binding, in `crates/runtime-composition/tests/review_r2_probes.rs`:

- a foreign exit while the bound driver is `Running` is refused before anything moves, and
  the composition's own driver still releases it afterwards;
- a foreign exit from a twin instance of the *same* composition, host configuration and
  `RunId`, whose driver reached `Shutdown`, is refused, with the refused exit still releasing
  cleanly through its own composition;
- a lost owner plus another driver's exit is refused;
- every refusal asserts zero dispose-hook starts, zero fiber disposals and zero registry
  removals, and that the refused exit still sees its own plugin registration intact.

Owner-token semantics, in `crates/runtime-core/tests/k3_async.rs`
(`driver_owner_token_recognizes_only_its_own_drivers_exit`,
`driver_owner_token_is_opaque_and_send_sync`): two drivers over the same run shape are
distinct owners, an exit recognizes only its own driver's token, and the token is `Send +
Sync`, cloneable, and opaque in `Debug`.

Cleanup interruption, in `crates/runtime-composition/tests/k6_r2_cleanup_interruption.rs`:
dropping the release future before it is ever polled (nothing has started, so re-entry must do
the whole cleanup), cancellation while a dispose hook is pending, and cancellation while fiber
disposal is pending; each re-entry continues the untouched modules, never starts a second hook
run, keeps earlier failures in the final `RollbackReport`, and only after real completion does a
further call resolve `AlreadyReleased`. A fifth case covers the strictest drop point of the orderly path: a release accepted by
`dispose_after_driver` but dropped before its first poll starts nothing, stays recoverable
best-effort, and ends `AlreadyReleased` only after the flow really completed.

Registration-disposal parking, in `crates/capability-graph/tests/semantic.rs`
(`take_plus_dispose_taken_matches_one_shot_removal`): an absent `take` is the same no-op as an
absent removal, a successful take removes only the registration while the fibers are disposed by
the parked step, and a failing fiber cleanup produces exactly the failure the one-shot `remove`
produces. The reachability boundary stated in §3 was checked over the composition path with a
review probe: cancellation lands in the fiber step, and a registration that can no longer be
unregistered is reported through the documented best-effort owner-loss path, not lost.

End-to-end lifecycle through the loader: `crates/runtime-loader/tests/k6_r2_end_to_end.rs`
and `crates/runtime-loader/examples/r2_host.rs`.
# K6: Supported API Inventory

Status: K6 audit record, based on the public surfaces at base `3ee011b`.
This inventory is the K6 stabilization contract: what Kernis R2 promises,
what each subsystem promises, what is compatibility/experimental surface,
and what is internal implementation detail. See
[R2-KERNEL-CONTRACT.md](R2-KERNEL-CONTRACT.md) for the final Stage 2
entry document and
[COMPATIBILITY.md](COMPATIBILITY.md) for the compatibility policy.

## Tier definitions

| Tier | Meaning | Compatibility promise |
| --- | --- | --- |
| **Host-facing supported** | Reachable through the canonical host entry crate `runtime-loader`; the K6 R2 host example and acceptance suites use only this surface | Rust public API compatibility within R2 (see COMPATIBILITY.md) |
| **Subsystem supported** | A crate's own public API for callers that deliberately depend on that crate directly (embedded hosts, recovery tooling, plugin-authoring helpers) | Stable within the subsystem's authority; cross-crate signature changes follow the same R2 policy |
| **Experimental / compatibility** | Legacy baseline names, aliases, reserved extension points, and documented fail-fast helpers kept for proven behavior | Behavior retained; may be deprecated or narrowed with a migration note; never silently repurposed |
| **Internal implementation detail** | Private items, `pub(crate)` items, and private modules | No promise; may change freely |

The canonical host entry decision (Option A: `runtime-loader` is the
complete Stage 2 host entry crate) and the re-export closure rule are
recorded in ADR 0007.

## Closure rule for the host entry crate

Every type that supported host code must **name** — enum payloads it
matches on, and values it binds from a supported-path signature — is
re-exported through `runtime-loader` (mirrored in
`runtime-composition`'s host vocabulary block). Types that are only ever
consumed through method-call inference (inspection accessors such as
`Runtime::scope`, `Runtime::capability_registry`,
`Runtime::capability_context`) are reachable without naming; deep
inspection of them is subsystem tier.

### K6 gap fix (this milestone)

At base `3ee011b` the umbrella was not closed: 19 host-reachable types
appeared in supported-path signatures or error payloads but were not
nameable through `runtime-loader`. K6 adds re-exports (additive only, no
removals, no renames):

- `runtime-core` now re-exports `execution_stream::{StreamItem,
  KeyedStreamItem, SequenceError}`,
  `capability_graph::{CapabilityHandle, ScopeError}`,
  `workflow_graph::WorkflowGraphError`, and
  `workflow_recovery::{AttemptId, CapabilityReplayIdentity,
  DurableRunState, JournalError, RecoveryDecision, StoreError,
  WorkflowReplayIdentity}` (previously leaked through its own public
  signatures without being nameable).
- `runtime-composition` and `runtime-loader` mirror the same vocabulary
  plus `RuntimeEvent`, `Cancellation`, `ReconstructionError`,
  `FactoryResolutionError`, and `LegacyMutationOperation`.
- The R2 acceptance suite proved a second closure wave: a host that reads
  durable facts through `Runtime::durable_state` must also name
  `StoreErrorKind` (corruption classification), `RecoveryAction` (recovery
  decisions), `RecoveredEffectState` (effect state), and the record types
  `CompletionRecord` / `DispatchRecord` / `OutcomeRecord`; the direct
  `runtime-core` path additionally needed `OperationId`, `EffectSemantics`,
  and `KnownEffectOutcome` nameable on `runtime-core` itself (it already
  exported them nowhere; composition/loader had the latter via their
  `workflow_recovery` block). All additions are additive re-exports along
  the same chain. The single `runtime_loader` import block of
  `crates/runtime-loader/tests/k6_r2_end_to_end.rs` is the standing
  closure proof.
- The R2 owner-loss guard repair is the third wave: `DriverOwnerState`
  (the read-only owner-lifecycle observation behind
  `RuntimeHandle::owner_state`, K3) and `OwnerLossReleaseError` (the
  typed rejection of the guarded
  `CompositionHandle::release_after_owner_loss`, K4) join the vocabulary
  as additive re-exports along the same chain (core → composition →
  loader). Scenario N of `k6_r2_end_to_end.rs` keeps the standing import
  block covering them.
- The R2 repair review is the fourth wave, and it changed how closure is
  *proved*. Importing a name list in a test is weak evidence: examples and
  tests compile partly through type inference, so a name a host cannot
  spell can still go unnoticed. Closure is now proved structurally by a
  separate compile-only crate, `fixtures/r2-host-fixture`, which declares
  `runtime-loader` as its only dependency, is not a workspace member (so
  nothing can reach a subsystem crate through workspace unification), and
  annotates every intermediate value with an explicit type. It is checked
  with `cargo check --manifest-path fixtures/r2-host-fixture/Cargo.toml
  --all-targets` on stable and on the MSRV, plus clippy `-D warnings` on
  the same manifest, and `.github/workflows/ci.yml` runs exactly those three
  commands in its `r2-host-fixture` job — the fixture is a gate, not only a
  command in a document. `crates/runtime-loader/tests/review_r2_exports.rs`
  stays as the in-workspace vocabulary regression.
- This wave adds, additively along the same chain: `DriverOwner`,
  `CompositionReleaseOutcome`, `CompositionReleaseRejection` and
  `CompositionReleaseFuture` (the owner-bound release of ADR 0009),
  `FactoryRegistry`,
  `RUN_DEFINITION_FORMAT`, `LEGACY_RUN_DEFINITION_FORMAT_V1` (ADR 0008
  makes the legacy tag a host-visible fact, because a durable identity
  carrying it is refused), `RecoveryReason`, `StoreInvariant`,
  `StoreRevision` (bound from `DurableRunState::revision`),
  `CapabilityConflictReason` (the payload a host matches in
  `CompositionError::CapabilityDefinitionConflict`), and `InvalidId`
  (returned by `Id::new`, the host's entry point for every identifier).
  `RuntimeAssembly` was already re-exported by `runtime-composition`; the
  review proved the loader mirror omitted it, and it is now present.
- The repair-branch reconciliation is a fifth wave, and it adds no
  host-facing name. Its only API change is in the subsystem tier:
  `CapabilityRegistry::take` and the associated `dispose_taken` let the
  composition sweep remove a registration synchronously and park its fiber
  disposal (ADR 0009 §3), with the one-shot `remove` kept as their
  composition so every existing caller keeps its behaviour and failure
  shape. `CapabilityRegistry` is a subsystem-supported type, so the change
  is additive under `COMPATIBILITY.md` §1 and needs no loader re-export;
  the closure fixture is unchanged.
- The third-review correction is a sixth wave, and it changes the host tier by
  kind rather than by widening it. **Promoted to host tier** (a host binds
  them): `CapabilityId` (from `CapabilityHandle::id()` and the `ScopeError`
  variants), `Generation` and `EntryId` (public fields of `CapabilityPin`).
  They are re-exported core → composition → loader and named by the fixture.
  **Kept at subsystem tier**: `CapabilityGraphError` (payload of
  `ScopeError::GraphAdmission`, `DefinitionError::CapabilityGraph`,
  `ReconstructionError::CapabilityGraph`), `JournalInvariant` (payload of
  `JournalError::InvariantViolation`) and `IdempotencyKey` (payload of
  `StoreError::IdempotencyConflict`) are match-only payloads — a host matches
  them by inference and reaches for the subsystem crate only to store one;
  `WorkflowGraph` and `DurableJournal` are inspection-accessor returns
  (`Runtime::workflow()`, `Runtime::journal()`) and join `Scope`,
  `CapabilityRegistry` and `CapabilityContext` in that tier. ADR 0007 §2 states
  the rule that separates the two cases.
  This wave also adds `crates/runtime-loader/tests/export_coverage.rs`, which
  extracts every umbrella `pub use` name and fails when one is missing from the
  closure evidence. The fixture is a whitelist; this is what makes a whitelist
  safe, and it is the direct answer to the review's observation that three
  exports were never named by either proof.
- Deliberate exclusions, restated with the evidence that produced them. The
  store seam is exercised in the fixture through a generic
  `S: DurableStore` parameter, and the pass-through vocabulary is bound by
  name only — that is the boundary. Types reachable only by method-call
  inference from an inspection accessor (`Scope`, `CapabilityRegistry`,
  `CapabilityContext`, and the deep-API types behind them: `RegistryError`,
  `FiberError`, `EffectError`, `DependencyPin`, `EntryId`, `Generation`,
  `PluginFuture`), the legacy configuration vocabulary (`TaskConfig`,
  `EffectSpec`, §runtime-core below), and the store-authoring/conformance
  vocabulary (`DurableMutation`, `CommitRequest`, `CommitResult`,
  `CommitLedgerEntry`, `IdempotencyKey`, `DurableJournal`,
  `JournalInvariant`, `classify_recovery`, `AttemptAdmission`,
  `EffectIntent`, `CancellationRecord`) stay at the subsystem tier exactly
  as the closure rule above states. A host implementing its own
  `DurableStore` is a store author — documented subsystem work; the
  supported R2 host selects one of the two exported store implementations
  or stays generic over the exported trait.

## runtime-loader — canonical host entry (K5)

Role: host-facing umbrella + logical reference resolution only. It owns
no authority (see authority table in R2-KERNEL-CONTRACT.md) and adds
exactly the K5 resolution layer.

Host-facing supported (own items):

| Item | Purpose |
| --- | --- |
| `ModuleReference`, `ModuleVersion` | Exact-version logical module request (`id@version`, final-`@` grammar, fail-closed parsing) |
| `CatalogEntry`, `ModuleCatalog`, `ModuleRegistrationFactory` | Explicit host-provided reference→factory catalog (trusted host authority) |
| `RuntimeLoader`, `ResolvedModules` | Deterministic dependency-closure resolution into fresh process-local registrations |
| `LoaderError`, `InvalidReferenceReason`, `IncompatibleEntryReason`, `ModuleFactoryError` | Resolution-phase typed failures |

Plus the mirrored host vocabulary from `runtime-composition` (see its
section) and `kernis_core::Id`. Non-goals (documented in the crate and
ADR 0006): no filesystem/dylib/WASM/network loading, no discovery, no
watchers, no semver solving.

## runtime-composition — composition authority (K4)

Host-facing supported (all also re-exported by the loader):

| Group | Items |
| --- | --- |
| Stable declarations | `ModuleDefinition` (builder: `new`, `depends_on`, `with_task`, `with_declarative_capability`, `with_reactive_capability`, `with_capability`, `requiring_config`, `with_optional_config`), `CapabilityContribution`, `CapabilityOwnership`, `ReactiveCapabilityDeclaration`, `ConfigRequirement` |
| Process-local registration | `ModuleRegistration` (`factory`, `plugin`, `plugin_with_config`, `on_activate`, `on_dispose`), `LifecycleHook`, `HookFuture`, `CapabilityFactoryFn`, `lifecycle_hook` |
| Planning | `CompositionBuilder` (`register`, `build`), `CompositionPlan` (`definition`, `module_order`, `slots`, async `start`, `start_with_store`, `restore`), `CapabilitySlot`, `HostConfig` |
| Activation / shutdown | `RuntimeAssembly` (`runtime`, `runtime_mut`, `module_order`, `host_config`, async `shutdown`, `into_driver`), `CompositionHandle` (`module_order`, `dispose_after_driver` — `&mut self`, owner-bound, and deliberately **not** `async`: it validates the `DriverExit` against the `DriverOwner` this handle was separated with and records its acceptance before returning the `CompositionReleaseFuture` that is then awaited, so a future dropped before its first poll cannot hide the release; async `release_after_owner_loss` — bound-guarded, proves the bound driver's `DriverOwnerState::OwnerDropped` before any cleanup), `CompositionReleaseFuture`, `CompositionReleaseOutcome` (`is_released`, `into_released`), `CompositionReleaseRejection` (`ForeignDriverExit`, `AlreadyReleased`, `OrderlySweepInterrupted`, `into_exit`), `CompositionDriverShutdown` |
| Errors | `CompositionError` + `ActivationStage`, `ConstructionStage`, `CapabilityConflictReason`, `FactoryConflictReason`, `PluginConflictReason`, `CleanupResource`, `RollbackFailure`, `RollbackReport`; `StartupFailure`; `CompositionShutdownFailure`; `OwnerLossReleaseError` |

Internal: `ActivatedModule`, `CleanupAuthority`, and `CleanupLedger` are
crate-private. The ledger is the cleanup authority's queue: it holds the
remaining modules, the current module, the in-flight dispose hook or fiber
disposer, and the report built so far, so cancelling a cleanup future
mid-await cannot forget work that had already started (ADR 0009 §3).
Composition coordinates existing authorities; it creates no second
registry/runtime/store.

## runtime-core — deterministic coordination + driver (K2/K3)

Direct users depend on this crate deliberately (subsystem entry); the
umbrella re-exports its host-facing vocabulary.

Host-facing supported:

- Declarative construction: `RunDefinition`, `TaskDefinition`,
  `CapabilityDeclaration`, `CapabilityRequirement`,
  `DefinitionIdentity`, `DefinitionError`, `RUN_DEFINITION_FORMAT`,
  `LEGACY_RUN_DEFINITION_FORMAT_V1` (the frozen historical tag, ADR 0008),
  `FactoryRegistry`, `FactoryResolutionError`, `RUN` entry points
  `Runtime::start_from_definition`, `start_from_definition_with_store`,
  `restore_from_definition`.
- K3 driver: `RuntimeDriver::new` → `(driver, handle)` +
  `RuntimeHandle` (`drive`, `wake`, `dispatch_effect`, `recover`,
  `cancel_task`, drain methods, `shutdown`, the synchronous read-only
  `owner_state`, and `owner()` → `DriverOwner`), `DriverFuture`,
  `DriveResult`, `ShutdownStatus`, `DriverOwnerState`, `DriverOwner`
  (opaque, cloneable, `Send + Sync`; `same_driver_as` is the only way to
  compare it with `DriverExit::owner()`, ADR 0009), `DriverExit`,
  `DriverError`, `EffectDispatcher`, `EffectDispatchRequest`,
  `EffectDispatchFuture`, `EffectDispatchError`.
- Coordination results: `StepResult`, `RuntimeEvent`, `Cancellation`,
  `TaskAttempt`, `CapabilityPin`, `RecoveryDecision`, `DurableRunState`.
- Errors: `RuntimeError`, `ReconstructionError`,
  `LegacyMutationOperation`, plus payload domains `StoreError`,
  `JournalError`, `WorkflowGraphError`, `ScopeError`, `SequenceError`,
  `DefinitionError`, `FactoryResolutionError` (nameable after K6).

Experimental / compatibility: the legacy direct-construction and
legacy-identity mutation path (`Runtime::start_run`,
`start_run_with_store`, `restore_run`, `configure_task`,
`apply_workflow_mutation`) remains supported for direct
subsystem users and K1 replay identity; declarative runtimes reject the
legacy mutators fail-closed (`RuntimeError::DeclarativeMutationUnsupported`).
`TaskConfig`/`EffectSpec` belong to this legacy configuration path.
Inspection accessors `scope()`, `capability_registry()`,
`capability_context()` return subsystem-tier types by reference.

Internal: the command mailbox, driver-owner guard, admission/dispatch
internals, static stream capacities, and the durable-commit ledger
plumbing are private.

## workflow-recovery — durable authority (K1 + M2-B)

Host-facing supported (umbrella: the subset marked ★):

- Store ports: `DurableStore` ★ (trait), `InMemoryDurableStore` ★
  (deterministic conformance backend), `FileDurableStore` ★ (embedded
  redb physical store, single file, handle-per-operation).
- Run facts: `RunId` ★, `DurableRunState` ★ (read-only materialized
  facts), `AttemptAdmission`, `CompletionRecord`, `CancellationRecord`,
  `CapabilityReplayIdentity` ★, `WorkflowReplayIdentity` ★,
  `StoreRevision`.
- Effect/recovery vocabulary: `OperationId` ★, `AttemptId` ★,
  `EffectSemantics` ★, `EffectIntent`, `DispatchRecord`,
  `KnownEffectOutcome` ★, `OutcomeRecord`, `RecoveredEffectState`,
  `RecoveryAction`, `RecoveryReason`, `RecoveryDecision` ★.
- Errors: `StoreError` ★ + `StoreErrorKind`, `StoreInvariant`,
  `JournalError` ★ + `JournalInvariant` (see experimental note).

Subsystem supported: `DurableJournal` (deterministic in-memory fact
journal used by conformance/K1 tests), `classify_recovery`,
`IdempotencyKey`, `DurableMutation`, `CommitRequest`,
`CommitLedgerEntry`, `CommitResult`.

Experimental / compatibility: `JournalInvariant::Reserved` is a reserved
extension point (no behavior; never repurposed without a compatibility
note). `StoreInvariant` exposes the full internal invariant taxonomy
because `StoreError::InvariantViolation` carries it; retained as-is.

Internal: `DurableRunState` construction/validation/persistence methods,
`PersistedRunSnapshot`, the v5 postcard snapshot wire format, the ledger
replay rebuild, and bootstrap open policy are `pub(crate)`/private — the
store is the only constructor.

## capability-graph — capability lifecycle authority (M2-A/M2-C)

Host-facing supported (plugin authoring; umbrella-re-exported):
`CapabilityValue`, `CapabilityDefinition`, `CapabilityHandle` ★,
`ResolvedDependencies`, `PluginDefinition`, `PluginRuntime`,
`PluginFactory`, `PluginConfig`, `PluginLoadContext`,
`CapabilityFiber`, `FiberState`, `ScopedEffect`, `ScopeError` ★.

Subsystem supported: `Scope` (hierarchical owning scope),
`ReactiveCapabilityRuntime` (+ `ReconcileReport`,
`FiberReconcileError`, `FiberCleanupFailure`, `ReactiveRuntimeError`),
`CapabilityRegistry`, `CapabilityGraph`, `ResolvedCapabilityGraph`,
`ValidatedCapabilityDefinition`, `CapabilityGraphError` (surfaces as a
`ReconstructionError` payload ★), `Generation`, `EntryId`, `FiberId`,
`DependencyBinding`, `DependencyPin`, `DependencyEpoch`, `EffectStack`,
`EffectScope`, `EffectError`, `FiberError`, `RegistryError`,
`PluginFuture`, `ConfigValidator`, `CapabilityContext`.

Experimental / compatibility (retained, documented, never repurposed):

- `Capability` — documented legacy baseline-API metadata struct.
- `FiberState` — Cordis-compatible state names.
- `ReactiveRuntime` — alias of `ReactiveCapabilityRuntime`.
- The plugin `Err(String)` error channel (`PluginFuture` /
  `CapabilityFactory` value-factory results) — stringly-typed by design
  at the plugin seam; the kernel wraps it into typed
  `ScopeError::ConstructionFailed` /
  `FactoryResolutionError::ConstructionFailed` reasons. Widening this to
  a typed plugin error is a future decision, not R2 cleanup.

Internal: fiber bookkeeping, effect stacks, and entry tables.

## workflow-graph — topology/completion authority (M1)

Subsystem supported: `WorkflowGraph` (`apply_batch` CAS, `complete`,
`ready_tasks`, `replay`, `replay_with_facts`), `WorkflowTopology`,
`ExecutionFacts`, `Task`, `WorkflowMutation`, `MutationBatch`,
`WorkflowMutationRecord`, `CompletionRecord`, `WorkflowGraphError` ★.

Experimental / compatibility: `WorkflowState` alias,
`record_completed`/`topology_revision` aliases. The legacy mutation path
is rejected (fail-closed) on declarative runtimes; umbrella hosts never
name this crate directly.

## execution-stream — observation authority (M2)

Subsystem supported: `Sequence`, `StreamSequencer`, `SequenceTracker`,
`SequenceObservation`, `StreamItem` ★, `KeyedStreamItem` ★,
`LosslessBuffer`, `CoalescingBuffer`, `LossyBuffer`, `PushError`,
`SequenceError` ★, `StreamError`, `BufferError`.

Experimental / compatibility: the documented fail-fast `next()` helpers
(`Sequence::next`, `core::Revision::next`,
`capability_graph::Generation::next`) panic only at the type's `MAX`
with a fallible `checked_next()` sibling; retained (see panic audit).

## kernis-core — neutral primitives

Host-facing supported: `Id` ★ (umbrella), `InvalidId`. Subsystem:
`Revision` (+ documented fail-fast `next`/`checked_next` pair).

## Public error audit

Separation proven by K5 acceptance (and re-proven by the K6 R2
end-to-end suite): `LoaderError` ≠ `CompositionError`/`StartupFailure` ≠
`RuntimeError`/`DriverError` ≠ `StoreError`; none collapses into an
`Unknown(String)` catch-all. Host-originated invalid input returns
typed errors everywhere on supported paths (blank ids/versions, missing
references, duplicate catalog entries, duplicate module ids, cycles,
missing configuration, wrong definition identity, corrupted durable
state, incompatible schema).

K6 stabilization (additive, non-breaking): the supported error domains
now implement `Error::source()` where the variant wraps a typed inner
error — `RuntimeError`, `DriverError`, `DefinitionError`,
`ReconstructionError`, `FactoryResolutionError` (runtime-core) and
`StartupFailure` (runtime-composition; `CompositionError` and
`LoaderError` already had it).

Documented limitations (retained, not rewritten in K6): String payloads
at the plugin factory seam, `StoreError::IoFailure`/`DataCorruption`,
`ActivationStage::*/ReconciliationFailed` reasons, and
`EffectDispatchError::UnknownOutcome`; these flatten `std::io::Error` /
factory diagnostics into `Display`. See COMPATIBILITY.md §"Error payload
stability".

## Panic audit (supported public paths)

Host-controllable input never panics on a supported path — verified by
audit and pinned by the K5/K6 failure suites. Every remaining
`expect()`/`unreachable!()` in non-test code falls in one of these
classes:

1. Documented public fail-fast at type `MAX` with a fallible sibling
   (`next()`/`checked_next()` pairs in core, capability-graph,
   execution-stream). Retained: reaching `MAX` is a host programming
   error, not input handling.
2. Mutex-poison propagation (capability-graph, runtime-core driver):
   panic message names the poisoned lock; standard Rust policy.
3. Proven internal invariants with a checking precondition established
   earlier in the same call or at admission-time validation
   (`runtime-core` `unreachable!("prepared/persisted")` markers,
   `workflow-graph` DFS bookkeeping, `runtime-loader` resolved-closure
   membership, `workflow-recovery` journal-shape and fixed-checksum
   cases, `capability-graph` post-admission topology invariants).
4. `Drop`/teardown cleanup paths where no error channel exists
   (`capability-graph` runtime teardown) — noted as the residual class
   to watch in Stage 3, non-blocking for R2.

## Dependency direction (verified 2026-09-10)

Cargo rejects package cycles, and every reverse edge the Stage 2 rules
forbid (e.g. `runtime-core → runtime-loader`,
`workflow-recovery → runtime-core`, `capability-graph →
runtime-composition`) would close a cycle against the existing forward
edge. Direction is therefore structurally enforced, and the Cargo.toml
audit confirms the declared edges:

```text
runtime-loader → runtime-composition → runtime-core → { workflow-graph,
capability-graph, workflow-recovery, execution-stream, kernis-core }
workflow-recovery → { workflow-graph, kernis-core }   (+ redb)
capability-graph → kernis-core          execution-stream → kernis-core
runtime-core → workflow-recovery        kernis-core → {}
graph-lab → everything except composition/loader (laboratory only)
```

No reverse edge exists. No graph-lab extension was needed; it remains an
experiment smoke binary, not a dependency verifier.

## Feature audit

The workspace declares zero crate features (all dependency features are
inline: tokio `sync/rt/macros`, serde `derive`, postcard `alloc`, pinned
`redb = "=2.6.3"`). K6 introduces no features (per the K6 contract,
K6 is not a feature-architecture milestone). `--all-features` is
therefore equivalent to the default build for every crate.

## Package metadata / publish policy

All nine crates carried only the workspace-inherited
`version/edition/rust-version/license`. K6 adds `description` and
`repository` to each crate and marks every crate `publish = false`:
R2 is an in-tree contract, the crates use path dependencies without
version requirements (publishing today would require a packaging design
that is outside K6), and no publication has been decided. The decision
is recorded in ADR 0007 and COMPATIBILITY.md; `cargo package` is not
part of R2 verification for this reason.

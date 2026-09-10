# Stage 2 Runtime Kernel Milestone Plan

Status: Approved implementation plan

Baseline: durable store port and completion/replay contract closure implemented
at `31abe514` on `feat/k1-s1-durable-store-port`; Stage 2 execution starts from
that change or its integrated equivalent.

This document is the authoritative implementation order for Stage 2. The
roadmap states direction; this plan defines milestone outcomes, dependencies,
acceptance contracts, and risk boundaries.

The milestone is the primary delivery unit. A milestone normally runs as one
continuous implementation context with focused proof between related changes.
Do not create a pre-planned Slice sequence. Introduce a Slice or fresh handoff
only when a material architecture decision, independent ownership boundary,
high-risk proof boundary, repeated implementation failure, or degraded context
makes continuing less safe or more expensive.

## Execution policy

1. Only one Stage 2 milestone is active at a time.
2. Start from the current integrated repository truth, not solely from this
   document. Revalidate the predecessor's evidence and preserve unrelated work.
3. At milestone start, publish one coordinator summary and one coherent worker
   handoff containing the milestone contract, current HEAD, focused proof, and
   boundary conditions.
4. Use research before production when a dependency, persistence engine,
   executor boundary, loader authority, or public compatibility decision is
   still unresolved. Record hard-to-reverse decisions in an ADR before making
   them implementation assumptions.
5. Continue related implementation blocks in the same context while its model
   of the repository remains accurate. A focused test completing is not by
   itself a handoff boundary.
6. A milestone is complete only when its observable acceptance scenarios pass,
   repository-required checks pass, required review blockers are resolved, and
   the status/evidence in this document is updated.
7. If the same approach fails twice for substantially the same reason, a core
   assumption is invalidated, or context quality materially degrades, stop and
   create a compact fresh-context handoff.
8. Do not pull later-milestone features forward unless repository evidence
   proves they are necessary to satisfy the active contract. If that changes a
   milestone boundary, update this plan explicitly.

Repository-required milestone checks remain:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

Focused tests should be used during implementation; the complete workspace
checks belong at the milestone boundary rather than after every internal edit.

## Milestone sequence

| Milestone | Outcome | Depends on | Status | Review |
| --- | --- | --- | --- | --- |
| K1 | Embedded physical durability | M2-B contract closure | Integrated | Independent - APPROVE |
| K2 | Declarative configuration and cold reconstruction | K1 | Implemented on `main` | Independent - pending |
| K3 | Explicit asynchronous execution boundary | K2 | Implemented on `main` | Independent - pending |
| K4 | Runtime and plugin composition API | K2, K3 | Integrated | Independent - APPROVE (re-review PASS, 0 blockers, after repair `e03778d`) |
| K5 | Minimal loader boundary | K4 | Implemented on `main` | Independent - pending |
| K6 | Runtime Kernel API stabilization and R2 closeout | K1-K5 | Planned | Independent |

The sequence is dependency order, not a promise that current implementation
details will remain optimal. Reordering requires an explicit plan update that
records the new evidence and preserves completed contracts.

## K1 — Embedded Physical Durability

### K1 Closeout Repair status

Status: Integrated. The repair closed idempotency
lineage validation, bootstrap error classification, identity deserialization
invariants, ledger-only physical wire state, cross-process/logical-corruption
proof, and the interrupted `create_run` to workflow-identity crash window. A
further concurrent first-open CI stability repair is recorded under K1
completion evidence below. Both independent durability/recovery reviews returned
APPROVE with no blocker.

Repository truth: `main` now contains the candidate at
`e84acf331f4813e52e853fcb28db2caf2c8bdf62`. Full GitHub CI on that exact SHA
passed (Format, Clippy, Test, Graph lab); K1 is an integrated milestone.

### Outcome

One embedded store implementation persists the existing typed durable facts to
physical storage and can reopen them through a new store instance after process
termination. Claims are limited to the guarantees actually demonstrated for
the selected backend and operating conditions.

### Contract

**Behavior**

- A run created by one store instance can be restored by a separately opened
  store instance without cloning process memory.
- Atomic mutation batches, expected-revision CAS, idempotency keys, append-before-
  effect ordering, exact attempt lineage, completion replay, cancellation, and
  workflow replay identity behave like the in-memory conformance adapter.
- Backend unavailable, I/O, corruption, and domain conflicts remain distinct
  through `StoreError` and `RuntimeError`.
- Persistent data has an explicit schema/format version and incompatible data
  fails closed.

**Constraints and preserved behavior**

- Select the backend only after a focused comparison against the current
  `DurableStore` contract and ADR-0002.
- Keep `InMemoryDurableStore` as the deterministic conformance backend.
- Keep Fiber, live capability handles, streams, registry state, and effect
  closures outside persistent truth.
- Do not weaken synchronous deterministic Runtime Core semantics merely to fit
  a backend API.

**Non-goals**

- Distributed storage, network partitions, replication, worker leasing,
  compaction policy, arbitrary migrations, or provider reconciliation.
- General event sourcing or durable serialization of the runtime object graph.
- Unqualified power-loss or `fsync` guarantees that have not been proven.

### Acceptance scenarios

- A child process creates a run, records each supported fact class, exits, and
  a new process restores the run and obtains the same recovery decisions.
- Crash-window cases remain safe: before dispatch, after dispatch, after
  outcome, and after completion.
- Two open store connections demonstrate expected-revision conflict and
  idempotent replay behavior without partial commits.
- Reopening incompatible or corrupted data returns the documented error class
  and performs no recovery action.
- The physical adapter passes the same conformance cases as the in-memory
  adapter.

### Decision and risk boundaries

Backend selection is a research-to-production decision boundary. If more than
one backend remains materially valid after the focused experiment, stop for an
architecture decision rather than implementing both. Persistent schema,
transaction atomicity, crash behavior, and multi-connection ownership require
independent review.

### Completion evidence to record

- Backend-selection ADR and dependency rationale.
- Conformance matrix for in-memory and physical adapters.
- Cross-process and crash-window verification actually run.
- Backend guarantees explicitly claimed and explicitly not claimed.
- Final integrated commit and independent review result.

### K1 Closeout Repair completion evidence

Result: Integrated. K1 is independently reviewable and its physical adapter
contract is closed; the accepted candidate has been fast-forwarded onto `main`.

Base: `31abe5145c2822ac11ce7d70557a069f098f0437`

Final integrated `main` HEAD: `e84acf331f4813e52e853fcb28db2caf2c8bdf62`.
Accepted candidate implementation SHA on `feat/k1-s1-durable-store-port`:
`ceda06fcd0e81e232a12b9768e974f58bd909bab` (implementation; the documentation
commit records this evidence). Earlier accepted K1 implementation review:
`c12befe4287cdebfcb737c39ebadd693ed71f92b`.

Bootstrap crash window: Closed. An existing row is repaired only when the
pristine bootstrap predicate holds; identity recovery uses the existing
`workflow-replay-identity` idempotency key and `StoreRevision::INITIAL` CAS.

#### Concurrent first-open CI stability repair

Status: Integrated and proven on `main`.

Main CI evidence: GitHub Actions run `32829753451` for the exact final SHA
`e84acf3` on `main` passed all four gates (Format, Clippy, Test, Graph lab).

Root cause: On the failed final GitHub run
(`concurrent_first_openers_wait_for_bootstrap_instead_of_reporting_corruption`)
a first opener that was not the creator exhausted the normal-operation
acquisition budget (`40 * 5ms = 200ms`) while the winning opener still held the
redb handle mid-bootstrap (create + table-init + durable commit). redb allows
exactly one `Database` handle per file, so that `DatabaseAlreadyOpen` was
transient bootstrap ownership, but the tiny iteration-based budget misclassified
it as `StoreError::BackendUnavailable` under CI load.

Repair: A `FileDurableStore::open` bootstrap path now uses a named, bounded,
wall-clock policy (`BOOTSTRAP_WAIT_DEADLINE = 5s`, `BOOTSTRAP_WAIT_POLL = 10ms`)
via a dedicated `open_bootstrap_database` helper. It retries only
`DatabaseError::DatabaseAlreadyOpen`, so ordinary permission/I/O/corruption
errors still map immediately and are never retried. After the bounded deadline
the backend is still classified `BackendUnavailable`; an opened-but-structurally
uninitialized store still fails closed as `DataCorruption`. The small normal
`open_existing_database` budget used by ordinary CRUD operations is unchanged,
so general runtime contention semantics are preserved and no global ownership
primitive was added.

Focused race stress: 100/100 consecutive focused passes, plus 20/20 full
`physical` suite runs.

Focused verification: `cargo test -p workflow-recovery --all-features` (all
green); `cargo test -p workflow-recovery --test physical --all-features` (8
passed); `cargo test -p runtime-core --test k1_physical --all-features` (9
passed, 0 failed), including physical interrupted-bootstrap reopen,
conflicting-definition rejection, non-pristine missing-identity rejection,
initialized-run reclassification, physical/logical recovery and corruption.

Broad verification: `cargo fmt --all -- --check` PASS; `cargo clippy --workspace
--all-targets --all-features -- -D warnings` PASS; `cargo test --workspace
--all-features` PASS; `cargo run -p graph-lab` PASS.

Implementation blocks / explicit Slices actually used: Fresh-context continuous
repair with no explicit Slice. The blocks were failing acceptance tests, the
ordered commit ledger and binding validation, fallible identity constructors and
serde, bootstrap/error-category closure, ledger-only v5 snapshot replay,
cross-process/logical-corruption proof, and the concurrent first-open CI
stability repair above.

Material decisions: `WorkflowGraph` remains topology, prerequisite, and
completion authority. The durable store persists only `run_id` and the ordered
commit ledger; the idempotency lookup and all durable fact views are rebuilt by
ledger replay. Each ledger entry binds idempotency key, mutation batch, and
revision. Physical values use the v5 postcard snapshot with an outer checksum;
incompatible versions and invalid replay lineage fail closed.

Independent review: Spec review APPROVE, 0 blockers; standards/architecture
review APPROVE for the earlier `5a3e09e..c12befe` delta, 0 documented breaches
and 1 LOW judgement-call smell. The concurrent first-open CI stability repair
delta (`ab92bbe..e84acf3`) had its focused independent review and returned
APPROVE, 0 blockers before `main` integration.

Remaining risks and non-goals: No fsync or power-loss guarantee, schema
migration framework, distributed store, worker lease/fencing, compaction,
timer/provider reconciliation, or K2 configuration reconstruction is claimed.
These remain outside K1 and require their own milestone evidence.

## K2 — Declarative Configuration and Cold Reconstruction

### Outcome

A caller can construct or restore a Runtime from a stable declarative run
definition plus durable state, without cloning a previously live
`WorkflowGraph`, `Scope`, or Runtime instance.

### Contract

**Behavior**

- A typed run definition describes workflow topology, task configuration,
  required capabilities, stable definition identities, and the information
  needed to rebuild process-local objects.
- Equivalent definitions produce the same canonical replay identity.
- Restore reconstructs a fresh workflow, capability coordinator, registry,
  fibers, queues, and handles from definition plus durable facts.
- Missing factories, duplicate logical identities, incompatible definitions,
  and replay mismatches fail before execution resumes.
- Definition evolution is explicit and CAS-protected when it changes the
  canonical replay identity.

**Constraints and preserved behavior**

- Separate serializable/stable configuration from process-local factories,
  values, futures, handles, and disposers.
- `WorkflowGraph` remains topology/prerequisite authority;
  `capability-graph` remains capability lifecycle authority.
- Do not make the persistent store a service locator or plugin loader.
- Existing direct construction APIs remain supported unless an explicit
  compatibility decision supersedes them.
- Preserve the exact main-branch `kernis-workflow-replay-v1` algorithm for
  `start_run*`, `restore_run`, and their live-object identity mutations.
- Keep declarative `kernis-run-definition-v1` canonicalization separate from
  legacy live-object identity provenance; do not add automatic fallback or
  durable identity migration.

**Non-goals**

- Filesystem discovery, dynamic libraries, WASM, HMR, remote configuration,
  secret management, or provider SDKs.
- Persisting arbitrary Rust closures or trait objects.

### Acceptance scenarios

- A process writes durable state, exits, and another process reconstructs the
  run from the same declarative definition and K1 store.
- Reordered but semantically equivalent input produces the same identity;
  meaningful topology or capability configuration changes do not.
- Missing or incompatible capability factories fail closed without emitting
  lifecycle or execution observations.
- Approved A-to-B-to-A definition transitions cannot replay a stale identity
  update.
- A K1/main durable run restores through the legacy API, while a declarative
  runtime rejects `configure_task` and `apply_workflow_mutation` before any
  legacy identity write.

### Decision and risk boundaries

Canonicalization, definition versioning, and the separation between stable
descriptors and executable factories are public compatibility decisions.
Escalate if the implementation would require serializing process-local
capability behavior or duplicating WorkflowGraph semantics. Independent review
is required for replay identity and public configuration contracts.

### Completion evidence to record

- Definition schema and compatibility decision.
- Determinism, mismatch, evolution, and cross-process reconstruction proof.
- Public API migration notes.
- Final integrated commit and independent review result.

### Candidate completion evidence (2026-09-01)

Result: Candidate / implementation PASS; K2 is not integrated.

Base: `1e8cf70` (`main`, K1 integrated).

Final integrated HEAD: N/A while the candidate awaits independent review;
implementation candidate commit is `5fe0ea4`.

Implementation blocks / explicit Slices actually used: One continuous K2
milestone; no explicit Slice or handoff was needed.

Material decisions: Added serializable-friendly `RunDefinition`,
`TaskDefinition`, `CapabilityRequirement`, `DefinitionIdentity`, and a
process-local `FactoryRegistry`; kept `WorkflowGraph`, capability scope, and
`DurableStore` as the existing authorities; kept the exact main-branch
`kernis-workflow-replay-v1` live-object algorithm separate from the
`kernis-run-definition-v1` canonicalizer; tracked identity provenance
internally; rejected legacy mutators on declarative runtimes before durable
writes; reused the durable replay-identity mutation and revision-CAS protocol;
excluded task display labels only from the versioned RunDefinition identity.

Focused verification: K2 declarative suite 14 passed; runtime-core K1 physical
suite 9 passed; workflow-recovery all-features suite 51 passed; staged diff
check passed.

Broad verification: `cargo fmt --all -- --check` passed; workspace clippy with
`-D warnings` passed with exit code 0; workspace tests 227 passed; graph-lab
smoke passed; `git diff --check main...HEAD` passed; no separate repository
architecture verifier exists beyond the CI gates inspected in
`.github/workflows/ci.yml`.

Independent review: Pending; candidate is ready for independent review of the
public definition and replay-identity contract.

Remaining risks and non-goals: No loader, plugin discovery/SDK/lifecycle,
secret management, hot reload, WASM/dynamic library support, distributed
execution, async executor redesign, DI/service locator, or serialization of
arbitrary process-local runtime objects is claimed. Existing direct
construction APIs remain supported; durable schema migration and power-loss
fsync guarantees remain outside K2.

### Repository status reconciliation (2026-09-10, K4 preflight)

The K2 implementation (`017e2af`), the review-feedback compatibility fix
(`5fe0ea4`), and the closeout record (`684ae84`) are integrated on `main`,
and `origin/main` matches the local `main`. GitHub Actions CI passed on the
current `main` HEAD `ebfc7d3` (run 34446922406). No independent-review
APPROVE is recorded for K2, so K2 is not marked Integrated. This
reconciliation records repository-provable state only; it creates no new
milestone identity and no new Slice.

## K3 — Explicit Asynchronous Execution Boundary

### Outcome

The kernel exposes one explicit asynchronous host boundary for driving work,
external effect dispatch, cancellation, and backpressure while preserving the
deterministic synchronous authorities underneath it.

### Contract

**Behavior**

- A host can await runnable work, dispatch an effect through an explicit
  interface, record its exact attempt outcome, and stop or cancel cleanly.
- Concurrent host activity cannot bypass durable admission/dispatch ordering,
  mutate an in-flight capability pin, or create two authoritative owners for
  one attempt.
- Shutdown and cancellation have documented behavior before dispatch, during
  an unknown outcome, and after a known outcome.
- Backpressure remains observational unless an explicitly lossless lifecycle
  boundary blocks progress.

**Constraints and preserved behavior**

- Keep Tokio-specific task handles and scheduler internals out of public
  kernel contracts unless an ADR demonstrates that coupling is required.
- Preserve current deterministic step/recovery semantics as the reference
  model.
- Do not infer external-effect success from task completion, cancellation, or
  stream delivery.

**Non-goals**

- Distributed scheduling, worker leasing, remote queues, provider-specific
  retry policy, or a general actor framework.
- Parallel execution merely for throughput without an ownership contract.

### Acceptance scenarios

- Async driving produces the same durable facts and recovery classifications
  as equivalent deterministic stepping.
- Cancellation at each dispatch boundary preserves exact attempt lineage.
- Concurrent wakeups or host calls cannot double-dispatch one admitted
  attempt.
- Backpressure and shutdown tests demonstrate no loss of correctness facts or
  disposer ownership.

### Decision and risk boundaries

Executor neutrality, ownership of spawned work, concurrent mutation, and
shutdown ordering are architecture boundaries. Stop for a decision if the
existing synchronous `DurableStore` port cannot safely support the chosen host
model. Concurrency and lifecycle changes require independent review.

### Completion evidence to record

- Async boundary ADR and explicit ownership model.
- Race, cancellation, shutdown, backpressure, and recovery proof.
- Evidence that synchronous deterministic behavior remains supported.
- Final integrated commit and independent review result.

### Candidate completion evidence (2026-09-01)

Result: Candidate / implementation PASS; K3 is not integrated.

Integrated K2 base: `684ae84a3da94472e4b2263a5c3bfd734574c96f` on local `main`.
The previous implementation/test checkpoint is `bd785f7`; this follow-up adds
the final observation-retention and failed-shutdown proof before the final
local checkpoint is reported separately.

Implementation blocks / explicit Slices actually used: One continuous K3
milestone; no explicit Slice or handoff was needed.

Material decisions: Added the executor-neutral single-owner
`RuntimeDriver<S, D>` and typed `RuntimeHandle`; kept the synchronous Runtime,
WorkflowGraph, capability runtime, execution streams, and DurableStore as
their existing authorities; made durable dispatch and exact `AttemptId`
lineage precede external calls; preserved unknown outcomes and explicit
idempotent retry/reconciliation semantics; and kept shutdown classification
and lossless lifecycle backpressure inside the driver. No Tokio type or async
DurableStore API crosses the public kernel seam. Successful shutdown transfers
final buffered observations to a post-shutdown handle drain before releasing
the runtime; the driver is intentionally `Send` while remaining executor-
neutral through standard-library futures.

Focused verification: K3 async suite 23 passed; K1 physical suite 9 passed;
K2 declarative suite 14 passed; existing runtime suite 9 passed; M2-B durable
suite 18 passed; M2-C1 repair suite 1 passed; M2-C2 integration suite 4
passed; workflow-recovery all-features suite 51 passed.

Broad verification: format, workspace clippy with `-D warnings`, workspace
tests (250 passed), graph-lab smoke, and the K2-base diff check passed locally.
These are candidate results, not CI or integration claims.

Independent review: Deferred until all K3 implementation, documentation,
milestone verification, and checkpoint work is complete; one final review is
required over the complete K2-integrated-base to K3-final range.

Remaining risks and non-goals: No distributed scheduling, provider-specific
retry policy, async DurableStore redesign, general actor framework, plugin
composition, loader, HMR, or capability lifecycle replacement is claimed.

### Repository status reconciliation (2026-09-10, K4 preflight)

The K3 candidate range `d198ac2..715c72a` plus the follow-up repairs
(`bd785f7`, `795deee`) and the owner-loss review repair (`ebfc7d3`, with the
`k3_review_owner_drop.rs` regression) are integrated on `main`, and
`origin/main` matches the local `main`. GitHub Actions CI passed on the exact
final HEAD `ebfc7d3` (run 34446922406). The final independent review over the
complete K3 range is still pending and now also covers the post-closeout
repairs, so K3 is not marked Integrated. This reconciliation records
repository-provable state only; it creates no new milestone identity and no
new Slice.

## K4 — Runtime and Plugin Composition API

### Outcome

A host can assemble a complete runtime from typed modules/plugins without
manually wiring internal crates, while ownership and lifecycle authority remain
unambiguous.

### Contract

**Behavior**

- A composition API registers stable plugin/module identities, capability
  definitions, workflow contributions, configuration requirements, and owned
  lifecycle hooks.
- Construction order is deterministic; missing requirements, duplicate
  ownership, and dependency cycles fail with structured errors.
- Partial startup rolls back only resources owned by the failed composition in
  reverse order.
- Replacement affects future resolution while in-flight attempts retain exact
  capability pins.

**Constraints and preserved behavior**

- The composition layer coordinates existing authorities; it does not become
  a second capability registry, workflow engine, durable store, or stream.
- Prefer static Rust composition and explicit traits over dynamic reflection,
  `Any`-based service location, or ambient global registration.
- Build on K2 definitions and K3 lifecycle/execution boundaries.

**Non-goals**

- Filesystem package discovery, dynamic-library ABI, WASM, HMR watcher,
  marketplace/package manager, or remote plugin execution.
- Cordis syntax compatibility or JavaScript-style proxy behavior.

### Acceptance scenarios

- A small host composes multiple plugins with capability and workflow
  contributions, starts a run, shuts down, and releases owned effects exactly
  once.
- Duplicate identities, missing dependencies, and cycles fail deterministically
  before unrelated work starts.
- Partial initialization failure rolls back owned resources without removing
  pre-existing or sibling registrations.
- Reactive replacement preserves the M2-C in-flight pinning contract.

### Decision and risk boundaries

Plugin ownership, contribution conflicts, rollback, and public extension traits
are long-lived API decisions. A request for dynamic loading is a K5 boundary,
not an implementation detail of K4. Independent lifecycle/API review is
required.

### Completion evidence to record

- Composition ownership model and public API examples.
- Deterministic ordering, conflict, rollback, and replacement proof.
- Cross-crate authority review.
- Final integrated commit and independent review result.

### Candidate delivery evidence (2026-09-10)

The K4 candidate range `4023042..003014d` from
`feat/k4-runtime-plugin-composition` on top of the integrated `main` base
`ebfc7d3` is integrated on `main` by fast-forward merge on 2026-09-10:
`4023042` (K2/K3 repository status reconciliation), `3ad46a0` (ADR 0005
composition architecture decision), `cbb0fc2` (the `runtime-composition`
crate), `466ed96` (acceptance scenarios A-J), `e61cb0b` (candidate delivery
evidence), `bccce29` (integration status reconciliation), and `003014d`
(repair of the stable-1.98 clippy `result_large_err` CI failure by boxing
`StartupFailure::cause`). GitHub Actions CI passed on the integrated HEAD
`003014d` (run 34456738676) after that repair; the preceding HEAD `bccce29`
failed the new lint (run 34454356269). The independent lifecycle/API review
remains pending, so K4 is not marked Integrated. This section records
repository-provable state only; it creates no new milestone identity and no
Slice.

The composition layer is a policy crate over existing authorities: stable
serializable `ModuleDefinition` plus process-local `ModuleRegistration`,
validated by `CompositionBuilder::build()` into a deterministic
`CompositionPlan`, activated through K2 construction/restore into a
`RuntimeAssembly` that bridges to the K3 driver or shuts down exactly once.
One capability slot has one declared typed ownership path (declarative via
merged `RunDefinition` + `FactoryRegistry`, reactive via the owning plugin
fiber). Rollback releases only composition-owned resources in reverse
activation order, continues after individual cleanup failures, and never
fake-disposes un-acquired resources. No second capability registry, plugin
runtime, workflow graph, durable store, execution stream, or Runtime exists.

Focused acceptance evidence: 22 K4 tests across `k4_composition` (6),
`k4_conflicts` (10), `k4_rollback` (1), and `k4_compat` (5) cover all
mandatory scenarios A-J, including registration-order independence,
deterministic cycle failure, contribution-conflict rejection before
activation, partial-startup rollback with continue-after-failure, K2 cold
reconstruction across simulated processes, K3 drive/dispatch/shutdown and
`OwnerDropped` behavior, and M2-C exact attempt pinning across reactive
replacement.

Focused compatibility evidence from the same verification run: K3 async suite
23 passed; K3 owner-loss regression suite 3 passed; K2 declarative suite 14
passed; K1 physical suite 9 passed; existing runtime suite 9 passed; M2-B
durable suite 18 passed; M2-C1 repair suite 1 passed; M2-C2 integration suite
4 passed; `cargo test -p workflow-recovery --all-features` 50 passed. Broad
candidate verification (format, workspace clippy with `-D warnings`, 275
workspace tests, graph-lab smoke, and the base diff check) is recorded in
[K4-runtime-plugin-composition.md](K4-runtime-plugin-composition.md). GitHub
Actions CI passed on the integrated HEAD `003014d` (run 34456738676); the
independent review remains the open item.

### Independent review result and cleanup-ownership repair (2026-09-10)

The independent lifecycle/API review returned CHANGES REQUIRED on integrated
`main` `2ba3fd4`. One blocker: the K4 ownership contract did not close.
`rollback_modules` released dispose hooks and fibers but never unregistered
composition-owned `PluginRuntime` registrations through
`CapabilityRegistry::remove`, and `RuntimeAssembly::into_driver` returned a
`CompositionHandle` that claimed composition disposal without ever holding
the Runtime/`CapabilityRegistry` authority the cleanup required.

The repair is delivered on `fix/k4-composition-cleanup-ownership` (base
`2ba3fd4`), not as a new milestone or Slice. One authority-correct internal
cleanup primitive (`hook → fibers → plugin unregistration`, reverse
activation order, continue-after-failure) now serves startup rollback,
driverless `shutdown`, and the driver path. `CompositionHandle` loses its
unconditional `dispose`: orderly completion passes the K3 `DriverExit` to
`dispose_after_driver`, which re-acquires the Runtime, unregisters the
composition-owned plugins, and returns the released runtime with the
preserved `ShutdownStatus`; owner loss uses the explicit
`release_after_owner_loss`, which best-effort releases the remaining
process-local handles and reports every outstanding registration as a
structured `CleanupResource::PluginRegistration` failure. K3
`DriverError::OwnerDropped` semantics and the capability-graph upstream APIs
are unchanged. Four focused cleanup-ownership tests
(`k4_cleanup_ownership`) plus the extended A/I scenarios prove the invariant
"successful composition cleanup ⇒ no composition-owned plugin registration
remains in the still-live Runtime" on every release path.

### Independent re-review pass reconciliation (2026-09-10, K5 preflight)

The independent re-review over the cleanup-ownership repair returned PASS
with 0 blockers. The repair (`e03778d`, "fix: hold registry authority in K4
composition cleanup") is integrated on `main` and is the `main` HEAD that
K5 takes as its base. The independent lifecycle/API review process for K4 is
therefore closed: the CHANGES REQUIRED blocker was repaired in place, not as
a new milestone, and the re-review returned PASS. K4 is a completed,
integrated milestone. This reconciliation records the recorded review
outcome and repository-provable state only; it creates no new milestone
identity and no new Slice.

## K5 — Minimal Loader Boundary

### Outcome

A host can resolve declarative plugin references through a minimal, explicit
loader boundary and feed the resulting static/in-process plugin definitions to
K4 composition.

### Contract

**Behavior**

- A loader resolves stable logical plugin references against an explicit
  registry/catalog and returns typed plugin definitions.
- Unknown, duplicate, incompatible, or cyclic references fail before runtime
  activation.
- Resolution and activation are separate phases; failed resolution creates no
  runtime-owned effects.
- Loader diagnostics retain enough identity to explain which definition and
  version failed.

**Constraints and preserved behavior**

- Loader policy remains outside capability lifecycle and Runtime durable
  authority.
- Begin with an in-process/static registry unless research proves a stronger
  boundary is required.
- Treat filesystem, network, and executable-code authority as security
  decisions, not convenience adapters.

**Non-goals**

- Dynamic-library ABI, WASM sandbox, arbitrary code download, package manager,
  filesystem watcher, automatic HMR, or remote marketplace.

### Acceptance scenarios

- Declarative references resolve deterministically and compose through K4.
- Missing or incompatible references fail with no partial activation.
- Catalog ordering does not change the resulting composition identity.
- Loader errors do not masquerade as capability, workflow, or durable-store
  failures.

### Decision and risk boundaries

Any move from an in-process registry to filesystem, dynamic library, WASM, or
network loading requires a separate research decision and may become a later
milestone rather than an expanded K5. Loader authority and executable-code
boundaries require independent security/architecture review.

### Completion evidence to record

- Loader authority and trust-boundary decision.
- Determinism, failure isolation, and composition integration proof.
- Explicit list of unsupported loading mechanisms.
- Final integrated commit and independent review result.

### Candidate completion evidence (2026-09-10, ready for independent review)

K5 is delivered as a candidate on `feat/k5-minimal-loader-boundary` with
base `e03778d` (integrated K4 head). It is not integrated; integration
awaits the independent review.

- Loader authority and trust boundary: ADR 0006
  (`docs/architecture/0006-k5-minimal-loader-boundary.md`) records the
  loader/composition separation, the `ModuleReference` logical-request
  semantics, exact-version fail-closed matching, the reference-to-factory
  catalog ownership model, fresh construction per resolution, the
  resolution/activation phase split, the independent `LoaderError` taxonomy,
  the explicit process-local host-trusted catalog, and the explicit
  exclusion of filesystem/dylib/WASM/network loading. The API reference is
  `docs/runtime/K5-minimal-loader-boundary.md`.
- Crate boundary: new `crates/runtime-loader`
  (`loader → composition → core`; composition never references loader
  types). The crate re-exports the host-facing vocabulary so a host depends
  on `runtime-loader` alone; every K5 acceptance suite imports only that
  crate. Vocabulary stays clean: `PluginDefinition`/`PluginRuntime` remain
  capability-graph names and are not reused for loader objects.
- Determinism, failure isolation, and composition integration proof:
  acceptance scenarios A-P are implemented as 16 scenario tests plus 6 unit
  tests in `crates/runtime-loader` (`tests/k5_resolution.rs` A-H,
  `tests/k5_failures.rs` I/J/K/M, `tests/k5_composition.rs` L/N/O/P). B and
  C pin insertion/root-order independence of the resolved set, K4 module
  order, merged K2 canonical identity, and activation events; I pins a
  cycle path rotated to its smallest member and identical from either root;
  L proves two resolutions build two independent registration/plugin
  lifecycles; M proves resolution failures have zero activation effect
  (graph-level failures invoke no factory at all); N proves a K4 activation
  failure reached through the loader stays a `StartupFailure`/
  `CompositionError` with unchanged rollback semantics; O proves two
  independently constructed catalogs in two "processes" produce the same K2
  durable identity and cold-reconstruct through `FileDurableStore`; P
  re-runs the full K3 drive/dispatch/shutdown/`dispose_after_driver`
  contract on a loader-resolved composition. The end-to-end example
  (`cargo run -p runtime-loader --example host_end_to_end`) walks
  catalog → resolve → compose → activate → drive → orderly release with no
  CLI, config parser, or discovery.
- Focused verification on the candidate branch: workspace `cargo fmt --all
  -- --check` clean; `cargo clippy --workspace --all-targets
  --all-features -- -D warnings` clean on stable 1.98.1; `cargo test
  --workspace --all-features` 301 passed / 0 failed; `cargo run -p
  graph-lab` healthy. Focused suites: K5 loader (22), K4
  composition/conflicts/rollback/compat/cleanup-ownership (26), K3 async +
  owner-drop regression (26), K2 declarative (13), K1 physical (9), M2-C
  (m2c1 1 + m2c2 4 + reactive 11), workflow-recovery (unit 6 + durable 12 +
  physical 8 + recovery 24). `git diff --check` against the base is clean.
- Explicitly unsupported (and unimplemented): filesystem discovery,
  directory scanning, dynamic libraries/ABI, WASM/WASI, network/Git/URL
  loading, package managers, marketplaces, registry servers, HMR/watchers,
  automatic reload, remote execution, SemVer solving/lockfiles, manifest or
  config parsing, CLI front-ends, sandboxes, and signing.
- Final integrated commit and independent review result: pending — K5
  integration happens only after independent review APPROVE, mirroring the
  K4 process.

### Main integration reconciliation (2026-09-10)

By repository-owner instruction the candidate branch was fast-forward
merged into `main`: the K5 range `dbf80e6..c0ce2a4` from
`feat/k5-minimal-loader-boundary` is now on integrated `main` on top of
base `e03778d`, with this reconciliation as the following `main`-status
documentation commit. This merge records implementation on integrated
`main`, not review acceptance: no independent-review APPROVE is recorded
yet, CI on the merged `main` result is to be observed, and K5 is
therefore not marked Integrated — exactly the K2/K3 convention. The
pending items remain the independent lifecycle/API review of the loader
boundary and the recorded final integrated commit; a review-initiated
repair would land as its own follow-up fix, mirroring the K4 process.

### Review repair reconciliation (2026-09-10)

The independent review of candidate `c0ce2a4` returned CHANGES REQUIRED
with one blocker: the `ModuleReference` `id@version` textual serde form
was not bijective when a `ModuleVersion` contained `@`. The contract
repair — `@` reserved as the version delimiter inside `ModuleVersion`
only (final-`@` separator grammar, typed
`InvalidReferenceReason::ReservedVersionDelimiter`, ids may contain
`@`, fail-closed on invalid external text, round-trip regression tests,
loader semantics and the K4 boundary unchanged) — landed as follow-up
fix `a4bc425` on the candidate branch and is now merged into `main`.
Verification on the repair: workspace 305 passed / 0 failed, loader
crate 26 tests (10 unit + 16 scenario, A-P unchanged), fmt/clippy clean,
K4/K3/K2/K1 focused suites unchanged, `git diff --check` clean. This
records implementation of the repair on integrated `main`, not review
acceptance: the independent re-review remains pending and K5 is not
marked Integrated.

## K6 — Runtime Kernel API Stabilization and R2 Closeout

### Outcome

The Stage 2 kernel has a coherent supported public surface, documented
compatibility policy, end-to-end examples, and release evidence. K6 stabilizes
the results of K1-K5; it does not add a new runtime subsystem.

### Contract

**Behavior**

- Supported construction, configuration, persistence, execution, composition,
  loading, shutdown, and recovery paths are documented and exercised together.
- Public errors preserve actionable domain boundaries and stable classification.
- Crate exports and feature flags expose supported APIs without requiring
  callers to depend on internal implementation modules.
- Upgrade and compatibility expectations are explicit for definitions,
  persistent schema, and public Rust APIs.

**Constraints and preserved behavior**

- Stabilize only behavior already proven by completed milestones.
- Remove or hide obsolete experimental surfaces only with migration notes and
  repository-wide evidence.
- Keep the three-structure separation and all frozen durability/lifecycle
  invariants.

**Non-goals**

- New loader mechanisms, distributed runtime, agent loop, provider SDK,
  durable Fiber, or speculative Stage 3 abstractions.

### Acceptance scenarios

- A documented end-to-end host creates a configured composed runtime, persists
  progress, terminates, reopens, restores, continues safely, and shuts down.
- Public examples compile and exercise only supported exports.
- Compatibility tests cover persistent schema/definition mismatches and
  supported upgrade behavior.
- README, roadmap, ADRs, runtime contracts, crate docs, and package metadata
  agree on status and scope.

### Decision and risk boundaries

SemVer promises, schema compatibility, feature layout, and removal of public
experimental APIs require explicit review. Any newly discovered missing
behavior is routed back to the owning milestone contract instead of being
silently implemented as K6 cleanup.

### Completion evidence to record

- Supported API inventory and compatibility policy.
- End-to-end cold-restart/composition example evidence.
- Documentation and package consistency audit.
- Full workspace verification, independent public-contract review, final
  integrated commit, and R2 closeout record.

## Stage 3 entry gate

Stage 3 planning starts only after K6 closes Stage 2. Declarative meta-framework
ergonomics, richer plugin lifecycle, HMR, sandboxed executable plugins,
distributed execution, provider SDKs, and agent-facing facilities remain
research candidates rather than scheduled commitments. Each requires new
evidence and its own milestone contract; none should be pulled into K1-K6 by
default.

## Plan maintenance

At milestone completion, update the sequence table status and append the
following concise evidence under that milestone:

```text
Result:
Base:
Final integrated HEAD:
Implementation blocks / explicit Slices actually used:
Material decisions:
Focused verification:
Broad verification:
Independent review:
Remaining risks and non-goals:
```

Before activating the next milestone, verify the predecessor is integrated,
read its recorded evidence, inspect current source/tests/ADRs, and generate a
fresh worker handoff. Future milestone details in this document are stable
outcome constraints, not permission to ignore newer repository evidence.

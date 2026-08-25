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
| K1 | Embedded physical durability | M2-B contract closure | Accepted candidate, ready for integration | Independent - APPROVE |
| K2 | Declarative configuration and cold reconstruction | K1 | Planned | Independent |
| K3 | Explicit asynchronous execution boundary | K2 | Planned | Independent |
| K4 | Runtime and plugin composition API | K2, K3 | Planned | Independent |
| K5 | Minimal loader boundary | K4 | Planned | Independent |
| K6 | Runtime Kernel API stabilization and R2 closeout | K1-K5 | Planned | Independent |

The sequence is dependency order, not a promise that current implementation
details will remain optimal. Reordering requires an explicit plan update that
records the new evidence and preserves completed contracts.

## K1 — Embedded Physical Durability

### K1 Closeout Repair status

Status: Accepted candidate, ready for integration. The repair closed idempotency
lineage validation, bootstrap error classification, identity deserialization
invariants, ledger-only physical wire state, cross-process/logical-corruption
proof, and the interrupted `create_run` to workflow-identity crash window. A
further concurrent first-open CI stability repair is recorded under K1
completion evidence below. Both independent durability/recovery reviews returned
APPROVE with no blocker.

Repository truth: `main` is still at `b6d28b2c` and does not yet contain the
candidate; K1 is therefore an accepted feature-branch candidate awaiting
integration, not an integrated milestone. It must not be described as the final
integrated K1 HEAD until `main` contains the candidate and its CI passes.

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

Result: Accepted candidate, ready for integration. K1 is independently
reviewable and its physical adapter contract is closed; it is pending main
integration.

Base: `31abe5145c2822ac11ce7d70557a069f098f0437`

Accepted candidate implementation HEAD: `ceda06fcd0e81e232a12b9768e974f58bd909bab`
on `feat/k1-s1-durable-store-port` (implementation; the documentation commit
records this evidence). Earlier accepted K1 implementation review:
`c12befe4287cdebfcb737c39ebadd693ed71f92b`. `main` remains at
`b6d28b2c92c04747c1825c45c319471e02ff1a5c`; do not label any branch SHA as the
final integrated K1 HEAD until `main` contains it.

Bootstrap crash window: Closed. An existing row is repaired only when the
pristine bootstrap predicate holds; identity recovery uses the existing
`workflow-replay-identity` idempotency key and `StoreRevision::INITIAL` CAS.

#### Concurrent first-open CI stability repair

Status: Accepted candidate, ready for integration.

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
delta (`ab92bbe..ceda06f`) requires its own focused independent review; see K1
closeout.

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

# KERNIS R2 — Stage 2 Kernel Contract

Status: Stage 2 entry-point contract. R2 closes with the K6 independent
review; until that review completes, this document records the candidate
contract, not a self-declared closure.

This is the final entry document for Stage 2 (the Runtime Kernel, R2).
It states what the kernel is today, which surface a host should use, and
where each guarantee lives. It links to the milestone documents and ADRs
instead of repeating them.

## What KERNIS R2 is

R2 is a **local, embedded, deterministic runtime kernel** that composes
capability-bearing modules into one coordinated run:

- one declarative `RunDefinition` describes tasks, dependencies, capability
  requirements, and logical effects for a run;
- modules are resolved from a host-supplied explicit catalog by logical
  `ModuleReference` (exact version, no discovery), composed into one
  runtime assembly, and driven to durable completion;
- external effects are recorded as durable facts before and after dispatch,
  so a cold restart restores exactly what happened and never re-executes
  completed or unconfirmed work by accident.

R2 is explicitly **not** a framework, plugin marketplace, distributed
runtime, or dynamic-loading host (see Non-goals).

## Status vocabulary

One small vocabulary is used for milestone state (defined here; used by
`README.md`, `docs/ROADMAP.md`, and `docs/runtime/STAGE2-MILESTONE-PLAN.md`):

- **Planned** — contract defined, implementation not delivered.
- **Candidate** — implementation delivered on a branch, awaiting
  integration.
- **Integrated** — the implementation range is on `main` and CI passed on
  it. This says nothing about review.
- **Reviewed** — an independent review APPROVE (or a re-review PASS with
  0 blockers after repairs) is recorded in the milestone plan.
- **Closed** — Integrated and Reviewed, with closeout evidence recorded in
  the milestone plan.

Dated reconciliation entries inside the milestone plan preserve the wording
of the day they were written; the milestone table and the current-status
sections use this vocabulary.

Current Stage 2 state: K1, K4, K5 are Integrated and Reviewed (K5 after the
`a4bc425` repair and re-review PASS). K2 and K3 are Integrated; their
independent reviews were never separately recorded and are absorbed into
the K6/R2 combined review. K6 is Integrated (merged into `main` as
`c6c3a60`; CI run 34489698724 passed all gates); its independent review
is still pending, so K6 is not Reviewed and R2 is not Closed until that
review completes.

## Crate layering

```text
runtime-loader        host entry: ModuleReference resolution against a ModuleCatalog
    ↓
runtime-composition   ModuleDefinition/ModuleRegistration → CompositionPlan → RuntimeAssembly
    ↓
runtime-core          deterministic coordination, RunDefinition, sync Runtime + async driver
    ↓
workflow-graph        task topology and completion authority
capability-graph      capability/service/plugin lifecycle authority
workflow-recovery     durable facts and recovery classification (DurableStore)
execution-stream      ordered observation streams
    ↓
core                  neutral primitives (Id)
```

Direction is structurally enforced by Cargo: a reverse edge would be a
package cycle and fail to resolve. `graph-lab` is an executable smoke
experiment, not a runtime dependency. See ADR 0001 for the three-structure
architecture and ADR 0006 for the loader boundary.

## Authority model

Each authority owns exactly one kind of truth; the kernel only coordinates.

| Authority | Owns |
| --- | --- |
| `WorkflowGraph` | topology and completion facts |
| `DurableStore` (`workflow-recovery`) | durable truth and recovery classification |
| Capability scope / `ReactiveCapabilityRuntime` | capability lifecycle and replacement |
| Execution streams | observation only — never authority |
| `Runtime` (`runtime-core`) | deterministic coordination between authorities |
| `RuntimeDriver` / `RuntimeHandle` | async ownership and effect awaiting for one runtime |
| Composition (K4) | module assembly, activation order, rollback |
| Loader (K5) | logical reference resolution only — owns no authority |

Details: `docs/architecture/0001-three-structures.md`,
`docs/runtime/K5-minimal-loader-boundary.md`.

## Recommended host entry

`runtime-loader` is the single supported host entry (ADR 0007). A host
depends on `runtime_loader` alone; the crate re-exports every type a host
must name across the load → compose → activate → drive lifecycle. The
closure rule and the four API tiers (host-facing / subsystem /
experimental-compat / internal) are recorded in
[`K6-supported-api-inventory.md`](K6-supported-api-inventory.md).

## Paths

### Construction

Stable, serializable `ModuleDefinition`s declare tasks
(`TaskDefinition`), capability contributions, and effects.
`RunDefinition` is the merged declarative definition of the whole run; its
identity is `kernis-run-definition-v1` and must match a durable run's
recorded identity on restore. See [`COMPATIBILITY.md`](COMPATIBILITY.md) §2
and `docs/runtime/K2-declarative-cold-reconstruction.md`.

### Loading

`RuntimeLoader::new(&catalog).resolve(roots)` deterministically closes the
dependency-reference graph against the host-supplied `ModuleCatalog`
(exact-version, no substitution) and instantiates fresh
`ModuleRegistration`s through catalog factories. Failures are typed
`LoaderError` (missing/duplicate/incompatible reference, factory failure).
There is zero dynamic loading: no discovery, dylibs, WASM, or network
(ADR 0006).

### Composition

`ResolvedModules::compose()` (or `CompositionBuilder`) validates the merged
plan — module ordering, capability ownership, contribution conflicts — with
no side effects, yielding `CompositionPlan`. Activation is K2
(`start_with_store` / `restore`) and full cleanup is K4 rollback. See
`docs/runtime/K4-runtime-plugin-composition.md` and ADR 0005.

### Execution

`plan.start_with_store(run_id, config, store)` or `plan.restore(...)` yields
a `RuntimeAssembly`; `into_driver(dispatcher)` yields the supported triple
`(RuntimeDriver, RuntimeHandle, CompositionHandle)`. The host spawns
`driver.run()` and issues typed commands through the handle: `drive`,
`dispatch_effect`, `recover`, `cancel_task`, `drain_*_events`, `shutdown`.
Effects carry `EffectSemantics` (`Idempotent` / `NonIdempotent`); dispatch
outcomes are `Succeeded` / `Failed` / unknown, and unknown outcomes restore
to recovery classification, never silent re-execution.
See `docs/runtime/K3-explicit-async-boundary.md` and ADR 0004.

### Durability and recovery

`DurableStore` records intents, dispatches, outcomes, completions, and
cancellations as durable facts; `FileDurableStore` is the embedded physical
store (redb backend, `KERNIS-DURABLE-STATE` format v5). Recovery
classification is deterministic from durable facts alone: known success
completes without re-execution; idempotent unknown may retry; non-idempotent
unknown requires reconciliation and shutdown classifies it explicitly.
The `Runtime` is a coordinator — the store remains the recovery authority.
See `docs/runtime/M2-B-durable-state-model.md` and
[`COMPATIBILITY.md`](COMPATIBILITY.md) §3.

### Shutdown (the supported lifecycle)

The single normal lifecycle (§22 of the K6 contract):

```text
RuntimeAssembly::into_driver
→ RuntimeDriver::run            (spawned by the host)
→ RuntimeHandle::shutdown       → ShutdownStatus
→ DriverExit
→ CompositionHandle::dispose_after_driver
→ final Runtime release         (drop of the returned shutdown outcome)
```

Abnormal owner loss: a dropped handle makes the driver exit with
`DriverExit::owner_dropped()`; the host then calls
`CompositionHandle::release_after_owner_loss`, which performs best-effort
cleanup and returns a structured report of any incomplete registration
cleanup. Examples and docs must not use the pre-K4 incomplete shutdown
order.

## Public error domains

Failures never collapse into a catch-all error. Four independent domains,
each `std::error::Error` with `source()` chains where a cause exists:

- `LoaderError` — reference/catalog resolution (K5).
- `CompositionError` / `StartupFailure` — plan validation and activation,
  with the rollback report (K4).
- `RuntimeError` / `DriverError` — coordination and async ownership (K2/K3).
- `StoreError` (+ `StoreErrorKind`) — durable backend facts and corruption
  fail-closed (K1/M2-B).

Audit details and the documented `String`-payload limitations:
[`K6-supported-api-inventory.md`](K6-supported-api-inventory.md).

## Compatibility guarantees

Recorded in [`COMPATIBILITY.md`](COMPATIBILITY.md): Rust API surface rules,
`RunDefinition` identity (v1, no v2 created), physical schema
`KERNIS-DURABLE-STATE` version 5, `ModuleReference` textual grammar
bijectivity (K5 repair `a4bc425`), exact-version semantics, K4
`ModuleDefinition` frozen contribution semantics, and the preserved legacy
`kernis-workflow-replay-v1` identity.

## MSRV

The workspace declares `rust-version = "1.85"` and treats it as a contract:
CI contains a `msrv` job running `cargo check --workspace --all-features` on
1.85, and K6 verified `cargo +1.85 test --workspace --all-features`.

## Non-goals (unsupported by design in R2)

No dynamic module loading (filesystem discovery, dylibs, WASM, network), no
HMR, no provider SDK or plugin ABI, no scheduler subsystem, no durable-fiber
serialization, no distributed runtime, no agent loop, no MCP integration,
and no unqualified fsync/power-loss guarantee. Missing behaviors belong to
future milestones, not to R2.

## End-to-end example

- `crates/runtime-loader/examples/r2_host.rs` — the canonical R2 host: the
  full supported lifecycle plus a genuine cold restart (fresh catalog,
  loader, registrations, plan, and runtime against the same physical store;
  durable facts verified; remaining work continued, never redone).

  ```bash
  cargo run -p runtime-loader --example r2_host
  ```

- `crates/runtime-loader/examples/host_end_to_end.rs` — the K5 loader walk.
- `crates/runtime-loader/tests/k6_r2_end_to_end.rs` — the R2 acceptance
  proof, including true cross-process restart via the K1/K2 child-process
  pattern.

## Known limitations

- Some error variants carry `String` payloads (factory reasons, invalid
  identities); their *text* is not a stability promise — the variant and
  domain are. (Inventory audit, §8.)
- The physical store guards backend-open panics on corrupted files
  (typed `DataCorruption`), but a redb panic during commit after opening a
  externally-corrupted file is not guarded.
- K2 and K3 carry no separately recorded independent review; the K6/R2
  combined review covers them.
- All workspace crates are `publish = false`; publication is a deliberate
  deferral, not an oversight.

# Kernis

A Rust Meta-Runtime Kernel

## What Kernis is

Kernis is a Rust meta-runtime kernel research and implementation project.

It studies runtime and meta-framework systems such as Cordis, durable
workflow runtimes, effect systems, and execution frameworks, validates their
useful semantics through reproducible experiments, and implements the
mechanisms that survive those experiments as a reusable runtime kernel.

Its long-term goal is to evolve that kernel into a composable meta-framework
for building dynamic, reactive, durable runtimes.

Kernis is not a Cordis clone and not a generic graph framework. Research is
the method; implementation is the outcome.

## Why it exists

Runtime systems often collapse capability composition, workflow orchestration,
and execution data into one abstraction. Kernis keeps these concerns separate
so that ownership, mutation, lifecycle, recovery, and observation semantics
can be tested independently before they become reusable kernel contracts.

## Architecture

Kernis preserves three independent semantic structures:

### Capability Runtime

Capability composition owns scopes, exact provider identity, dependency
resolution, process-local fibers, replacement, withdrawal, and effect cleanup.

### Workflow Runtime

Workflow orchestration owns task topology, dependencies, readiness, mutation,
completion, and deterministic task admission.

### Execution Streams

Execution Streams own ordered, bounded observations such as lifecycle events,
progress, telemetry, and high-frequency output. Stream loss is not recovery
authority.

### Durable Authority

DurableStore/DurableJournal own workflow replay identity, completion lineage,
effect intent, dispatch, outcome, cancellation, and operation/attempt lineage.
`WorkflowGraph` remains the authority for topology and completion semantics;
durable completion facts are replayed into a fresh graph before scheduling.
Durable facts are kept separate from process-local fibers and live capability
handles.

### Runtime Core

Runtime Core coordinates these authorities without owning all of their truth.
`Runtime` remains a synchronous deterministic task-admission boundary and
owns one process-local `ReactiveCapabilityRuntime`. Reactive mutation crosses
an explicit async reconciliation boundary before it affects a new admission.

No shared public `Graph` trait or generic dynamic graph runtime is used to
unify these domains.

## Research methodology

Kernis follows this loop:

```text
Research
  -> validate semantics
  -> implement kernel mechanism
  -> stabilize reusable boundary
  -> evolve toward meta-framework
```

Experiments can reject an abstraction, but research exists in service of
implementation rather than as an endpoint separate from the kernel.

## Cordis and other references

Cordis is a primary research reference for capability composition, contextual
dependency injection, lifecycle, reactive replacement, plugin composition,
and meta-framework semantics. Kernis does not aim to reproduce Cordis
literally. It also draws on workflow runtime research, durability/recovery
systems, Effect/ZIO-like concepts, typed execution-stream semantics, and
future validated references.

## Current implementation status

- M1 Runtime Core: implemented and integrated.
- M2-A Capability Runtime: integrated.
- M2-B0: complete.
- M2-B1: in-memory `DurableStore`/restart slice and durable completion/replay
  contract closure implemented.
- Stage 2 K1: embedded physical `FileDurableStore` implemented with redb,
  versioned typed snapshots, reopen, CAS/idempotency, and crash-window proof.
- Stage 2 K2: declarative definition, process-local factory separation, and
  cold reconstruction candidate implemented on
  `feat/k2-declarative-cold-reconstruction`; not integrated.
- M2-C1: Integrated / Closed at
  `589827af0156fa0d3f25f5bb6f4044f2be61b527`.
- M2-C2: Integrated / Closed at
  `60066dcfb7d7038d64da441f3ee852893fbd9119`.

The current implementation deliberately makes no unqualified fsync/power-loss
claim and does not include a plugin loader, HMR watcher, provider SDK,
WASM/dynamic-library ABI, MCP integration, agent loop, distributed runtime, or
durable fiber serialization.

## Long-term direction

### Stage 1 — Research & Semantic Validation

Validate the architecture and preserve the useful invariants from E01-E05,
M1, M2-A, M2-B, M2-C1, and M2-C2.

### Stage 2 — Runtime Kernel

Develop the current implementation into a reusable kernel. The ordered K1-K6
milestone contracts cover physical durability, declarative reconstruction, the
async execution boundary, runtime/plugin composition, a minimal loader, and
API stabilization. See the
[Stage 2 Runtime Kernel Milestone Plan](docs/runtime/STAGE2-MILESTONE-PLAN.md).

### Stage 3 — Meta-Framework

Long-term possibilities include declarative capability/plugin composition,
runtime configuration, plugin lifecycle, dynamic replacement, workflow
integration, durable execution, extension APIs, and developer-facing
framework ergonomics. These are direction, not promises of an unvalidated ABI
or distributed design.

## Workspace

```text
crates/
  core/               Neutral primitives (`kernis-core` package)
  capability-graph/   Capability/service/plugin dependency experiments
  workflow-graph/     Task DAG and orchestration experiments
  execution-stream/   Typed ordered runtime stream primitives
  workflow-recovery/  Durable facts and recovery classification
  runtime-core/       Deterministic runtime coordination and recovery
  graph-lab/          Small executable for experiments and smoke checks

docs/
  architecture/       Architecture decisions
  research/           Research, references, and semantic experiments
  runtime/            Runtime authority and milestone contracts
```

Domain crate names remain descriptive; the project identity is Kernis.

## Repository

[github.com/jesenzhang/kernis](https://github.com/jesenzhang/kernis)

## First commands

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo run -p graph-lab
```

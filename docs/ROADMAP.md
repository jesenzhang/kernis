# Kernis Roadmap

Kernis is a Rust meta-runtime kernel research and implementation project.
Research validates which runtime and meta-framework semantics belong in a
reusable kernel; implementation is the outcome of that research.

## Stage 1 — Research & Semantic Validation

Validate and freeze the architecture through reproducible experiments:

- E01-E05 semantic research;
- M1 Runtime Core;
- M2-A Capability Runtime;
- M2-B durable authority, in-memory restart slice, and completion/replay
  contract closure;
- M2-C1 reactive lifecycle;
- M2-C2 Runtime-owned reactive capability boundary.

Stage 1 establishes the separation between capability composition, workflow
orchestration, and execution streams. It does not mean research stops; new
research remains evidence for later kernel decisions.

## Stage 2 — Runtime Kernel

Turn the validated implementation into a reusable runtime kernel. The ordered
implementation plan is maintained in
[`runtime/STAGE2-MILESTONE-PLAN.md`](runtime/STAGE2-MILESTONE-PLAN.md):

1. K1 Embedded Physical Durability;
2. K2 Declarative Configuration and Cold Reconstruction;
3. K3 Explicit Asynchronous Execution Boundary;
4. K4 Runtime and Plugin Composition API;
5. K5 Minimal Loader Boundary;
6. K6 Runtime Kernel API Stabilization and R2 Closeout.

K2 and K3 are implemented on integrated `main` and pass CI
(GitHub Actions run 34446922406 on `ebfc7d3`), with focused completion
evidence recorded in the Stage 2 plan. Their independent reviews remain
pending — only targeted review-feedback fixes are recorded — so neither is
marked Integrated. K4 is implemented on integrated `main` (fast-forward of
`feat/k4-runtime-plugin-composition` ending at `e61cb0b` plus its status
reconciliation) with acceptance evidence in
[`runtime/K4-runtime-plugin-composition.md`](runtime/K4-runtime-plugin-composition.md);
its independent review is pending, so it is likewise not marked Integrated.

The milestone is the delivery unit. Slice boundaries are introduced only when
current implementation risk, ownership, verification, or context quality makes
one useful; the plan does not prescribe a Slice chain.

## Stage 3 — Meta-Framework

The long-term direction is a composable meta-framework for dynamic, reactive,
durable runtimes. Possible capabilities include declarative capability/plugin
composition, runtime configuration, plugin lifecycle, dynamic replacement,
workflow integration, durable execution, extension APIs, and developer-facing
framework ergonomics.

These are directional goals, not commitments to an unvalidated ABI, loader,
distributed scheduler, or provider SDK.

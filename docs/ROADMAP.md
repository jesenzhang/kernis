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

Turn the validated implementation into a reusable runtime kernel. Candidate
directions, intentionally not implemented by the R1 identity repositioning,
include:

- physical durability adapter;
- runtime and plugin composition APIs;
- configuration reconstruction and API stabilization;
- an explicit async execution boundary;
- a minimal loader boundary;
- further Cordis and meta-runtime semantic research.

Each candidate requires its own semantic boundary and regression evidence.

## Stage 3 — Meta-Framework

The long-term direction is a composable meta-framework for dynamic, reactive,
durable runtimes. Possible capabilities include declarative capability/plugin
composition, runtime configuration, plugin lifecycle, dynamic replacement,
workflow integration, durable execution, extension APIs, and developer-facing
framework ergonomics.

These are directional goals, not commitments to an unvalidated ABI, loader,
distributed scheduler, or provider SDK.

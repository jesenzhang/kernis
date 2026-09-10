# ADR 0007: R2 host entry surface and public API tiers

Status: accepted for K6 (Stage 2 / R2 closeout)

## Context

K5 made `runtime-loader` the de facto host entry: it re-exports the
host-facing vocabulary of K4/K3/K2/K1 and capability lifecycle, and the
K5 acceptance suites and end-to-end example import `runtime-loader`
alone and complete resolve → compose → activate → drive → shutdown
against that dependency alone. K6 must decide whether that role is
official, and must pin what "supported public surface" means before any
export stabilization, documentation, or compatibility promise is made.

Two candidate answers existed at K6 start:

- **A.** `runtime-loader` is the Stage 2 recommended complete host entry
  crate; the lower crates remain subsystem entries for deliberate
  dependents.
- **B.** Hosts depend on loader/composition/runtime crates separately and
  assemble their own import surface.

Creating a new umbrella crate (`kernis`, `kernis-runtime`, `kernis-sdk`)
was explicitly on the table and is explicitly rejected here.

## Decision

### 1. Option A: `runtime-loader` is the canonical host entry

`runtime-loader` is the recommended single host dependency for Stage 2.
Rationale:

- The K5 example and all sixteen acceptance scenarios already prove the
  full host flow on that crate alone; Option B would formalize a split
  nobody needs.
- The re-export lists are curated host vocabulary (three grouped blocks
  mirroring composition's documented "upstream types the host passes
  through or receives"), not an undifferentiated dump.
- The crate owns no authority: its authority is exactly logical
  reference resolution (ADR 0006). Documentation freezes that the host
  entry surface is not an authority owner.

Rejected: creating any new facade crate. The existing layering already
provides a coherent host surface; a second umbrella would duplicate the
closure problem and blur the authority narrative. Revisit only if a
future inventory proves the crate layering cannot express a needed host
surface.

### 2. Re-export closure rule

Supported host code must be able to **name** every type it receives on a
supported path: enum payloads it matches on and values it binds. Types
reachable only through method-call inference (inspection accessors) are
subsystem tier. The K6 audit found 19 host-reachable types that violated
this at base `3ee011b` (e.g. `RuntimeEvent`, `StreamItem`,
`Cancellation`, `RecoveryDecision`, `DurableRunState`, `StoreError`,
`AttemptId`); K6 closes the gaps with additive `pub use` only — nothing
was hidden or removed at this stage.

### 3. Four public API tiers

Host-facing supported / subsystem supported / experimental-compatibility
/ internal, with the promises defined in
`docs/runtime/K6-supported-api-inventory.md` and
`docs/runtime/COMPATIBILITY.md`. Not all `pub` is stable: documented
legacy baseline names (`Capability`, `FiberState`, `WorkflowState`,
`ReactiveRuntime`, alias methods) and reserved extension points
(`JournalInvariant::Reserved`) are explicitly experimental/compatibility,
retained but never repurposed silently.

### 4. Error-domain stabilization is additive

The loader/composition/runtime/store error domains stay type-distinct
(no `Unknown(String)` facade). K6 adds `Error::source()` implementations
where supported error enums wrap typed inner errors, and explicitly
retains the documented String diagnostic payloads (plugin factory seam,
`StoreError::IoFailure`/`DataCorruption`, activation-stage reasons)
rather than rewriting proven diagnostics.

### 5. No publication at R2

All crates are `publish = false` with `description`/`repository`
metadata. Publication, versioning of the published set, and path→version
dependency conversion are a future packaging decision, not Stage 2
closeout work.

### 6. MSRV is a public contract

`rust-version = "1.85"` must be verified by an actual build, not
assumed; the CI gate added in K6 enforces `cargo +<msrv> check
--workspace --all-features`.

## Consequences

- Hosts get one stable import surface; new host-facing types must be
  threaded through the closure rule in all three runtime crates.
- The umbrella's re-export blocks become compatibility surface under
  COMPATIBILITY.md §1.
- Internal-only types stay exactly where they are (`pub(crate)`/private);
  the closure rule never forces them public beyond their existing
  crate's surface.
- Authority documentation must keep stating loader = entry, not owner
  (R2-KERNEL-CONTRACT.md authority table).
- The inventory document is the baseline for any future deprecation
  decision; deprecations follow the migration-note discipline.

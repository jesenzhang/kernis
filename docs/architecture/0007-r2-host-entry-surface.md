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

Supported host code must be able to **name** every type it must bind to a
named value: a value it stores in its own struct or field, names in a helper
signature, or returns from its own function. A type that a host only *matches*
inside an exported enum payload — `match error { ScopeError::GraphAdmission(..) => .. }`
— is bound by inference and does not have to be nameable; those payload types
stay at the subsystem tier, and a host that wants to store one depends on the
subsystem crate directly. Types reachable only through method-call inference
(inspection accessors) are subsystem tier as well. The K6 audit found 19
host-reachable types that violated
this at base `3ee011b` (e.g. `RuntimeEvent`, `StreamItem`,
`Cancellation`, `RecoveryDecision`, `DurableRunState`, `StoreError`,
`AttemptId`); K6 closes the gaps with additive `pub use` only — nothing
was hidden or removed at this stage.

The rule is about *naming*, so the proof has to name things. The 2026-09-28
R2 review found five more violations of this rule that every existing
consumer had missed, precisely because examples and test import lists let
type inference do the naming (`FactoryRegistry`, `RUN_DEFINITION_FORMAT`,
`RuntimeAssembly`, `RecoveryReason`, `StoreInvariant`). Closure is therefore
checked structurally, not by import lists: `fixtures/r2-host-fixture` is a
compile-only crate whose only dependency is `runtime-loader`, which is not a
workspace member (so no subsystem crate is reachable through workspace
unification), and which annotates every supported-path value with an
explicit type. It is compiled on stable and on the MSRV as part of the
closeout gate, and `crates/runtime-loader/tests/review_r2_exports.rs` keeps
the same vocabulary as an in-workspace regression. The repair's own audit
found three further gaps of the same kind, closed additively as well
(`StoreRevision`, `CapabilityConflictReason`, `InvalidId`; plus
`LEGACY_RUN_DEFINITION_FORMAT_V1` once ADR 0008 made the refused legacy tag
a fact a host must recognize). Inventory waves are listed in
`docs/runtime/K6-supported-api-inventory.md`.

That proof is a whitelist and was treated as stronger than it is. The third
review (2026-10-22, B1) showed two things: eight host-path names were still
unnameable, and three umbrella re-exports were never named by the fixture at
all, so deleting them would have kept both "proofs" green. The repair resolves
this by kind rather than by widening the surface:

- **Bound values are promoted.** `CapabilityId`, `Generation` and `EntryId`
  are the coordinate types a host binds from `CapabilityPin::generation`,
  `CapabilityPin::entry_id` and `CapabilityHandle::id()`; they are now
  re-exported along core → composition → loader and named by the fixture.
- **Match-only payloads stay subsystem tier.** `CapabilityGraphError`,
  `JournalInvariant` and `IdempotencyKey` are payloads a host matches without
  naming; the inventory lists each one with that tier.
- **Inspection accessors stay subsystem tier.** `Runtime::workflow()` and
  `Runtime::journal()` return `WorkflowGraph` and `DurableJournal`, which the
  inventory now names alongside `Scope`, `CapabilityRegistry` and
  `CapabilityContext`.
- **The proof can fail.** `crates/runtime-loader/tests/export_coverage.rs`
  extracts every `pub use` name from the umbrella and fails when one is absent
  from the closure evidence, which is what makes the whitelist safe.

Promoting any of the remaining names to host tier is a separate, recorded
decision, not a drive-by export.

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

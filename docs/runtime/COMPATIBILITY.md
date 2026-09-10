# Kernis Stage 2 (R2) Compatibility Policy

Status: R2 closeout policy. This document defines what the Stage 2
Runtime Kernel promises across releases, and what it explicitly does not
promise. It is part of the R2 contract; see
[R2-KERNEL-CONTRACT.md](R2-KERNEL-CONTRACT.md) and the supported API
tiers in [K6-supported-api-inventory.md](K6-supported-api-inventory.md).

Kernis R2 makes **no indefinite backward-compatibility promise**. Each
dimension below has an explicit, bounded policy. Breaking a promise is a
milestone-level decision with a recorded migration note, never a
drive-by change.

## 1. Rust public API compatibility

Policy for the **host-facing supported** tier (the `runtime-loader`
umbrella and its mirrored `runtime-composition` vocabulary):

- Additive changes (new items, new defaulted capabilities, new trait
  methods with defaults) may ship in minor releases.
- Removing or renaming a public item, changing a signature, or changing
  observable semantics requires a major-version change (pre-1.0: an
  explicit 0.x breaking release with a migration note) **and** a
  repository-wide search proving no supported path needs the old form
  (K6 §7 discipline).
- The re-export closure rule of ADR 0007 is part of the promise: a type
  a host must name on a supported path stays nameable through
  `runtime-loader`.
- The **subsystem supported** tier follows the same process per crate;
  because subsystem users depend on crates directly, narrowing that tier
  is cheaper but still requires the same evidence and migration note.
- **Experimental / compatibility** items (`Capability`, `FiberState`,
  `ReactiveRuntime`, `WorkflowState`, alias methods, the `next()`
  fail-fast pair, `JournalInvariant::Reserved`) may be deprecated or
  narrowed with a migration note; they are never silently repurposed.
- `pub(crate)`/private items carry no promise at any tier.

## 2. RunDefinition identity compatibility

- `RUN_DEFINITION_FORMAT = "kernis-run-definition-v1"` is a **versioned
  format** promise: the canonicalization behind
  `RunDefinition::canonical_identity` is frozen for v1. R2 stores and
  compares these identities; a v1 durable identity must keep validating
  under the same definition bytes.
- Classification: **stable within v1**; semantic change ⇒ new format tag
  (v2) plus an explicit migration decision. K6 explicitly created no
  v2.
- Presentation-only fields (task display labels) remain excluded from
  the identity, as documented by K2.
- The typed constructors (`DefinitionIdentity::new`,
  `RunDefinition::validate`) stay fail-closed on invalid stable
  declaration fields.

## 3. DurableStore physical schema compatibility

- The `FileDurableStore` file format is a **versioned physical schema**:
  `FORMAT_MAGIC = "KERNIS-DURABLE-STATE"`, `FORMAT_VERSION: u16 = 5`
  (v5 postcard snapshot + outer checksum, ledger-only wire state).
- Promise: a store file written by any R2 release reopens on any later
  R2 release and recovers identical durable state. The K6 R2
  acceptance suite pins this with a written-then-reopened physical
  fixture (`k6_r2_end_to_end`), and the K1 cross-process proof remains
  green.
- Incompatible `FORMAT_VERSION`/magic and corrupted payloads remain
  **fail-closed** (`StoreError::DataCorruption`, distinct from
  `IoFailure`/`BackendUnavailable`). This fail-closed behavior is itself
  part of the contract.
- No automatic schema migration is promised; schema bumps are a
  milestone decision with a migration tool/decision recorded before
  release. Power-loss/`fsync` durability remains explicitly unclaimed
  (K1 boundary).
- `InMemoryDurableStore` is the deterministic conformance reference; it
  has no on-disk format and no schema promise.

## 4. ModuleReference textual compatibility

- The `id@version` display/serde form is a **versioned textual
  format**: the final `@` is the separator; an `Id` may contain `@`; a
  `ModuleVersion` may not contain `@` (`@` is reserved as the version
  delimiter); blank versions are rejected. Serialize/deserialize is
  bijective for every valid reference (`org@app@1` parses as id
  `org@app`, version `1`).
- Invalid external text fails closed with
  `LoaderError::InvalidReference` + typed `InvalidReferenceReason`.
- Grammar changes require a new tagged representation; R2 promises the
  K5 repair (`a4bc425`) grammar verbatim.

## 5. ModuleVersion exact semantics

- `ModuleVersion` is an **exact opaque label**: matching is exact;
  `app@2` never resolves `app@1`. There is no ordering, no semver
  solving, no ranges, and no lockfiles — and the R2 contract promises
  none. Loader metadata (versions) never enters `RunDefinition`,
  `DurableStore`, or replay identity, so version-string handling cannot
  affect durable behavior.

## 6. K4 ModuleDefinition compatibility

- `ModuleDefinition` is the stable, serializable half of the two-plane
  model. Its contribution semantics are frozen for R2: task
  contributions merge into the canonical merged `RunDefinition`
  (identity-ordered); declarative capabilities contribute to the merged
  definition + factory registry; reactive capabilities contribute
  nothing durable; one capability slot has exactly one declared
  ownership path; `requiring_config`/`with_optional_config` gate
  activation on `HostConfig`.
- Adding a new contribution kind is a policy decision (it changes the
  merged identity surface) and requires a milestone record; R2 adds no
  new contribution kind.
- `ModuleRegistration` (the process-local half) is never serialized and
  has no wire-compatibility promise by design.

## 7. Legacy K1 workflow replay identity

- The live-object `kernis-workflow-replay-v1` algorithm is **preserved
  verbatim** for `start_run*`/`restore_run` and their legacy identity
  mutations (K2 constraint). Legacy durable runs keep restoring through
  the legacy API.
- `kernis-run-definition-v1` canonicalization stays **separate** from
  legacy provenance: no automatic fallback and no durable identity
  migration is provided or promised. A declarative runtime rejects
  `configure_task`/`apply_workflow_mutation` fail-closed before any
  legacy identity write.
- K6 verified with pinned regression tests that a known-valid legacy
  identity still restores, mismatches still fail closed
  (`RuntimeError::WorkflowReplayIdentityMismatch` /
  `RuntimeError::DefinitionMismatch`), presentation-only differences
  keep their documented identity effect, and semantic changes change
  the identity.

## 8. Error payload stability

- Error **domains** are stable and non-collapsing: resolution →
  `LoaderError`; planning/activation → `CompositionError` /
  `StartupFailure`; execution/coordination → `RuntimeError` /
  `DriverError`; durable truth → `StoreError` (+ `StoreErrorKind`
  classification). None collapses into a catch-all string error.
- Typed variant structure for the R2 host-facing error enums is part of
  the public API tier (removal/renaming follows §1). `Error::source()`
  chains the typed inner errors of the supported domains after K6.
- Explicitly **not promised**: the exact text of any `Display` output,
  and the contents of String-carrying diagnostic payloads (plugin
  factory `Err(String)`, `StoreError::IoFailure`/`DataCorruption`
  messages, `ActivationStage` reasons, `EffectDispatchError` reason).
  Treat them as human-readable diagnostics only.

## 9. MSRV

- The workspace declares `rust-version = "1.85"` (edition 2024). K6
  verifies this is a real contract, not unverified metadata: the
  declared MSRV is checked with `cargo +1.85 check --workspace
  --all-features` (and `cargo +1.85 test --workspace --all-features`
  at K6 closeout), and CI runs `cargo check --workspace --all-features`
  on 1.85 in a dedicated `msrv` job on every push.
- Raising the MSRV is a public API-tier change under §1 (a
  dependency-level MSRV bump forces it too); it requires the verification
  above plus a closeout note. Lowering it requires the same proof at the
  lower toolchain.

## 10. Publication and packaging

- All workspace crates are `publish = false` at R2 closeout: no
  crates.io publication has been decided, the crates interdepend via
  unversioned path dependencies, and packaging is a future milestone
  decision (ADR 0007). `cargo package` is therefore not an R2 gate.
  Crate `description`/`repository` metadata is set so the intent is
  discoverable if publication is later decided.

## 11. Explicitly not promised (non-goals)

No promise of: dynamic loading (filesystem/dylib/WASM/network) or any
loader beyond the K5 in-process catalog; HMR/hot reload; distributed or
multi-process scheduling; durable Fiber or runtime-object serialization;
arbitrary schema migrations; power-loss/fsync guarantees beyond what
specific tests demonstrate; provider SDKs, MCP, or agent-loop facilities;
SemVer solving; any Stage 3 meta-framework ergonomics.

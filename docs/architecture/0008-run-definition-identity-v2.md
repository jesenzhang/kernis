# ADR 0008: Declarative run-definition identity v2 and the legacy v1 read policy

Status: proposed for the K6/R2 repair (Stage 2 / R2 closeout); it supersedes the "no v2"
clause of `docs/runtime/COMPATIBILITY.md` §2, of
`docs/runtime/R2-KERNEL-CONTRACT.md` "Compatibility guarantees", and of the K6 "Material
decisions" record in `docs/runtime/STAGE2-MILESTONE-PLAN.md` once the independent
re-review accepts it.

## Context

`RunDefinition::canonical_identity` computes the durable identity that
`Runtime::start_from_definition*` writes and `Runtime::restore_from_definition` compares
(`docs/runtime/K2-declarative-cold-reconstruction.md`, `COMPATIBILITY.md` §2). The
canonicalization is `kernis-run-definition-v1`: a flat string in which every field is
appended as `:<byte-length>:<value>` (`crates/runtime-core/src/definition.rs`,
`canonical_identity` / `append_identity_part`).

Length-prefixing the *strings* does not make the *structure* decodable. Lists (task
prerequisites, capability requirements, capability dependencies) are delimited only by fixed
keyword strings, and those keywords are ordinary business strings. The 2026-09-28 K6/R2
review produced a two-definition counterexample that passes `validate()` and yields
**byte-identical** `kernis-run-definition-v1` identities, after which
`restore_from_definition` accepted a definition with different admission semantics
(`docs/reviews/2026-09-28-K6-R2-rereview.md` R2-01; counterexample and log in
`docs/reviews/evidence/r2-2026-09-28/`). Both strings contain this token sequence:

```text
... :12:dependencies:12:capabilities:1:x:12:capabilities:6:effect:6:absent ...
```

Definition A reads `dependencies = []`, `required_capabilities = [(x, capabilities)]`;
definition B reads `dependencies = [capabilities, x]`, `required_capabilities = []`.
`"dependencies"`, `"capabilities"`, and `"x"` are simultaneously structural markers and
legitimate user-supplied identifiers, so the v1 grammar is not injective: identity equality
no longer implies definition equivalence, and the restore guard K2 and R2 depend on ("a
changed definition fails closed before any effect") is unsound for v1 bytes.

Two properties make this undecidable *per stored identity* rather than merely inconvenient:

1. From a stored v1 identity alone there is no way to know which parse was the original one,
   so "compare the recomputed v1 identity" can never prove the incoming definition is the
   definition that produced the durable state.
2. The stored bytes carry no per-identity ambiguity marker: an identity that happens to have
   a unique parse is indistinguishable, in the format contract, from one with several.

`COMPATIBILITY.md` §2 already prescribes the exit: "semantic change => new format tag (v2)
plus an explicit migration decision"; the v1 canonicalization itself is frozen and must not
be redefined in place. K6 deliberately created no v2. This ADR creates one because the
alternative - keeping a non-injective identity - invalidates a restore-safety promise that
K2, K6, and R2 all state.

## Decision

### 1. New write format: `kernis-run-definition-v2`

`RUN_DEFINITION_FORMAT` becomes `"kernis-run-definition-v2"`, and it is the only format the
kernel writes. The v2 canonical representation separates structure from user data by
construction:

```text
canonical    := "kernis-run-definition-v2" capabilities tasks

capabilities := "#capabilities[" COUNT "]" *capability "#end"
capability   := "#capability" VALUE VALUE VALUE requirements
requirements := "#requires[" COUNT "]" *pair "#end"
pair         := VALUE VALUE                       (capability id, definition identity)

tasks        := "#tasks[" COUNT "]" *task "#end"
task         := "#task" VALUE dependencies requirements effect
dependencies := "#depends[" COUNT "]" *VALUE "#end"
effect       := "#effect#absent"
              / "#effect#present" VALUE semantics
semantics    := "#idempotent" / "#non_idempotent"

VALUE        := "%" <decimal byte length> ":" <that many UTF-8 bytes>
COUNT        := <decimal number>                  (terminated by "]")
```

Properties, each of which is a test obligation rather than an aspiration:

- **Tags and data are disjoint.** Every structural token begins with `#`; every data token
  begins with `%` and is length-delimited, so no identifier (`tasks`, `capabilities`,
  `dependencies`, `effect`, `present`, `absent`, `#end`, `%3:x`, `12:capabilities`, digits,
  `:`, `#`, `@`, or non-ASCII text) can imitate structure or shift a field boundary.
- **The data marker is not `@`.** `@` is the host-facing module-reference separator
  (`module@version`, K5), and a durable K2 identity must stay lexically distinguishable
  from loader reference syntax; the reviewed loader regression
  (`scenario_o_cold_reconstruction_works_through_the_loader`) asserts that no `@` appears
  in a composition-derived definition identity, and `%` keeps that guard meaningful
  instead of forcing it to be relaxed.
- **Collections carry an element count and an explicit end tag**, so an empty list and a
  missing field are distinct, and a list of the wrong arity cannot re-read as its neighbour.
- **Every field is tagged**, so reordering canonicalizer fields cannot silently redefine what
  a byte sequence means.
- **Semantic equivalence rules are unchanged from K2:** declarations, prerequisites,
  requirements, and dependencies are ordered by logical identity, task display labels stay
  excluded, and meaningful topology / capability / effect-semantics changes must change the
  identity.

### 2. v1 bytes are frozen as a historical interpretation, not as a write format

`kernis-run-definition-v1` stays byte-for-byte as it is: the v1 canonicalizer remains in the
crate, keeps producing the same output for the same definition, and is pinned by golden-byte
tests so no later change can silently redefine it. Its tag is exported as
`LEGACY_RUN_DEFINITION_FORMAT_V1` for diagnostics. The kernel never *writes* a v1 identity,
and no v1 output changes as a side effect of this repair.

`kernis-workflow-replay-v1` (the K1 live-object identity behind `start_run*`/`restore_run`)
is untouched: same algorithm, same task-label field, same field ordering, same pinned
fixture.

### 3. Legacy v1 declarative identities are read fail-closed with a typed error

`Runtime::restore_from_definition` classifies the durable identity **before** any factory
construction, reconstruction, or store write:

- v2 durable identity -> require exact equality with the recomputed v2 identity, else the
  existing `RuntimeError::DefinitionMismatch`.
- v1 declarative durable identity -> new typed
  `RuntimeError::LegacyDefinitionIdentityUnsupported { durable_identity, supported_format }`.
  Automatic restore and automatic migration are both refused: the stored bytes cannot
  establish which definition produced them, and guessing would silently accept a definition
  with different task admission semantics - the defect under repair.
- any other identity (including a `kernis-workflow-replay-v1` legacy live-object run) -> the
  existing `RuntimeError::DefinitionMismatch`, unchanged.

Refusing every v1 identity, including ones that happen to be uniquely parsable, is
deliberate: per-identity decidability would make the accepted set a function of the
canonicalizer's keyword list, and any later keyword addition could retroactively make a
previously accepted identity ambiguous. The rejected alternatives record that trade-off.

### 4. Migration contract (decided here, deliberately not implemented in this package)

A v1 -> v2 migration needs evidence the durable state does not contain, so this package ships
the contract and no migration code, per the repair handoff ("本修复包不自动执行真实数据迁移"):

- **Inputs:** the *trusted original* `RunDefinition` (a source-controlled authoring input,
  not a value reconstructed from the ambiguous identity) plus an explicit host-side
  authorization for that run id.
- **Preconditions:** the durable identity is a v1 declarative identity; the trusted
  definition validates; its recomputed **v1** identity equals the durable identity
  (otherwise the host is not holding the definition that produced the durable state).
- **Write:** one `DurableMutation::RecordWorkflowReplayIdentity(<v2 identity>)` committed with
  the existing compare-and-swap discipline (`CommitRequest::expected_revision` at the revision
  that was read, plus an idempotency key), so a stale writer cannot re-apply a superseded
  mapping: A -> B -> A sequences must fail closed on revision mismatch.
- **Refusals:** anything not provably a v1 declarative identity stays fail-closed. No
  fallback matching, no re-derivation from history, no best-effort scoring.
- **Non-goals here:** no automatic migration at open/restore time, no CLI, and no rewrite of
  the physical `KERNIS-DURABLE-STATE` v5 format (the identity is an opaque string in the
  existing v5 payload, so no `FORMAT_VERSION` bump is required).

### 5. Breaking-change and migration note (pre-1.0)

Per `COMPATIBILITY.md` §1/§2 this is an explicit 0.x change of observable semantics, not an
additive one:

- Declarative run-definition identity bytes change for every definition (new format tag).
  Repository-wide call sites that asserted the v1 prefix are updated to the constant plus
  golden-byte fixtures; no supported path keeps producing or requiring a v1 identity.
- Durable declarative runs written before this decision no longer auto-restore; they return
  `RuntimeError::LegacyDefinitionIdentityUnsupported` until an authorized migration exists.
  The remedy is named in the error and in `COMPATIBILITY.md` §2.
- One new `RuntimeError` variant (a new case in a public enum) and a new
  `RUN_DEFINITION_FORMAT` value; `DefinitionError`, `StoreError`, and `RuntimeError` domains
  stay distinct (ADR 0007 §4).

## Consequences

- `RUN_DEFINITION_FORMAT` is host-facing and must stay nameable through `runtime-loader`;
  `LEGACY_RUN_DEFINITION_FORMAT_V1` and the new typed error join the K6 inventory under the
  ADR 0007 §2 closure rule, which the same repair closes for R2-04.
- Identity acceptance becomes provably injective in the encoded structure: the R2-01
  counterexample must produce different identities and a rejected restore with zero factory
  calls and zero durable writes.
- `COMPATIBILITY.md` §2, `R2-KERNEL-CONTRACT.md`,
  `K2-declarative-cold-reconstruction.md`, the K6 inventory, README, and ROADMAP record v2
  plus the fail-closed v1 policy; the Stage 2 plan records this repair without erasing the K6
  "no v2" historical wording.
- Any future keyword, field, or ordering change to the v2 canonicalizer is itself a format
  change requiring a new tag and this ADR's migration discipline.
- Hosts holding pre-repair durable declarative runs are blocked from automatic restore until
  the migration API lands. That is the accepted cost of refusing to guess, and the reason the
  contract is written down now rather than during an incident.

## Rejected alternatives

- **Hash the current canonical string (any digest, any width).** Compression preserves
  collisions; two definitions that hash alike still authorize each other's restore. Nothing
  about the non-injective grammar is fixed.
- **Reserve-word escaping or renaming the colliding keywords.** Patches the two strings in the
  counterexample while the ambiguity class stays open for every identifier a host may
  legitimately use - and silently changes v1 bytes.
- **Unique-parse v1 decoder** (auto-restore when the v1 token stream admits exactly one
  grammar parse). Rejected for this package: the accepted set depends on the keyword list,
  the decoder becomes permanent compatibility surface, and its proof obligation ("no future
  keyword can make an accepted identity ambiguous") cannot be met by a repair bounded to R2.
  It remains the only acceptable basis for a later, separate ADR.
- **Change `kernis-run-definition-v1` in place** (add counts to the v1 string). One tag would
  produce two different byte meanings over time, silently breaking the v1 promise
  `COMPATIBILITY.md` §2 freezes: a compatibility violation dressed as a fix.
- **Guess-migration on open** (rewrite a v1 identity to v2 using the currently supplied
  definition). Exactly the failure mode of the defect: a different definition that recomputes
  the same v1 bytes would be blessed by the migration itself.
- **Auto-restore v1 with a warning.** Keeps accepting a definition that may carry different
  admission semantics and downgrades a durable-safety violation to diagnostics.
- **Bump the durable physical `FORMAT_VERSION`.** The stored payload already carries the
  identity as an opaque string; the ambiguity is in the identity grammar, so a schema bump
  would break every store without resolving the identity question.
# K4: Runtime and Plugin Composition API

Status: Integrated on `main`; review CHANGES REQUIRED; cleanup-ownership
repair on `fix/k4-composition-cleanup-ownership` pending independent
re-review

Integrated `main` base: `ebfc7d3`. The K4 candidate range
`4023042..003014d` from `feat/k4-runtime-plugin-composition` is integrated
on `main` by fast-forward merge on 2026-09-10: `4023042` (K2/K3 repository
status reconciliation), `3ad46a0` (ADR 0005 composition architecture
decision), `cbb0fc2` (the `runtime-composition` crate), `466ed96`
(acceptance scenarios A-J), `e61cb0b` (candidate delivery evidence),
`bccce29` (integration status reconciliation), and `003014d` (repair of the
stable-1.98 clippy `result_large_err` CI failure by boxing
`StartupFailure::cause`), with the `main` status head at `2ba3fd4`.

The independent lifecycle/API review returned CHANGES REQUIRED with one
blocker: composition-owned plugin registrations were never unregistered
during rollback/shutdown, and `CompositionHandle` claimed composition
disposal without holding the Runtime/`CapabilityRegistry` authority that
cleanup requires. The surgical repair is delivered on
`fix/k4-composition-cleanup-ownership` (base `2ba3fd4`) and awaits an
independent re-review; it is not a new milestone. This document describes
the repaired, review-contract-consistent API.

K4 adds a host-facing composition layer that assembles one runtime from typed
modules without manual internal-crate wiring. It does not replace the K2
declarative authority, the K3 driver boundary, the capability lifecycle, or
the durable store, and it is not a second `CapabilityRegistry`,
`PluginRuntime`, workflow graph, `DurableStore`, execution stream, or
`Runtime`.

## Interface

`runtime-composition` exposes two contribution planes and one activation
product:

- `ModuleDefinition`: the stable, serializable module surface — module id,
  module dependencies, task contributions (`TaskDefinition`), capability slot
  contributions, and configuration requirements (`ConfigRequirement`). It
  contains no closure, future, handle, or process-local object and is
  cold-reconstruction material.
- `ModuleRegistration`: the process-local wrapper around one definition that
  carries factory closures, `PluginRuntime` handles, and lifecycle hooks. It
  is never serialized and never written to the `DurableStore`.
- `CompositionBuilder`: `register(registration)?` rejects duplicate module
  identity immediately; `build()?` performs all validation and deterministic
  planning and acquires nothing.
- `CompositionPlan`: the validated plan — merged K2 `RunDefinition`,
  deterministic module order, capability slot table, and the merged
  `FactoryRegistry` — with `start`, `start_with_store`, and `restore`
  activation through K2.
- `RuntimeAssembly<S>`: the activated composition with `runtime()`,
  `runtime_mut()`, `module_order()`, `host_config()`, the async exactly-once
  `shutdown()` driverless path, and `into_driver(dispatcher)` returning the
  K3 `RuntimeDriver`/`RuntimeHandle` pair plus a `CompositionHandle`.
- `CompositionHandle`: composition-owned activation resources with no
  unconditional `dispose`. Orderly completion consumes the K3 `DriverExit`
  in `dispose_after_driver`, which re-acquires the Runtime, runs cleanup
  with full registry authority, and returns `CompositionDriverShutdown`
  (`rollback`, the preserved `shutdown_status`, and the released `runtime`
  for final inspection and release). After owner loss
  (`DriverError::OwnerDropped`), `release_after_owner_loss` best-effort
  releases the remaining process-local handles and reports every outstanding
  registration as a lost-authority failure.
- `CompositionError`: typed variants for every planning, conflict,
  activation, rollback, and construction failure — no generic string
  catch-all, with underlying definition/runtime errors preserved as `source`.
  Startup failure returns a `StartupFailure` carrying the causal error plus
  the `RollbackReport`.

## Deterministic planning and ownership

`build()` validates the complete composition before anything activates:
module dependency resolution, a deterministic topological order
(Kahn traversal with a smallest-`Id` frontier so registration order cannot
change the order, the merged definition, or behavior), one-owner capability
slot arbitration, task/factory/plugin contribution conflicts, and
configuration requirements. Planning is side-effect free: no activation,
registration, or resource acquisition happens during validation.

Each capability slot declares exactly one typed ownership path
(`CapabilityOwnership`):

- `Declarative`: merged into the K2 `RunDefinition` and constructed by the
  merged `FactoryRegistry` during `Runtime` construction/restore; may be
  required by tasks; its definition identity is part of the K2 replay
  identity.
- `Reactive`: published by its owning `PluginRuntime` fiber for the fiber
  lifetime; contributes nothing to the merged `RunDefinition` and cannot be
  required by a task.

Factory and plugin contributions must match the slot table exactly
(undeclared slots, factories on reactive slots, identity mismatches,
duplicate contributions, missing coverage, and unowned publications all fail
with typed errors before activation).

## Activation, rollback, and release

Activation runs register → validate → plan → merge → merged factories →
ownership ledger → K2 construct/restore → per-module plugin registration →
fiber instantiate/start → `activate` hooks → reconciliation to the stable
reactive boundary. No task executes before activation succeeds. The ownership
ledger records only resources activation actually acquired; there is no
registry wipe, teardown scope, or Runtime recreation rollback.

On partial startup failure the ledger unwinds in reverse activation order: a
module's `dispose` hook runs only if its `activate` hook completed, started
fibers dispose in reverse contribution order, successfully registered plugin
runtimes unregister in reverse registration order through
`CapabilityRegistry::remove`, un-armed disposers never run, cleanup continues
after an individual failure, and every failure is collected into the
`RollbackReport` with the structured `CleanupResource` class (`DisposeHook`,
`Fiber`, or `PluginRegistration`). Pre-existing or sibling registrations the
composition never acquired are untouched.

One authority-correct cleanup primitive backs every release path: startup
rollback and driverless `RuntimeAssembly::shutdown` run it against the
still-live Runtime, and the driver path recovers the Runtime through
`DriverExit::into_runtime` inside `CompositionHandle::dispose_after_driver`
before claiming cleanup completion. The invariant: when K4 reports
composition cleanup completed, no composition-owned `PluginRuntime`
registration remains inside the still-live Runtime. After the driver owner
is lost the registry authority disappeared with the Runtime;
`release_after_owner_loss` therefore releases only the composition's
remaining process-local handles (hooks and fibers, exactly once) and records
every outstanding registration as a `PluginRegistration` failure instead of
claiming orderly completion.

## Evidence

The focused K4 suites are `cargo test -p runtime-composition`.
They cover every mandatory acceptance scenario:

- A — three-module composition starts, drives to completion through the K3
  driver, shuts down, and releases exactly once (`k4_composition`);
- B — `A B C` / `C A B` / `B C A` registration yields identical activation
  order, identical merged-definition replay identity and slot table, and
  identical behavior (`k4_composition`);
- C/D — duplicate module and missing dependency fail with typed errors and
  zero activation (`k4_composition`);
- E — cycles fail deterministically with an identical cycle path regardless
  of registration order (`k4_composition`);
- F — duplicate task ownership, duplicate capability ownership, conflicting
  factories, slot/identity/plugin-claim conflicts, and configuration
  conflicts all fail before activation; a missing required configuration
  fails activation with no side effects (`k4_conflicts`);
- G — partial startup rolls back B then A: each armed disposer runs exactly
  once, the failing module's un-armed disposer and the never-activated module
  are not fake-disposed, cleanup continues after B's failing hook, and the
  collected report names the failure (`k4_rollback`);
- H — K2 cold reconstruction survives a simulated process replacement: same
  `ModuleDefinition`s, fresh `ModuleRegistration`s, the same
  `FileDurableStore`, restore succeeds with the reactive fiber republished
  (`k4_compat`);
- I — the K3 driver contract holds through the assembly: drive/dispatch/
  shutdown with `ShutdownStatus::Clean`, and owner loss resolves commands
  with `DriverError::OwnerDropped` while composition-owned release stays
  independent (`k4_compat`);
- J — reactive replacement preserves M2-C exact pinning: the old attempt
  keeps its exact V1 entry after `replace_and_reconcile`, the new attempt
  pins the V2 entry identity with the same replay identity (`k4_compat`).

A cross-plane case additionally proves a reactive plugin fiber resolves a
declaratively owned factory value as a dependency (`k4_compat`).

Cleanup-ownership regressions from the review repair (`k4_cleanup_ownership`,
plus the extended A/I scenarios): driverless `shutdown` unregisters the
plugin with the strong-count delta observed from inside the sweep before the
runtime drop; startup rollback after a registered plugin unregisters it even
when the module's own fiber disposal fails, continuing with the remaining
modules; while the driver owns the Runtime the composition handle disposed
nothing and the only orderly release entry point consumes the `DriverExit`,
after which the registration is verified absent from the returned runtime
with the `ShutdownStatus` preserved; owner loss releases hooks and fibers
exactly once and reports the lost registry authority as a structured
`PluginRegistration` failure without changing `OwnerDropped` classification.

Focused compatibility evidence from the same verification run: K3 async suite
23 passed; K3 owner-loss regression suite 3 passed; K2 declarative suite 14
passed; K1 physical suite 9 passed; existing runtime suite 9 passed; M2-B
durable suite 18 passed; M2-C1 repair suite 1 passed; M2-C2 integration suite
4 passed; `cargo test -p workflow-recovery --all-features` 50 passed.

## Candidate checkpoint verification

- `cargo fmt --all -- --check`: PASS
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`:
  PASS, 0 errors
- `cargo test --workspace --all-features`: PASS, 275 tests
- `cargo run -p graph-lab`: PASS
- `git diff --check ebfc7d3`: PASS
- GitHub Actions CI on `main` HEAD `003014d`: PASS, run 34456738676
  (Format, Clippy, Test, Graph lab).

The preceding integrated HEAD `bccce29` failed CI (run 34454356269):
stable toolchain 1.98 added the `result_large_err` lint, which flagged the
168-byte `Err` variant of the composition start results. `StartupFailure`
now boxes its `cause`, the workspace clippy and 275-test suite re-passed
locally on 1.98.1, and CI passed on the repair `003014d`. The local
checkpoint results above predate the merge; HTTPS to `github.com` was
filtered in this environment, so the pushes went over `ssh.github.com:443`.

## Review repair verification (2026-09-10)

The cleanup-ownership repair runs on
`fix/k4-composition-cleanup-ownership` (base `2ba3fd4`) and is a candidate
awaiting independent re-review, not an integration.

- `cargo fmt --all -- --check`: PASS (after applying rustfmt to the new
  `k4_cleanup_ownership` suite)
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`:
  PASS, 0 lint warnings (the only console warnings are cargo's incremental
  cache hard-link notices on this Windows filesystem, which are not lints)
- `cargo test --workspace --all-features`: PASS, 279 tests (the 275
  candidate baseline plus the 4 new `k4_cleanup_ownership` regressions)
- `cargo run -p graph-lab`: PASS
- `git diff --check 2ba3fd4...HEAD`: PASS

Focused K4 suites on the repair: `k4_composition` 6, `k4_conflicts` 10,
`k4_rollback` 1, `k4_compat` 5, `k4_cleanup_ownership` 4 — 26 passed.
Focused compatibility suites from the same run, unchanged against the
candidate baseline: K3 async 23, K3 owner-loss regression 3, K2 declarative
14, K1 physical 9, existing runtime suite 9, M2-B durable 18, M2-C1 repair
1, M2-C2 integration 4, `cargo test -p workflow-recovery --all-features` 50.

## Non-goals

Filesystem package discovery, dynamic-library ABI, WASM, HMR watching,
marketplace/package management, remote plugin execution, dynamic loading, and
any K5 loader boundary are explicitly out of scope. Plugin/module hot reload,
configuration hot updates, and multi-runtime hosting are not claimed.

This document remains a pending-review record until the independent review
over the complete integrated range is complete. K4 is implemented on
integrated `main`, but is not marked Integrated by this document.

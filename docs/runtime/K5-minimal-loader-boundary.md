# K5: Minimal Loader Boundary

Status: Candidate — ready for independent review. Not integrated.

Base: `main` at `e03778d` (K4 integrated, independent re-review PASS).
Branch: `feat/k5-minimal-loader-boundary`.

K5 adds one thin, host-facing layer between "the host declares which logical
modules a run needs" and "K4 receives resolved in-process module
registrations":

```text
host-declared ModuleReference roots
        |
        v
RuntimeLoader + explicit ModuleCatalog   (phase 1: resolution — LoaderError)
        |  ResolvedModules (inert: ordered references + fresh registrations)
        v
K4 CompositionBuilder / CompositionPlan  (phase 2: composition — CompositionError)
        |
        v
Runtime activation / driver              (phase 3: activation — StartupFailure)
```

The host only names logical needs; it never hand-assembles the module set.
K5 is explicitly **not** a dynamic plugin loader: no filesystem discovery,
directory scanning, `dlopen`/`LoadLibrary`, dynamic-library ABI, WASM/WASI,
network/Git/URL fetching, package manager, marketplace, registry server,
hot reload, watcher, or remote execution. It reuses every existing authority
unchanged: K2 declarative identity and reconstruction, M2-C capability
lifecycle, the K3 driver boundary, and K4 composition, activation, and
rollback. K5 owns exactly one thing: turning references into registrations,
deterministically and with typed failures. See ADR 0006 for the full
decision record.

## Interface

`crates/runtime-loader` (dependency direction: `loader → composition →
core`; composition never references loader types). A host may depend on
`runtime-loader` alone — it re-exports the host-facing composition,
capability, runtime, and durable vocabulary.

- `ModuleReference` — a stable logical module identity (`Id`) plus an exact
  `ModuleVersion` label, displayed as `id@version`. A logical request, not
  an artifact handle: no path, URL, digest, or package coordinate. Exact
  matching only — `app@2` never resolves an `app@1` entry. Loader metadata
  only: versions never enter `RunDefinition`, `DurableStore`, or replay
  identity.
- `ModuleVersion` — exact opaque label; blank rejected.
- `CatalogEntry` — one exact reference, its declared dependency references,
  and a `ModuleRegistrationFactory` (`Arc<dyn Fn() ->
  Result<ModuleRegistration, ModuleFactoryError> + Send + Sync>`).
- `ModuleCatalog` — the explicit, process-local set of entries the host is
  willing to run. `register` is fallible-by-value and rejects a duplicate
  exact reference instead of overwriting. Distinct versions of the same id
  are distinct entries.
- `RuntimeLoader::new(&catalog).resolve(roots)` — synchronous,
  side-effect-free resolution to `ResolvedModules`.
- `ResolvedModules` — inert data: resolved references in deterministic
  order plus one freshly constructed registration each;
  `into_composition_builder()` / `compose()` hand them to K4 explicitly.
- `LoaderError` — the loader's own taxonomy (see below).

## Deterministic resolution

1. **Collect roots** — sorted; a repeated exact root is rejected
   (`DuplicateRootReference`), never silently deduplicated.
2. **Close the graph** — deterministic frontier traversal over
   `BTreeMap`/`BTreeSet`; a reference absent from the catalog fails
   `MissingReference` naming the requested reference, the requesting parent
   (if any), and the dependency path; a cycle fails `DependencyCycle` with
   a path rotated to its lexicographically smallest member, so the same
   cycle always reports the same path no matter which root surfaced it.
3. **Classify compatibility** — each entry's declared dependency identities
   must match the produced definition's dependency identities; the produced
   definition's id must equal the entry reference's id
   (`IncompatibleEntry`).
4. **Construct** — only after the graph is confirmed, factories run in
   stable reference order, once per entry per `resolve`. Nothing is cached:
   the catalog is not a live registration cache, and two resolutions (or
   two processes with two catalogs) get two independent registration sets.

Catalog insertion order and root request order cannot change the resolved
set, the K4 module order, the merged K2 canonical identity, or activation
behavior.

## Error taxonomy: three phases, three families

| Phase | Type | Raised by | Examples |
| --- | --- | --- | --- |
| Resolution (before any Runtime effect) | `LoaderError` | `resolve` / `ModuleCatalog::register` | `InvalidReference`, `DuplicateCatalogEntry`, `DuplicateRootReference`, `MissingReference`, `DependencyCycle`, `IncompatibleEntry`, `RegistrationConstructionFailed` |
| Composition planning (side-effect-free validation) | `CompositionError` | `compose` / `build` | duplicate module id, slot/factory/plugin conflicts |
| Activation | `StartupFailure` (carrying `CompositionError`) | `start` / `restore` | missing required config, activation hook failure, rollback report |

The boundary is honest in both directions: the loader never rewrites a K4
failure into a loader error (an activation failure reached through the
loader is still a `StartupFailure` — scenario N), and K4/K3 behavior is
unchanged for loader-resolved compositions (scenarios N-P). A resolution
failure has zero Runtime-owned effect: no hook runs, no plugin registers,
and graph-level failures (missing/cycle/duplicate) do not even invoke a
factory.

## Trust boundary

The catalog is host-supplied, explicit, and process-local. Every factory is
code compiled into the binary before start — exactly as trusted as any K4
registration today. K5 adds no new trust surface and therefore no sandbox,
signature, or permission machinery. Vocabulary is also boundary-clean:
`PluginDefinition`/`PluginRuntime` remain capability-graph's plugin
lifecycle names and are never reused for loader concepts.

## Acceptance evidence

The loader suites import only `runtime_loader`, which is itself part of the
acceptance story (a single-crate host). Scenario coverage:

| Scenario | Test |
| --- | --- |
| A transitive resolve → K4 activation | `k5_resolution.rs::scenario_a_references_resolve_transitively_and_compose_through_k4` |
| B catalog insertion order irrelevant | `k5_resolution.rs::scenario_b_catalog_insertion_order_changes_nothing` |
| C root order irrelevant | `k5_resolution.rs::scenario_c_root_order_changes_nothing` |
| D unknown root | `k5_resolution.rs::scenario_d_unknown_root_fails_before_any_activation` |
| E missing transitive reference | `k5_resolution.rs::scenario_e_missing_transitive_dependency_names_the_requester` |
| F version mismatch fail-closed | `k5_resolution.rs::scenario_f_version_mismatch_fails_closed` |
| G duplicate catalog entry | `k5_resolution.rs::scenario_g_duplicate_catalog_entry_is_rejected` |
| H duplicate roots | `k5_resolution.rs::scenario_h_duplicate_root_reference_is_rejected` |
| I cycle, deterministic path | `k5_failures.rs::scenario_i_reference_cycle_reports_a_deterministic_path` |
| J incompatible registration | `k5_failures.rs::scenario_j_incompatible_registration_fails_closed` |
| K dependency metadata disagreement | `k5_failures.rs::scenario_k_dependency_metadata_disagreement_is_rejected` |
| L fresh process-local registrations | `k5_composition.rs::scenario_l_every_resolution_constructs_fresh_registrations` |
| M resolution failure zero activation effect | `k5_failures.rs::scenario_m_resolution_failures_have_zero_activation_effect` |
| N K4 rollback regression | `k5_composition.rs::scenario_n_k4_rollback_semantics_survive_the_loader_boundary` |
| O K2 cold reconstruction, two processes | `k5_composition.rs::scenario_o_cold_reconstruction_works_through_the_loader` |
| P K3 driver contract kept | `k5_composition.rs::scenario_p_driver_contract_is_kept_for_loader_resolved_compositions` |
| End-to-end example | `cargo run -p runtime-loader --example host_end_to_end` |

Verification on the candidate branch: `cargo fmt --all -- --check` clean;
`cargo clippy --workspace --all-targets --all-features -- -D warnings`
clean (stable 1.98.1); `cargo test --workspace --all-features` 301 passed /
0 failed (loader crate: 22 = 6 unit + 16 scenario tests); `cargo run -p
graph-lab` healthy; `git diff --check` against base `e03778d` clean.

## Non-goals

Filesystem or directory discovery; dynamic libraries and their ABI;
WASM/WASI; network, Git, or URL loading; package managers, marketplaces,
registry servers; hot reload, watchers, automatic reload; remote module
execution; SemVer solving, version ranges, or lockfiles; manifest/TOML/
YAML/JSON schema and config parsers; CLI front-ends; sandboxes or signing;
any change to K2 durable identity, K3 driver semantics, or K4 composition
rules. Any future dynamic-loading milestone should add a discovery adapter
feeding this same explicit catalog, not change this boundary.

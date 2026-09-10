# ADR 0006: Minimal loader boundary

Status: accepted for K5 implementation

## Context

K4 gave the host one composition call chain — register modules, build a
validated plan, activate through K2, drive through K3, release exactly the
owned resources. What the host still has to do by hand is assemble the
module set itself: pick which process-local `ModuleRegistration` objects to
construct, make sure every declared module dependency is actually present,
construct everything up front even for modules the run will not use, and
fail with whatever internal error the construction chain happens to
produce. There is no named place where the host says *which logical modules
this run needs* and gets back exactly the registrations that satisfy that
request — or a typed reason why it cannot.

K5 adds that named place: a thin loader boundary between "the host declares
logical module references" and "K4 receives resolved in-process module
registrations". It is deliberately not a plugin loader in the dynamic sense.

## Decision

### A separate loader layer, not more composition

Add a new host-facing crate `runtime-loader` that depends on
`runtime-composition` (and through it `kernis-core`), never the reverse.
Composition answers *how do these concrete modules assemble into one
runtime*; the loader answers *which concrete registrations correspond to
these logical requests*. Those are different failure domains: composition
validates contributions against each other, while the loader resolves
identity references against an explicit catalog. Folding resolution into
`CompositionBuilder` would make "the host asked for something that does not
exist" a composition error, and would force composition to know about
version references and catalogs — vocabulary it must not own. The dependency
direction `loader → composition → core` keeps each crate's error taxonomy
and vocabulary honest.

### `ModuleReference` is a logical request, not an artifact handle

A `ModuleReference` is a stable logical module identity (`kernis_core::Id`)
plus an exact `ModuleVersion` label, formatted `id@version`. It names *what*
the host wants, not *where* it comes from: no path, no URL, no digest, no
package coordinate. It is not the K2 durable identity and does not enter
`RunDefinition`, `DurableStore`, or replay identity — versions are loader
metadata used only to select catalog entries. Resolution is an exact match
on the pair: `app@1.2.0` never matches an `app@1.1.0` entry.

### Exact version matching, not a version solver

The catalog contains what the host explicitly registered, so there is no
resolution *problem* to solve: a request either names a registered entry or
it fails. SemVer ranges, compatibility windows, and dependency solving would
introduce a second, hidden selection policy that could make the resolved set
depend on which versions happen to be present — the opposite of K5's
determinism requirement. If a future milestone needs ranges, that is a new
contract with its own review, not a silent extension of this one.

### The catalog owns reference-to-factory bindings, and nothing else

`ModuleCatalog` maps each exact `ModuleReference` to a `CatalogEntry`: the
declared dependency references and a factory that constructs a fresh
`ModuleRegistration` (`Fn() -> Result<ModuleRegistration,
ModuleFactoryError>`, `Send + Sync`, held behind an `Arc`). The catalog
owns: duplicate-reference rejection, declared dependency metadata for graph
closure, and the construction recipe. It does not own instances, activation
state, capability registrations, or any live runtime object — see the next
point. Catalog construction is explicit host code (or its equivalent at the
process startup edge); `ModuleCatalog::register` returns
`DuplicateCatalogEntry` instead of overwriting, so two sources can never
quietly shadow one another.

### Fresh construction per resolution, so the catalog is not a cache

Each `resolve` invokes every entry factory exactly once and hands the
resulting registrations to the caller; nothing constructed is retained. If
the catalog cached live `ModuleRegistration` objects, it would silently
become a process-wide plugin runtime cache with identity shared across runs
and processes — reintroducing exactly the stable/process-local confusion K2
and K4 separated. Fresh construction makes "two resolutions, two
registration object sets" a property of the contract, verified in acceptance
scenario L (distinct factory invocations, distinct plugin instances, two
independent activations).

### Resolution and activation are fully separated

`RuntimeLoader::resolve` is a synchronous, side-effect-free graph
computation over reference and metadata: closure collection, cycle and
missing-reference classification, typed compatibility checks, then factory
construction of the confirmed closure. It never composes, starts, restores,
registers, instantiates fibers, or runs hooks. The returned
`ResolvedModules` is inert data — ordered references plus constructed
registrations — which the host hands to K4 explicitly
(`into_composition_builder()` / `compose()`). Failures at resolve time
therefore have provably zero Runtime-owned effect: no hook ran, no plugin
was registered, and for graph-level failures not even a factory was
invoked. This preserves the K4 rule that planning acquires nothing, one
phase earlier.

### Loader errors stay the loader's own taxonomy

`LoaderError` (`InvalidReference`, `DuplicateCatalogEntry`,
`DuplicateRootReference`, `MissingReference`, `DependencyCycle`,
`IncompatibleEntry`, `RegistrationConstructionFailed`) is a third error
family, distinct from `CompositionError` (planning) and `StartupFailure`
(activation), with a three-phase boundary: each phase reports its own
error type and nothing is re-labeled. A K4 activation failure stays a
`StartupFailure` even when the composition arrived through the loader
(scenario N); a missing reference never becomes a `CompositionError`; a
duplicate module identity inside the composed set stays K4's
`DuplicateModule` rather than being rewritten into a loader error. Missing
and cyclic diagnostics carry the requested identity, the requesting parent,
and a deterministic dependency path; the cycle path is rotated to its
lexicographically smallest member so the same cycle always reports the same
path regardless of which root was requested.

### The catalog is explicit, process-local, and host-trusted

The host code compiles the modules in, registers the entries it wants
available, and passes references in. There is no discovery, no ambient
environment reading, and no cross-process catalog state. K5's trust
boundary is the process boundary: every factory in the catalog is host-
supplied code that was linked into the binary before it started, which is
exactly as trusted as any other K4 registration today. The loader adds no
new trust and therefore deliberately acquires no sandbox, signature, or
permission machinery. This is also the vocabulary boundary: the loader's
objects are Module references/catalog/resolver — the existing
`PluginDefinition`/`PluginRuntime` names belong to capability-graph's
capability lifecycle and are not reused for loader concepts.

### Why filesystem, dylibs, WASM, and network stay out

Filesystem discovery, directory scanning, `dlopen`/`LoadLibrary`, dynamic
library ABIs, WASM/WASI, network or Git/URL fetching, package managers,
registries, hot reload, and watchers each make the resolved set depend on
state outside the compiled program — files on disk, bytes on a wire, remote
content that changes. That would violate the contract K5 exists to make
explicit: the host knows what it asked for, the same inputs resolve to the
same closure, every failure is typed and occurs before any runtime effect,
and everything executed was compiled and reviewed with the host. A future
dynamic-loading milestone may add a discovery *adapter* that feeds this
same explicit catalog; that adapter, not this boundary, is where those
concerns belong.

## Consequences

- The canonical host story is: construct a `ModuleCatalog`,
  `RuntimeLoader::new(&catalog).resolve(roots)`, `resolved.compose()`,
  then the unchanged K4/K3 lifecycle. A host can depend on
  `runtime-loader` alone; it re-exports the K4 and underlying host-facing
  vocabulary.
- `loader → composition → core` is a workspace-enforced dependency rule;
  composition must never reference loader types.
- Module version strings never reach durable state: two catalogs in two
  processes resolving the same references compose to the same K2 canonical
  identity, and cold reconstruction (K2) works through the loader unchanged
  (scenario O).
- Compatibility checking is deliberately shallow: the loader compares
  catalog-declared dependency references against the produced definition's
  dependency identities and rejects disagreement; deeper semantic
  compatibility is out of scope until there is a real need for it.
- If two entries with the same logical `Id` but different versions end up
  in one closure, the loader resolves both faithfully and K4's
  `DuplicateModule` rejects the composition — still before activation.
  Version-conflict policy belongs to whichever milestone defines
  versioning semantics.

## Rejected alternatives

- *A manifest/config format (TOML/YAML/JSON) with a parser*: the format is
  a second source of truth that can disagree with the code, and parsing is
  not resolution. The catalog is host code; if a config-driven host wants a
  file, it can translate the file into catalog entries itself.
- *The catalog storing ready `ModuleRegistration` instances*: turns the
  catalog into a live plugin cache, shares process-local objects across
  runs and processes, and defeats fresh-construction guarantees.
- *SemVer ranges or a lockfile*: a selection solver reintroduces
  input-dependent resolution and is unjustified when every entry is
  explicitly registered.
- *Filesystem directory scanning as a first catalog source*: makes
  resolution environment-dependent and its failures untyped before the
  contract even exists.
- *`PluginRuntime`-style naming for loader objects* (`PluginDefinition`,
  `PluginCatalog`): collides with capability-graph's existing plugin
  lifecycle vocabulary.
- *Building loader logic into `runtime-composition`*: conflates planning
  errors with resolution errors and drags catalog vocabulary into the
  composition contract.

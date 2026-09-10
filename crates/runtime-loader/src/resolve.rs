//! Deterministic, side-effect-free reference resolution.
//!
//! [`RuntimeLoader::resolve`] runs three phases in strict order and never
//! crosses a phase boundary early:
//!
//! 1. **Reference graph closure** — roots are deduplicated (duplicate roots
//!    fail closed) and the exact-reference dependency closure is computed
//!    over the catalog in a deterministic breadth-first order. A reference
//!    with no catalog entry fails with [`LoaderError::MissingReference`]
//!    before any factory runs.
//! 2. **Cycle classification** — the reference dependency graph is checked
//!    for cycles. A cycle is a loader failure
//!    ([`LoaderError::DependencyCycle`]) with a deterministic closed path;
//!    it never waits to surface later as a K4 composition cycle.
//! 3. **Fresh construction** — every resolved entry's factory is called
//!    exactly once, in stable reference order, and each produced
//!    [`ModuleRegistration`] is checked against its catalog entry (module
//!    identity, and logical agreement between declared reference
//!    dependencies and the produced module dependencies).
//!
//! None of the phases touch a Runtime: resolution constructs ordinary
//! process-local values and nothing else. A successful resolution hands
//! back [`ResolvedModules`], and only the host's later explicit call into
//! K4 composition can lead to activation.

use crate::catalog::{CatalogEntry, ModuleCatalog};
use crate::error::{IncompatibleEntryReason, LoaderError};
use crate::reference::ModuleReference;
use kernis_core::Id;
use runtime_composition::{
    CompositionBuilder, CompositionError, CompositionPlan, ModuleRegistration,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// A resolved reference together with its freshly constructed registration,
/// retained for host inspection.
struct ResolvedEntry {
    reference: ModuleReference,
    registration: ModuleRegistration,
}

/// One entry's deterministic BFS discovery record: the direct dependency
/// requester (absent for roots) and the reference path from the requesting
/// root down to this reference.
#[derive(Clone, Debug)]
struct Discovered {
    required_by: Option<ModuleReference>,
    path: Vec<ModuleReference>,
}

/// The successful result of loader resolution: the full deterministic
/// dependency closure, one fresh process-local [`ModuleRegistration`] per
/// resolved reference, and nothing activated.
///
/// The registrations were produced by the catalog factories during this
/// resolution only; the loader holds no live-registration cache. A second
/// resolution against the same catalog constructs a second, independent set
/// of registrations, which is what makes cold reconstruction safe.
pub struct ResolvedModules {
    entries: Vec<ResolvedEntry>,
}

impl fmt::Debug for ResolvedModules {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedModules")
            .field(
                "references",
                &self
                    .entries
                    .iter()
                    .map(|e| &e.reference)
                    .collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

impl ResolvedModules {
    /// Returns the resolved references in deterministic order.
    pub fn references(&self) -> impl Iterator<Item = &ModuleReference> {
        self.entries.iter().map(|entry| &entry.reference)
    }

    /// Returns the fresh registrations aligned with [`Self::references`].
    pub fn registrations(&self) -> impl Iterator<Item = &ModuleRegistration> {
        self.entries.iter().map(|entry| &entry.registration)
    }

    /// Returns the number of resolved modules.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns whether nothing was resolved.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Feeds every resolved registration into a fresh K4
    /// [`CompositionBuilder`].
    ///
    /// This is the phase handoff: the returned builder carries
    /// [`CompositionError`]s from K4 planning, never a
    /// [`LoaderError`], so composition-phase failures stay attributable to
    /// K4. Planning still acquires no resource.
    pub fn into_composition_builder(self) -> Result<CompositionBuilder, CompositionError> {
        let mut builder = CompositionBuilder::new();
        for entry in self.entries {
            builder = builder.register(entry.registration)?;
        }
        Ok(builder)
    }

    /// Convenience handoff: [`Self::into_composition_builder`] followed by
    /// K4 `build()`. The result is a validated deterministic
    /// [`CompositionPlan`]; activation remains a separate explicit host
    /// step (`start`, `start_with_store`, or `restore`).
    pub fn compose(self) -> Result<CompositionPlan, CompositionError> {
        self.into_composition_builder()?.build()
    }
}

/// The K5 loader: resolves exact module references against one explicit
/// [`ModuleCatalog`].
///
/// The loader borrows its catalog; it owns no state between resolutions,
/// holds no live-registration cache, and performs no I/O. Resolution is
/// synchronous by contract: an in-process catalog lookup needs no executor,
/// and the API deliberately does not speculate about future async loading
/// mechanisms.
#[derive(Debug)]
pub struct RuntimeLoader<'a> {
    catalog: &'a ModuleCatalog,
}

impl<'a> RuntimeLoader<'a> {
    /// Creates a loader over one explicit catalog value.
    #[must_use]
    pub const fn new(catalog: &'a ModuleCatalog) -> Self {
        Self { catalog }
    }

    /// Resolves the requested root references, with their full reference
    /// dependency closure, into fresh process-local registrations.
    ///
    /// The result is deterministic: the resolved order, and the selection
    /// and content of every error, are independent of catalog insertion
    /// order and root input order.
    ///
    /// # Errors
    ///
    /// Fails with a typed [`LoaderError`] on duplicate roots, missing
    /// references, reference cycles, produced registrations that
    /// contradict their catalog entry, or factory-reported construction
    /// failure. Every failure occurs before any Runtime exists and before
    /// any activation-capable artifact is handed to the host.
    pub fn resolve<I: IntoIterator<Item = ModuleReference>>(
        &self,
        roots: I,
    ) -> Result<ResolvedModules, LoaderError> {
        let roots = collect_roots(roots)?;
        let resolved = self.resolve_graph(&roots)?;
        self.construct(&resolved)
    }

    /// Phase 1 + 2: deterministic reference closure and cycle
    /// classification, without calling any factory.
    fn resolve_graph(
        &self,
        roots: &BTreeSet<ModuleReference>,
    ) -> Result<BTreeMap<ModuleReference, Discovered>, LoaderError> {
        let mut resolved: BTreeMap<ModuleReference, Discovered> = BTreeMap::new();
        let mut missing: BTreeMap<ModuleReference, Discovered> = BTreeMap::new();
        let mut frontier: BTreeMap<ModuleReference, Discovered> = roots
            .iter()
            .map(|reference| {
                (
                    reference.clone(),
                    Discovered {
                        required_by: None,
                        path: vec![reference.clone()],
                    },
                )
            })
            .collect();

        while !frontier.is_empty() {
            let mut next: BTreeMap<ModuleReference, Discovered> = BTreeMap::new();
            for (reference, discovery) in frontier {
                if resolved.contains_key(&reference) {
                    continue;
                }
                let Some(entry) = self.catalog.get(&reference) else {
                    missing.entry(reference).or_insert(discovery);
                    continue;
                };
                resolved.insert(reference.clone(), discovery.clone());
                for dependency in sorted_dependencies(entry) {
                    if resolved.contains_key(&dependency) {
                        continue;
                    }
                    let mut path = discovery.path.clone();
                    path.push(dependency.clone());
                    next.entry(dependency.clone()).or_insert(Discovered {
                        required_by: Some(reference.clone()),
                        path,
                    });
                }
            }
            frontier = next;
        }

        if let Some((requested, discovery)) = missing.into_iter().next() {
            return Err(LoaderError::MissingReference {
                requested,
                required_by: discovery.required_by,
                path: discovery.path,
            });
        }

        detect_cycles(roots, &resolved, self.catalog)?;
        Ok(resolved)
    }

    /// Phase 3: fresh construction in stable reference order, with the
    /// loader-truth checks that keep catalog metadata and produced K4
    /// registrations in agreement.
    fn construct(
        &self,
        resolved: &BTreeMap<ModuleReference, Discovered>,
    ) -> Result<ResolvedModules, LoaderError> {
        let mut entries = Vec::with_capacity(resolved.len());
        for reference in resolved.keys() {
            let entry = self
                .catalog
                .get(reference)
                .expect("the resolved closure only contains catalog entries");
            let registration = (entry.factory())().map_err(|source| {
                LoaderError::RegistrationConstructionFailed {
                    reference: reference.clone(),
                    source,
                }
            })?;
            let definition = registration.definition();
            if &definition.id != reference.id() {
                return Err(LoaderError::IncompatibleEntry {
                    reference: reference.clone(),
                    reason: IncompatibleEntryReason::ProducedIdentityMismatch {
                        actual: definition.id.clone(),
                    },
                });
            }
            let declared: BTreeSet<Id> = entry
                .dependencies()
                .iter()
                .map(|dependency| dependency.id().clone())
                .collect();
            let produced: BTreeSet<Id> = definition.dependencies.iter().cloned().collect();
            if declared != produced {
                return Err(LoaderError::IncompatibleEntry {
                    reference: reference.clone(),
                    reason: IncompatibleEntryReason::DeclaredDependenciesDisagree {
                        declared: declared.into_iter().collect(),
                        produced: produced.into_iter().collect(),
                    },
                });
            }
            entries.push(ResolvedEntry {
                reference: reference.clone(),
                registration,
            });
        }
        Ok(ResolvedModules { entries })
    }
}

fn collect_roots<I: IntoIterator<Item = ModuleReference>>(
    roots: I,
) -> Result<BTreeSet<ModuleReference>, LoaderError> {
    let mut sorted: Vec<ModuleReference> = roots.into_iter().collect();
    sorted.sort();
    if let Some(duplicate) = sorted.windows(2).find(|pair| pair[0] == pair[1]) {
        return Err(LoaderError::DuplicateRootReference {
            reference: duplicate[0].clone(),
        });
    }
    Ok(sorted.into_iter().collect())
}

fn sorted_dependencies(entry: &CatalogEntry) -> Vec<ModuleReference> {
    let mut deps: Vec<ModuleReference> = entry.dependencies().to_vec();
    deps.sort();
    deps
}

/// Phase 2: deterministic cycle classification over the reference graph.
///
/// DFS proceeds over sorted roots with sorted dependency edges. The
/// reported closed path is rotated to start at the smallest reference in
/// the cycle, so the same cycle is reported identically regardless of
/// which root or catalog order surfaced it.
fn detect_cycles(
    roots: &BTreeSet<ModuleReference>,
    resolved: &BTreeMap<ModuleReference, Discovered>,
    catalog: &ModuleCatalog,
) -> Result<(), LoaderError> {
    let mut completed: BTreeSet<ModuleReference> = BTreeSet::new();
    let mut path: Vec<ModuleReference> = Vec::new();
    let mut on_path: BTreeMap<ModuleReference, usize> = BTreeMap::new();
    for root in roots {
        if !completed.contains(root) {
            visit(
                root,
                resolved,
                catalog,
                &mut completed,
                &mut path,
                &mut on_path,
            )?;
        }
    }
    Ok(())
}

fn visit(
    reference: &ModuleReference,
    resolved: &BTreeMap<ModuleReference, Discovered>,
    catalog: &ModuleCatalog,
    completed: &mut BTreeSet<ModuleReference>,
    path: &mut Vec<ModuleReference>,
    on_path: &mut BTreeMap<ModuleReference, usize>,
) -> Result<(), LoaderError> {
    if completed.contains(reference) {
        return Ok(());
    }
    if let Some(position) = on_path.get(reference).copied() {
        let mut cycle: Vec<ModuleReference> = path[position..].to_vec();
        rotate_to_smallest(&mut cycle);
        cycle.push(cycle[0].clone());
        return Err(LoaderError::DependencyCycle { path: cycle });
    }
    on_path.insert(reference.clone(), path.len());
    path.push(reference.clone());
    let entry = catalog
        .get(reference)
        .expect("the resolved closure only contains catalog entries");
    for dependency in sorted_dependencies(entry) {
        if resolved.contains_key(&dependency) {
            visit(&dependency, resolved, catalog, completed, path, on_path)?;
        }
    }
    path.pop();
    on_path.remove(reference);
    completed.insert(reference.clone());
    Ok(())
}

fn rotate_to_smallest(cycle: &mut [ModuleReference]) {
    let smallest = cycle
        .iter()
        .enumerate()
        .min_by_key(|(_, reference)| *reference)
        .map(|(index, _)| index)
        .expect("a detected cycle is never empty");
    cycle.rotate_left(smallest);
}

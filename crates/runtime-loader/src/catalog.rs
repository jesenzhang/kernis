//! The explicit, process-local module catalog.
//!
//! A [`ModuleCatalog`] is a host-provided value, not a global registry: there
//! is no ambient registration, static state, discovery, or implicit
//! inventory. The catalog pairs stable declarative metadata (the exact
//! [`ModuleReference`] and its exact reference dependencies) with a
//! [`ModuleRegistrationFactory`] that constructs a *fresh* K4
//! [`ModuleRegistration`] on demand.
//!
//! The factory boundary is the loader's central process-local contract:
//! K4 registrations own executable objects (factories, plugin runtimes,
//! lifecycle hooks), so the catalog never stores a live registration and
//! hands the same process-local lifecycle state to two independent Runtime
//! reconstructions. Each resolution calls each factory exactly once and gets
//! a fresh registration.
//!
//! A factory only constructs process-local values. It never activates a
//! Runtime, starts a fiber, dispatches an effect, writes the durable store,
//! or runs a module lifecycle hook — those remain K4 activation behavior.

use crate::error::LoaderError;
use crate::reference::ModuleReference;
use runtime_composition::ModuleRegistration;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

/// Constructs one fresh process-local [`ModuleRegistration`] for a catalog
/// entry.
///
/// The loader calls this during resolution. The returned registration must
/// carry the module identity the catalog reference names and module
/// dependencies consistent, at logical dependency identity, with the exact
/// reference dependencies the entry declared; otherwise resolution fails
/// with [`LoaderError::IncompatibleEntry`].
pub type ModuleRegistrationFactory =
    Arc<dyn Fn() -> Result<ModuleRegistration, crate::error::ModuleFactoryError> + Send + Sync>;

/// One catalog entry: stable declarative metadata plus the fresh-registration
/// factory.
///
/// `dependencies` are exact reference edges, not registration order: the
/// dependency closure is derived from these declared references, so a module
/// is resolved because it was referenced, never because it happened to be
/// registered first.
#[derive(Clone)]
pub struct CatalogEntry {
    reference: ModuleReference,
    dependencies: Vec<ModuleReference>,
    factory: ModuleRegistrationFactory,
}

impl CatalogEntry {
    /// Creates an entry for one exact reference with its fresh-registration
    /// factory and no declared dependencies.
    #[must_use]
    pub fn new<F>(reference: ModuleReference, factory: F) -> Self
    where
        F: Fn() -> Result<ModuleRegistration, crate::error::ModuleFactoryError>
            + Send
            + Sync
            + 'static,
    {
        Self {
            reference,
            dependencies: Vec::new(),
            factory: Arc::new(factory),
        }
    }

    /// Creates an entry from an already shared factory.
    #[must_use]
    pub fn with_factory(reference: ModuleReference, factory: ModuleRegistrationFactory) -> Self {
        Self {
            reference,
            dependencies: Vec::new(),
            factory,
        }
    }

    /// Declares one exact dependency reference this module requires.
    #[must_use]
    pub fn depends_on(mut self, dependency: ModuleReference) -> Self {
        self.dependencies.push(dependency);
        self
    }

    /// Declares exact dependency references this module requires.
    #[must_use]
    pub fn depends_on_all<I: IntoIterator<Item = ModuleReference>>(mut self, deps: I) -> Self {
        self.dependencies.extend(deps);
        self
    }

    /// Returns the exact reference this entry answers.
    #[must_use]
    pub fn reference(&self) -> &ModuleReference {
        &self.reference
    }

    /// Returns the declared exact dependency references.
    #[must_use]
    pub fn dependencies(&self) -> &[ModuleReference] {
        &self.dependencies
    }

    pub(crate) fn factory(&self) -> &ModuleRegistrationFactory {
        &self.factory
    }
}

impl fmt::Debug for CatalogEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CatalogEntry")
            .field("reference", &self.reference)
            .field("dependencies", &self.dependencies)
            .finish_non_exhaustive()
    }
}

/// Host-provided, process-local catalog of available module entries.
///
/// Entries are keyed by exact reference in a total order, so lookup and
/// traversal never depend on registration order. `register` fails closed on
/// a duplicate exact reference — it never silently overwrites.
#[derive(Default)]
pub struct ModuleCatalog {
    entries: BTreeMap<ModuleReference, CatalogEntry>,
}

impl fmt::Debug for ModuleCatalog {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModuleCatalog")
            .field("entries", &self.entries.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl ModuleCatalog {
    /// Creates an empty catalog.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    /// Adds one entry, rejecting a duplicate exact reference without
    /// overwriting the existing entry.
    ///
    /// # Errors
    ///
    /// Returns [`LoaderError::DuplicateCatalogEntry`] when the exact
    /// reference is already present.
    pub fn register(self, entry: CatalogEntry) -> Result<Self, LoaderError> {
        let Self { mut entries } = self;
        if entries.contains_key(&entry.reference) {
            return Err(LoaderError::DuplicateCatalogEntry {
                reference: entry.reference,
            });
        }
        entries.insert(entry.reference.clone(), entry);
        Ok(Self { entries })
    }

    /// Returns whether the catalog contains one exact reference.
    #[must_use]
    pub fn contains(&self, reference: &ModuleReference) -> bool {
        self.entries.contains_key(reference)
    }

    /// Returns the entry registered for one exact reference.
    #[must_use]
    pub fn get(&self, reference: &ModuleReference) -> Option<&CatalogEntry> {
        self.entries.get(reference)
    }

    /// Returns the registered exact references in stable order.
    pub fn references(&self) -> impl Iterator<Item = &ModuleReference> {
        self.entries.keys()
    }

    /// Returns the number of registered entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns whether the catalog is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::{CatalogEntry, ModuleCatalog};
    use crate::error::LoaderError;
    use runtime_composition::{ModuleDefinition, ModuleRegistration};

    fn entry(id: &str, version: &str) -> CatalogEntry {
        let definition_id = kernis_core::Id::new(id).expect("valid module id");
        CatalogEntry::new(
            crate::reference::ModuleReference::new(id, version).expect("valid reference"),
            move || {
                Ok(ModuleRegistration::new(ModuleDefinition::new(
                    definition_id.clone(),
                )))
            },
        )
    }

    #[test]
    fn registration_rejects_duplicate_exact_references() {
        let catalog = ModuleCatalog::new()
            .register(entry("a", "1"))
            .expect("first entry registers");
        let result = catalog.register(entry("a", "1"));
        assert!(matches!(
            result,
            Err(LoaderError::DuplicateCatalogEntry { .. })
        ));
    }

    #[test]
    fn different_versions_are_different_entries() {
        let catalog = ModuleCatalog::new()
            .register(entry("a", "1"))
            .expect("a@1 registers")
            .register(entry("a", "2"))
            .expect("a@2 is a distinct entry");
        assert_eq!(catalog.len(), 2);
        assert!(catalog.contains(&crate::reference::ModuleReference::new("a", "1").expect("v")));
    }
}

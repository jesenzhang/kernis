//! Runtime ownership, scope hierarchy, and capability admission.

use crate::definition::{CapabilityDefinition, CapabilityId, EntryId, Generation};
use crate::resolver::{CapabilityGraph, CapabilityGraphError, ValidatedCapabilityDefinition};
use std::any::Any;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};

type Cleanup = Box<dyn FnOnce(Box<dyn Any + Send + Sync>) + Send>;

/// A reader-visible capability value with runtime-owned cleanup.
///
/// Readers can inspect or downcast the value, but this type deliberately has
/// no public disposal operation. The runtime-owned instance slot is the only
/// object that owns the value, and dropping that slot invokes the cleanup
/// callback at most once.
pub struct CapabilityValue {
    value: Option<Box<dyn Any + Send + Sync>>,
    cleanup: Mutex<Option<Cleanup>>,
}

impl CapabilityValue {
    /// Wraps a value whose normal Rust Drop implementation is its cleanup.
    #[must_use]
    pub fn from_value<T>(value: T) -> Self
    where
        T: Any + Send + Sync,
    {
        Self {
            value: Some(Box::new(value)),
            cleanup: Mutex::new(None),
        }
    }

    /// Wraps a value with a runtime-owned, exactly-once cleanup callback.
    #[must_use]
    pub fn new<T, F>(value: T, cleanup: F) -> Self
    where
        T: Any + Send + Sync,
        F: FnOnce(T) + Send + 'static,
    {
        let cleanup = Box::new(move |value: Box<dyn Any + Send + Sync>| {
            let value = value
                .downcast::<T>()
                .expect("capability cleanup type must match its value");
            cleanup(*value);
        });
        Self {
            value: Some(Box::new(value)),
            cleanup: Mutex::new(Some(cleanup)),
        }
    }

    /// Returns the value as a concrete type when it has type T.
    #[must_use]
    pub fn downcast_ref<T: 'static>(&self) -> Option<&T> {
        self.value.as_ref()?.downcast_ref()
    }
}

impl Drop for CapabilityValue {
    fn drop(&mut self) {
        let cleanup = self
            .cleanup
            .get_mut()
            .expect("capability cleanup lock must not be poisoned")
            .take();
        if let Some(cleanup) = cleanup {
            if let Some(value) = self.value.take() {
                cleanup(value);
            }
        }
    }
}

/// Exact dependency handles resolved for one construction attempt.
#[derive(Clone, Debug, Default)]
pub struct ResolvedDependencies {
    handles: BTreeMap<CapabilityId, CapabilityHandle>,
}

impl ResolvedDependencies {
    pub(crate) fn from_handles(handles: BTreeMap<CapabilityId, CapabilityHandle>) -> Self {
        Self { handles }
    }

    /// Looks up the exact published handle resolved for dependency id.
    #[must_use]
    pub fn get(&self, id: &CapabilityId) -> Option<&CapabilityHandle> {
        self.handles.get(id)
    }

    /// Returns the exact dependency handles in deterministic identifier order.
    pub fn iter(&self) -> impl Iterator<Item = (&CapabilityId, &CapabilityHandle)> {
        self.handles.iter()
    }

    /// Returns the number of resolved dependencies.
    #[must_use]
    pub fn len(&self) -> usize {
        self.handles.len()
    }
    /// Returns whether no dependencies were resolved.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.handles.is_empty()
    }
}

struct InstanceSlot {
    value: CapabilityValue,
    dependencies: ResolvedDependencies,
}

struct CapabilityEntry {
    definition: Arc<CapabilityDefinition>,
    instance: Arc<InstanceSlot>,
    generation: Generation,
    entry_id: EntryId,
}

impl CapabilityEntry {
    fn handle(&self) -> CapabilityHandle {
        CapabilityHandle {
            definition: Arc::clone(&self.definition),
            instance: Arc::clone(&self.instance),
            generation: self.generation,
            entry_id: self.entry_id,
        }
    }
}

/// A reader-owned reference to one exact capability entry.
#[derive(Clone)]
pub struct CapabilityHandle {
    definition: Arc<CapabilityDefinition>,
    instance: Arc<InstanceSlot>,
    generation: Generation,
    entry_id: EntryId,
}

impl fmt::Debug for CapabilityHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CapabilityHandle")
            .field("id", &self.id())
            .field("kind", &self.kind())
            .field("generation", &self.generation())
            .field("entry_id", &self.entry_id())
            .finish()
    }
}

impl CapabilityHandle {
    /// Returns the capability identifier.
    #[must_use]
    pub fn id(&self) -> &CapabilityId {
        &self.definition.id
    }

    /// Returns the capability kind.
    #[must_use]
    pub fn kind(&self) -> &str {
        &self.definition.kind
    }

    /// Returns the stable provider/configuration identity for replay.
    #[must_use]
    pub fn replay_identity(&self) -> &str {
        &self.definition.replay_identity
    }

    /// Returns the exact publication generation.
    #[must_use]
    pub fn generation(&self) -> Generation {
        self.generation
    }

    /// Returns the exact runtime entry identity.
    #[must_use]
    pub fn entry_id(&self) -> EntryId {
        self.entry_id
    }

    /// Returns the reader-visible capability value.
    ///
    /// The returned value has no disposal method. Its lifetime is secured by
    /// this handle, while cleanup remains owned by the runtime slot.
    #[must_use]
    pub fn instance(&self) -> &CapabilityValue {
        &self.instance.value
    }

    /// Returns the exact dependency snapshot retained by this entry.
    #[must_use]
    pub fn dependencies(&self) -> &ResolvedDependencies {
        &self.instance.dependencies
    }

    /// Returns the concrete value when it has type T.
    #[must_use]
    pub fn downcast_ref<T: 'static>(&self) -> Option<&T> {
        self.instance().downcast_ref()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Lifecycle {
    Open = 0,
    Closing = 1,
    Closed = 2,
}

impl Lifecycle {
    fn from_raw(value: u8) -> Self {
        match value {
            0 => Self::Open,
            1 => Self::Closing,
            _ => Self::Closed,
        }
    }
}

struct ScopeState {
    parent: Option<Scope>,
    entries: RwLock<BTreeMap<CapabilityId, CapabilityEntry>>,
    lifecycle: AtomicU8,
    topology: Arc<RwLock<()>>,
    children: Mutex<Vec<Weak<ScopeState>>>,
}

/// A hierarchical scope that owns local capability instances.
///
/// Lookup walks from the current scope toward its parent. A child can publish
/// a local capability without changing the parent's entry. Handles keep exact
/// instances alive until the last reader releases them.
#[derive(Clone)]
pub struct Scope {
    state: Arc<ScopeState>,
}

static NEXT_ENTRY_ID: AtomicU64 = AtomicU64::new(1);

/// Reserves the next value of a monotonically increasing identifier counter.
///
/// A relaxed compare-and-swap loop is used instead of
/// `AtomicU64::fetch_update`: the latter is deprecated on current stable
/// (`try_update` replaces it), while `try_update` does not exist on the
/// declared MSRV 1.85. Reserving the value this way keeps both toolchains
/// warning-free with the same relaxed ordering and the same contract —
/// `Some(reserved)` for the caller, `None` when the counter would overflow.
pub(crate) fn allocate_monotonic(counter: &AtomicU64) -> Option<u64> {
    let mut current = counter.load(Ordering::Relaxed);
    loop {
        let next = current.checked_add(1)?;
        match counter.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return Some(current),
            Err(actual) => current = actual,
        }
    }
}

fn allocate_entry_id(counter: &AtomicU64) -> EntryId {
    EntryId::from_raw(allocate_monotonic(counter).expect("runtime entry id overflow"))
}

fn next_entry_id() -> EntryId {
    allocate_entry_id(&NEXT_ENTRY_ID)
}

impl Scope {
    /// Creates an empty root scope.
    #[must_use]
    pub fn root() -> Self {
        let topology = Arc::new(RwLock::new(()));
        Self {
            state: Arc::new(ScopeState {
                parent: None,
                entries: RwLock::new(BTreeMap::new()),
                lifecycle: AtomicU8::new(Lifecycle::Open as u8),
                topology,
                children: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Creates an empty child scope inheriting from this scope.
    ///
    /// If the parent is already closing or closed, the child is created
    /// closed and cannot publish new entries.
    #[must_use]
    pub fn child(&self) -> Self {
        let child = Self {
            state: Arc::new(ScopeState {
                parent: Some(self.clone()),
                entries: RwLock::new(BTreeMap::new()),
                lifecycle: AtomicU8::new(Lifecycle::Open as u8),
                topology: Arc::clone(&self.state.topology),
                children: Mutex::new(Vec::new()),
            }),
        };
        let _topology = self
            .state
            .topology
            .write()
            .expect("scope topology lock poisoned");
        if !self.is_open() {
            child
                .state
                .lifecycle
                .store(Lifecycle::Closed as u8, Ordering::Release);
        }
        self.state
            .children
            .lock()
            .expect("scope child lock poisoned")
            .push(Arc::downgrade(&child.state));
        child
    }

    pub(crate) fn parent(&self) -> Option<Self> {
        self.state.parent.clone()
    }

    /// Looks up the nearest capability visible from this scope.
    ///
    /// New lookups are rejected after the scope begins teardown. Handles
    /// already returned before that boundary remain valid.
    #[must_use]
    pub fn get(&self, id: &CapabilityId) -> Option<CapabilityHandle> {
        if !self.is_open() {
            return None;
        }

        let local = self
            .state
            .entries
            .read()
            .expect("scope entry lock poisoned")
            .get(id)
            .map(CapabilityEntry::handle);
        if local.is_some() {
            return local;
        }

        self.state.parent.as_ref()?.get(id)
    }

    /// Looks up only a capability published directly by this scope.
    #[must_use]
    pub fn get_local(&self, id: &CapabilityId) -> Option<CapabilityHandle> {
        if !self.is_open() {
            return None;
        }
        self.state
            .entries
            .read()
            .expect("scope entry lock poisoned")
            .get(id)
            .map(CapabilityEntry::handle)
    }

    /// Returns the generation of the nearest visible entry.
    #[must_use]
    pub fn generation(&self, id: &CapabilityId) -> Option<Generation> {
        self.get(id).map(|handle| handle.generation())
    }

    /// Returns whether this scope has a local entry for a capability.
    ///
    /// Context isolation uses this to distinguish a local publication from a
    /// parent fallback without copying the parent entry map.
    #[must_use]
    pub fn has_local(&self, id: &CapabilityId) -> bool {
        self.has_local_entry(id)
    }

    /// Validates a candidate against the current visible graph.
    ///
    /// The returned proof is a snapshot for inspection. Publication performs
    /// the same validation again while holding the topology admission lock.
    pub fn validate_definition(
        &self,
        definition: CapabilityDefinition,
    ) -> Result<ValidatedCapabilityDefinition, ScopeError> {
        let _topology = self
            .state
            .topology
            .read()
            .expect("scope topology lock poisoned");
        if !self.is_open() {
            return Err(ScopeError::Closed);
        }
        self.visible_graph()
            .validate_candidate(definition)
            .map_err(Into::into)
    }

    /// Provides a new local capability using a transactional factory.
    ///
    /// The factory runs only after graph admission and receives exact handles
    /// for the visible dependencies. A local entry that already exists must
    /// be changed through replace with its expected generation.
    pub fn provide<F>(
        &self,
        definition: CapabilityDefinition,
        construct: F,
    ) -> Result<CapabilityHandle, ScopeError>
    where
        F: FnOnce(&ResolvedDependencies) -> Result<CapabilityValue, String>,
    {
        self.publish(definition, PublishExpectation::New, construct)
    }

    /// Replaces a local capability only when expected_generation is current.
    ///
    /// Construction may run concurrently with another replacement, but the
    /// final publication compares the generation again under the topology
    /// lock. A stale constructor therefore returns ReplacementConflict and
    /// cannot change the current entry.
    pub fn replace<F>(
        &self,
        definition: CapabilityDefinition,
        expected_generation: Generation,
        construct: F,
    ) -> Result<CapabilityHandle, ScopeError>
    where
        F: FnOnce(&ResolvedDependencies) -> Result<CapabilityValue, String>,
    {
        self.publish(
            definition,
            PublishExpectation::Replace(expected_generation),
            construct,
        )
    }

    /// Publishes a value that was initialized asynchronously after the normal
    /// graph admission boundary. The supplied dependency snapshot is checked
    /// again at publication, so a provider replacement cannot publish a stale
    /// value into a new epoch.
    pub(crate) fn provide_value(
        &self,
        definition: CapabilityDefinition,
        dependencies: ResolvedDependencies,
        value: CapabilityValue,
    ) -> Result<CapabilityHandle, ScopeError> {
        for dependency in &definition.dependencies {
            let expected = dependencies
                .get(&dependency.id)
                .expect("validated dependency snapshot must contain every requirement");
            let current =
                self.get(&dependency.id)
                    .ok_or_else(|| ScopeError::MissingDependency {
                        capability: definition.id.clone(),
                        dependency: dependency.id.clone(),
                    })?;
            if current.generation() != expected.generation()
                || current.entry_id() != expected.entry_id()
            {
                return Err(ScopeError::DependencyChanged {
                    capability: definition.id.clone(),
                    dependency: dependency.id.clone(),
                });
            }
        }
        self.publish_preconstructed(definition, PublishExpectation::New, dependencies, value)
    }

    /// Removes one exact local publication. A stale fiber cannot remove a
    /// later replacement because the expected generation is checked under the
    /// topology lock.
    pub(crate) fn remove_local(
        &self,
        capability: &CapabilityId,
        expected_generation: Generation,
    ) -> Result<(), ScopeError> {
        self.withdraw(capability, expected_generation).map(drop)
    }

    /// Withdraws one exact local publication and returns a guard retaining its
    /// value until the caller finishes dependent teardown.
    pub fn withdraw(
        &self,
        capability: &CapabilityId,
        expected_generation: Generation,
    ) -> Result<CapabilityHandle, ScopeError> {
        let old = {
            let _topology = self
                .state
                .topology
                .write()
                .expect("scope topology lock poisoned");
            if !self.is_open() {
                return Err(ScopeError::Closed);
            }
            let mut entries = self
                .state
                .entries
                .write()
                .expect("scope entry lock poisoned");
            let Some(entry) = entries.get(capability) else {
                return Err(ScopeError::NoLocalEntry {
                    capability: capability.clone(),
                });
            };
            if entry.generation != expected_generation {
                return Err(ScopeError::ReplacementConflict {
                    capability: capability.clone(),
                    expected: expected_generation,
                    actual: entry.generation,
                });
            }
            entries.remove(capability)
        };
        let old = old.expect("checked local entry must exist");
        let handle = old.handle();
        drop(old);
        Ok(handle)
    }

    fn publish<F>(
        &self,
        definition: CapabilityDefinition,
        expectation: PublishExpectation,
        construct: F,
    ) -> Result<CapabilityHandle, ScopeError>
    where
        F: FnOnce(&ResolvedDependencies) -> Result<CapabilityValue, String>,
    {
        let dependencies = {
            let _topology = self
                .state
                .topology
                .read()
                .expect("scope topology lock poisoned");
            self.check_expectation(&definition.id, expectation)?;
            let _validated = self
                .visible_graph()
                .validate_candidate(definition.clone())
                .map_err(ScopeError::from)?;
            self.resolve_dependencies(&definition)?
        };

        let value = construct(&dependencies).map_err(|reason| ScopeError::ConstructionFailed {
            capability: definition.id.clone(),
            reason,
        })?;
        self.publish_preconstructed(definition, expectation, dependencies, value)
    }

    fn publish_preconstructed(
        &self,
        definition: CapabilityDefinition,
        expectation: PublishExpectation,
        dependencies: ResolvedDependencies,
        value: CapabilityValue,
    ) -> Result<CapabilityHandle, ScopeError> {
        let instance = Arc::new(InstanceSlot {
            value,
            dependencies: dependencies.clone(),
        });

        let (old, handle) = {
            let _topology = self
                .state
                .topology
                .write()
                .expect("scope topology lock poisoned");

            // Re-read the exact dependency identities while holding the
            // topology admission lock. A provider cannot replace between
            // this check and publication, so an async initializer can never
            // publish an old epoch after a concurrent replacement.
            let current_dependencies = self.resolve_dependencies(&definition)?;
            for dependency in &definition.dependencies {
                let expected = dependencies
                    .get(&dependency.id)
                    .expect("validated dependency snapshot must contain every requirement");
                let current = current_dependencies
                    .get(&dependency.id)
                    .expect("validated current dependencies must contain every requirement");
                if current.generation() != expected.generation()
                    || current.entry_id() != expected.entry_id()
                {
                    return Err(ScopeError::DependencyChanged {
                        capability: definition.id.clone(),
                        dependency: dependency.id.clone(),
                    });
                }
            }

            self.validate_descendant_admission(&definition)?;
            let mut entries = self
                .state
                .entries
                .write()
                .expect("scope entry lock poisoned");
            self.check_expectation_locked(&definition.id, expectation, &entries)?;
            self.visible_graph_with_entries(&entries)
                .validate_candidate(definition.clone())
                .map_err(ScopeError::from)?;

            let generation = match expectation {
                PublishExpectation::New => Generation::FIRST,
                PublishExpectation::Replace(_) => entries
                    .get(&definition.id)
                    .expect("replacement entry was checked above")
                    .generation
                    .checked_next()
                    .ok_or_else(|| ScopeError::GenerationExhausted {
                        capability: definition.id.clone(),
                    })?,
            };
            let entry = CapabilityEntry {
                definition: Arc::new(definition),
                instance: Arc::clone(&instance),
                generation,
                entry_id: next_entry_id(),
            };
            let handle = entry.handle();
            let old = entries.insert(entry.definition.id.clone(), entry);
            (old, handle)
        };

        drop(old);
        Ok(handle)
    }

    fn check_expectation(
        &self,
        capability: &CapabilityId,
        expectation: PublishExpectation,
    ) -> Result<(), ScopeError> {
        let entries = self
            .state
            .entries
            .read()
            .expect("scope entry lock poisoned");
        self.check_expectation_locked(capability, expectation, &entries)
    }

    fn check_expectation_locked(
        &self,
        capability: &CapabilityId,
        expectation: PublishExpectation,
        entries: &BTreeMap<CapabilityId, CapabilityEntry>,
    ) -> Result<(), ScopeError> {
        match Lifecycle::from_raw(self.state.lifecycle.load(Ordering::Acquire)) {
            Lifecycle::Open => {}
            Lifecycle::Closing | Lifecycle::Closed => return Err(ScopeError::Closed),
        }
        match expectation {
            PublishExpectation::New => {
                if let Some(entry) = entries.get(capability) {
                    return Err(ScopeError::AlreadyProvided {
                        capability: capability.clone(),
                        generation: entry.generation,
                    });
                }
            }
            PublishExpectation::Replace(expected) => {
                let Some(entry) = entries.get(capability) else {
                    return Err(ScopeError::NoLocalEntry {
                        capability: capability.clone(),
                    });
                };
                if entry.generation != expected {
                    return Err(ScopeError::ReplacementConflict {
                        capability: capability.clone(),
                        expected,
                        actual: entry.generation,
                    });
                }
            }
        }
        Ok(())
    }

    fn resolve_dependencies(
        &self,
        definition: &CapabilityDefinition,
    ) -> Result<ResolvedDependencies, ScopeError> {
        let mut handles = BTreeMap::new();
        for dependency in &definition.dependencies {
            let handle = self
                .get(&dependency.id)
                .ok_or_else(|| ScopeError::MissingDependency {
                    capability: definition.id.clone(),
                    dependency: dependency.id.clone(),
                })?;
            handles.insert(dependency.id.clone(), handle);
        }
        Ok(ResolvedDependencies { handles })
    }

    fn is_open(&self) -> bool {
        Lifecycle::from_raw(self.state.lifecycle.load(Ordering::Acquire)) == Lifecycle::Open
    }

    fn has_local_entry(&self, id: &CapabilityId) -> bool {
        self.state
            .entries
            .read()
            .expect("scope entry lock poisoned")
            .contains_key(id)
    }

    fn visible_definitions(&self) -> BTreeMap<CapabilityId, CapabilityDefinition> {
        if !self.is_open() {
            return BTreeMap::new();
        }
        let mut definitions = self
            .state
            .parent
            .as_ref()
            .map_or_else(BTreeMap::new, Scope::visible_definitions);
        let entries = self
            .state
            .entries
            .read()
            .expect("scope entry lock poisoned");
        for (id, entry) in entries.iter() {
            definitions.insert(id.clone(), (*entry.definition).clone());
        }
        definitions
    }

    fn visible_graph(&self) -> CapabilityGraph {
        CapabilityGraph::from_definitions(self.visible_definitions().into_values())
    }

    fn visible_graph_with_entries(
        &self,
        entries: &BTreeMap<CapabilityId, CapabilityEntry>,
    ) -> CapabilityGraph {
        let mut definitions = self
            .state
            .parent
            .as_ref()
            .map_or_else(BTreeMap::new, Scope::visible_definitions);
        for (id, entry) in entries {
            definitions.insert(id.clone(), (*entry.definition).clone());
        }
        CapabilityGraph::from_definitions(definitions.into_values())
    }

    fn live_descendants(&self) -> Vec<Scope> {
        let children = self
            .state
            .children
            .lock()
            .expect("scope child lock poisoned")
            .iter()
            .filter_map(Weak::upgrade)
            .map(|state| Scope { state })
            .collect::<Vec<_>>();
        let mut descendants = Vec::new();
        for child in children {
            descendants.push(child.clone());
            descendants.extend(child.live_descendants());
        }
        descendants
    }

    fn candidate_affects(&self, descendant: &Scope, capability: &CapabilityId) -> bool {
        let mut current = descendant.clone();
        loop {
            if Arc::ptr_eq(&current.state, &self.state) {
                return true;
            }
            if !current.is_open() || current.has_local_entry(capability) {
                return false;
            }
            let Some(parent) = current.state.parent.as_ref() else {
                return false;
            };
            current = parent.clone();
        }
    }

    fn validate_descendant_admission(
        &self,
        definition: &CapabilityDefinition,
    ) -> Result<(), ScopeError> {
        for descendant in self.live_descendants() {
            if !descendant.is_open() || !self.candidate_affects(&descendant, &definition.id) {
                continue;
            }
            descendant
                .visible_graph()
                .validate_candidate(definition.clone())
                .map_err(ScopeError::from)?;
        }
        Ok(())
    }

    /// Stops new lookups and mutations, then releases local ownership in
    /// dependency-aware teardown order.
    ///
    /// Existing handles are not invalidated. A dependent entry retains the
    /// exact dependency handles it was constructed with, so those dependencies
    /// remain alive until the dependent handle is released.
    pub fn teardown(&self) {
        let plans = {
            let _topology = self
                .state
                .topology
                .write()
                .expect("scope topology lock poisoned");
            let mut scopes = Vec::new();
            self.collect_descendants_child_first(&mut scopes);
            scopes.push(self.clone());

            let mut plans = Vec::new();
            for scope in scopes {
                if scope
                    .state
                    .lifecycle
                    .compare_exchange(
                        Lifecycle::Open as u8,
                        Lifecycle::Closing as u8,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_err()
                {
                    continue;
                }
                let entries = std::mem::take(
                    &mut *scope
                        .state
                        .entries
                        .write()
                        .expect("scope entry lock poisoned"),
                );
                let order = runtime_teardown_order(&entries);
                scope
                    .state
                    .lifecycle
                    .store(Lifecycle::Closed as u8, Ordering::Release);
                plans.push(TeardownPlan { entries, order });
            }
            plans
        };

        drop_teardown_plans(plans);
    }

    fn collect_descendants_child_first(&self, scopes: &mut Vec<Scope>) {
        let children = self
            .state
            .children
            .lock()
            .expect("scope child lock poisoned")
            .iter()
            .filter_map(Weak::upgrade)
            .map(|state| Scope { state })
            .collect::<Vec<_>>();
        for child in children {
            child.collect_descendants_child_first(scopes);
            scopes.push(child);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PublishExpectation {
    New,
    Replace(Generation),
}

struct TeardownPlan {
    entries: BTreeMap<CapabilityId, CapabilityEntry>,
    order: Vec<CapabilityId>,
}

fn drop_teardown_plans(plans: Vec<TeardownPlan>) {
    for mut plan in plans {
        for id in plan.order {
            if let Some(entry) = plan.entries.remove(&id) {
                drop(entry);
            }
        }
        for (_, entry) in plan.entries {
            drop(entry);
        }
    }
}

fn runtime_teardown_order(entries: &BTreeMap<CapabilityId, CapabilityEntry>) -> Vec<CapabilityId> {
    let graph = runtime_graph(entries);
    graph
        .resolve()
        .expect("published capability topology must remain valid during teardown")
        .teardown_order()
}

fn runtime_graph(entries: &BTreeMap<CapabilityId, CapabilityEntry>) -> CapabilityGraph {
    // Definitions are keyed by logical capability identity. Current published
    // entries are inserted first and therefore win over older snapshot
    // generations. Exact snapshot handles still recurse for ownership
    // pinning, so logical order and actual resource lifetime remain separate.
    let mut graph = CapabilityGraph::default();
    let mut visited = BTreeSet::new();
    for entry in entries.values() {
        graph.insert((*entry.definition).clone());
        visited.insert(entry.entry_id);
    }
    for entry in entries.values() {
        for (_, dependency) in entry.instance.dependencies.iter() {
            insert_snapshot(&mut graph, dependency, &mut visited);
        }
    }
    graph
}

fn insert_snapshot(
    graph: &mut CapabilityGraph,
    handle: &CapabilityHandle,
    visited: &mut BTreeSet<EntryId>,
) {
    if !visited.insert(handle.entry_id) {
        return;
    }
    graph
        .definitions
        .entry(handle.id().clone())
        .or_insert_with(|| (*handle.definition).clone());
    for (_, dependency) in handle.dependencies().iter() {
        insert_snapshot(graph, dependency, visited);
    }
}

/// Errors produced while publishing a scoped capability.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScopeError {
    /// A graph invariant rejected the runtime candidate.
    GraphAdmission(CapabilityGraphError),
    /// A required capability is not visible from the scope being changed.
    MissingDependency {
        /// Capability whose construction cannot proceed.
        capability: CapabilityId,
        /// Missing required capability.
        dependency: CapabilityId,
    },
    /// The factory rejected construction of the replacement.
    ConstructionFailed {
        /// Capability that failed to construct.
        capability: CapabilityId,
        /// Factory-provided failure description.
        reason: String,
    },
    /// A local entry already exists and must be changed with replace.
    AlreadyProvided {
        /// Capability whose local slot is occupied.
        capability: CapabilityId,
        /// Current generation in the local slot.
        generation: Generation,
    },
    /// Replacement was requested for a capability without a local entry.
    NoLocalEntry {
        /// Capability that has no local entry in this scope.
        capability: CapabilityId,
    },
    /// The expected generation no longer names the current local entry.
    ReplacementConflict {
        /// Capability whose replacement lost the race.
        capability: CapabilityId,
        /// Generation observed by the stale replacement.
        expected: Generation,
        /// Generation currently published.
        actual: Generation,
    },
    /// An asynchronous initializer resolved a dependency snapshot that is no
    /// longer the currently visible publication.
    DependencyChanged {
        /// Capability being initialized.
        capability: CapabilityId,
        /// Dependency whose exact entry changed.
        dependency: CapabilityId,
    },
    /// The local generation counter cannot advance further.
    GenerationExhausted {
        /// Capability whose generation counter is exhausted.
        capability: CapabilityId,
    },
    /// The scope has begun or completed teardown.
    Closed,
}

impl From<CapabilityGraphError> for ScopeError {
    fn from(error: CapabilityGraphError) -> Self {
        match error {
            CapabilityGraphError::MissingDependency {
                capability,
                dependency,
            } => Self::MissingDependency {
                capability,
                dependency,
            },
            other => Self::GraphAdmission(other),
        }
    }
}

impl fmt::Display for ScopeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GraphAdmission(error) => write!(f, "runtime graph admission rejected: {error}"),
            Self::MissingDependency {
                capability,
                dependency,
            } => write!(
                f,
                "capability {capability} requires missing dependency {dependency}"
            ),
            Self::ConstructionFailed { capability, reason } => {
                write!(f, "failed to construct capability {capability}: {reason}")
            }
            Self::AlreadyProvided {
                capability,
                generation,
            } => write!(
                f,
                "capability {capability} already has local generation {generation}"
            ),
            Self::NoLocalEntry { capability } => {
                write!(f, "capability {capability} has no local entry to replace")
            }
            Self::ReplacementConflict {
                capability,
                expected,
                actual,
            } => write!(
                f,
                "replacement conflict for {capability}: expected generation {expected}, actual {actual}"
            ),
            Self::DependencyChanged {
                capability,
                dependency,
            } => write!(
                f,
                "dependency {dependency} changed while initializing {capability}"
            ),
            Self::GenerationExhausted { capability } => {
                write!(f, "generation exhausted for capability {capability}")
            }
            Self::Closed => f.write_str("scope is already closing or closed"),
        }
    }
}

impl std::error::Error for ScopeError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Capability;
    use kernis_core::Id;
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

    fn id(value: &str) -> CapabilityId {
        Id::new(value).expect("test id is valid")
    }

    fn capability(value: &str) -> Capability {
        Capability::new(id(value), "service")
    }

    fn definition(value: &str) -> CapabilityDefinition {
        CapabilityDefinition::new(id(value), "service")
    }

    fn labels(ids: &[CapabilityId]) -> Vec<&str> {
        ids.iter().map(Id::as_str).collect()
    }

    fn value(label: &str, disposed: &Arc<AtomicUsize>) -> CapabilityValue {
        let disposed = Arc::clone(disposed);
        CapabilityValue::new(label.to_owned(), move |_label: String| {
            disposed.fetch_add(1, Ordering::SeqCst);
        })
    }

    fn label(handle: &CapabilityHandle) -> &str {
        handle
            .downcast_ref::<String>()
            .expect("test value type")
            .as_str()
    }

    #[test]
    fn resolve_simple_dependency() {
        let mut graph = CapabilityGraph::default();
        graph.insert(capability("a"));
        graph.insert(capability("b"));
        graph.require(&id("a"), &id("b")).expect("edge is valid");

        let resolved = graph.resolve().expect("graph is valid");
        assert_eq!(labels(resolved.construction_order()), vec!["b", "a"]);
    }

    #[test]
    fn resolve_multi_level_dependency() {
        let mut graph = CapabilityGraph::default();
        graph.insert(definition("a").depends_on(id("b")));
        graph.insert(definition("b").depends_on(id("c")));
        graph.insert(definition("c"));

        let resolved = graph.resolve().expect("graph is valid");
        assert_eq!(labels(resolved.construction_order()), vec!["c", "b", "a"]);
        assert_eq!(labels(&resolved.teardown_order()), vec!["a", "b", "c"]);
    }

    #[test]
    fn resolution_is_deterministic() {
        let mut forward = CapabilityGraph::default();
        forward.insert(definition("a").depends_on(id("b")).depends_on(id("c")));
        forward.insert(definition("b").depends_on(id("d")));
        forward.insert(definition("c").depends_on(id("d")));
        forward.insert(definition("d"));

        let mut reverse = CapabilityGraph::default();
        reverse.insert(definition("d"));
        reverse.insert(definition("c").depends_on(id("d")));
        reverse.insert(definition("b").depends_on(id("d")));
        reverse.insert(definition("a").depends_on(id("c")).depends_on(id("b")));

        assert_eq!(
            forward.resolve().expect("graph is valid"),
            reverse.resolve().expect("graph is valid")
        );
    }

    #[test]
    fn missing_dependency_is_rejected() {
        let mut graph = CapabilityGraph::default();
        graph.insert(definition("a").depends_on(id("missing")));

        assert_eq!(
            graph.resolve(),
            Err(CapabilityGraphError::MissingDependency {
                capability: id("a"),
                dependency: id("missing"),
            })
        );
    }

    #[test]
    fn cycle_is_rejected() {
        let mut graph = CapabilityGraph::default();
        graph.insert(definition("a").depends_on(id("b")));
        graph.insert(definition("b").depends_on(id("c")));
        graph.insert(definition("c").depends_on(id("a")));

        assert!(matches!(
            graph.resolve(),
            Err(CapabilityGraphError::Cycle { .. })
        ));
    }

    #[test]
    fn cycle_error_contains_path() {
        let mut graph = CapabilityGraph::default();
        graph.insert(definition("a").depends_on(id("b")));
        graph.insert(definition("b").depends_on(id("c")));
        graph.insert(definition("c").depends_on(id("a")));

        let error = graph.resolve().expect_err("cycle must be rejected");
        assert_eq!(
            error,
            CapabilityGraphError::Cycle {
                path: vec![id("a"), id("b"), id("c"), id("a")],
            }
        );
        assert_eq!(
            error.to_string(),
            "capability dependency cycle: a -> b -> c -> a"
        );
    }

    #[test]
    fn teardown_order_is_reverse_resolution_order() {
        let mut graph = CapabilityGraph::default();
        graph.insert(definition("a").depends_on(id("b")));
        graph.insert(definition("b").depends_on(id("c")));
        graph.insert(definition("c"));

        let resolved = graph.resolve().expect("graph is valid");
        assert_eq!(labels(&resolved.teardown_order()), vec!["a", "b", "c"]);
    }

    #[test]
    fn child_inherits_parent_capability() {
        let root = Scope::root();
        let disposed = Arc::new(AtomicUsize::new(0));
        root.provide(definition("model"), |_| Ok(value("openai", &disposed)))
            .expect("root capability constructs");
        let child = root.child();

        assert_eq!(
            label(&child.get(&id("model")).expect("inherited model")),
            "openai"
        );
    }

    #[test]
    fn child_override_does_not_mutate_parent() {
        let root = Scope::root();
        let root_disposed = Arc::new(AtomicUsize::new(0));
        let child_disposed = Arc::new(AtomicUsize::new(0));
        root.provide(definition("model"), |_| Ok(value("openai", &root_disposed)))
            .expect("root capability constructs");
        let child = root.child();
        child
            .provide(definition("model"), |_| {
                Ok(value("deepseek", &child_disposed))
            })
            .expect("child override constructs");

        assert_eq!(
            label(&root.get(&id("model")).expect("root model")),
            "openai"
        );
        assert_eq!(
            label(&child.get(&id("model")).expect("child model")),
            "deepseek"
        );
        assert_eq!(root_disposed.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn sibling_scope_isolation() {
        let root = Scope::root();
        let root_disposed = Arc::new(AtomicUsize::new(0));
        let first_disposed = Arc::new(AtomicUsize::new(0));
        let second_disposed = Arc::new(AtomicUsize::new(0));
        root.provide(definition("model"), |_| Ok(value("openai", &root_disposed)))
            .expect("root capability constructs");
        let first = root.child();
        let second = root.child();
        first
            .provide(definition("model"), |_| {
                Ok(value("deepseek", &first_disposed))
            })
            .expect("first override constructs");
        second
            .provide(definition("model"), |_| {
                Ok(value("anthropic", &second_disposed))
            })
            .expect("second override constructs");

        assert_eq!(
            label(&first.get(&id("model")).expect("first model")),
            "deepseek"
        );
        assert_eq!(
            label(&second.get(&id("model")).expect("second model")),
            "anthropic"
        );
        assert_eq!(
            label(&root.get(&id("model")).expect("root model")),
            "openai"
        );
        assert_eq!(root_disposed.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn replacement_changes_new_reads_and_advances_generation() {
        let scope = Scope::root();
        let first_disposed = Arc::new(AtomicUsize::new(0));
        let second_disposed = Arc::new(AtomicUsize::new(0));
        let first = scope
            .provide(definition("model"), |_| Ok(value("v1", &first_disposed)))
            .expect("v1 constructs");
        let generation = first.generation();
        assert_eq!(generation, Generation::FIRST);
        scope
            .replace(definition("model"), generation, |_| {
                Ok(value("v2", &second_disposed))
            })
            .expect("v2 constructs");

        let current = scope.get(&id("model")).expect("current model");
        assert_eq!(label(&current), "v2");
        assert_eq!(current.generation(), generation.next());
        drop(first);
        assert_eq!(first_disposed.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn in_flight_reader_survives_replacement() {
        let scope = Scope::root();
        let first_disposed = Arc::new(AtomicUsize::new(0));
        let second_disposed = Arc::new(AtomicUsize::new(0));
        scope
            .provide(definition("model"), |_| Ok(value("v1", &first_disposed)))
            .expect("v1 constructs");
        let reader = scope.get(&id("model")).expect("reader gets v1");
        let generation = reader.generation();
        let barrier = Arc::new(Barrier::new(2));
        let reader_barrier = Arc::clone(&barrier);
        let reader_thread = thread::spawn(move || {
            assert_eq!(label(&reader), "v1");
            reader_barrier.wait();
            assert_eq!(label(&reader), "v1");
            reader
        });
        barrier.wait();

        scope
            .replace(definition("model"), generation, |_| {
                Ok(value("v2", &second_disposed))
            })
            .expect("v2 constructs");
        assert_eq!(
            label(&scope.get(&id("model")).expect("new reader gets v2")),
            "v2"
        );
        assert_eq!(first_disposed.load(Ordering::SeqCst), 0);

        drop(reader_thread.join().expect("reader thread completes"));
        assert_eq!(first_disposed.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn failed_replacement_keeps_old_capability() {
        let scope = Scope::root();
        let disposed = Arc::new(AtomicUsize::new(0));
        let old = scope
            .provide(definition("model"), |_| Ok(value("v1", &disposed)))
            .expect("v1 constructs");

        let error = scope
            .replace(definition("model"), old.generation(), |_| {
                Err::<CapabilityValue, _>("constructor failed".to_owned())
            })
            .expect_err("replacement must fail");

        assert_eq!(
            error,
            ScopeError::ConstructionFailed {
                capability: id("model"),
                reason: "constructor failed".to_owned(),
            }
        );
        assert_eq!(
            label(&scope.get(&id("model")).expect("old model remains")),
            "v1"
        );
        assert_eq!(disposed.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn consumer_cannot_dispose_live_instance() {
        let scope = Scope::root();
        let handle = scope
            .provide(definition("model"), |_| {
                Ok(CapabilityValue::from_value("live".to_owned()))
            })
            .expect("model constructs");

        let value = handle.instance();
        assert_eq!(
            value.downcast_ref::<String>().map(String::as_str),
            Some("live")
        );
        // The reader-visible type has no dispose method. Cleanup is exercised
        // through the runtime-owned slot in the exactly-once tests below.
    }

    #[test]
    fn self_dependency_is_rejected_at_runtime_admission() {
        let scope = Scope::root();
        let constructed = Arc::new(AtomicUsize::new(0));
        let constructed_for_factory = Arc::clone(&constructed);
        let error = scope
            .provide(definition("a").depends_on(id("a")), move |_| {
                constructed_for_factory.fetch_add(1, Ordering::SeqCst);
                Ok(CapabilityValue::from_value("invalid".to_owned()))
            })
            .expect_err("self dependency must be rejected");

        assert!(matches!(
            error,
            ScopeError::GraphAdmission(CapabilityGraphError::Cycle { .. })
        ));
        assert_eq!(constructed.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn runtime_cycle_is_rejected() {
        let scope = Scope::root();
        let a = scope
            .provide(definition("a"), |_| {
                Ok(CapabilityValue::from_value("a".to_owned()))
            })
            .expect("a constructs");
        scope
            .provide(definition("b").depends_on(id("a")), |_| {
                Ok(CapabilityValue::from_value("b".to_owned()))
            })
            .expect("b constructs");

        let error = scope
            .replace(definition("a").depends_on(id("b")), a.generation(), |_| {
                Ok(CapabilityValue::from_value("new-a".to_owned()))
            })
            .expect_err("runtime cycle must be rejected");
        assert!(matches!(
            error,
            ScopeError::GraphAdmission(CapabilityGraphError::Cycle { .. })
        ));
        assert_eq!(label(&scope.get(&id("a")).expect("old a remains")), "a");
    }

    #[test]
    fn candidate_replacement_that_introduces_cycle_is_rejected() {
        let scope = Scope::root();
        let a = scope
            .provide(definition("a"), |_| {
                Ok(CapabilityValue::from_value("a".to_owned()))
            })
            .expect("a constructs");
        scope
            .provide(definition("b").depends_on(id("a")), |_| {
                Ok(CapabilityValue::from_value("b".to_owned()))
            })
            .expect("b constructs");
        let constructed = Arc::new(AtomicUsize::new(0));
        let constructed_for_factory = Arc::clone(&constructed);

        let error = scope
            .replace(
                definition("a").depends_on(id("b")),
                a.generation(),
                move |_| {
                    constructed_for_factory.fetch_add(1, Ordering::SeqCst);
                    Ok(CapabilityValue::from_value("invalid".to_owned()))
                },
            )
            .expect_err("candidate cycle must be rejected");
        assert!(matches!(
            error,
            ScopeError::GraphAdmission(CapabilityGraphError::Cycle { .. })
        ));
        assert_eq!(constructed.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn missing_dependency_is_rejected_before_construction() {
        let scope = Scope::root();
        let constructed = Arc::new(AtomicUsize::new(0));
        let constructed_for_factory = Arc::clone(&constructed);
        let error = scope
            .provide(definition("a").depends_on(id("missing")), move |_| {
                constructed_for_factory.fetch_add(1, Ordering::SeqCst);
                Ok(CapabilityValue::from_value("invalid".to_owned()))
            })
            .expect_err("missing dependency must be rejected");

        assert_eq!(
            error,
            ScopeError::MissingDependency {
                capability: id("a"),
                dependency: id("missing"),
            }
        );
        assert_eq!(constructed.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn constructor_receives_resolved_dependency_snapshot() {
        let scope = Scope::root();
        scope
            .provide(definition("model"), |_| {
                Ok(CapabilityValue::from_value("model-v1".to_owned()))
            })
            .expect("model constructs");
        let service = scope
            .provide(
                definition("service").depends_on(id("model")),
                |dependencies| {
                    let model = dependencies
                        .get(&id("model"))
                        .expect("model dependency")
                        .downcast_ref::<String>()
                        .expect("model value")
                        .clone();
                    Ok(CapabilityValue::from_value(model))
                },
            )
            .expect("service constructs");

        assert_eq!(label(&service), "model-v1");
        assert_eq!(
            label(service.dependencies().get(&id("model")).unwrap()),
            "model-v1"
        );
        assert_eq!(
            service
                .dependencies()
                .get(&id("model"))
                .unwrap()
                .generation(),
            Generation::FIRST
        );
    }

    #[test]
    fn dependency_snapshot_survives_provider_replacement() {
        let scope = Scope::root();
        scope
            .provide(definition("model"), |_| {
                Ok(CapabilityValue::from_value("model-v1".to_owned()))
            })
            .expect("model v1 constructs");
        let service = scope
            .provide(
                definition("service").depends_on(id("model")),
                |dependencies| {
                    let model = dependencies
                        .get(&id("model"))
                        .expect("model dependency")
                        .downcast_ref::<String>()
                        .expect("model value")
                        .clone();
                    Ok(CapabilityValue::from_value(model))
                },
            )
            .expect("service constructs");
        let model_generation = scope.generation(&id("model")).unwrap();

        scope
            .replace(definition("model"), model_generation, |_| {
                Ok(CapabilityValue::from_value("model-v2".to_owned()))
            })
            .expect("model v2 constructs");

        assert_eq!(label(&service), "model-v1");
        assert_eq!(
            label(service.dependencies().get(&id("model")).unwrap()),
            "model-v1"
        );
        assert_eq!(
            label(&scope.get(&id("model")).expect("new model")),
            "model-v2"
        );
    }

    #[test]
    fn published_entry_pins_exact_dependency_generation() {
        let scope = Scope::root();
        let model_disposed = Arc::new(AtomicUsize::new(0));
        scope
            .provide(definition("model"), |_| {
                Ok(value("model-v1", &model_disposed))
            })
            .expect("model v1 constructs");
        let service = scope
            .provide(definition("service").depends_on(id("model")), |_| {
                Ok(CapabilityValue::from_value("service".to_owned()))
            })
            .expect("service constructs");
        let old_model = service.dependencies().get(&id("model")).unwrap().clone();
        let model_generation = old_model.generation();

        scope
            .replace(definition("model"), model_generation, |_| {
                Ok(CapabilityValue::from_value("model-v2".to_owned()))
            })
            .expect("model v2 constructs");
        assert_eq!(model_disposed.load(Ordering::SeqCst), 0);

        drop(service);
        scope.teardown();
        drop(old_model);
        assert_eq!(model_disposed.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn failed_conflicting_replacement_does_not_change_current_entry() {
        let scope = Scope::root();
        let first = scope
            .provide(definition("model"), |_| {
                Ok(CapabilityValue::from_value("v1".to_owned()))
            })
            .expect("v1 constructs");
        let current = scope
            .replace(definition("model"), first.generation(), |_| {
                Ok(CapabilityValue::from_value("v2".to_owned()))
            })
            .expect("v2 constructs");
        let error = scope
            .replace(definition("model"), first.generation(), |_| {
                Ok(CapabilityValue::from_value("stale".to_owned()))
            })
            .expect_err("stale replacement must fail");

        assert_eq!(
            error,
            ScopeError::ReplacementConflict {
                capability: id("model"),
                expected: first.generation(),
                actual: current.generation(),
            }
        );
        assert_eq!(
            label(&scope.get(&id("model")).expect("current remains")),
            "v2"
        );
    }

    #[test]
    fn stale_reader_cannot_remove_new_entry_identity() {
        let scope = Scope::root();
        let old = scope
            .provide(definition("model"), |_| {
                Ok(CapabilityValue::from_value("v1".to_owned()))
            })
            .expect("v1 constructs");
        let v2 = scope
            .replace(definition("model"), old.generation(), |_| {
                Ok(CapabilityValue::from_value("v2".to_owned()))
            })
            .expect("v2 constructs");
        let v3 = scope
            .replace(definition("model"), v2.generation(), |_| {
                Ok(CapabilityValue::from_value("v3".to_owned()))
            })
            .expect("v3 constructs");

        assert_ne!(old.entry_id(), v3.entry_id());
        drop(old);
        assert_eq!(label(&scope.get(&id("model")).expect("v3 remains")), "v3");
        assert_eq!(scope.generation(&id("model")), Some(v3.generation()));
    }

    #[test]
    fn teardown_respects_dependency_order() {
        let scope = Scope::root();
        let events = Arc::new(Mutex::new(Vec::<String>::new()));
        let track = |label: &str, events: &Arc<Mutex<Vec<String>>>| {
            let events = Arc::clone(events);
            CapabilityValue::new(label.to_owned(), move |label: String| {
                events.lock().expect("event lock").push(label);
            })
        };
        scope
            .provide(definition("c"), |_| Ok(track("c", &events)))
            .expect("c constructs");
        scope
            .provide(definition("b").depends_on(id("c")), |_| {
                Ok(track("b", &events))
            })
            .expect("b constructs");
        scope
            .provide(definition("a").depends_on(id("b")), |_| {
                Ok(track("a", &events))
            })
            .expect("a constructs");

        scope.teardown();
        assert_eq!(
            *events.lock().expect("event lock"),
            vec!["a".to_owned(), "b".to_owned(), "c".to_owned()]
        );
    }

    #[test]
    fn teardown_rejects_new_mutations() {
        let scope = Scope::root();
        scope.teardown();

        let provide = scope.provide(definition("a"), |_| {
            Ok(CapabilityValue::from_value("a".to_owned()))
        });
        assert!(matches!(provide, Err(ScopeError::Closed)));
    }

    #[test]
    fn cleanup_occurs_exactly_once() {
        let scope = Scope::root();
        let disposed = Arc::new(AtomicUsize::new(0));
        let handle = scope
            .provide(definition("a"), |_| Ok(value("a", &disposed)))
            .expect("a constructs");

        scope.teardown();
        scope.teardown();
        assert_eq!(disposed.load(Ordering::SeqCst), 0);
        drop(handle);
        assert_eq!(disposed.load(Ordering::SeqCst), 1);
        scope.teardown();
        assert_eq!(disposed.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn teardown_is_idempotent() {
        let scope = Scope::root();
        let disposed = Arc::new(AtomicUsize::new(0));
        scope
            .provide(definition("a"), |_| Ok(value("a", &disposed)))
            .expect("a constructs");

        scope.teardown();
        scope.teardown();
        assert_eq!(disposed.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn in_flight_dependent_pins_dependency_until_release() {
        let scope = Scope::root();
        let model_disposed = Arc::new(AtomicUsize::new(0));
        scope
            .provide(definition("model"), |_| Ok(value("model", &model_disposed)))
            .expect("model constructs");
        let service = scope
            .provide(definition("service").depends_on(id("model")), |_| {
                Ok(CapabilityValue::from_value("service".to_owned()))
            })
            .expect("service constructs");

        scope.teardown();
        assert_eq!(model_disposed.load(Ordering::SeqCst), 0);
        drop(service);
        assert_eq!(model_disposed.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn parent_scope_remains_alive_after_child_teardown() {
        let root = Scope::root();
        let parent_disposed = Arc::new(AtomicUsize::new(0));
        root.provide(definition("model"), |_| {
            Ok(value("parent", &parent_disposed))
        })
        .expect("parent model constructs");
        let child = root.child();
        child
            .provide(definition("service").depends_on(id("model")), |_| {
                Ok(CapabilityValue::from_value("child-service".to_owned()))
            })
            .expect("child service constructs");

        child.teardown();
        assert!(child.get(&id("service")).is_none());
        assert_eq!(
            label(&root.get(&id("model")).expect("parent remains")),
            "parent"
        );
        assert_eq!(parent_disposed.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn parent_teardown_does_not_leave_open_invalid_child() {
        let root = Scope::root();
        root.provide(definition("model"), |_| {
            Ok(CapabilityValue::from_value("model".to_owned()))
        })
        .expect("parent model constructs");
        let child = root.child();
        let service = child
            .provide(definition("service").depends_on(id("model")), |_| {
                Ok(CapabilityValue::from_value("service".to_owned()))
            })
            .expect("child service constructs");

        root.teardown();

        assert_eq!(label(&service), "service");
        assert!(child.get(&id("service")).is_none());
        assert!(matches!(
            child.provide(definition("new"), |_| {
                Ok(CapabilityValue::from_value("new".to_owned()))
            }),
            Err(ScopeError::Closed)
        ));
    }

    #[test]
    fn parent_teardown_preserves_in_flight_child_handles() {
        let root = Scope::root();
        let model_disposed = Arc::new(AtomicUsize::new(0));
        root.provide(definition("model"), |_| Ok(value("model", &model_disposed)))
            .expect("parent model constructs");
        let child = root.child();
        let service = child
            .provide(definition("service").depends_on(id("model")), |_| {
                Ok(CapabilityValue::from_value("service".to_owned()))
            })
            .expect("child service constructs");
        let pinned_model = service
            .dependencies()
            .get(&id("model"))
            .expect("service pins model")
            .clone();

        root.teardown();

        assert_eq!(label(&service), "service");
        assert_eq!(label(&pinned_model), "model");
        assert_eq!(model_disposed.load(Ordering::SeqCst), 0);
        drop(service);
        assert_eq!(model_disposed.load(Ordering::SeqCst), 0);
        drop(pinned_model);
        assert_eq!(model_disposed.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn closed_descendant_rejects_lookup() {
        let (root, child) = root_with_child();
        root.teardown();

        assert!(child.get(&id("model")).is_none());
        let nested = child.child();
        assert!(nested.get(&id("model")).is_none());
    }

    #[test]
    fn closed_descendant_rejects_mutation() {
        let (root, child) = root_with_child();
        root.teardown();

        assert!(matches!(
            child.validate_definition(definition("new")),
            Err(ScopeError::Closed)
        ));
        assert!(matches!(
            child.provide(definition("new"), |_| {
                Ok(CapabilityValue::from_value("new".to_owned()))
            }),
            Err(ScopeError::Closed)
        ));
        assert!(matches!(
            child.replace(definition("model"), Generation::FIRST, |_| {
                Ok(CapabilityValue::from_value("new".to_owned()))
            }),
            Err(ScopeError::Closed)
        ));
        let nested = child.child();
        assert!(matches!(
            nested.provide(definition("nested"), |_| {
                Ok(CapabilityValue::from_value("nested".to_owned()))
            }),
            Err(ScopeError::Closed)
        ));
    }

    #[test]
    fn nested_scope_teardown_is_child_first() {
        let root = Scope::root();
        let events = Arc::new(Mutex::new(Vec::<String>::new()));
        let track = |label: &str, events: &Arc<Mutex<Vec<String>>>| {
            let events = Arc::clone(events);
            CapabilityValue::new(label.to_owned(), move |label: String| {
                events.lock().expect("event lock").push(label);
            })
        };
        root.provide(definition("model"), |_| Ok(track("root", &events)))
            .expect("root model constructs");
        let child = root.child();
        child
            .provide(definition("service").depends_on(id("model")), |_| {
                Ok(track("child", &events))
            })
            .expect("child service constructs");
        let grandchild = child.child();
        grandchild
            .provide(definition("task").depends_on(id("service")), |_| {
                Ok(track("grandchild", &events))
            })
            .expect("grandchild task constructs");

        root.teardown();

        assert_eq!(
            *events.lock().expect("event lock"),
            vec![
                "grandchild".to_owned(),
                "child".to_owned(),
                "root".to_owned()
            ]
        );
    }

    #[test]
    fn parent_teardown_is_idempotent_with_closed_children() {
        let root = Scope::root();
        let root_disposed = Arc::new(AtomicUsize::new(0));
        let child_disposed = Arc::new(AtomicUsize::new(0));
        root.provide(definition("root"), |_| Ok(value("root", &root_disposed)))
            .expect("root value constructs");
        let child = root.child();
        child
            .provide(definition("child"), |_| Ok(value("child", &child_disposed)))
            .expect("child value constructs");

        child.teardown();
        root.teardown();
        child.teardown();
        root.teardown();

        assert_eq!(child_disposed.load(Ordering::SeqCst), 1);
        assert_eq!(root_disposed.load(Ordering::SeqCst), 1);
    }

    fn root_with_child() -> (Scope, Scope) {
        let root = Scope::root();
        root.provide(definition("model"), |_| {
            Ok(CapabilityValue::from_value("model".to_owned()))
        })
        .expect("parent model constructs");
        let child = root.child();
        (root, child)
    }

    #[test]
    fn generation_overflow_is_not_silent() {
        assert_eq!(Generation::MAX.checked_next(), None);
        assert!(std::panic::catch_unwind(|| Generation::MAX.next()).is_err());

        let scope = Scope::root();
        let entry = CapabilityEntry {
            definition: Arc::new(definition("model")),
            instance: Arc::new(InstanceSlot {
                value: CapabilityValue::from_value("model".to_owned()),
                dependencies: ResolvedDependencies::default(),
            }),
            generation: Generation::MAX,
            entry_id: next_entry_id(),
        };
        scope
            .state
            .entries
            .write()
            .expect("scope entry lock")
            .insert(id("model"), entry);
        let error = scope
            .replace(definition("model"), Generation::MAX, |_| {
                Ok(CapabilityValue::from_value("next".to_owned()))
            })
            .expect_err("generation overflow must be structured");
        assert_eq!(
            error,
            ScopeError::GenerationExhausted {
                capability: id("model")
            }
        );
    }

    #[test]
    fn entry_id_overflow_is_not_silent() {
        let counter = AtomicU64::new(u64::MAX);
        assert!(std::panic::catch_unwind(|| allocate_entry_id(&counter)).is_err());
        assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
    }

    #[test]
    #[should_panic(expected = "published capability topology must remain valid during teardown")]
    fn teardown_planning_does_not_silently_fallback() {
        let mut entries = BTreeMap::new();
        entries.insert(
            id("invalid"),
            CapabilityEntry {
                definition: Arc::new(definition("invalid").depends_on(id("missing"))),
                instance: Arc::new(InstanceSlot {
                    value: CapabilityValue::from_value("invalid".to_owned()),
                    dependencies: ResolvedDependencies::default(),
                }),
                generation: Generation::FIRST,
                entry_id: next_entry_id(),
            },
        );

        let _ = runtime_teardown_order(&entries);
    }

    #[test]
    fn concurrent_stale_replacement_returns_conflict() {
        let scope = Arc::new(Scope::root());
        let initial = scope
            .provide(definition("model"), |_| {
                Ok(CapabilityValue::from_value("v1".to_owned()))
            })
            .expect("v1 constructs");
        let generation = initial.generation();
        let ready = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let worker_scope = Arc::clone(&scope);
        let worker_ready = Arc::clone(&ready);
        let worker_release = Arc::clone(&release);
        let worker = thread::spawn(move || {
            worker_scope.replace(definition("model"), generation, move |_| {
                worker_ready.wait();
                worker_release.wait();
                Ok(CapabilityValue::from_value("worker".to_owned()))
            })
        });

        ready.wait();
        let winner = scope
            .replace(definition("model"), generation, |_| {
                Ok(CapabilityValue::from_value("winner".to_owned()))
            })
            .expect("main thread wins");
        release.wait();
        let loser = worker.join().expect("worker joins");

        assert!(matches!(
            loser,
            Err(ScopeError::ReplacementConflict {
                capability,
                expected,
                actual,
            }) if capability == id("model")
                && expected == generation
                && actual == winner.generation()
        ));
        assert_eq!(
            label(&scope.get(&id("model")).expect("winner remains")),
            "winner"
        );
    }
}

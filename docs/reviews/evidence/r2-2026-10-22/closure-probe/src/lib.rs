use runtime_loader::{
    CapabilityHandle, CapabilityPin, Runtime, ScopeError, StoreError,
};

pub fn scope_graph_admission(_: CapabilityGraphError) {}

pub fn scope_missing_dependency(_: CapabilityId) {}

pub fn journal_invariant(_: JournalInvariant) {}

pub fn store_idempotency(_: IdempotencyKey) {}

pub fn pin_fields(pin: CapabilityPin) -> (Generation, EntryId) {
    (pin.generation, pin.entry_id)
}

pub fn handle_accessors(handle: &CapabilityHandle) -> CapabilityId {
    handle.id().clone()
}

pub fn runtime_workflow(runtime: &Runtime) -> WorkflowGraph {
    runtime.workflow()
}

pub fn runtime_journal(runtime: &Runtime) -> DurableJournal {
    runtime.journal()
}

pub fn classify_scope(error: ScopeError) -> bool {
    matches!(error, ScopeError::GraphAdmission(_))
}

pub fn classify_store(error: StoreError) -> bool {
    matches!(error, StoreError::IdempotencyConflict { .. })
}

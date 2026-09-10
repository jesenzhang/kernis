//! K6 R2 end-to-end acceptance suite (scenarios D–M of the K6 contract).
//!
//! Every import below comes from `runtime_loader` alone: this file is also
//! the closure proof for the supported host entry (ADR 0007) — a host can
//! declare, resolve, compose, drive, recover, restore, and fail closed
//! without naming a single subsystem crate.
//!
//! The cross-process tests reuse the established K1/K2 child-process
//! pattern (`current_exe --exact <test>` with an environment handoff):
//! process A writes and exits, process B opens. That is a physical proof
//! that durable progress survives process death, not just object drop.

use runtime_loader::{
    CapabilityDeclaration, CapabilityDefinition, CapabilityRequirement, CapabilityValue,
    CatalogEntry, CompositionDriverShutdown, CompositionError, CompositionHandle, DriveResult,
    DriverExit, DurableRunState, EffectDispatchError, EffectDispatchFuture, EffectDispatchRequest,
    EffectDispatcher, EffectSemantics, FileDurableStore, HostConfig, Id, KnownEffectOutcome,
    LoaderError, ModuleCatalog, ModuleDefinition, ModuleReference, ModuleRegistration, OperationId,
    PluginDefinition, PluginFactory, PluginRuntime, RecoveredEffectState, RecoveryAction, RunId,
    RuntimeError, RuntimeHandle, RuntimeLoader, ShutdownStatus, StartupFailure, StepResult,
    StoreErrorKind, TaskDefinition,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::{env, fs, process};

const RUN_ID: &str = "k6-r2-run";
const PUBLISH_OP: &str = "publish-document";
const CHILD_MODE: &str = "K6_R2_CHILD_MODE";
const CHILD_STORE: &str = "K6_R2_CHILD_STORE";

fn id(value: &str) -> Id {
    Id::new(value).expect("test ids are valid")
}

fn reference(module: &str, version: &str) -> ModuleReference {
    ModuleReference::new(module, version).expect("test references are valid")
}

fn operation(value: &str) -> OperationId {
    OperationId::new(value).expect("test operations are valid")
}

fn run_id() -> RunId {
    RunId::new(RUN_ID).expect("test run id is valid")
}

/// Temporarily owns one physical store file for a test.
struct TempStore {
    dir: PathBuf,
    path: PathBuf,
}

impl TempStore {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "kernis-k6-r2-{tag}-{}-{}",
            process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock is after the unix epoch")
                .as_nanos()
        ));
        fs::create_dir_all(&dir).expect("the temp store directory is created");
        Self {
            path: dir.join("run.redb"),
            dir,
        }
    }
}

impl Drop for TempStore {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a current-thread runtime builds")
        .block_on(future)
}

// ---- Catalog: stable declarations both phases rebuild from scratch. ----

fn provider_registration() -> ModuleRegistration {
    ModuleRegistration::new(
        ModuleDefinition::new(id("content-store")).with_declarative_capability(
            CapabilityDeclaration::new(id("document-store"), "provider", "v1"),
        ),
    )
    .factory(id("document-store"), "v1", |_| {
        Ok(CapabilityValue::from_value("in-memory-store".to_owned()))
    })
}

fn indexer_registration() -> ModuleRegistration {
    let factory: PluginFactory =
        Arc::new(|_| Box::pin(async { Ok(CapabilityValue::from_value("index-ready".to_owned())) }));
    ModuleRegistration::new(
        ModuleDefinition::new(id("indexer"))
            .depends_on(id("content-store"))
            .with_reactive_capability(id("index-updates"), "service", "v1"),
    )
    .plugin(PluginRuntime::new(PluginDefinition::new(
        id("index-plugin"),
        CapabilityDefinition::new(id("index-updates"), "service").with_replay_identity("v1"),
        factory,
    )))
}

fn publisher_registration(semantics: EffectSemantics, extra_task: bool) -> ModuleRegistration {
    let mut definition = ModuleDefinition::new(id("publisher"))
        .depends_on(id("indexer"))
        .with_task(TaskDefinition::new(id("prepare"), "prepare one document"))
        .with_task(
            TaskDefinition::new(id("publish"), "publish one document")
                .depends_on(id("prepare"))
                .require_capability(CapabilityRequirement::new(id("document-store"), "v1"))
                .with_effect(operation(PUBLISH_OP), semantics),
        );
    if extra_task {
        definition = definition.with_task(TaskDefinition::new(id("audit"), "audit after publish"));
    }
    ModuleRegistration::new(definition)
}

fn catalog(semantics: EffectSemantics, extra_task: bool) -> ModuleCatalog {
    let provider = CatalogEntry::new(reference("content-store", "1"), || {
        Ok(provider_registration())
    });
    let indexer = CatalogEntry::new(reference("indexer", "1"), || Ok(indexer_registration()))
        .depends_on(reference("content-store", "1"));
    let publisher = CatalogEntry::new(reference("publisher", "1"), move || {
        Ok(publisher_registration(semantics, extra_task))
    })
    .depends_on(reference("indexer", "1"));
    ModuleCatalog::new()
        .register(provider)
        .expect("content-store@1 registers")
        .register(indexer)
        .expect("indexer@1 registers")
        .register(publisher)
        .expect("publisher@1 registers")
}

// ---- Dispatchers: succeed known, or leave the outcome unknown. ----

#[derive(Default)]
struct AckDispatcher {
    calls: Arc<Mutex<Vec<OperationId>>>,
}

impl EffectDispatcher for AckDispatcher {
    fn dispatch(&mut self, request: EffectDispatchRequest) -> EffectDispatchFuture {
        let calls = Arc::clone(&self.calls);
        Box::pin(async move {
            calls
                .lock()
                .expect("the calls lock is healthy")
                .push(request.operation_id.clone());
            Ok(KnownEffectOutcome::Succeeded)
        })
    }
}

struct UnknownDispatcher {
    calls: Arc<Mutex<Vec<OperationId>>>,
}

impl EffectDispatcher for UnknownDispatcher {
    fn dispatch(&mut self, request: EffectDispatchRequest) -> EffectDispatchFuture {
        let calls = Arc::clone(&self.calls);
        Box::pin(async move {
            calls
                .lock()
                .expect("the calls lock is healthy")
                .push(request.operation_id.clone());
            Err(EffectDispatchError::unknown("the external reply was lost"))
        })
    }
}

// ---- Sessions built from fresh objects each time. ----

/// A restored (or started) driver session: everything here is constructed
/// inside the call — nothing is shared between phases of any test.
type Session = (
    RuntimeHandle,
    tokio::task::JoinHandle<DriverExit<FileDurableStore>>,
    CompositionHandle,
    Arc<Mutex<Vec<OperationId>>>,
);

async fn start_session(store: FileDurableStore, semantics: EffectSemantics) -> Session {
    let catalog = catalog(semantics, false);
    let plan = RuntimeLoader::new(&catalog)
        .resolve([reference("publisher", "1")])
        .expect("the closure resolves")
        .compose()
        .expect("the resolved modules compose");
    let dispatcher = AckDispatcher::default();
    let calls = Arc::clone(&dispatcher.calls);
    let assembly = plan
        .start_with_store(run_id(), &HostConfig::new(), store)
        .await
        .expect("the composition activates");
    let (driver, handle, composition) = assembly.into_driver(dispatcher);
    let join = tokio::spawn(driver.run());
    (handle, join, composition, calls)
}

async fn restore_session(store: FileDurableStore, semantics: EffectSemantics) -> Session {
    let catalog = catalog(semantics, false);
    let plan = RuntimeLoader::new(&catalog)
        .resolve([reference("publisher", "1")])
        .expect("the same stable references resolve")
        .compose()
        .expect("the plan composes identically");
    let dispatcher = AckDispatcher::default();
    let calls = Arc::clone(&dispatcher.calls);
    let assembly = plan
        .restore(run_id(), &HostConfig::new(), store)
        .await
        .expect("the cold restore reconstructs the runtime");
    let (driver, handle, composition) = assembly.into_driver(dispatcher);
    let join = tokio::spawn(driver.run());
    (handle, join, composition, calls)
}

/// The one supported shutdown tail (§22): orderly driver shutdown, driver
/// join, composition dispose. Asserts the full release at every step.
async fn orderly_finish(
    session: Session,
    expect_status: ShutdownStatus,
) -> CompositionDriverShutdown<FileDurableStore> {
    let (handle, join, composition, _calls) = session;
    let status = handle.shutdown().await.expect("orderly shutdown");
    assert_eq!(status, expect_status);
    let exit = join.await.expect("the driver task joins");
    let outcome = composition.dispose_after_driver(exit).await;
    assert_eq!(outcome.shutdown_status, expect_status);
    assert!(
        outcome.rollback.failures.is_empty(),
        "dispose must not fail: {:?}",
        outcome.rollback.failures
    );
    assert_eq!(
        outcome.rollback.cleaned.len(),
        3,
        "all three modules were cleaned up"
    );
    outcome
}

fn durable_facts(outcome: &CompositionDriverShutdown<FileDurableStore>) -> DurableRunState {
    outcome
        .runtime
        .durable_state()
        .expect("durable state reads")
}

// ---- Child process: process A writes and exits. ----

async fn child_phase(mode: &str, path: &Path) {
    match mode {
        "phase1" => {
            let store = FileDurableStore::open(path).expect("the child opens the store");
            let session = start_session(store, EffectSemantics::Idempotent).await;
            let prepared = session.0.drive().await.expect("driving succeeds");
            assert!(matches!(&prepared,
                    DriveResult::Step(StepResult::Completed { task_id, .. })
                        if *task_id == id("prepare")));
            let outcome = orderly_finish(session, ShutdownStatus::Clean).await;
            let state = durable_facts(&outcome);
            assert!(state.is_completed(&id("prepare")));
            assert!(!state.is_completed(&id("publish")));
            assert_eq!(state.dispatches(&operation(PUBLISH_OP)).count(), 0);
        }
        "complete" => {
            let store = FileDurableStore::open(path).expect("the child opens the store");
            let (handle, join, composition, calls) =
                start_session(store, EffectSemantics::Idempotent).await;
            let prepared = handle.drive().await.expect("driving succeeds");
            assert!(matches!(&prepared,
                DriveResult::Step(StepResult::Completed { task_id, .. }) if *task_id == id("prepare")));
            let dispatched = handle.drive().await.expect("driving succeeds");
            assert!(matches!(&dispatched,
                DriveResult::EffectCompleted { request, outcome }
                    if request.operation_id == operation(PUBLISH_OP)
                        && *outcome == KnownEffectOutcome::Succeeded));
            let completed = handle.drive().await.expect("driving succeeds");
            assert!(matches!(&completed,
                DriveResult::Step(StepResult::Completed { task_id, .. }) if *task_id == id("publish")));
            let status = handle.shutdown().await.expect("orderly shutdown");
            assert_eq!(status, ShutdownStatus::Clean);
            let exit = join.await.expect("the driver task joins");
            let outcome = composition.dispose_after_driver(exit).await;
            assert!(outcome.rollback.failures.is_empty());
            let state = outcome
                .runtime
                .durable_state()
                .expect("durable state reads");
            assert_eq!(state.completion_history().len(), 2);
            assert_eq!(state.dispatches(&operation(PUBLISH_OP)).count(), 1);
            drop(calls);
        }
        "unknown" => {
            let store = FileDurableStore::open(path).expect("the child opens the store");
            let catalog = catalog(EffectSemantics::NonIdempotent, false);
            let plan = RuntimeLoader::new(&catalog)
                .resolve([reference("publisher", "1")])
                .expect("the closure resolves")
                .compose()
                .expect("the resolved modules compose");
            let dispatcher = UnknownDispatcher {
                calls: Arc::new(Mutex::new(Vec::new())),
            };
            let assembly = plan
                .start_with_store(run_id(), &HostConfig::new(), store)
                .await
                .expect("the composition activates");
            let (driver, handle, composition) = assembly.into_driver(dispatcher);
            let join = tokio::spawn(driver.run());
            let prepared = handle.drive().await.expect("driving succeeds");
            assert!(matches!(&prepared,
                DriveResult::Step(StepResult::Completed { task_id, .. }) if *task_id == id("prepare")));
            let unknown = handle.drive().await.expect("driving succeeds");
            assert!(matches!(&unknown,
                    DriveResult::EffectUnknown { request, error }
                        if request.operation_id == operation(PUBLISH_OP)
                            && error.reason().contains("lost")));
            let status = handle.shutdown().await.expect("orderly shutdown");
            assert_eq!(
                status,
                ShutdownStatus::ReconciliationRequired {
                    operation_id: operation(PUBLISH_OP)
                }
            );
            let exit = join.await.expect("the driver task joins");
            let outcome = composition.dispose_after_driver(exit).await;
            assert!(outcome.rollback.failures.is_empty());
            let state = outcome
                .runtime
                .durable_state()
                .expect("durable state reads");
            assert_eq!(state.dispatches(&operation(PUBLISH_OP)).count(), 1);
            assert!(state.latest_outcome(&operation(PUBLISH_OP)).is_none());
        }
        other => panic!("unknown child mode {other}"),
    }
}

/// Launches this test binary as a child that runs only `test` under
/// `mode`, writing to `store`, and requires it to exit successfully.
fn spawn_child(test: &str, mode: &str, store: &Path) {
    let status = process::Command::new(env::current_exe().expect("the test executable path"))
        .arg("--exact")
        .arg(test)
        .arg("--nocapture")
        .env(CHILD_MODE, mode)
        .env(CHILD_STORE, store)
        .status()
        .expect("the child process spawns");
    assert!(
        status.success(),
        "child mode {mode} for {test} must exit successfully"
    );
}

/// When running as the handoff child for `mode`, returns the store path the
/// parent handed us; otherwise returns `None` (parent mode).
fn child_handoff(mode: &str) -> Option<PathBuf> {
    let child_mode = env::var(CHILD_MODE).ok()?;
    assert_eq!(child_mode, mode, "the child was launched for this mode");
    Some(
        env::var(CHILD_STORE)
            .expect("the child receives its store path")
            .into(),
    )
}

// ---- D + E + F + G + I: mid-run cross-process cold restart, continue. ----

#[test]
fn cold_restart_mid_run_survives_process_boundary() {
    const TEST: &str = "cold_restart_mid_run_survives_process_boundary";
    if let Some(store) = child_handoff("phase1") {
        block_on(child_phase("phase1", &store));
        return;
    }
    let temp = TempStore::new("mid-run");
    spawn_child(TEST, "phase1", &temp.path);
    block_on(async {
        // Process B: a genuinely fresh restore from the same physical store.
        let store = FileDurableStore::open(&temp.path).expect("the store reopens");
        let session = restore_session(store, EffectSemantics::Idempotent).await;
        let handle = &session.0;
        let dispatched = handle.drive().await.expect("driving succeeds");
        assert!(matches!(&dispatched,
            DriveResult::EffectCompleted { request, outcome }
                if request.operation_id == operation(PUBLISH_OP)
                    && *outcome == KnownEffectOutcome::Succeeded));
        let completed = handle.drive().await.expect("driving succeeds");
        assert!(matches!(&completed,
            DriveResult::Step(StepResult::Completed { task_id, .. }) if *task_id == id("publish")));
        let idle = handle.drive().await.expect("driving succeeds");
        assert!(matches!(idle, DriveResult::Step(StepResult::Idle)));
        let outcome = orderly_finish(session, ShutdownStatus::Clean).await;
        let state = durable_facts(&outcome);
        assert!(state.is_completed(&id("prepare")) && state.is_completed(&id("publish")));
        assert_eq!(
            state.completion_history().len(),
            2,
            "the restarted process must not complete work twice"
        );
        assert_eq!(
            state.dispatches(&operation(PUBLISH_OP)).count(),
            1,
            "the surviving effect dispatch is never re-run"
        );
    });
}

// ---- D + E + F + G: a fully completed run is never redone cross-process. ----

#[test]
fn cross_process_completed_run_is_not_redone() {
    const TEST: &str = "cross_process_completed_run_is_not_redone";
    if let Some(store) = child_handoff("complete") {
        block_on(child_phase("complete", &store));
        return;
    }
    let temp = TempStore::new("completed");
    spawn_child(TEST, "complete", &temp.path);
    block_on(async {
        let store = FileDurableStore::open(&temp.path).expect("the store reopens");
        let session = restore_session(store, EffectSemantics::Idempotent).await;
        let idle = session.0.drive().await.expect("driving succeeds");
        assert!(
            matches!(idle, DriveResult::Step(StepResult::Idle)),
            "a restored completed run has no work to redo, got {idle:?}"
        );
        let outcome = orderly_finish(session, ShutdownStatus::Clean).await;
        let state = durable_facts(&outcome);
        assert_eq!(state.completion_history().len(), 2);
        assert_eq!(state.dispatches(&operation(PUBLISH_OP)).count(), 1);
        assert_eq!(
            state
                .latest_outcome(&operation(PUBLISH_OP))
                .expect("the known outcome survived the restart")
                .outcome,
            KnownEffectOutcome::Succeeded
        );
    });
}

// ---- H: an unknown external outcome stays unknown-safe cross-process. ----

#[test]
fn unknown_effect_survives_process_boundary_without_reexecution() {
    const TEST: &str = "unknown_effect_survives_process_boundary_without_reexecution";
    if let Some(store) = child_handoff("unknown") {
        block_on(child_phase("unknown", &store));
        return;
    }
    let temp = TempStore::new("unknown");
    spawn_child(TEST, "unknown", &temp.path);
    block_on(async {
        let store = FileDurableStore::open(&temp.path).expect("the store reopens");
        let session = restore_session(store, EffectSemantics::NonIdempotent).await;
        let blocked = session.0.drive().await.expect("driving succeeds");
        assert!(matches!(&blocked,
            DriveResult::Step(StepResult::Blocked { task_id, operation_id, action })
                if *task_id == id("publish")
                    && *operation_id == Some(operation(PUBLISH_OP))
                    && *action == RecoveryAction::Reconcile));
        let decision = session
            .0
            .recover(operation(PUBLISH_OP))
            .await
            .expect("recovery classification succeeds");
        assert_eq!(decision.action, RecoveryAction::Reconcile);
        let outcome = orderly_finish(
            session,
            ShutdownStatus::ReconciliationRequired {
                operation_id: operation(PUBLISH_OP),
            },
        )
        .await;
        let state = durable_facts(&outcome);
        assert_eq!(
            state.effect_state(&operation(PUBLISH_OP)),
            RecoveredEffectState::OutcomeUnknown
        );
        assert_eq!(
            state.dispatches(&operation(PUBLISH_OP)).count(),
            1,
            "restoring must never re-dispatch an operation whose outcome is unknown"
        );
        assert!(state.latest_outcome(&operation(PUBLISH_OP)).is_none());
        assert!(!state.is_completed(&id("publish")));
    });
}

// ---- J: a wrong RunDefinition restore fails closed with a typed error. ----

#[test]
fn wrong_run_definition_restore_fails_closed() {
    let temp = TempStore::new("wrong-definition");
    block_on(async {
        // A run started under the correct definition, stopped orderly.
        let store = FileDurableStore::open(&temp.path).expect("the store opens");
        let session = start_session(store, EffectSemantics::Idempotent).await;
        let prepared = session.0.drive().await.expect("driving succeeds");
        assert!(matches!(
            prepared,
            DriveResult::Step(StepResult::Completed { .. })
        ));
        let outcome = orderly_finish(session, ShutdownStatus::Clean).await;
        drop(outcome);

        // Restoring the same durable run under a different definition
        // must fail closed with the typed identity mismatch.
        let store = FileDurableStore::open(&temp.path).expect("the store reopens");
        let extended = catalog(EffectSemantics::Idempotent, true);
        let plan = RuntimeLoader::new(&extended)
            .resolve([reference("publisher", "1")])
            .expect("the extended closure resolves")
            .compose()
            .expect("the extended plan composes");
        let failure: StartupFailure = plan
            .restore(run_id(), &HostConfig::new(), store)
            .await
            .expect_err("a wrong definition must not restore");
        assert!(
            matches!(
                failure.cause.as_ref(),
                CompositionError::RuntimeConstructionFailed {
                    source: RuntimeError::DefinitionMismatch { .. },
                    ..
                }
            ),
            "typed definition mismatch expected, got {failure:?}"
        );
    });
    // The failed attempt changed nothing: the correct definition still
    // restores with the durable progress intact.
    block_on(async {
        let store = FileDurableStore::open(&temp.path).expect("the store reopens");
        let session = restore_session(store, EffectSemantics::Idempotent).await;
        let outcome = orderly_finish(session, ShutdownStatus::Clean).await;
        let state = durable_facts(&outcome);
        assert!(state.is_completed(&id("prepare")));
        assert!(!state.is_completed(&id("publish")));
    });
}

// ---- K: corrupted physical durable state fails closed at open. ----

#[test]
fn corrupted_durable_state_fails_closed() {
    let temp = TempStore::new("corrupt-garbage");
    block_on(async {
        let store = FileDurableStore::open(&temp.path).expect("the store opens");
        let session = start_session(store, EffectSemantics::Idempotent).await;
        let outcome = orderly_finish(session, ShutdownStatus::Clean).await;
        drop(outcome);
    });
    fs::write(&temp.path, b"not a redb database").expect("the test overwrites the backend");
    let error =
        FileDurableStore::open(&temp.path).expect_err("a garbage backend must fail to open");
    assert_eq!(error.kind(), StoreErrorKind::DataCorruption);

    let temp = TempStore::new("corrupt-truncated");
    block_on(async {
        let store = FileDurableStore::open(&temp.path).expect("the store opens");
        let session = start_session(store, EffectSemantics::Idempotent).await;
        let outcome = orderly_finish(session, ShutdownStatus::Clean).await;
        drop(outcome);
    });
    let bytes = fs::read(&temp.path).expect("the backend is readable");
    fs::write(&temp.path, &bytes[..bytes.len() / 2]).expect("the test truncates the backend");
    let error =
        FileDurableStore::open(&temp.path).expect_err("a truncated backend must fail to open");
    assert_eq!(error.kind(), StoreErrorKind::DataCorruption);
}

// ---- L: a missing module reference stays a LoaderError. ----

#[test]
fn missing_module_reference_stays_loader_error() {
    let error = RuntimeLoader::new(&catalog(EffectSemantics::Idempotent, false))
        .resolve([reference("publisher", "1"), reference("auditor", "1")])
        .expect_err("an uncataloged reference must fail resolution");
    assert!(
        matches!(&error,
            LoaderError::MissingReference { requested, required_by: None, .. }
                if *requested == reference("auditor", "1")),
        "typed missing-reference failure expected, got {error:?}"
    );
}

// ---- M: a composition contribution conflict stays a CompositionError. ----

#[test]
fn duplicate_capability_ownership_stays_composition_error() {
    let provider = CatalogEntry::new(reference("content-store", "1"), || {
        Ok(provider_registration())
    });
    let rival = CatalogEntry::new(reference("rival-store", "1"), || {
        Ok(ModuleRegistration::new(
            ModuleDefinition::new(id("rival-store")).with_declarative_capability(
                CapabilityDeclaration::new(id("document-store"), "provider", "v1"),
            ),
        ))
    });
    let catalog = ModuleCatalog::new()
        .register(provider)
        .expect("content-store@1 registers")
        .register(rival)
        .expect("rival-store@1 registers");
    let resolved = RuntimeLoader::new(&catalog)
        .resolve([
            reference("content-store", "1"),
            reference("rival-store", "1"),
        ])
        .expect("both roots resolve");
    let error = resolved
        .compose()
        .expect_err("two modules cannot own one capability slot");
    assert!(
        matches!(
            error,
            CompositionError::DuplicateCapabilityOwnership { .. }
                | CompositionError::CapabilityDefinitionConflict { .. }
        ),
        "a typed composition conflict expected, got {error:?}"
    );
}

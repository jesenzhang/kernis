//! Executor-neutral asynchronous ownership for [`crate::Runtime`].
//!
//! The driver is deliberately separate from the synchronous coordinator. It
//! owns the runtime on one execution path and exposes only typed command
//! futures to callers. No runtime borrow or synchronization guard is retained
//! while an external effect future is pending.

use crate::{Cancellation, Runtime, RuntimeError, RuntimeEvent, StepResult};
use execution_stream::{KeyedStreamItem, StreamItem};
use kernis_core::Id;
use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};
use workflow_recovery::{
    AttemptId, DurableRunState, DurableStore, EffectSemantics, KnownEffectOutcome, OperationId,
    RecoveredEffectState, RecoveryDecision, RunId,
};

/// Future returned by an [`EffectDispatcher`].
pub type EffectDispatchFuture =
    Pin<Box<dyn Future<Output = Result<KnownEffectOutcome, EffectDispatchError>> + Send + 'static>>;

/// A dispatcher-side result that does not establish a known external outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EffectDispatchError {
    /// The durable dispatch boundary was crossed, but the external result is
    /// not known to the host.
    UnknownOutcome {
        /// Stable diagnostic supplied by the dispatcher adapter.
        reason: String,
    },
}

impl EffectDispatchError {
    /// Creates an unknown-outcome error with a stable diagnostic.
    #[must_use]
    pub fn unknown(reason: impl Into<String>) -> Self {
        Self::UnknownOutcome {
            reason: reason.into(),
        }
    }

    /// Returns the dispatcher diagnostic.
    #[must_use]
    pub fn reason(&self) -> &str {
        match self {
            Self::UnknownOutcome { reason } => reason,
        }
    }
}

impl fmt::Display for EffectDispatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownOutcome { reason } => {
                write!(f, "external effect outcome is unknown: {reason}")
            }
        }
    }
}

impl std::error::Error for EffectDispatchError {}

/// Typed request handed to an asynchronous effect adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectDispatchRequest {
    /// Workflow execution identity.
    pub run_id: RunId,
    /// Logical workflow task owning the effect.
    pub task_id: Id,
    /// Stable logical operation identity.
    pub operation_id: OperationId,
    /// Exact durable attempt identity allocated by [`crate::Runtime`].
    pub attempt_id: AttemptId,
    /// Retry safety semantics for the logical operation.
    pub semantics: EffectSemantics,
}

/// Adapter seam for awaiting one external effect.
pub trait EffectDispatcher: Send {
    /// Starts the external operation represented by `request`.
    ///
    /// The returned future must own everything it needs and must not borrow
    /// the dispatcher or the runtime. Returning `Err` means that the external
    /// outcome is unknown; adapters must not use it to represent a known
    /// failure. Return `Ok(KnownEffectOutcome::Failed)` when failure is known.
    fn dispatch(&mut self, request: EffectDispatchRequest) -> EffectDispatchFuture;
}

/// Result of one asynchronous driver wakeup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DriveResult {
    /// The synchronous runtime produced a non-effect step result.
    Step(StepResult),
    /// The dispatcher returned a known outcome which the driver durably
    /// recorded for the exact request attempt.
    EffectCompleted {
        /// Request that was durably dispatched.
        request: EffectDispatchRequest,
        /// Known outcome recorded by the driver.
        outcome: KnownEffectOutcome,
    },
    /// The dispatcher could not establish an external outcome. No outcome was
    /// fabricated or recorded.
    EffectUnknown {
        /// Request whose dispatch is now durably authoritative.
        request: EffectDispatchRequest,
        /// Typed adapter explanation.
        error: EffectDispatchError,
    },
    /// A lossless lifecycle observation could not fit in the bounded stream.
    /// The driver retains this item and retries it after a caller drains the
    /// stream and submits another wakeup.
    Backpressured {
        /// Rejected lifecycle item retained for lossless retry.
        item: StreamItem<RuntimeEvent>,
    },
}

/// Classification returned by a successful driver shutdown.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShutdownStatus {
    /// No uncompleted dispatch or recovery decision remains.
    Clean,
    /// A durable intent has not crossed the dispatch boundary yet.
    PendingDispatch {
        /// Operation waiting for its first dispatch.
        operation_id: OperationId,
    },
    /// An idempotent operation has an unknown external outcome and is pending
    /// explicit retry or reconciliation.
    PendingUnknown {
        /// Operation whose outcome is unknown.
        operation_id: OperationId,
    },
    /// A non-idempotent operation has an unknown external outcome and cannot
    /// be retried automatically.
    ReconciliationRequired {
        /// Operation requiring external reconciliation.
        operation_id: OperationId,
    },
    /// A known failure has been durably recorded and remains an observed
    /// failure because the synchronous recovery policy does not invent a
    /// retry.
    ObservedFailure {
        /// Operation whose known failure remains observed.
        operation_id: OperationId,
    },
}

/// Error returned by a driver command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DriverError {
    /// The driver owner was dropped before orderly shutdown completed.
    OwnerDropped,
    /// The driver has completed or is completing a successful shutdown.
    ShuttingDown,
    /// The owned synchronous runtime rejected the command.
    Runtime(RuntimeError),
    /// The synchronous runtime returned an attempt different from the one
    /// that produced the pending step. The external adapter was not called.
    AttemptLineageMismatch {
        /// Logical operation whose lineage was inconsistent.
        operation_id: OperationId,
        /// Attempt reported by the pending step.
        expected: AttemptId,
        /// Attempt currently retained by the runtime, if one was found.
        actual: Option<AttemptId>,
    },
}

impl fmt::Display for DriverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OwnerDropped => f.write_str("runtime driver owner was dropped"),
            Self::ShuttingDown => f.write_str("runtime driver is shutting down"),
            Self::Runtime(error) => write!(f, "runtime driver command failed: {error}"),
            Self::AttemptLineageMismatch {
                operation_id,
                expected,
                actual,
            } => {
                write!(
                    f,
                    "attempt lineage mismatch for {operation_id}: expected {expected}, got "
                )?;
                match actual {
                    Some(actual) => write!(f, "{actual}"),
                    None => f.write_str("none"),
                }
            }
        }
    }
}

impl std::error::Error for DriverError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Runtime(error) => Some(error),
            Self::OwnerDropped | Self::ShuttingDown | Self::AttemptLineageMismatch { .. } => None,
        }
    }
}

/// Runtime and final status returned after a successful driver shutdown.
pub struct DriverExit<S>
where
    S: DurableStore + Send,
{
    runtime: Runtime<S>,
    status: ShutdownStatus,
}

impl<S> DriverExit<S>
where
    S: DurableStore + Send,
{
    /// Returns the final shutdown classification.
    #[must_use]
    pub const fn shutdown_status(&self) -> &ShutdownStatus {
        &self.status
    }

    /// Returns the released synchronous runtime by value.
    #[must_use]
    pub fn into_runtime(self) -> Runtime<S> {
        self.runtime
    }

    /// Borrows the released synchronous runtime for inspection.
    #[must_use]
    pub const fn runtime(&self) -> &Runtime<S> {
        &self.runtime
    }
}

/// Awaitable response to one typed [`RuntimeHandle`] command.
#[must_use = "a driver command is observed by awaiting its response future"]
pub struct DriverFuture<T> {
    state: Arc<Mutex<ResponseState<T>>>,
}

impl<T> Future for DriverFuture<T> {
    type Output = Result<T, DriverError>;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = lock(self.get_mut().state.as_ref());
        match state.result.take() {
            Some(result) => Poll::Ready(result),
            None => {
                state.waker = Some(context.waker().clone());
                Poll::Pending
            }
        }
    }
}

struct ResponseState<T> {
    result: Option<Result<T, DriverError>>,
    waker: Option<Waker>,
}

struct ResponseSender<T> {
    state: Arc<Mutex<ResponseState<T>>>,
}

fn response_channel<T>() -> (ResponseSender<T>, DriverFuture<T>) {
    let state = Arc::new(Mutex::new(ResponseState {
        result: None,
        waker: None,
    }));
    (
        ResponseSender {
            state: Arc::clone(&state),
        },
        DriverFuture { state },
    )
}

impl<T> ResponseSender<T> {
    fn complete(self, result: Result<T, DriverError>) {
        let waker = {
            let mut state = lock(&self.state);
            if state.result.is_some() {
                return;
            }
            state.result = Some(result);
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

impl<T> Drop for ResponseSender<T> {
    fn drop(&mut self) {
        let waker = {
            let mut state = lock(&self.state);
            if state.result.is_none() {
                state.result = Some(Err(DriverError::OwnerDropped));
            }
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

struct DriverOwnerGuard(CommandMailbox);

impl Drop for DriverOwnerGuard {
    fn drop(&mut self) {
        let pending = {
            let mut state = lock(&self.0.state);
            if state.closed {
                return;
            }
            state.closed = true;
            state.owner_dropped = true;
            state.waker = None;
            state.commands.drain(..).collect::<Vec<_>>()
        };
        for command in pending {
            command.reject(DriverError::OwnerDropped);
        }
    }
}

#[derive(Clone)]
struct CommandMailbox {
    state: Arc<Mutex<MailboxState>>,
}

struct MailboxState {
    commands: VecDeque<DriverCommand>,
    closed: bool,
    owner_dropped: bool,
    waker: Option<Waker>,
    shutdown_execution_events: Vec<StreamItem<RuntimeEvent>>,
    shutdown_progress_events: Vec<KeyedStreamItem<Id, RuntimeEvent>>,
    shutdown_telemetry_events: Vec<StreamItem<RuntimeEvent>>,
}

impl CommandMailbox {
    fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(MailboxState {
                commands: VecDeque::new(),
                closed: false,
                owner_dropped: false,
                waker: None,
                shutdown_execution_events: Vec::new(),
                shutdown_progress_events: Vec::new(),
                shutdown_telemetry_events: Vec::new(),
            })),
        }
    }

    fn submit(&self, command: DriverCommand) -> Result<(), DriverCommand> {
        let waker = {
            let mut state = lock(&self.state);
            if state.closed {
                return Err(command);
            }
            state.commands.push_back(command);
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
        Ok(())
    }

    fn next(&self) -> NextCommand {
        NextCommand {
            mailbox: self.clone(),
        }
    }

    fn close_and_reject_pending(
        &self,
        execution_events: Vec<StreamItem<RuntimeEvent>>,
        progress_events: Vec<KeyedStreamItem<Id, RuntimeEvent>>,
        telemetry_events: Vec<StreamItem<RuntimeEvent>>,
    ) {
        let pending = {
            let mut state = lock(&self.state);
            state.closed = true;
            state.waker = None;
            state.shutdown_execution_events = execution_events;
            state.shutdown_progress_events = progress_events;
            state.shutdown_telemetry_events = telemetry_events;
            state.commands.drain(..).collect::<Vec<_>>()
        };
        for command in pending {
            self.complete_after_close(command);
        }
    }

    fn take_shutdown_execution_events(&self) -> Option<Vec<StreamItem<RuntimeEvent>>> {
        let mut state = lock(&self.state);
        if state.closed {
            Some(std::mem::take(&mut state.shutdown_execution_events))
        } else {
            None
        }
    }

    fn take_shutdown_progress_events(&self) -> Option<Vec<KeyedStreamItem<Id, RuntimeEvent>>> {
        let mut state = lock(&self.state);
        if state.closed {
            Some(std::mem::take(&mut state.shutdown_progress_events))
        } else {
            None
        }
    }

    fn take_shutdown_telemetry_events(&self) -> Option<Vec<StreamItem<RuntimeEvent>>> {
        let mut state = lock(&self.state);
        if state.closed {
            Some(std::mem::take(&mut state.shutdown_telemetry_events))
        } else {
            None
        }
    }

    fn complete_after_close(&self, command: DriverCommand) {
        if lock(&self.state).owner_dropped {
            command.reject(DriverError::OwnerDropped);
            return;
        }
        match command {
            DriverCommand::DrainExecutionEvents { reply } => {
                match self.take_shutdown_execution_events() {
                    Some(events) => reply.complete(Ok(events)),
                    None => reply.complete(Err(DriverError::ShuttingDown)),
                }
            }
            DriverCommand::DrainProgressEvents { reply } => {
                match self.take_shutdown_progress_events() {
                    Some(events) => reply.complete(Ok(events)),
                    None => reply.complete(Err(DriverError::ShuttingDown)),
                }
            }
            DriverCommand::DrainTelemetryEvents { reply } => {
                match self.take_shutdown_telemetry_events() {
                    Some(events) => reply.complete(Ok(events)),
                    None => reply.complete(Err(DriverError::ShuttingDown)),
                }
            }
            command => command.reject(DriverError::ShuttingDown),
        }
    }
}

struct NextCommand {
    mailbox: CommandMailbox,
}

impl Future for NextCommand {
    type Output = Option<DriverCommand>;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = lock(&self.get_mut().mailbox.state);
        if let Some(command) = state.commands.pop_front() {
            Poll::Ready(Some(command))
        } else if state.closed {
            Poll::Ready(None)
        } else {
            state.waker = Some(context.waker().clone());
            Poll::Pending
        }
    }
}

enum DriverCommand {
    Wake {
        reply: ResponseSender<DriveResult>,
    },
    DispatchEffect {
        operation_id: OperationId,
        reply: ResponseSender<DriveResult>,
    },
    Recover {
        operation_id: OperationId,
        reply: ResponseSender<RecoveryDecision>,
    },
    CancelTask {
        task_id: Id,
        reply: ResponseSender<Cancellation>,
    },
    DrainExecutionEvents {
        reply: ResponseSender<Vec<StreamItem<RuntimeEvent>>>,
    },
    DrainProgressEvents {
        reply: ResponseSender<Vec<KeyedStreamItem<Id, RuntimeEvent>>>,
    },
    DrainTelemetryEvents {
        reply: ResponseSender<Vec<StreamItem<RuntimeEvent>>>,
    },
    Shutdown {
        reply: ResponseSender<ShutdownStatus>,
    },
}

impl DriverCommand {
    fn reject(self, error: DriverError) {
        match self {
            Self::Wake { reply } | Self::DispatchEffect { reply, .. } => reply.complete(Err(error)),
            Self::Recover { reply, .. } => reply.complete(Err(error)),
            Self::CancelTask { reply, .. } => reply.complete(Err(error)),
            Self::DrainExecutionEvents { reply } => reply.complete(Err(error)),
            Self::DrainProgressEvents { reply } => reply.complete(Err(error)),
            Self::DrainTelemetryEvents { reply } => reply.complete(Err(error)),
            Self::Shutdown { reply } => reply.complete(Err(error)),
        }
    }
}

/// Cloneable typed command handle for one [`RuntimeDriver`].
///
/// Cloning this handle does not clone or share the runtime. Every command is
/// serialized by the one driver that owns the runtime.
#[derive(Clone)]
pub struct RuntimeHandle {
    mailbox: CommandMailbox,
}

impl RuntimeHandle {
    /// Requests one deterministic scheduler step and, when needed, awaits its
    /// external effect dispatch and outcome.
    pub fn drive(&self) -> DriverFuture<DriveResult> {
        self.wake()
    }

    /// Submits an equivalent explicit wakeup command.
    pub fn wake(&self) -> DriverFuture<DriveResult> {
        let (reply, future) = response_channel();
        self.submit(DriverCommand::Wake { reply }, future)
    }

    /// Dispatches one already-prepared operation through the owned
    /// [`EffectDispatcher`].
    pub fn dispatch_effect(&self, operation_id: OperationId) -> DriverFuture<DriveResult> {
        let (reply, future) = response_channel();
        self.submit(
            DriverCommand::DispatchEffect {
                operation_id,
                reply,
            },
            future,
        )
    }

    /// Runs the existing synchronous recovery classifier for one operation.
    pub fn recover(&self, operation_id: OperationId) -> DriverFuture<RecoveryDecision> {
        let (reply, future) = response_channel();
        self.submit(
            DriverCommand::Recover {
                operation_id,
                reply,
            },
            future,
        )
    }

    /// Records a durable cancellation through the owned runtime.
    pub fn cancel_task(&self, task_id: Id) -> DriverFuture<Cancellation> {
        let (reply, future) = response_channel();
        self.submit(DriverCommand::CancelTask { task_id, reply }, future)
    }

    /// Drains lifecycle observations without exposing the runtime owner.
    ///
    /// After a successful shutdown, the final buffered lifecycle observations
    /// remain available through this handle and are returned once.
    pub fn drain_execution_events(&self) -> DriverFuture<Vec<StreamItem<RuntimeEvent>>> {
        let (reply, future) = response_channel();
        self.submit(DriverCommand::DrainExecutionEvents { reply }, future)
    }

    /// Drains coalescible progress observations without exposing the runtime
    /// owner. Final buffered progress observations remain drainable once after
    /// a successful shutdown.
    pub fn drain_progress_events(&self) -> DriverFuture<Vec<KeyedStreamItem<Id, RuntimeEvent>>> {
        let (reply, future) = response_channel();
        self.submit(DriverCommand::DrainProgressEvents { reply }, future)
    }

    /// Drains lossy telemetry observations without exposing the runtime owner.
    /// Final buffered telemetry observations remain drainable once after a
    /// successful shutdown.
    pub fn drain_telemetry_events(&self) -> DriverFuture<Vec<StreamItem<RuntimeEvent>>> {
        let (reply, future) = response_channel();
        self.submit(DriverCommand::DrainTelemetryEvents { reply }, future)
    }

    /// Requests an orderly shutdown and returns its durable-work
    /// classification. A store or backpressure error leaves the driver alive
    /// so the caller can drain/retry; ownership is released only after a
    /// successful response.
    pub fn shutdown(&self) -> DriverFuture<ShutdownStatus> {
        let (reply, future) = response_channel();
        self.submit(DriverCommand::Shutdown { reply }, future)
    }

    fn submit<T>(&self, command: DriverCommand, future: DriverFuture<T>) -> DriverFuture<T> {
        if let Err(command) = self.mailbox.submit(command) {
            self.mailbox.complete_after_close(command);
        }
        future
    }
}

/// Single owner of a synchronous [`Runtime`] and asynchronous effect adapter.
pub struct RuntimeDriver<S, D>
where
    S: DurableStore + Send,
    D: EffectDispatcher,
{
    runtime: Runtime<S>,
    dispatcher: D,
    mailbox: CommandMailbox,
    pending_lifecycle: Option<StreamItem<RuntimeEvent>>,
    _owner_guard: DriverOwnerGuard,
}

impl<S, D> RuntimeDriver<S, D>
where
    S: DurableStore + Send,
    D: EffectDispatcher,
{
    /// Creates a driver and its typed command handle.
    pub fn new(runtime: Runtime<S>, dispatcher: D) -> (Self, RuntimeHandle) {
        let mailbox = CommandMailbox::new();
        (
            Self {
                runtime,
                dispatcher,
                mailbox: mailbox.clone(),
                pending_lifecycle: None,
                _owner_guard: DriverOwnerGuard(mailbox.clone()),
            },
            RuntimeHandle { mailbox },
        )
    }

    /// Runs the owner until one successful shutdown releases the runtime.
    pub async fn run(mut self) -> DriverExit<S> {
        loop {
            let command = self
                .mailbox
                .next()
                .await
                .expect("driver mailbox closes only during successful shutdown");
            match command {
                DriverCommand::Wake { reply } => {
                    reply.complete(self.drive_once().await);
                }
                DriverCommand::DispatchEffect {
                    operation_id,
                    reply,
                } => {
                    reply.complete(self.dispatch_effect_now(operation_id, None).await);
                }
                DriverCommand::Recover {
                    operation_id,
                    reply,
                } => reply.complete(self.recover_now(&operation_id)),
                DriverCommand::CancelTask { task_id, reply } => {
                    reply.complete(self.cancel_now(&task_id));
                }
                DriverCommand::DrainExecutionEvents { reply } => {
                    reply.complete(Ok(self.runtime.drain_execution_events()));
                }
                DriverCommand::DrainProgressEvents { reply } => {
                    reply.complete(Ok(self.runtime.drain_progress_events()));
                }
                DriverCommand::DrainTelemetryEvents { reply } => {
                    reply.complete(Ok(self.runtime.drain_telemetry_events()));
                }
                DriverCommand::Shutdown { reply } => match self.shutdown_now() {
                    Ok(status) => {
                        let execution_events = self.runtime.drain_execution_events();
                        let progress_events = self.runtime.drain_progress_events();
                        let telemetry_events = self.runtime.drain_telemetry_events();
                        reply.complete(Ok(status.clone()));
                        self.mailbox.close_and_reject_pending(
                            execution_events,
                            progress_events,
                            telemetry_events,
                        );
                        return DriverExit {
                            runtime: self.runtime,
                            status,
                        };
                    }
                    Err(error) => reply.complete(Err(error)),
                },
            }
        }
    }

    async fn drive_once(&mut self) -> Result<DriveResult, DriverError> {
        if let Err(error) = self.retry_pending_lifecycle() {
            return self.drive_runtime_error(error);
        }
        let step = match self.runtime.step() {
            Ok(step) => step,
            Err(error) => return self.drive_runtime_error(error),
        };
        match step {
            StepResult::EffectPending {
                task_id: _,
                attempt_id,
                operation_id,
            } => {
                self.dispatch_effect_now(operation_id, Some(attempt_id))
                    .await
            }
            step => Ok(DriveResult::Step(step)),
        }
    }

    async fn dispatch_effect_now(
        &mut self,
        operation_id: OperationId,
        expected_attempt_id: Option<AttemptId>,
    ) -> Result<DriveResult, DriverError> {
        if let Err(error) = self.retry_pending_lifecycle() {
            return Err(self.runtime_error(error));
        }
        let intent = match self.runtime.journal().intent(&operation_id) {
            Ok(intent) => intent.clone(),
            Err(error) => return Err(self.runtime_error(RuntimeError::Journal(error))),
        };
        if let Some(expected) = expected_attempt_id.as_ref() {
            let actual = self
                .runtime
                .attempts()
                .iter()
                .rev()
                .find(|attempt| {
                    attempt.task_id == intent.task_id
                        && attempt.operation_id.as_ref() == Some(&operation_id)
                })
                .map(|attempt| attempt.attempt_id.clone());
            if actual.as_ref() != Some(expected) {
                return Err(DriverError::AttemptLineageMismatch {
                    operation_id,
                    expected: expected.clone(),
                    actual,
                });
            }
        }
        let attempt_id = match self.runtime.dispatch_effect(&operation_id) {
            Ok(attempt_id) => attempt_id,
            Err(error) => return Err(self.runtime_error(error)),
        };
        if let Some(expected) = expected_attempt_id {
            if expected != attempt_id {
                return Err(DriverError::AttemptLineageMismatch {
                    operation_id,
                    expected,
                    actual: Some(attempt_id),
                });
            }
        }
        let request = EffectDispatchRequest {
            run_id: self.runtime.run_id().clone(),
            task_id: intent.task_id,
            operation_id: operation_id.clone(),
            attempt_id,
            semantics: intent.semantics,
        };
        match self.dispatcher.dispatch(request.clone()).await {
            Ok(outcome) => {
                if let Err(error) = self.runtime.record_effect_outcome(
                    &operation_id,
                    request.attempt_id.clone(),
                    outcome,
                ) {
                    return Err(self.runtime_error(error));
                }
                Ok(DriveResult::EffectCompleted { request, outcome })
            }
            Err(error) => Ok(DriveResult::EffectUnknown { request, error }),
        }
    }

    fn recover_now(&mut self, operation_id: &OperationId) -> Result<RecoveryDecision, DriverError> {
        if let Err(error) = self.retry_pending_lifecycle() {
            return Err(self.runtime_error(error));
        }
        self.runtime
            .recover(operation_id)
            .map_err(|error| self.runtime_error(error))
    }

    fn cancel_now(&mut self, task_id: &Id) -> Result<Cancellation, DriverError> {
        self.runtime
            .cancel_task(task_id)
            .map_err(|error| self.runtime_error(error))
    }

    fn retry_pending_lifecycle(&mut self) -> Result<(), RuntimeError> {
        let Some(item) = self.pending_lifecycle.take() else {
            return Ok(());
        };
        let retry_copy = item.clone();
        match self.runtime.retry_execution_event(item) {
            Ok(()) => Ok(()),
            Err(RuntimeError::ExecutionBackpressure { item }) => {
                self.pending_lifecycle = Some(item.clone());
                Err(RuntimeError::ExecutionBackpressure { item })
            }
            Err(error) => {
                self.pending_lifecycle = Some(retry_copy);
                Err(error)
            }
        }
    }

    fn drive_runtime_error(&mut self, error: RuntimeError) -> Result<DriveResult, DriverError> {
        match error {
            RuntimeError::ExecutionBackpressure { item } => {
                self.pending_lifecycle = Some(item.clone());
                Ok(DriveResult::Backpressured { item })
            }
            error => Err(DriverError::Runtime(error)),
        }
    }

    fn runtime_error(&mut self, error: RuntimeError) -> DriverError {
        if let RuntimeError::ExecutionBackpressure { item } = &error {
            self.pending_lifecycle = Some(item.clone());
        }
        DriverError::Runtime(error)
    }

    fn shutdown_now(&mut self) -> Result<ShutdownStatus, DriverError> {
        if let Err(error) = self.retry_pending_lifecycle() {
            return Err(self.runtime_error(error));
        }
        let state = self
            .runtime
            .durable_state()
            .map_err(|error| self.runtime_error(error))?;
        let known_outcomes = state
            .operation_ids()
            .into_iter()
            .filter(|operation_id| {
                matches!(
                    state.effect_state(operation_id),
                    RecoveredEffectState::OutcomeKnown(_)
                )
            })
            .collect::<Vec<_>>();
        for operation_id in known_outcomes {
            if let Err(error) = self.runtime.recover(&operation_id) {
                return Err(self.runtime_error(error));
            }
        }
        if let Some(item) = &self.pending_lifecycle {
            return Err(
                self.runtime_error(RuntimeError::ExecutionBackpressure { item: item.clone() })
            );
        }
        let state = self
            .runtime
            .durable_state()
            .map_err(|error| self.runtime_error(error))?;
        Ok(classify_shutdown(&state))
    }
}

fn classify_shutdown(state: &DurableRunState) -> ShutdownStatus {
    let mut pending_dispatch = None;
    let mut pending_unknown = None;
    let mut observed_failure = None;
    for operation_id in state.operation_ids() {
        let Some(intent) = state.intent(&operation_id) else {
            continue;
        };
        match state.effect_state(&operation_id) {
            RecoveredEffectState::Prepared => {
                if !state.is_cancelled(&intent.task_id) && pending_dispatch.is_none() {
                    pending_dispatch = Some(operation_id);
                }
            }
            RecoveredEffectState::OutcomeUnknown => {
                if intent.semantics == EffectSemantics::NonIdempotent {
                    return ShutdownStatus::ReconciliationRequired { operation_id };
                }
                if pending_unknown.is_none() {
                    pending_unknown = Some(operation_id);
                }
            }
            RecoveredEffectState::OutcomeKnown(KnownEffectOutcome::Failed) => {
                if observed_failure.is_none() {
                    observed_failure = Some(operation_id);
                }
            }
            RecoveredEffectState::OutcomeKnown(KnownEffectOutcome::Succeeded)
            | RecoveredEffectState::NotPrepared => {}
        }
    }
    if let Some(operation_id) = pending_unknown {
        ShutdownStatus::PendingUnknown { operation_id }
    } else if let Some(operation_id) = pending_dispatch {
        ShutdownStatus::PendingDispatch { operation_id }
    } else if let Some(operation_id) = observed_failure {
        ShutdownStatus::ObservedFailure { operation_id }
    } else {
        ShutdownStatus::Clean
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

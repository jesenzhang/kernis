//! Shared fixtures for the K5 loader acceptance suites.
//!
//! The suites deliberately import every host-facing type from
//! `runtime_loader` alone, which is itself part of the acceptance story: a
//! host declares references, resolves, composes, and activates without
//! naming any internal crate.

#![allow(dead_code, missing_docs)]

use runtime_loader::{
    CapabilityDefinition, CapabilityValue, EffectDispatchFuture, EffectDispatchRequest,
    EffectDispatcher, Id, KnownEffectOutcome, LifecycleHook, ModuleFactoryError, ModuleReference,
    OperationId, PluginDefinition, PluginFactory, PluginLoadContext, PluginRuntime, RunId,
    lifecycle_hook,
};
use std::fs;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn id(value: &str) -> Id {
    Id::new(value).expect("test id is valid")
}

pub fn run_id(value: &str) -> RunId {
    RunId::new(value).expect("test run id is valid")
}

pub fn operation(value: &str) -> OperationId {
    OperationId::new(value).expect("test operation is valid")
}

pub fn reference(value: &str, version: &str) -> ModuleReference {
    ModuleReference::new(value, version).expect("test reference is valid")
}

pub fn value(value: &str) -> CapabilityValue {
    CapabilityValue::from_value(value.to_owned())
}

pub fn provider_definition(capability: &str, identity: &str) -> CapabilityDefinition {
    CapabilityDefinition::new(id(capability), "provider").with_replay_identity(identity)
}

pub fn service_definition(capability: &str, identity: &str) -> CapabilityDefinition {
    CapabilityDefinition::new(id(capability), "service").with_replay_identity(identity)
}

pub fn plugin_factory<F, Fut>(function: F) -> PluginFactory
where
    F: Fn(PluginLoadContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<CapabilityValue, String>> + Send + 'static,
{
    Arc::new(move |context| Box::pin(function(context)))
}

pub fn ok_plugin(
    plugin: &str,
    capability: &str,
    identity: &str,
    published: &str,
) -> Arc<PluginRuntime> {
    let published = published.to_owned();
    PluginRuntime::new(PluginDefinition::new(
        id(plugin),
        service_definition(capability, identity),
        plugin_factory(move |_| {
            let published = published.clone();
            async move { Ok(CapabilityValue::from_value(published)) }
        }),
    ))
}

/// Factory wrapper that adapts an infallible construction closure to the
/// catalog factory signature.
pub fn ok_factory<F>(
    factory: F,
) -> impl Fn() -> Result<runtime_loader::ModuleRegistration, ModuleFactoryError> + Send + Sync + 'static
where
    F: Fn() -> runtime_loader::ModuleRegistration + Send + Sync + 'static,
{
    move || Ok(factory())
}

#[derive(Clone, Default)]
pub struct SuccessDispatcher {
    requests: Arc<Mutex<Vec<EffectDispatchRequest>>>,
}

impl SuccessDispatcher {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn request_count(&self) -> usize {
        self.requests
            .lock()
            .expect("requests lock is healthy")
            .len()
    }
}

impl EffectDispatcher for SuccessDispatcher {
    fn dispatch(&mut self, request: EffectDispatchRequest) -> EffectDispatchFuture {
        self.requests
            .lock()
            .expect("requests lock is healthy")
            .push(request);
        Box::pin(async move { Ok(KnownEffectOutcome::Succeeded) })
    }
}

#[derive(Clone, Default)]
pub struct Recorder {
    events: Arc<Mutex<Vec<String>>>,
}

impl Recorder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn events(&self) -> Vec<String> {
        self.events.lock().expect("events lock is healthy").clone()
    }

    pub fn count(&self, marker: &str) -> usize {
        self.events()
            .iter()
            .filter(|event| event.contains(marker))
            .count()
    }

    pub fn hook(&self, name: &str, fail: Option<&str>) -> LifecycleHook {
        let events = Arc::clone(&self.events);
        let name = name.to_owned();
        let fail = fail.map(str::to_owned);
        lifecycle_hook(move || {
            let events = Arc::clone(&events);
            let name = name.clone();
            let fail = fail.clone();
            async move {
                events.lock().expect("events lock is healthy").push(name);
                match fail {
                    Some(reason) => Err(reason),
                    None => Ok(()),
                }
            }
        })
    }
}

pub struct TempStore {
    directory: PathBuf,
    path: PathBuf,
}

impl TempStore {
    pub fn new(label: &str) -> Self {
        let number = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "kernis-k5-loader-{label}-{}-{number}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).expect("temporary directory creates");
        Self {
            path: directory.join("runtime.redb"),
            directory,
        }
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }
}

impl Drop for TempStore {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

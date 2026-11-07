use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use closure_core::action::Action;
use closure_core::capability::{Capability, CapabilitySet};
use closure_core::error::{DiscordError, ModuleError};
use closure_core::event::Event;
use closure_core::module::{EventContext, Module, ModuleManifest};
use closure_core::subscription::Flow;
use serde_json::Value;
use tokio::runtime::Handle;

use crate::runtime::{Breaker, CallError, HostApi, Pool, ScriptRuntime};
use crate::{Grants, Limits, effective_capabilities};

/// A script with owner grants applied to its manifest.
pub struct ScriptModule {
    manifest: ModuleManifest,
    runtime: Arc<dyn ScriptRuntime>,
    pool: Pool,
    limits: Limits,
}

impl std::fmt::Debug for ScriptModule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScriptModule")
            .field("manifest", &self.manifest)
            .finish_non_exhaustive()
    }
}

impl ScriptModule {
    pub fn new(
        mut manifest: ModuleManifest,
        grants: &Grants,
        runtime: Arc<dyn ScriptRuntime>,
        limits: Limits,
    ) -> Result<Self, String> {
        let limits = limits.bounded(Limits::default()).map_err(str::to_owned)?;
        let effective = effective_capabilities(&manifest.capabilities, grants, &manifest.id);
        let denied: Vec<_> = manifest
            .capabilities
            .iter()
            .filter(|cap| !effective.contains(*cap))
            .collect();
        if !denied.is_empty() {
            tracing::warn!(module = %manifest.id, ?denied, "script capabilities were not granted");
        }
        manifest.capabilities = effective;
        Ok(Self {
            manifest,
            runtime,
            pool: Pool::new(limits.workers, limits.workers)?,
            limits,
        })
    }
}

struct Host {
    ctx: EventContext,
    capabilities: CapabilitySet,
    handle: Handle,
    deadline: Instant,
    output_limit: usize,
    action_limit: usize,
    state: Mutex<HostState>,
    breaker: Arc<Breaker>,
    cancelled: tokio::sync::watch::Receiver<()>,
}

#[derive(Default)]
struct HostState {
    output: usize,
    actions: usize,
    failure: Option<String>,
}

/// What an entry of a list is counted as, in bytes, when the number of entries is bounded.
const LIST_ENTRY_ALLOWANCE: usize = 4096;
/// A list holds at least this many entries, whatever the output limit.
const MIN_LISTED: usize = 16;

impl Host {
    fn failure(&self, message: &str) -> String {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .failure
            .get_or_insert_with(|| message.to_owned())
            .clone()
    }

    fn check(&self) -> Result<(), String> {
        if self.cancelled.has_changed().is_err() || self.breaker.status().disabled {
            return Err("script invocation is no longer active".into());
        }
        if let Some(error) = self.limit_failure() {
            return Err(error);
        }
        if Instant::now() >= self.deadline {
            return Err(self.failure("script deadline exceeded"));
        }
        Ok(())
    }

    fn output(&self, value: Value) -> Result<Value, String> {
        self.check()?;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let size =
            crate::runtime::json_size(&value, self.output_limit.saturating_sub(state.output));
        if size.is_err() {
            state.failure = Some("script output exceeds the limit".into());
            return Err("script output exceeds the limit".into());
        }
        state.output += size?;
        Ok(value)
    }

    fn reserve_action(&self, action: &Action) -> Result<(), String> {
        self.check()?;
        if !self.capabilities.contains(action.capability()) {
            return Err(format!(
                "capability `{}` is not granted to this script",
                action.capability()
            ));
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.actions += 1;
        if state.actions > self.action_limit {
            state.failure = Some("script action count exceeds the limit".into());
            return Err("script action count exceeds the limit".into());
        }
        Ok(())
    }

    fn storage_allowed(&self) -> Result<(), String> {
        self.check()?;
        if self.capabilities.contains(Capability::Storage) {
            Ok(())
        } else {
            Err("capability `storage` is not granted to this script".into())
        }
    }

    fn run<T>(&self, future: impl Future<Output = Result<T, String>>) -> Result<T, String> {
        self.check()?;
        let mut cancelled = self.cancelled.clone();
        self.handle.block_on(async {
            tokio::select! {
                result = tokio::time::timeout_at(tokio::time::Instant::from_std(self.deadline), future) => result.map_err(|_| self.failure("script deadline exceeded"))?,
                _ = cancelled.changed() => Err("script invocation is no longer active".into()),
            }
        })
    }
}

impl HostApi for Host {
    fn execute(&self, value: Value) -> Result<Value, String> {
        let value = self.output(value)?;
        let action: Action = serde_json::from_value(value).map_err(|error| error.to_string())?;
        self.reserve_action(&action)?;
        let outcome = self.run(async {
            self.ctx
                .api
                .execute(action)
                .await
                .map_err(|error| error.to_string())
        })?;
        self.output(serde_json::to_value(outcome).map_err(|error| error.to_string())?)
    }
    fn get(&self, key: &str) -> Result<Value, String> {
        self.storage_allowed()?;
        let value = self.run(async {
            self.ctx
                .store()
                .get::<Value>(key)
                .await
                .map_err(|error| error.to_string())
        })?;
        self.output(value.unwrap_or(Value::Null))
    }
    fn set(&self, key: &str, value: Value) -> Result<(), String> {
        self.storage_allowed()?;
        let value = self.output(value)?;
        self.run(async {
            self.ctx
                .store()
                .set(key, &value)
                .await
                .map_err(|error| error.to_string())
        })
    }
    fn delete(&self, key: &str) -> Result<bool, String> {
        self.storage_allowed()?;
        let deleted = self.run(async {
            self.ctx
                .store()
                .delete(key)
                .await
                .map_err(|error| error.to_string())
        })?;
        self.output(Value::Bool(deleted))?;
        Ok(deleted)
    }
    fn list(&self, prefix: &str, limit: usize) -> Result<Value, String> {
        self.storage_allowed()?;
        // What is read must fit what the script may be given back: an entry holds up to 32 KiB, so
        // a list longer than the output allows is not built to be refused afterwards.
        let limit = limit.min((self.output_limit / LIST_ENTRY_ALLOWANCE).max(MIN_LISTED));
        let entries = self.run(async {
            self.ctx
                .store()
                .list(prefix, limit)
                .await
                .map_err(|error| error.to_string())
        })?;
        self.output(Value::Array(entries.into_iter().map(|(key, entry)| serde_json::json!({"key": key, "value": entry.value, "revision": entry.revision})).collect()))
    }
    fn limit_failure(&self) -> Option<String> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .failure
            .clone()
    }
}

#[async_trait]
impl Module for ScriptModule {
    fn manifest(&self) -> &ModuleManifest {
        &self.manifest
    }

    async fn on_event(&self, ctx: &EventContext, event: &Event) -> Result<(), ModuleError> {
        self.on_event_flow(ctx, event).await.map(drop)
    }

    async fn on_event_flow(&self, ctx: &EventContext, event: &Event) -> Result<Flow, ModuleError> {
        if ctx.module != self.manifest.id || event.guild_id() != ctx.guild_id {
            return Err(ModuleError::other(
                "script context does not match its module or guild",
            ));
        }
        let input =
            serde_json::to_value(event).map_err(|error| ModuleError::other(error.to_string()))?;
        let (active, cancelled) = tokio::sync::watch::channel(());
        let mut scoped_ctx = ctx.clone();
        scoped_ctx.api = scoped_ctx.api.scoped_to(ctx.guild_id);
        let host = Arc::new(Host {
            ctx: scoped_ctx,
            capabilities: self.manifest.capabilities.clone(),
            handle: Handle::current(),
            deadline: Instant::now() + self.limits.wall_time(),
            output_limit: self.limits.output_bytes,
            action_limit: self.limits.actions,
            state: Mutex::new(HostState::default()),
            breaker: self.pool.breaker(),
            cancelled,
        });
        let result = self
            .pool
            .call(Arc::clone(&self.runtime), host.clone(), input, self.limits)
            .await
            .map_err(|error| match error {
                CallError::Busy => {
                    ModuleError::Discord(DiscordError::Transient("script queue is full".into()))
                }
                CallError::Failed(error) => ModuleError::other(error),
            })?;
        if let Err(error) = host.output(result.clone()) {
            self.pool.record_failure();
            return Err(ModuleError::other(error));
        }
        // A scheduled job whose action failed for a reason that passes is tried again, so the
        // failure is not reported as a success; any other failure is only logged.
        let mut retry: Option<DiscordError> = None;
        if let Some(actions) = result.as_array() {
            for value in actions {
                let result = match serde_json::from_value::<Action>(value.clone()) {
                    Ok(action) => match host.reserve_action(&action) {
                        Ok(()) => match tokio::time::timeout_at(
                            tokio::time::Instant::from_std(host.deadline),
                            host.ctx.api.execute(action),
                        )
                        .await
                        {
                            Ok(Ok(_)) => Ok(()),
                            Ok(Err(error)) => {
                                if error.is_retryable() && retry.is_none() {
                                    retry = Some(error.clone());
                                }
                                Err(error.to_string())
                            }
                            Err(_) => Err(host.failure("script deadline exceeded")),
                        },
                        Err(error) => Err(error),
                    },
                    Err(error) => Err(error.to_string()),
                };
                if let Err(error) = result {
                    tracing::error!(module = %self.manifest.id, %error, "script action failed");
                }
            }
        }
        if let (Some(error), Event::Scheduled(_)) = (retry, event) {
            return Err(error.into());
        }
        if let Some(error) = host.limit_failure() {
            self.pool.record_failure();
            return Err(ModuleError::other(error));
        }
        drop(active);
        Ok(Flow::Continue)
    }
}

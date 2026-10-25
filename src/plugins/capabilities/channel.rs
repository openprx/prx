//! A bounded polling channel; transport permissions remain bound to operator trust.
use super::recovery::{CallOutcome, CapabilityKind, GuestOp, Respawn, Supervisor};
use crate::plugins::{
    error::{PluginError, PluginResult},
    host::HostState,
    manifest::PluginManifest,
};
use std::{collections::HashSet, sync::Arc};
use wasmtime::AsContextMut;

const INTERFACES: [&str; 2] = ["prx:plugin/channel-exports@0.1.0", "prx:plugin/channel-exports"];
pub struct WasmChannel {
    name: String,
    supervisor: Arc<Supervisor>,
    allowed_users: HashSet<String>,
}

struct ChannelCall {
    operation: &'static str,
    payload: String,
}
impl GuestOp for ChannelCall {
    type Output = String;
    fn operation(&self) -> &'static str {
        self.operation
    }
    fn invoke<'a>(
        self,
        store: &'a mut wasmtime::Store<HostState>,
        instance: &'a wasmtime::component::Instance,
    ) -> futures_util::future::BoxFuture<'a, CallOutcome<String>>
    where
        Self: 'a,
    {
        Box::pin(async move {
            let function = INTERFACES.iter().find_map(|name| {
                let interface = instance.get_export_index(store.as_context_mut(), None, name)?;
                let export = instance.get_export_index(store.as_context_mut(), Some(&interface), self.operation)?;
                instance.get_func(store.as_context_mut(), &export)
            });
            let Some(function) = function else {
                return CallOutcome::GuestError(PluginError::Runtime("missing channel export".into()));
            };
            let mut result = [wasmtime::component::Val::Bool(false)];
            if let Err(error) = function
                .call_async(
                    store.as_context_mut(),
                    &[wasmtime::component::Val::String(self.payload)],
                    &mut result,
                )
                .await
            {
                return super::recovery::unwound(store, &error, || {
                    format!("channel {} failed: {error}", self.operation)
                });
            }
            let parsed = match result.first() {
                Some(wasmtime::component::Val::Result(value)) => match value.as_ref() {
                    Ok(Some(value)) => match value.as_ref() {
                        wasmtime::component::Val::String(value) => Ok(value.clone()),
                        _ => Err(PluginError::Runtime("channel result must contain a string".into())),
                    },
                    Err(Some(value)) => Err(PluginError::Runtime(format!("channel returned error: {value:?}"))),
                    _ => Err(PluginError::Runtime("channel result is missing".into())),
                },
                _ => Err(PluginError::Runtime("channel result has an invalid shape".into())),
            };
            match parsed {
                Ok(value) => CallOutcome::Completed(value),
                Err(error) => CallOutcome::GuestError(error),
            }
        })
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Inbound {
    id: String,
    sender: String,
    reply_target: String,
    content: String,
    #[serde(default)]
    is_group: bool,
    #[serde(default)]
    thread_ts: Option<String>,
}

impl WasmChannel {
    pub async fn new(
        engine: &wasmtime::Engine,
        component: &wasmtime::component::Component,
        manifest: &PluginManifest,
        permissions: HashSet<String>,
        events: Option<Arc<crate::plugins::event_bus::EventBus>>,
    ) -> PluginResult<Self> {
        if manifest.capabilities_of_type("channel").len() != 1 {
            return Err(PluginError::Manifest(
                "a plugin must declare exactly one channel capability".into(),
            ));
        }
        let allowed_users = manifest
            .config
            .get("allowed_users")
            .map(|value| {
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let respawn = Respawn::new(
            CapabilityKind::Channel,
            engine,
            component,
            manifest,
            permissions,
            None,
            events,
            super::common::register_common_host_functions,
        );
        let mut live = respawn.instantiate().await?;
        for operation in ["send", "poll"] {
            let found = INTERFACES.iter().any(|name| {
                live.instance
                    .get_export_index(live.store.as_context_mut(), None, name)
                    .and_then(|interface| {
                        live.instance
                            .get_export_index(live.store.as_context_mut(), Some(&interface), operation)
                    })
                    .is_some()
            });
            if !found {
                return Err(PluginError::Instantiation(format!(
                    "channel export '{operation}' is missing"
                )));
            }
        }
        Ok(Self {
            name: format!("wasm:{}", manifest.plugin.name),
            allowed_users,
            supervisor: Arc::new(Supervisor::new(respawn, live).with_bounded_lock_wait()),
        })
    }
    pub fn diagnostics(&self) -> serde_json::Value {
        self.supervisor
            .diagnostics(serde_json::json!({"channel":self.name,"plugin":self.supervisor.plugin_name()}))
    }
    pub fn is_quarantined(&self) -> bool {
        self.supervisor.is_quarantined()
    }

    pub fn name(&self) -> &str {
        &self.name
    }
    pub async fn send(&self, message: &crate::channels::traits::SendMessage) -> anyhow::Result<()> {
        anyhow::ensure!(!message.recipient.trim().is_empty(), "channel recipient is empty");
        let payload = serde_json::json!({"content":message.content, "recipient":message.recipient,
            "subject":message.subject, "thread_ts":message.thread_ts, "quote_timestamp":message.quote_timestamp,
            "quote_author":message.quote_author})
        .to_string();
        anyhow::ensure!(
            self.supervisor.admit_payload("send", payload.len()),
            "channel payload exceeds its cap"
        );
        let reply = self
            .supervisor
            .call_detached(ChannelCall {
                operation: "send",
                payload,
            })
            .await?;
        anyhow::ensure!(
            reply.len() <= self.supervisor.max_payload_bytes(),
            "channel reply exceeds its cap"
        );
        Ok(())
    }
    pub async fn poll(&self) -> anyhow::Result<Vec<crate::channels::traits::ChannelMessage>> {
        let reply = self
            .supervisor
            .call_detached(ChannelCall {
                operation: "poll",
                payload: "{}".into(),
            })
            .await?;
        self.parse_messages(&reply)
    }
    fn parse_messages(&self, reply: &str) -> anyhow::Result<Vec<crate::channels::traits::ChannelMessage>> {
        anyhow::ensure!(
            reply.len() <= self.supervisor.max_payload_bytes(),
            "channel batch exceeds its cap"
        );
        let rows: Vec<Inbound> = serde_json::from_str(reply)?;
        anyhow::ensure!(rows.len() <= 32, "channel batch exceeds 32 messages");
        let mut messages = Vec::new();
        for row in rows {
            anyhow::ensure!(
                !row.id.trim().is_empty()
                    && row.id.len() <= 256
                    && !row.sender.trim().is_empty()
                    && row.sender.len() <= 256
                    && !row.reply_target.trim().is_empty()
                    && row.reply_target.len() <= 256
                    && row.content.len() <= 65536,
                "invalid channel message bounds"
            );
            if !(self.allowed_users.contains("*") || self.allowed_users.contains(&row.sender)) {
                continue;
            }
            messages.push(crate::channels::traits::ChannelMessage {
                id: row.id,
                sender: row.sender,
                reply_target: row.reply_target,
                content: row.content,
                channel: self.name.clone(),
                timestamp: chrono::Utc::now().timestamp().try_into().unwrap_or(0),
                chat_kind: if row.is_group {
                    crate::channels::traits::ChatKind::Group
                } else {
                    crate::channels::traits::ChatKind::Dm
                },
                is_group_hint: row.is_group,
                thread_ts: row.thread_ts,
                ..Default::default()
            });
        }
        Ok(messages)
    }
}

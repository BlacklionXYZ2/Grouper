//! Model providers reached through the outbound connection of a hosting device.

use std::collections::HashSet;
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use core_protocol::{HubMessage, ProviderConfig};
use providers::{Delta, OnDelta, Provider, ProviderError, Request, Response};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::registry::Route;
use crate::server::{Hub, QueueResult};

const MODEL_TIMEOUT: Duration = Duration::from_secs(600);
const MODEL_REPLY_CAP: usize = 64;
const MAX_REQUESTER_CALLS: usize = 8;
const MAX_REQUEST_IDS_PER_CONNECTION: usize = 65_536;
const MAX_HOST_CALLS: usize = 16;
const MAX_MODEL_DELTA_BYTES: usize = 64 * 1024;
const MAX_MODEL_STREAM_BYTES: usize = 8 * 1024 * 1024;

pub(crate) enum ModelReply {
    Delta(Value),
    Finished {
        response: Option<Value>,
        error: Option<String>,
    },
}

pub(crate) enum ModelCaller {
    Local(mpsc::Sender<ModelReply>),
    Device {
        device_id: String,
        conn_id: u64,
        call_id: String,
        thread_id: String,
        provider_id: String,
        model: String,
    },
}

pub(crate) struct ModelRoute {
    pub(crate) host_id: String,
    pub(crate) host_conn: u64,
    pub(crate) caller: ModelCaller,
    pub(crate) streamed_bytes: usize,
}

struct CancelOnDrop {
    hub: Weak<Hub>,
    call_id: Option<String>,
}

impl CancelOnDrop {
    fn disarm(&mut self) {
        self.call_id = None;
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        let (Some(hub), Some(call_id)) = (self.hub.upgrade(), self.call_id.take()) else {
            return;
        };
        tokio::spawn(async move { hub.cancel_local_model_call(&call_id).await });
    }
}

struct ViaDevice {
    hub: Weak<Hub>,
    provider_id: String,
    device_id: String,
    device_name: String,
}

#[async_trait]
impl Provider for ViaDevice {
    async fn complete(&self, request: &Request) -> Result<Response, ProviderError> {
        self.complete_stream(request, &|_| {}).await
    }

    async fn complete_stream(
        &self,
        request: &Request,
        on_delta: OnDelta<'_>,
    ) -> Result<Response, ProviderError> {
        let Some(hub) = self.hub.upgrade() else {
            return Err(ProviderError::Unavailable(
                "the hub stopped while making the model call".into(),
            ));
        };
        hub.complete_on_device(
            &self.provider_id,
            &self.device_id,
            &self.device_name,
            request,
            on_delta,
        )
        .await
    }
}

impl Hub {
    /// Build a provider for work running on the hub. Device-hosted providers use
    /// [`ViaDevice`]; ordinary providers keep their existing direct path.
    pub(crate) fn build_provider(
        self: &Arc<Self>,
        config: &ProviderConfig,
    ) -> Result<Arc<dyn Provider>, ProviderError> {
        let Some(host) = config.via_device() else {
            return providers::build(config);
        };
        let (device_id, device_name) = self.resolve_device(host).ok_or_else(|| {
            ProviderError::Config(format!(
                "provider {} names unknown hosting device {host:?}",
                config.id
            ))
        })?;
        Ok(Arc::new(ViaDevice {
            hub: Arc::downgrade(self),
            provider_id: config.id.clone(),
            device_id,
            device_name,
        }))
    }

    async fn complete_on_device(
        self: &Arc<Self>,
        provider_id: &str,
        device_id: &str,
        device_name: &str,
        request: &Request,
        on_delta: OnDelta<'_>,
    ) -> Result<Response, ProviderError> {
        let request = serde_json::to_value(request).map_err(|e| ProviderError::BadResponse {
            provider: provider_id.into(),
            message: e.to_string(),
        })?;
        let call_id = format!(
            "m{}",
            self.next_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let (reply, mut replies) = mpsc::channel(MODEL_REPLY_CAP);
        let host_conn = {
            let live = self.live.lock().await;
            let Some(host) = live.get(device_id) else {
                return Err(ProviderError::Unavailable(format!(
                    "the model lives on {device_name}, which is offline"
                )));
            };
            host.conn_id
        };
        {
            let mut calls = self.model_calls.lock().await;
            if calls.values().filter(|route| route.host_id == device_id && route.host_conn == host_conn).count()
                >= MAX_HOST_CALLS
            {
                return Err(ProviderError::Unavailable(format!(
                    "the model host {device_name} is busy; try again later"
                )));
            }
            calls.insert(
                call_id.clone(),
                ModelRoute {
                    host_id: device_id.into(),
                    host_conn,
                    caller: ModelCaller::Local(reply),
                    streamed_bytes: 0,
                },
            );
        }
        let queued = self
            .queue_to_model_host(
                device_id,
                host_conn,
                HubMessage::ModelRequest {
                    call_id: call_id.clone(),
                    provider_id: provider_id.into(),
                    request,
                },
            )
            .await;
        if queued != QueueResult::Sent {
            self.model_calls.lock().await.remove(&call_id);
            return Err(ProviderError::Unavailable(if queued == QueueResult::Full {
                format!("the model host {device_name} is busy; try again later")
            } else {
                format!("the model lives on {device_name}, which is offline")
            }));
        }
        let mut cancel = CancelOnDrop {
            hub: Arc::downgrade(self),
            call_id: Some(call_id.clone()),
        };
        let result = tokio::time::timeout(MODEL_TIMEOUT, async {
            while let Some(reply) = replies.recv().await {
                match reply {
                    ModelReply::Delta(value) => {
                        let delta: Delta = serde_json::from_value(value).map_err(|e| {
                            ProviderError::BadResponse {
                                provider: provider_id.into(),
                                message: format!("bad delta from {device_name}: {e}"),
                            }
                        })?;
                        on_delta(delta);
                    }
                    ModelReply::Finished {
                        response: Some(value),
                        error: None,
                    } => {
                        return serde_json::from_value(value).map_err(|e| {
                            ProviderError::BadResponse {
                                provider: provider_id.into(),
                                message: format!("bad response from {device_name}: {e}"),
                            }
                        });
                    }
                    ModelReply::Finished {
                        error: Some(error), ..
                    } => {
                        return Err(ProviderError::Unavailable(error));
                    }
                    ModelReply::Finished { .. } => {
                        return Err(ProviderError::BadResponse {
                            provider: provider_id.into(),
                            message: format!("empty response from {device_name}"),
                        });
                    }
                }
            }
            Err(ProviderError::Unavailable(format!(
                "the model connection to {device_name} closed"
            )))
        })
        .await;
        let still_pending = self.model_calls.lock().await.contains_key(&call_id);
        if !still_pending {
            cancel.disarm();
        }
        match result {
            Ok(result) => result,
            Err(_) => Err(ProviderError::Unavailable(format!(
                "model call on {device_name} timed out"
            ))),
        }
    }

    pub(crate) async fn cancel_local_model_call(&self, call_id: &str) {
        let route = self.model_calls.lock().await.remove(call_id);
        if let Some(route) = route {
            self.queue_to_model_host(
                &route.host_id,
                route.host_conn,
                HubMessage::ModelCancel {
                    call_id: call_id.into(),
                },
            )
            .await;
        }
    }

    /// A non-host device asks the hub to relay a call to the configured host.
    pub(crate) async fn relay_model_request(
        self: &Arc<Self>,
        requester_id: &str,
        requester_conn: u64,
        requester_call: String,
        thread_id: String,
        provider_id: String,
        request: Value,
    ) {
        let fail = |message: String| HubMessage::ModelResponse {
            call_id: requester_call.clone(),
            response: None,
            error: Some(message),
        };
        let Some(thread) = self.db.thread(&thread_id) else {
            self.send_to_conn(requester_id, requester_conn, fail("model calls require a running owned thread".into())).await;
            return;
        };
        {
            let key = (requester_id.to_string(), requester_conn);
            let mut seen = self.model_request_ids.lock().await;
            let ids = seen.entry(key).or_default();
            if ids.contains(&requester_call) {
                drop(seen);
                self.send_to_conn(requester_id, requester_conn, fail("duplicate model call id".into())).await;
                return;
            }
            if ids.len() >= MAX_REQUEST_IDS_PER_CONNECTION {
                drop(seen);
                self.send_to_conn(requester_id, requester_conn, fail("too many model call ids on this connection".into())).await;
                return;
            }
            ids.insert(requester_call.clone());
        }
        if thread.device_id != requester_id || thread.status != "running" {
            self.send_to_conn(requester_id, requester_conn, fail("model calls require a running thread owned by this device".into())).await;
            return;
        }
        let Some(chosen_model) = self.db.find_model(&thread.model) else {
            self.send_to_conn(requester_id, requester_conn, fail(format!("thread {} has unknown model {}", thread.id, thread.model))).await;
            return;
        };
        let request_model = serde_json::from_value::<Request>(request.clone()).map(|r| r.model);
        if chosen_model.provider_id != provider_id || request_model.ok().as_deref() != Some(chosen_model.model.as_str()) {
            self.send_to_conn(requester_id, requester_conn, fail("model request does not match the thread's chosen provider and model".into())).await;
            return;
        }
        if let Some(why) = crate::limits::relayed_model_refusal(&self.db, &thread) {
            self.send_to_conn(requester_id, requester_conn, fail(why)).await;
            return;
        }
        let config = match self.db.provider(&provider_id) {
            Ok(Some(config)) => config,
            Ok(None) => {
                self.send_to_conn(
                    requester_id,
                    requester_conn,
                    fail(format!("no provider {provider_id}")),
                )
                .await;
                return;
            }
            Err(error) => {
                self.send_to_conn(requester_id, requester_conn, fail(error.to_string()))
                    .await;
                return;
            }
        };
        let Some(host_key) = config.via_device() else {
            self.send_to_conn(
                requester_id,
                requester_conn,
                fail(format!("provider {provider_id} is not hosted by a device")),
            )
            .await;
            return;
        };
        let Some((host_id, host_name)) = self.resolve_device(host_key) else {
            self.send_to_conn(
                requester_id,
                requester_conn,
                fail(format!(
                    "provider {provider_id} names unknown hosting device {host_key:?}"
                )),
            )
            .await;
            return;
        };
        if host_id == requester_id {
            self.send_to_conn(
                requester_id,
                requester_conn,
                fail(format!(
                    "provider {provider_id} must be called directly on its hosting device"
                )),
            )
            .await;
            return;
        }
        let host_conn = {
            let live = self.live.lock().await;
            live.get(&host_id).map(|host| host.conn_id)
        };
        let Some(host_conn) = host_conn else {
            self.send_to_conn(
                requester_id,
                requester_conn,
                fail(format!("the model lives on {host_name}, which is offline")),
            )
            .await;
            return;
        };
        let call_id = format!(
            "r{}",
            self.next_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        {
            let mut calls = self.model_calls.lock().await;
            let duplicate = calls.values().any(|route| matches!(
                &route.caller,
                ModelCaller::Device { device_id, conn_id, call_id, .. }
                    if device_id == requester_id && *conn_id == requester_conn && call_id == &requester_call
            ));
            if duplicate {
                drop(calls);
                self.send_to_conn(requester_id, requester_conn, fail("duplicate model call id".into())).await;
                return;
            }
            let requester_calls = calls.values().filter(|route| matches!(
                &route.caller,
                ModelCaller::Device { device_id, .. } if device_id == requester_id
            )).count();
            let host_calls = calls.values().filter(|route| route.host_id == host_id && route.host_conn == host_conn).count();
            if requester_calls >= MAX_REQUESTER_CALLS || host_calls >= MAX_HOST_CALLS {
                drop(calls);
                self.send_to_conn(requester_id, requester_conn, fail("model relay is busy; try again later".into())).await;
                return;
            }
            calls.insert(
                call_id.clone(),
                ModelRoute {
                    host_id: host_id.clone(),
                    host_conn,
                    caller: ModelCaller::Device {
                        device_id: requester_id.into(),
                        conn_id: requester_conn,
                        call_id: requester_call.clone(),
                        thread_id: thread.id,
                        provider_id: chosen_model.provider_id,
                        model: chosen_model.model,
                    },
                    streamed_bytes: 0,
                },
            );
        }
        let queued = self
            .queue_to_model_host(
                &host_id,
                host_conn,
                HubMessage::ModelRequest {
                    call_id: call_id.clone(),
                    provider_id,
                    request,
                },
            )
            .await;
        if queued != QueueResult::Sent {
            self.model_calls.lock().await.remove(&call_id);
            self.send_to_conn(
                requester_id,
                requester_conn,
                fail(if queued == QueueResult::Full {
                    "model host is busy; try again later".into()
                } else {
                    format!("the model lives on {host_name}, which is offline")
                }),
            )
            .await;
            return;
        }
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            tokio::time::sleep(MODEL_TIMEOUT).await;
            if let Some(hub) = weak.upgrade() {
                hub.timeout_device_model_call(&call_id).await;
            }
        });
    }

    pub(crate) async fn relay_model_cancel(
        &self,
        requester_id: &str,
        requester_conn: u64,
        requester_call: &str,
    ) {
        let route_id = {
            let calls = self.model_calls.lock().await;
            calls.iter().find_map(|(id, route)| match &route.caller {
                ModelCaller::Device {
                    device_id,
                    conn_id,
                    call_id,
                    ..
                } if device_id == requester_id
                    && *conn_id == requester_conn
                    && call_id == requester_call =>
                {
                    Some(id.clone())
                }
                _ => None,
            })
        };
        let Some(route_id) = route_id else { return };
        if let Some(route) = self.model_calls.lock().await.remove(&route_id) {
            self.queue_to_model_host(
                &route.host_id,
                route.host_conn,
                HubMessage::ModelCancel { call_id: route_id },
            )
            .await;
        }
    }

    async fn timeout_device_model_call(&self, route_id: &str) {
        let route = self.model_calls.lock().await.remove(route_id);
        let Some(ModelRoute {
            host_id,
            host_conn,
            caller: ModelCaller::Device { device_id, conn_id, call_id, .. },
            ..
        }) = route else {
            return;
        };
        self.queue_to_model_host(&host_id, host_conn, HubMessage::ModelCancel { call_id: route_id.into() }).await;
        self.send_to_conn(
            &device_id,
            conn_id,
            HubMessage::ModelResponse {
                call_id,
                response: None,
                error: Some("model call timed out".into()),
            },
        )
        .await;
    }

    pub(crate) async fn model_delta(
        &self,
        host_id: &str,
        host_conn: u64,
        call_id: &str,
        delta: Value,
    ) {
        let bytes = serde_json::to_vec(&delta).map_or(MAX_MODEL_DELTA_BYTES + 1, |v| v.len());
        let (target, violation) = {
            let mut calls = self.model_calls.lock().await;
            let valid = calls.get(call_id).is_some_and(|r| r.host_id == host_id && r.host_conn == host_conn);
            if !valid {
                return;
            }
            let route = calls.get_mut(call_id).expect("validated route");
            route.streamed_bytes = route.streamed_bytes.saturating_add(bytes);
            let violation = bytes > MAX_MODEL_DELTA_BYTES || route.streamed_bytes > MAX_MODEL_STREAM_BYTES;
            let route = if violation { calls.remove(call_id) } else { None };
            let target = route.as_ref().map(|r| target_of(&r.caller)).or_else(|| calls.get(call_id).map(|r| target_of(&r.caller)));
            (target, violation)
        };
        if violation {
            self.queue_to_model_host(host_id, host_conn, HubMessage::ModelCancel { call_id: call_id.into() }).await;
            if let Some(target) = target {
                self.fail_target(target, "model stream exceeded the 64 KB delta or 8 MB call limit").await;
            }
            return;
        }
        match target {
            Some(EitherTarget::Local(tx)) => {
                if let Err(mpsc::error::TrySendError::Full(_)) = tx.try_send(ModelReply::Delta(delta)) {
                    self.abort_model_route(call_id, "model reply queue is full").await;
                }
            }
            Some(EitherTarget::Device {
                device_id,
                conn_id,
                call_id,
            }) => {
                self.send_to_conn(
                    &device_id,
                    conn_id,
                    HubMessage::ModelDelta { call_id, delta },
                )
                .await;
            }
            None => {}
        }
    }

    pub(crate) async fn model_response(
        self: &Arc<Self>,
        host_id: &str,
        host_conn: u64,
        call_id: &str,
        response: Option<Value>,
        error: Option<String>,
    ) {
        let route = {
            let mut calls = self.model_calls.lock().await;
            if calls
                .get(call_id)
                .is_some_and(|r| r.host_id == host_id && r.host_conn == host_conn)
            {
                calls.remove(call_id)
            } else {
                None
            }
        };
        match route.map(|r| r.caller) {
            Some(ModelCaller::Local(tx)) => {
                let _ = tx.try_send(ModelReply::Finished { response, error });
            }
            Some(ModelCaller::Device {
                device_id,
                conn_id,
                call_id,
                thread_id,
                provider_id,
                model,
            }) => {
                if error.is_none()
                    && let Some(value) = response.as_ref()
                    && let Ok(model_response) = serde_json::from_value::<Response>(value.clone())
                {
                    let usage = model_response.usage;
                    let thread = self.db.thread(&thread_id);
                    let conversation_id = thread.as_ref().map(|t| t.conversation_id.as_str());
                    let cost = self.db.record_usage(
                        conversation_id,
                        Some(&thread_id),
                        &provider_id,
                        &model,
                        usage.input_tokens,
                        usage.output_tokens,
                        usage.cache_read_tokens,
                        usage.cache_write_tokens,
                    );
                    self.relayed_usage
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .entry(thread_id.clone())
                        .or_default()
                        .push(usage);
                    if let Some(thread) = thread.as_ref() {
                        crate::limits::after_usage(self, &thread.conversation_id, Some(thread), cost);
                    } else {
                        crate::limits::after_daily_usage(self, cost);
                    }
                }
                self.send_to_conn(
                    &device_id,
                    conn_id,
                    HubMessage::ModelResponse {
                        call_id,
                        response,
                        error,
                    },
                )
                .await;
            }
            None => {}
        }
    }

    /// End or cancel every model route tied to one WebSocket connection.
    pub(crate) async fn model_connection_gone(&self, device_id: &str, conn_id: u64) {
        self.model_request_ids.lock().await.remove(&(device_id.to_string(), conn_id));
        let removed: Vec<(String, ModelRoute)> = {
            let mut calls = self.model_calls.lock().await;
            let ids: Vec<String> = calls
                .iter()
                .filter(|(_, route)| {
                    (route.host_id == device_id && route.host_conn == conn_id)
                        || matches!(&route.caller, ModelCaller::Device { device_id: id, conn_id: c, .. }
                            if id == device_id && *c == conn_id)
                })
                .map(|(id, _)| id.clone())
                .collect();
            ids.into_iter()
                .filter_map(|id| calls.remove(&id).map(|route| (id, route)))
                .collect()
        };
        for (id, route) in removed {
            let host_gone = route.host_id == device_id && route.host_conn == conn_id;
            if !host_gone {
                self.queue_to_model_host(
                    &route.host_id,
                    route.host_conn,
                    HubMessage::ModelCancel { call_id: id },
                )
                .await;
                continue;
            }
            let host_name = self
                .resolve_device(device_id)
                .map(|(_, name)| name)
                .unwrap_or_else(|| device_id.into());
            let error = format!("the model lives on {host_name}, which is offline");
            match route.caller {
                ModelCaller::Local(tx) => {
                    let _ = tx.try_send(ModelReply::Finished {
                        response: None,
                        error: Some(error),
                    });
                }
                    ModelCaller::Device { device_id, conn_id, call_id, .. } => {
                    self.send_to_conn(
                        &device_id,
                        conn_id,
                        HubMessage::ModelResponse {
                            call_id,
                            response: None,
                            error: Some(error),
                        },
                    )
                    .await;
                }
            }
        }
    }

    async fn abort_model_route(&self, call_id: &str, message: &str) {
        let route = self.model_calls.lock().await.remove(call_id);
        let Some(route) = route else { return };
        self.queue_to_model_host(&route.host_id, route.host_conn, HubMessage::ModelCancel { call_id: call_id.into() }).await;
        self.fail_target(target_of(&route.caller), message).await;
    }

    async fn fail_target(&self, target: EitherTarget, message: &str) {
        match target {
            EitherTarget::Local(tx) => {
                let _ = tx.try_send(ModelReply::Finished { response: None, error: Some(message.into()) });
            }
            EitherTarget::Device { device_id, conn_id, call_id } => {
                self.send_to_conn(
                    &device_id,
                    conn_id,
                    HubMessage::ModelResponse { call_id, response: None, error: Some(message.into()) },
                )
                .await;
            }
        }
    }

    pub(crate) fn consume_relayed_usage(&self, thread_id: &str, usage: providers::Usage) -> bool {
        let mut pending = self.relayed_usage.lock().unwrap_or_else(|e| e.into_inner());
        let Some(rows) = pending.get_mut(thread_id) else { return false };
        let Some(index) = rows.iter().position(|candidate| *candidate == usage) else { return false };
        rows.remove(index);
        if rows.is_empty() {
            pending.remove(thread_id);
        }
        true
    }

    /// Providers hosted on an offline device are unavailable for automatic
    /// selection. Explicitly pinned/named models are retained so their call emits
    /// the precise offline error instead of silently switching models.
    pub(crate) async fn unavailable_device_providers(&self) -> HashSet<String> {
        let online: HashSet<String> = self.live.lock().await.keys().cloned().collect();
        self.db
            .providers()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|provider| {
                let host = provider.via_device()?;
                let unavailable = self
                    .resolve_device(host)
                    .is_none_or(|(id, _)| !online.contains(&id));
                unavailable.then_some(provider.id)
            })
            .collect()
    }

    pub(crate) async fn route_model(
        &self,
        requested: Option<&str>,
        task_type: Option<&str>,
        tags: &[String],
        prefer_cheaper: bool,
    ) -> Result<Route, String> {
        let unavailable = self.unavailable_device_providers().await;
        self.db
            .route_model_available(requested, task_type, tags, prefer_cheaper, &unavailable)
    }

    pub(crate) async fn default_model(&self, role: &str) -> Option<core_protocol::ModelEntry> {
        let unavailable = self.unavailable_device_providers().await;
        self.db.default_model_available(role, &unavailable)
    }
}

enum EitherTarget {
    Local(mpsc::Sender<ModelReply>),
    Device {
        device_id: String,
        conn_id: u64,
        call_id: String,
    },
}

fn target_of(caller: &ModelCaller) -> EitherTarget {
    match caller {
        ModelCaller::Local(tx) => EitherTarget::Local(tx.clone()),
        ModelCaller::Device { device_id, conn_id, call_id, .. } => EitherTarget::Device {
            device_id: device_id.clone(),
            conn_id: *conn_id,
            call_id: call_id.clone(),
        },
    }
}

#[cfg(test)]
mod tests {
    use core_protocol::{HubMessage, ModelEntry, ProviderConfig};
    use providers::{Block, Message, Request, Response, Role, StopReason, Usage};
    use serde_json::json;
    use tokio::sync::{mpsc, watch};

    use super::*;
    use crate::db::{Db, random_hex};
    use crate::server::{CloseHandle, Live};

    fn live(conn_id: u64) -> (Live, mpsc::Receiver<HubMessage>) {
        let (tx, rx) = mpsc::channel(128);
        let (close, _close_rx) = CloseHandle::new();
        let (_closed_tx, closed) = watch::channel(false);
        (
            Live {
                conn_id,
                tx,
                tools: Vec::new(),
                mcp: None,
                usage_probes: false,
                close,
                closed,
            },
            rx,
        )
    }

    async fn fixture() -> (Arc<Hub>, String, String, String, Value, mpsc::Receiver<HubMessage>, mpsc::Receiver<HubMessage>) {
        let dir = std::env::temp_dir().join(format!("grouper-relay-security-{}", random_hex(4)));
        let db = Db::open(&dir).unwrap();
        let (requester, _) = db.add_device("Requester", "linux", false).unwrap();
        let (host, _) = db.add_device("Host", "windows", false).unwrap();
        db.upsert_provider(&ProviderConfig {
            id: "hosted".into(),
            kind: "scripted".into(),
            base_url: None,
            api_key: None,
            options: json!({"via_device": host.id}),
        })
        .unwrap();
        db.upsert_model(&ModelEntry {
            id: "chosen".into(),
            provider_id: "hosted".into(),
            model: "chosen-model".into(),
            input_price: 1.0,
            output_price: 1.0,
            cache_read_price: None,
            cache_write_price: None,
            context_window: 0,
            use_cases: String::new(),
            free: false,
            roles: vec!["thread".into()],
            local: true,
            modalities: vec!["text".into()],
        })
        .unwrap();
        let conversation = db.create_conversation("project", "relay").unwrap();
        let thread = db.create_thread(&conversation, "relay", &requester.id, "chosen", None, "brief").unwrap();
        let request = serde_json::to_value(Request {
            model: "chosen-model".into(),
            system: String::new(),
            messages: vec![Message::user_text("hello")],
            tools: Vec::new(),
            max_tokens: 32,
        })
        .unwrap();
        let hub = Hub::new(db, dir);
        let (requester_live, requester_rx) = live(11);
        let (host_live, host_rx) = live(22);
        hub.live.lock().await.insert(requester.id.clone(), requester_live);
        hub.live.lock().await.insert(host.id.clone(), host_live);
        (hub, requester.id, host.id, thread.id, request, requester_rx, host_rx)
    }

    #[tokio::test]
    async fn relay_rejects_reuse_caps_calls_accounts_usage_and_aborts_large_deltas() {
        let (hub, requester, host, thread, request, mut requester_rx, mut host_rx) = fixture().await;

        hub.relay_model_request(&requester, 11, "caller-0".into(), thread.clone(), "hosted".into(), request.clone()).await;
        let HubMessage::ModelRequest { call_id: route0, .. } = host_rx.recv().await.unwrap() else {
            panic!("host did not receive first request");
        };

        // A connection may never reuse a requester call id, even after its route completes.
        let response = Response {
            message: Message { role: Role::Assistant, content: vec![Block::Text { text: "ok".into() }], raw: None },
            stop: StopReason::EndTurn,
            usage: Usage { input_tokens: 7, output_tokens: 3, cache_read_tokens: 0, cache_write_tokens: 0 },
        };
        hub.model_response(&host, 22, &route0, Some(serde_json::to_value(response).unwrap()), None).await;
        assert!(matches!(requester_rx.recv().await, Some(HubMessage::ModelResponse { error: None, .. })));
        assert!((hub.db.spent_by_thread(&thread) - 0.00001).abs() < f64::EPSILON);
        hub.thread_event(
            &thread,
            core_protocol::AgentEvent::Usage {
                provider_id: "hosted".into(),
                model: "chosen-model".into(),
                input_tokens: 7,
                output_tokens: 3,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
        );
        assert!((hub.db.spent_by_thread(&thread) - 0.00001).abs() < f64::EPSILON, "usage must not be double-counted");
        hub.relay_model_request(&requester, 11, "caller-0".into(), thread.clone(), "hosted".into(), request.clone()).await;
        assert!(matches!(requester_rx.recv().await, Some(HubMessage::ModelResponse { error: Some(error), .. }) if error.contains("duplicate")));

        // Eight requester calls are admitted; the ninth gets a normal busy response
        // and does not enqueue a frame to the host.
        for n in 1..=MAX_REQUESTER_CALLS {
            hub.relay_model_request(
                &requester,
                11,
                format!("caller-{n}"),
                thread.clone(),
                "hosted".into(),
                request.clone(),
            )
            .await;
            assert!(matches!(host_rx.recv().await, Some(HubMessage::ModelRequest { .. })));
        }
        hub.relay_model_request(&requester, 11, "caller-9".into(), thread.clone(), "hosted".into(), request.clone()).await;
        assert!(matches!(requester_rx.recv().await, Some(HubMessage::ModelResponse { error: Some(error), .. }) if error.contains("busy")));
        assert!(host_rx.try_recv().is_err());

        // An oversized delta removes its route, cancels the host, and errors the requester.
        let route = hub
            .model_calls
            .lock()
            .await
            .iter()
            .find_map(|(id, route)| matches!(&route.caller, ModelCaller::Device { call_id, .. } if call_id == "caller-1").then(|| id.clone()))
            .unwrap();
        hub.model_delta(&host, 22, &route, Value::String("x".repeat(MAX_MODEL_DELTA_BYTES))).await;
        assert!(matches!(host_rx.recv().await, Some(HubMessage::ModelCancel { call_id }) if call_id == route));
        assert!(matches!(requester_rx.recv().await, Some(HubMessage::ModelResponse { error: Some(error), .. }) if error.contains("64 KB")));
        assert!(!hub.model_calls.lock().await.contains_key(&route));

        // The host-wide cap includes hub-local callers as well as device relays.
        {
            let mut calls = hub.model_calls.lock().await;
            let active = calls.values().filter(|route| route.host_id == host && route.host_conn == 22).count();
            for n in active..MAX_HOST_CALLS {
                let (tx, _rx) = mpsc::channel(1);
                calls.insert(
                    format!("local-cap-{n}"),
                    ModelRoute {
                        host_id: host.clone(),
                        host_conn: 22,
                        caller: ModelCaller::Local(tx),
                        streamed_bytes: 0,
                    },
                );
            }
        }
        let parsed: Request = serde_json::from_value(request).unwrap();
        let error = hub.complete_on_device("hosted", &host, "Host", &parsed, &|_| {}).await.unwrap_err();
        assert!(error.to_string().contains("busy"), "{error}");
    }

    #[tokio::test]
    async fn relay_requires_the_requesters_running_chosen_thread() {
        let (hub, requester, _host, thread, request, mut requester_rx, mut host_rx) = fixture().await;
        assert!(hub.db.finish_thread(&thread, "done", "done"));
        hub.relay_model_request(&requester, 11, "stale".into(), thread, "hosted".into(), request).await;
        assert!(matches!(requester_rx.recv().await, Some(HubMessage::ModelResponse { error: Some(error), .. }) if error.contains("running thread")));
        assert!(host_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_full_host_queue_returns_busy_without_disconnecting_the_host() {
        let (hub, requester, host, thread, request, mut requester_rx, _host_rx) = fixture().await;
        let (tx, _full_rx) = mpsc::channel(1);
        tx.try_send(HubMessage::Pong).unwrap();
        let (close, _close_rx) = CloseHandle::new();
        let (_closed_tx, closed) = watch::channel(false);
        hub.live.lock().await.insert(
            host.clone(),
            Live {
                conn_id: 22,
                tx,
                tools: Vec::new(),
                mcp: None,
                usage_probes: false,
                close,
                closed,
            },
        );
        hub.relay_model_request(&requester, 11, "full".into(), thread, "hosted".into(), request).await;
        assert!(matches!(
            requester_rx.recv().await,
            Some(HubMessage::ModelResponse { error: Some(error), .. }) if error.contains("busy")
        ));
        assert!(hub.is_online(&host).await, "backpressure must not evict the provider host");
    }
}

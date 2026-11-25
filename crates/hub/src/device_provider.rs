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
use crate::server::Hub;

const MODEL_TIMEOUT: Duration = Duration::from_secs(600);

pub(crate) enum ModelReply {
    Delta(Value),
    Finished {
        response: Option<Value>,
        error: Option<String>,
    },
}

pub(crate) enum ModelCaller {
    Local(mpsc::UnboundedSender<ModelReply>),
    Device {
        device_id: String,
        conn_id: u64,
        call_id: String,
    },
}

pub(crate) struct ModelRoute {
    pub(crate) host_id: String,
    pub(crate) host_conn: u64,
    pub(crate) caller: ModelCaller,
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
        let (reply, mut replies) = mpsc::unbounded_channel();
        let host_conn = {
            let live = self.live.lock().await;
            let Some(host) = live.get(device_id) else {
                return Err(ProviderError::Unavailable(format!(
                    "the model lives on {device_name}, which is offline"
                )));
            };
            host.conn_id
        };
        self.model_calls.lock().await.insert(
            call_id.clone(),
            ModelRoute {
                host_id: device_id.into(),
                host_conn,
                caller: ModelCaller::Local(reply),
            },
        );
        if !self
            .send_to_conn(
                device_id,
                host_conn,
                HubMessage::ModelRequest {
                    call_id: call_id.clone(),
                    provider_id: provider_id.into(),
                    request,
                },
            )
            .await
        {
            self.model_calls.lock().await.remove(&call_id);
            return Err(ProviderError::Unavailable(format!(
                "the model lives on {device_name}, which is offline"
            )));
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
            self.send_to_conn(
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
        provider_id: String,
        request: Value,
    ) {
        let fail = |message: String| HubMessage::ModelResponse {
            call_id: requester_call.clone(),
            response: None,
            error: Some(message),
        };
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
        self.model_calls.lock().await.insert(
            call_id.clone(),
            ModelRoute {
                host_id: host_id.clone(),
                host_conn,
                caller: ModelCaller::Device {
                    device_id: requester_id.into(),
                    conn_id: requester_conn,
                    call_id: requester_call.clone(),
                },
            },
        );
        if !self
            .send_to_conn(
                &host_id,
                host_conn,
                HubMessage::ModelRequest {
                    call_id: call_id.clone(),
                    provider_id,
                    request,
                },
            )
            .await
        {
            self.model_calls.lock().await.remove(&call_id);
            self.send_to_conn(
                requester_id,
                requester_conn,
                fail(format!("the model lives on {host_name}, which is offline")),
            )
            .await;
        }
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
            self.send_to_conn(
                &route.host_id,
                route.host_conn,
                HubMessage::ModelCancel { call_id: route_id },
            )
            .await;
        }
    }

    pub(crate) async fn model_delta(
        &self,
        host_id: &str,
        host_conn: u64,
        call_id: &str,
        delta: Value,
    ) {
        let target = {
            let calls = self.model_calls.lock().await;
            calls
                .get(call_id)
                .filter(|r| r.host_id == host_id && r.host_conn == host_conn)
                .map(|r| match &r.caller {
                    ModelCaller::Local(tx) => EitherTarget::Local(tx.clone()),
                    ModelCaller::Device {
                        device_id,
                        conn_id,
                        call_id,
                    } => EitherTarget::Device {
                        device_id: device_id.clone(),
                        conn_id: *conn_id,
                        call_id: call_id.clone(),
                    },
                })
        };
        match target {
            Some(EitherTarget::Local(tx)) => {
                let _ = tx.send(ModelReply::Delta(delta));
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
        &self,
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
                let _ = tx.send(ModelReply::Finished { response, error });
            }
            Some(ModelCaller::Device {
                device_id,
                conn_id,
                call_id,
            }) => {
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
                self.send_to_conn(
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
                    let _ = tx.send(ModelReply::Finished {
                        response: None,
                        error: Some(error),
                    });
                }
                ModelCaller::Device {
                    device_id,
                    conn_id,
                    call_id,
                } => {
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
    Local(mpsc::UnboundedSender<ModelReply>),
    Device {
        device_id: String,
        conn_id: u64,
        call_id: String,
    },
}

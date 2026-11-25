//! Device-side halves of a provider hosted behind an outbound connection.

use std::sync::{Arc, Weak};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use core_protocol::{NodeMessage, ProviderConfig};
use providers::{Delta, OnDelta, Provider, ProviderError, Request, Response};
use serde_json::Value;
use tokio::sync::{mpsc, watch};

use super::Threads;

const MODEL_TIMEOUT: Duration = Duration::from_secs(600);
const MODEL_REPLY_CAP: usize = 64;
const MAX_MODEL_DELTA_BYTES: usize = 64 * 1024;
const MAX_MODEL_STREAM_BYTES: usize = 8 * 1024 * 1024;
const MAX_HOST_MODEL_TASKS: usize = 16;

pub(crate) enum ModelReply {
    Delta(Value),
    Finished {
        response: Option<Value>,
        error: Option<String>,
    },
}

pub(crate) struct ModelWaiter {
    tx: mpsc::Sender<ModelReply>,
    streamed_bytes: usize,
}

struct CancelOnDrop {
    threads: Weak<Threads>,
    call_id: Option<String>,
}

impl CancelOnDrop {
    fn disarm(&mut self) {
        self.call_id = None;
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        let (Some(threads), Some(call_id)) = (self.threads.upgrade(), self.call_id.take()) else {
            return;
        };
        threads.model_waiters.lock().unwrap().remove(&call_id);
        threads.send(NodeMessage::ModelCancel { call_id });
    }
}

struct ViaHub {
    threads: Weak<Threads>,
    provider_id: String,
    host_name: String,
    thread_id: String,
}

#[async_trait]
impl Provider for ViaHub {
    async fn complete(&self, request: &Request) -> Result<Response, ProviderError> {
        self.complete_stream(request, &|_| {}).await
    }

    async fn complete_stream(
        &self,
        request: &Request,
        on_delta: OnDelta<'_>,
    ) -> Result<Response, ProviderError> {
        let Some(threads) = self.threads.upgrade() else {
            return Err(ProviderError::Unavailable(
                "the hub connection closed while making the model call".into(),
            ));
        };
        let request = serde_json::to_value(request).map_err(|e| ProviderError::BadResponse {
            provider: self.provider_id.clone(),
            message: e.to_string(),
        })?;
        let call_id = format!(
            "{:016x}-m{}",
            threads.approval_nonce,
            threads
                .next
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let (reply, mut replies) = mpsc::channel(MODEL_REPLY_CAP);
        threads
            .model_waiters
            .lock()
            .unwrap()
            .insert(call_id.clone(), ModelWaiter { tx: reply, streamed_bytes: 0 });
        threads.send(NodeMessage::ModelRequest {
            call_id: call_id.clone(),
            thread_id: self.thread_id.clone(),
            provider_id: self.provider_id.clone(),
            request,
        });
        let mut cancel = CancelOnDrop {
            threads: Arc::downgrade(&threads),
            call_id: Some(call_id),
        };
        let result = tokio::time::timeout(MODEL_TIMEOUT, async {
            while let Some(reply) = replies.recv().await {
                match reply {
                    ModelReply::Delta(value) => {
                        let delta: Delta = serde_json::from_value(value).map_err(|e| {
                            ProviderError::BadResponse {
                                provider: self.provider_id.clone(),
                                message: format!("bad relayed delta from {}: {e}", self.host_name),
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
                                provider: self.provider_id.clone(),
                                message: format!(
                                    "bad relayed response from {}: {e}",
                                    self.host_name
                                ),
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
                            provider: self.provider_id.clone(),
                            message: format!("empty relayed response from {}", self.host_name),
                        });
                    }
                }
            }
            Err(ProviderError::Unavailable(
                "the hub connection closed while making the model call".into(),
            ))
        })
        .await;
        let still_pending = threads
            .model_waiters
            .lock()
            .unwrap()
            .contains_key(cancel.call_id.as_deref().unwrap_or(""));
        if !still_pending {
            cancel.disarm();
        }
        match result {
            Ok(result) => result,
            Err(_) => Err(ProviderError::Unavailable(format!(
                "model call on {} timed out",
                self.host_name
            ))),
        }
    }
}

/// Build directly on the hosting device; use the hub relay on every other device.
pub(crate) fn build(
    config: &ProviderConfig,
    threads: &Arc<Threads>,
    thread_id: &str,
) -> Result<Arc<dyn Provider>, ProviderError> {
    let Some(host) = config.via_device() else {
        return providers::build(config);
    };
    let identity = threads.identity.lock().unwrap().clone();
    let local = identity
        .as_ref()
        .is_some_and(|(id, name)| host == id || host.eq_ignore_ascii_case(name));
    if local {
        providers::build(&config.without_via_device())
    } else {
        Ok(Arc::new(ViaHub {
            threads: Arc::downgrade(threads),
            provider_id: config.id.clone(),
            host_name: host.to_string(),
            thread_id: thread_id.to_string(),
        }))
    }
}

/// Begin a call that the hub assigned to this hosting device.
pub(crate) fn start_host_call(
    threads: &Arc<Threads>,
    call_id: String,
    provider_id: String,
    request: Value,
) {
    let send_error = |threads: &Threads, message: String| {
        threads.send(NodeMessage::ModelResponse {
            call_id: call_id.clone(),
            response: None,
            error: Some(message),
        });
    };
    let Some(config) = threads
        .providers
        .lock()
        .unwrap()
        .iter()
        .find(|p| p.id == provider_id)
        .cloned()
    else {
        return send_error(
            threads,
            format!("this device has no provider {provider_id:?}"),
        );
    };
    let Some(host) = config.via_device() else {
        return send_error(
            threads,
            format!("provider {provider_id} is not hosted by a device"),
        );
    };
    let Some((device_id, device_name)) = threads.identity.lock().unwrap().clone() else {
        return send_error(threads, "device identity is not established".into());
    };
    if host != device_id && !host.eq_ignore_ascii_case(&device_name) {
        return send_error(
            threads,
            format!("provider {provider_id} is hosted by {host}, not this device ({device_name})"),
        );
    }
    let request: Request = match serde_json::from_value(request) {
        Ok(request) => request,
        Err(error) => return send_error(threads, format!("bad model request: {error}")),
    };
    let provider = match providers::build(&config.without_via_device()) {
        Ok(provider) => provider,
        Err(error) => return send_error(threads, error.to_string()),
    };
    let (cancel, mut cancelled) = watch::channel(false);
    {
        let mut tasks = threads.model_tasks.lock().unwrap();
        if tasks.contains_key(&call_id) {
            return send_error(threads, format!("duplicate model call id {call_id}"));
        }
        if tasks.len() >= MAX_HOST_MODEL_TASKS {
            return send_error(threads, "model host is busy; try again later".into());
        }
        tasks.insert(call_id.clone(), cancel);
    }
    let weak = Arc::downgrade(threads);
    tokio::spawn(async move {
        let Some(threads) = weak.upgrade() else {
            return;
        };
        let delta_threads = threads.clone();
        let delta_call = call_id.clone();
        let streamed = Arc::new(AtomicUsize::new(0));
        let stream_failed = Arc::new(AtomicBool::new(false));
        let (stream_abort, mut stream_aborted) = watch::channel(false);
        let streamed_for_delta = streamed.clone();
        let failed_for_delta = stream_failed.clone();
        let on_delta = move |delta: Delta| {
            if let Ok(delta) = serde_json::to_value(delta) {
                let bytes = serde_json::to_vec(&delta).map_or(MAX_MODEL_DELTA_BYTES + 1, |v| v.len());
                let total = streamed_for_delta.fetch_add(bytes, Ordering::Relaxed).saturating_add(bytes);
                if bytes > MAX_MODEL_DELTA_BYTES || total > MAX_MODEL_STREAM_BYTES {
                    failed_for_delta.store(true, Ordering::Relaxed);
                    let _ = stream_abort.send(true);
                    return;
                }
                delta_threads.send(NodeMessage::ModelDelta {
                    call_id: delta_call.clone(),
                    delta,
                });
            }
        };
        let completion = provider.complete_stream(&request, &on_delta);
        let mut result = tokio::select! {
            result = completion => Some(result),
            _ = cancelled.wait_for(|cancelled| *cancelled) => None,
            _ = stream_aborted.wait_for(|aborted| *aborted) => Some(Err(ProviderError::BadResponse {
                provider: provider_id.clone(),
                message: "model stream exceeded the 64 KB delta or 8 MB call limit".into(),
            })),
        };
        if stream_failed.load(Ordering::Relaxed) {
            result = Some(Err(ProviderError::BadResponse {
                provider: provider_id.clone(),
                message: "model stream exceeded the 64 KB delta or 8 MB call limit".into(),
            }));
        }
        threads.model_tasks.lock().unwrap().remove(&call_id);
        if let Some(result) = result {
            let (response, error) = match result {
                Ok(response) => match serde_json::to_value(response) {
                    Ok(response) => (Some(response), None),
                    Err(error) => (None, Some(error.to_string())),
                },
                Err(error) => (None, Some(error.to_string())),
            };
            threads.send(NodeMessage::ModelResponse {
                call_id,
                response,
                error,
            });
        }
    });
}

pub(crate) fn cancel_host_call(threads: &Threads, call_id: &str) {
    if let Some(cancel) = threads.model_tasks.lock().unwrap().remove(call_id) {
        let _ = cancel.send(true);
    }
}

pub(crate) fn relay_delta(threads: &Threads, call_id: &str, delta: Value) {
    let bytes = serde_json::to_vec(&delta).map_or(MAX_MODEL_DELTA_BYTES + 1, |v| v.len());
    let mut waiters = threads.model_waiters.lock().unwrap();
    let Some(waiter) = waiters.get_mut(call_id) else { return };
    waiter.streamed_bytes = waiter.streamed_bytes.saturating_add(bytes);
    if bytes > MAX_MODEL_DELTA_BYTES || waiter.streamed_bytes > MAX_MODEL_STREAM_BYTES {
        let waiter = waiters.remove(call_id).expect("waiter exists");
        let _ = waiter.tx.try_send(ModelReply::Finished {
            response: None,
            error: Some("model stream exceeded the 64 KB delta or 8 MB call limit".into()),
        });
        drop(waiters);
        threads.send(NodeMessage::ModelCancel { call_id: call_id.to_string() });
    } else if let Err(mpsc::error::TrySendError::Full(_)) = waiter.tx.try_send(ModelReply::Delta(delta)) {
        waiters.remove(call_id);
        drop(waiters);
        threads.send(NodeMessage::ModelCancel { call_id: call_id.to_string() });
    }
}

pub(crate) fn relay_response(
    threads: &Threads,
    call_id: &str,
    response: Option<Value>,
    error: Option<String>,
) {
    if let Some(waiter) = threads.model_waiters.lock().unwrap().remove(call_id) {
        let _ = waiter.tx.try_send(ModelReply::Finished { response, error });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use providers::{Message, Request};
    use serde_json::json;
    use tokio::sync::{mpsc, watch};

    use super::*;
    use crate::Threads;

    fn hosted_config(reply: &str) -> ProviderConfig {
        ProviderConfig {
            id: "hosted".into(),
            kind: "scripted".into(),
            base_url: None,
            api_key: None,
            options: json!({
                "via_device": "device",
                "inline": {"default": [{"type": "text", "text": reply}]}
            }),
        }
    }

    fn request() -> Value {
        serde_json::to_value(Request {
            model: "model".into(),
            system: String::new(),
            messages: vec![Message::user_text("hello")],
            tools: Vec::new(),
            max_tokens: 32,
        })
        .unwrap()
    }

    #[tokio::test]
    async fn host_caps_tasks_and_aborts_oversized_streams() {
        let (out, mut output) = mpsc::channel(64);
        let threads = Arc::new(Threads::new(out, Arc::new(|| {})));
        *threads.identity.lock().unwrap() = Some(("device".into(), "Host".into()));
        threads.providers.lock().unwrap().push(hosted_config("ok"));
        {
            let mut tasks = threads.model_tasks.lock().unwrap();
            for n in 0..MAX_HOST_MODEL_TASKS {
                let (cancel, _rx) = watch::channel(false);
                tasks.insert(format!("existing-{n}"), cancel);
            }
        }
        start_host_call(&threads, "busy".into(), "hosted".into(), request());
        assert!(matches!(
            output.recv().await,
            Some(NodeMessage::ModelResponse { call_id, error: Some(error), .. })
                if call_id == "busy" && error.contains("busy")
        ));

        threads.model_tasks.lock().unwrap().clear();
        threads.providers.lock().unwrap().clear();
        threads.providers.lock().unwrap().push(hosted_config(&"x".repeat(MAX_MODEL_DELTA_BYTES)));
        start_host_call(&threads, "large".into(), "hosted".into(), request());
        let response = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(NodeMessage::ModelResponse { call_id, error, .. }) = output.recv().await
                    && call_id == "large"
                {
                    break error;
                }
            }
        })
        .await
        .unwrap();
        assert!(response.is_some_and(|error| error.contains("64 KB")));
        assert!(!threads.model_tasks.lock().unwrap().contains_key("large"));
    }
}

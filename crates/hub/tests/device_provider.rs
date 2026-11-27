//! A real core-client hosts scripted providers for hub conversations and threads.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use core_client::core_protocol::{
    AgentEvent, ChatRequest, DeviceInfo, Event, EventBody, ModelEntry, ProviderConfig,
};
use core_client::{Events, Status, ToolHandler};
use futures_util::StreamExt;
use hub::db::Db;
use hub::server::{self, Hub};
use hub::threads::ThreadRequest;
use serde_json::json;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

struct NoTools;
impl ToolHandler for NoTools {
    fn call(&self, tool: &str, _args: &str) -> Result<String, String> {
        Err(format!("unknown tool {tool}"))
    }
}

struct Quiet;
impl Events for Quiet {
    fn status(&self, _status: Status) {}
    fn presence(&self, _devices: Vec<DeviceInfo>) {}
}

fn temp() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("grouper-device-provider-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn model(id: &str, provider_id: &str, roles: &[&str]) -> ModelEntry {
    ModelEntry {
        id: id.into(),
        provider_id: provider_id.into(),
        model: format!("{id}-model"),
        input_price: 0.0,
        output_price: 0.0,
        cache_read_price: None,
        cache_write_price: None,
        context_window: 0,
        use_cases: String::new(),
        free: true,
        roles: roles.iter().map(|role| (*role).to_string()).collect(),
        local: false,
        modalities: vec!["text".into()],
        ..Default::default()
    }
}

async fn event_socket(
    addr: std::net::SocketAddr,
    token: &str,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let mut request = format!("ws://{addr}/v1/events")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    tokio_tungstenite::connect_async(request).await.unwrap().0
}

async fn next_for(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    conversation: &str,
) -> Event {
    loop {
        let message = socket
            .next()
            .await
            .expect("event socket closed")
            .expect("event socket error");
        if let Message::Text(text) = message {
            let event: Event = serde_json::from_str(&text).unwrap();
            if event.conversation_id == conversation {
                return event;
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hosted_provider_serves_hub_calls_streams_cancels_and_reports_offline() {
    let dir = temp();
    let db = Db::open(&dir.join("data")).unwrap();
    let (device, token) = db.add_device("Gaming PC", "windows", false).unwrap();
    let (laptop, laptop_token) = db.add_device("Laptop", "linux", false).unwrap();
    db.upsert_provider(&ProviderConfig {
        id: "fast-hosted".into(),
        kind: "scripted".into(),
        base_url: Some("http://127.0.0.1:45123/v1".into()),
        api_key: None,
        options: json!({
            "via_device": "Gaming PC",
            "inline": {
                "delay_ms": 120,
                "rules": [
                    {"system": "orchestrator of Grouper", "last": "orchestrator only",
                     "reply": [{"type": "text", "text": "orchestrator through Gaming PC"}]},
                    {"system": "orchestrator of Grouper", "last": "run hub thread",
                     "reply": [{"type": "tool_use", "name": "start_thread", "input": {
                         "title": "Hosted hub thread", "device": "hub", "goal": "hosted thread work",
                         "model": "fast"
                     }}]},
                    {"system": "You are a subagent", "last": "hosted thread work",
                     "reply": [{"type": "text", "text": "hub thread through Gaming PC"}]},
                    {"system": "You are a subagent", "last": "remote device thread work",
                     "reply": [{"type": "text", "text": "Laptop thread through Gaming PC"}]},
                    {"system": "You are a subagent", "last": "hosting device thread work",
                     "reply": [{"type": "text", "text": "Gaming PC thread called locally"}]},
                    {"system": "orchestrator of Grouper", "last": " done]",
                     "reply": [{"type": "text", "text": "orchestrator saw hosted thread"}]},
                    {"system": "You are Grouper", "last": "standard hello",
                     "reply": [{"type": "text", "text": "standard through Gaming PC"}]}
                ],
                "default": [{"type": "text", "text": "hosted default"}]
            }
        }),
    })
    .unwrap();
    db.upsert_provider(&ProviderConfig {
        id: "slow-hosted".into(),
        kind: "scripted".into(),
        base_url: Some("http://127.0.0.1:45124/v1".into()),
        api_key: None,
        options: json!({
            "via_device": device.id,
            "inline": {"delay_ms": 4000, "default": [{"type": "text", "text": "must be cancelled"}]}
        }),
    })
    .unwrap();
    db.upsert_model(&model(
        "fast",
        "fast-hosted",
        &["orchestrator", "standard", "thread"],
    ))
    .unwrap();
    db.upsert_model(&model("slow", "slow-hosted", &[])).unwrap();

    let hub = Hub::new(db, dir.join("data"));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(axum::serve(listener, server::router(hub.clone())).into_future());

    let mcp_config = dir.join("mcp.json");
    std::fs::write(
        &mcp_config,
        r#"{"includeSubagentChat":false,"mcpServers":{}}"#,
    )
    .unwrap();
    let config = core_client::Config {
        hub_url: format!("http://{addr}"),
        token: token.clone(),
        capabilities: Vec::new(),
        workspace_root: Some(dir.join("device-workspaces")),
        allow_shell: false,
        mcp_config: Some(mcp_config),
        folders: Vec::new(),
        usage_probes: false,
    };
    let (host_stop, host_stopped) = tokio::sync::watch::channel(false);
    let host_client = tokio::spawn(core_client::run(
        config,
        Arc::new(NoTools),
        Arc::new(Quiet),
        host_stopped,
    ));
    let laptop_config = core_client::Config {
        hub_url: format!("http://{addr}"),
        token: laptop_token,
        capabilities: Vec::new(),
        workspace_root: Some(dir.join("laptop-workspaces")),
        allow_shell: false,
        mcp_config: Some(dir.join("mcp.json")),
        folders: Vec::new(),
        usage_probes: false,
    };
    let (laptop_stop, laptop_stopped) = tokio::sync::watch::channel(false);
    let laptop_client = tokio::spawn(core_client::run(
        laptop_config,
        Arc::new(NoTools),
        Arc::new(Quiet),
        laptop_stopped,
    ));
    tokio::time::timeout(Duration::from_secs(10), async {
        while !hub.is_online(&device.id).await || !hub.is_online(&laptop.id).await {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("hosting device connected");
    // Let Welcome + Providers reach the client before the first call.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Standard chat is a hub model call, with deltas relayed from the device.
    let mut events = event_socket(addr, &token).await;
    let standard = hub
        .chat(ChatRequest {
            conversation_id: None,
            mode: Some("standard".into()),
            text: "standard hello".into(),
            model: Some("fast".into()),
        })
        .await
        .unwrap();
    let mut standard_deltas = String::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = next_for(&mut events, &standard).await;
            match event.body {
                EventBody::AssistantDelta { text } => standard_deltas.push_str(&text),
                EventBody::AssistantMessage { text } if text == "standard through Gaming PC" => {
                    break;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("standard hosted response");
    assert_eq!(standard_deltas, "standard through Gaming PC");

    // The project orchestrator and a hub-resident thread both use the same device host.
    let project = hub
        .chat(ChatRequest {
            conversation_id: None,
            mode: Some("project".into()),
            text: "run hub thread".into(),
            model: Some("fast".into()),
        })
        .await
        .unwrap();
    let mut thread_deltas = String::new();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let event = next_for(&mut events, &project).await;
            match event.body {
                EventBody::ThreadActivity {
                    event: AgentEvent::TextDelta { text },
                } => thread_deltas.push_str(&text),
                EventBody::AssistantMessage { text }
                    if text == "orchestrator saw hosted thread" =>
                {
                    break;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("hosted orchestrator and hub thread finished");
    assert_eq!(thread_deltas, "hub thread through Gaming PC");
    assert_eq!(
        hub.db.threads(Some(&project))[0].summary.as_deref(),
        Some("hub thread through Gaming PC")
    );

    // A thread on another device asks the hub to relay to the host, while a thread
    // on Gaming PC takes the direct local path. Starting both before waiting also
    // exercises concurrent hosted calls.
    let remote_conversation = hub
        .db
        .create_conversation("project", "remote device relay")
        .unwrap();
    let remote = hub
        .start_thread(
            &remote_conversation,
            ThreadRequest {
                title: "Remote device relay".into(),
                device: laptop.id.clone(),
                goal: "remote device thread work".into(),
                model: Some("fast".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let direct_conversation = hub
        .db
        .create_conversation("project", "hosting device direct")
        .unwrap();
    let direct = hub
        .start_thread(
            &direct_conversation,
            ThreadRequest {
                title: "Hosting device direct".into(),
                device: device.id.clone(),
                goal: "hosting device thread work".into(),
                model: Some("fast".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while [remote.id.as_str(), direct.id.as_str()].iter().any(|id| {
            hub.db
                .thread(id)
                .is_some_and(|thread| thread.status == "running")
        }) {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("non-host device thread completed through the relay");
    assert_eq!(
        hub.db.thread(&remote.id).unwrap().summary.as_deref(),
        Some("Laptop thread through Gaming PC")
    );
    assert_eq!(
        hub.db.thread(&direct.id).unwrap().summary.as_deref(),
        Some("Gaming PC thread called locally")
    );

    // Stopping a hub thread drops its provider future, which sends ModelCancel to
    // the host. The slow scripted response must never become thread output.
    let cancellation_conversation = hub
        .db
        .create_conversation("project", "cancellation")
        .unwrap();
    let slow = hub
        .start_thread(
            &cancellation_conversation,
            ThreadRequest {
                title: "Cancel hosted model".into(),
                device: "hub".into(),
                goal: "wait for the slow model".into(),
                model: Some("slow".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    hub.stop_thread(&slow.id, "user").await.unwrap();
    tokio::time::sleep(Duration::from_millis(4300)).await;
    let slow_events = hub.db.events(&cancellation_conversation, 0, Some(&slow.id));
    assert_eq!(hub.db.thread(&slow.id).unwrap().status, "stopped");
    assert!(!slow_events.iter().any(|event| matches!(
        &event.body,
        EventBody::ThreadActivity { event: AgentEvent::Text { text } } if text.contains("must be cancelled")
    )));

    // Once the host disconnects, a pinned orchestrator call reports the named
    // machine rather than trying its loopback URL from the hub.
    host_stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(10), host_client)
        .await
        .expect("client stopped")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while hub.is_online(&device.id).await {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("hosting device went offline");
    let offline = hub
        .chat(ChatRequest {
            conversation_id: None,
            mode: Some("project".into()),
            text: "orchestrator only".into(),
            model: Some("fast".into()),
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if hub.db.events(&offline, 0, None).iter().any(|event| {
                matches!(&event.body, EventBody::Error { message }
                    if message == "the model lives on Gaming PC, which is offline")
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("clear offline provider error");

    laptop_stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(10), laptop_client)
        .await
        .expect("laptop client stopped")
        .unwrap();
    server.abort();
    drop(hub);
    std::fs::remove_dir_all(dir).ok();
}

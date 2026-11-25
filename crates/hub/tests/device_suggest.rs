//! Two devices over WebSocket. `start_thread` with no device picks from needs and
//! from each device's record; naming a device that lacks a need is an error.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use core_agent::folders::FolderSpec;
use core_client::core_protocol::{
    Capability, DeviceInfo, FolderMode, ModelEntry, ProviderConfig, default_input_schema,
};
use core_client::{Events, Status, ToolHandler};
use hub::db::Db;
use hub::server::{self, Hub};
use hub::threads::ThreadRequest;
use serde_json::json;
use tokio::sync::watch;
use tokio::task::JoinHandle;

const WAIT: Duration = Duration::from_secs(90);

struct Tools;
impl ToolHandler for Tools {
    fn call(&self, _tool: &str, args: &str) -> Result<String, String> {
        Ok(args.to_string())
    }
}

struct Quiet;
impl Events for Quiet {
    fn status(&self, _status: Status) {}
    fn presence(&self, _devices: Vec<DeviceInfo>) {}
}

struct Client {
    stop: Option<watch::Sender<bool>>,
    task: Option<JoinHandle<()>>,
}

impl Drop for Client {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(true);
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

fn model() -> ModelEntry {
    ModelEntry {
        id: "fake".into(),
        provider_id: "s".into(),
        model: "fake-1".into(),
        input_price: 0.0,
        output_price: 0.0,
        cache_read_price: None,
        cache_write_price: None,
        context_window: 0,
        use_cases: "code".into(),
        free: true,
        roles: vec!["orchestrator".into(), "thread".into()],
        local: false,
        modalities: vec![],
    }
}

fn finish(db: &Db, device: &str, status: &str, grade: Option<i64>) {
    let thread = db
        .create_thread("seed", "t", device, "fake", None, "brief")
        .unwrap();
    db.set_thread_meta(&thread.id, "code", &[]);
    assert!(db.finish_thread(&thread.id, status, "s"));
    assert!(db.record_outcome(&thread.id));
    if let Some(grade) = grade {
        db.grade_thread(&thread.id, grade, "note").unwrap();
    }
}

async fn start(
    hub: &Arc<Hub>,
    conv: &str,
    device: &str,
    task_type: Option<&str>,
    needs: &[&str],
    title: &str,
) -> Result<core_client::core_protocol::ThreadInfo, String> {
    hub.start_thread(
        conv,
        ThreadRequest {
            title: title.into(),
            device: device.into(),
            goal: title.into(),
            task_type: task_type.map(str::to_string),
            needs: needs.iter().map(|need| (*need).to_string()).collect(),
            ..Default::default()
        },
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn start_thread_without_a_device_picks_from_needs_and_past_results() {
    let dir = std::env::temp_dir().join(format!("grouper-device-suggest-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("projects")).unwrap();
    let db = Db::open(&dir.join("data")).unwrap();
    db.upsert_provider(&ProviderConfig {
        id: "s".into(),
        kind: "scripted".into(),
        base_url: None,
        api_key: None,
        options: json!({"inline": {"default": [{"type": "text", "text": "done"}]}}),
    })
    .unwrap();
    db.upsert_model(&model()).unwrap();
    let (mac, mac_token) = db.add_device("MacBook", "macos", false).unwrap();
    let (pc, pc_token) = db.add_device("Gaming PC", "windows", false).unwrap();
    for _ in 0..3 {
        finish(&db, &pc.id, "done", Some(5));
        finish(&db, &mac.id, "failed", None);
    }
    let hub = Hub::new(db, dir.join("data"));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let router = server::router(hub.clone());
    tokio::spawn(async move { axum::serve(listener, router).await });
    std::fs::write(
        dir.join("mcp.json"),
        json!({"includeSubagentChat": false, "mcpServers": {}}).to_string(),
    )
    .unwrap();

    let conv = hub.db.create_conversation("project", "devices").unwrap();
    let connect =
        |token: String, root: PathBuf, caps: Vec<Capability>, folders: Vec<FolderSpec>| {
            let config = core_client::Config {
                hub_url: url.clone(),
                token,
                capabilities: caps,
                workspace_root: Some(root),
                allow_shell: true,
                mcp_config: Some(dir.join("mcp.json")),
                folders,
                usage_probes: false,
            };
            let (stop, stopped) = watch::channel(false);
            Client {
                stop: Some(stop),
                task: Some(tokio::spawn(core_client::run(
                    config,
                    Arc::new(Tools),
                    Arc::new(Quiet),
                    stopped,
                ))),
            }
        };
    let _mac_client = connect(
        mac_token,
        dir.join("mac-ws"),
        vec![],
        vec![FolderSpec {
            name: "projects".into(),
            path: dir.join("projects"),
            mode: FolderMode::Read,
        }],
    );
    let _pc_client = connect(
        pc_token,
        dir.join("pc-ws"),
        vec![
            Capability {
                name: "gpu".into(),
                description: "This machine has a GPU".into(),
                input_schema: default_input_schema(),
            },
            Capability {
                name: "codex__codex_chat".into(),
                description: "Codex".into(),
                input_schema: default_input_schema(),
            },
        ],
        vec![],
    );

    let ready = tokio::time::timeout(WAIT, async {
        loop {
            let mac_tools = hub.db.tools(Some(&mac.id));
            let pc_tools = hub.db.tools(Some(&pc.id));
            let folders = hub.db.device_folders(&mac.id);
            if hub.is_online(&mac.id).await
                && hub.is_online(&pc.id).await
                && mac_tools.iter().any(|tool| tool.name == "run_shell")
                && folders.iter().any(|folder| folder.name == "projects")
                && pc_tools.iter().any(|tool| tool.name == "run_shell")
                && pc_tools.iter().any(|tool| tool.name == "gpu")
                && pc_tools.iter().any(|tool| tool.name == "codex__codex_chat")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    if ready.is_err() {
        let mac_tools: Vec<_> = hub
            .db
            .tools(Some(&mac.id))
            .into_iter()
            .map(|tool| tool.name)
            .collect();
        let pc_tools: Vec<_> = hub
            .db
            .tools(Some(&pc.id))
            .into_iter()
            .map(|tool| tool.name)
            .collect();
        let folders = hub.db.device_folders(&mac.id);
        panic!(
            "timed out waiting for both devices (mac online {}, pc online {}); mac tools {mac_tools:?}; pc tools {pc_tools:?}; folders {folders:?}",
            hub.is_online(&mac.id).await,
            hub.is_online(&pc.id).await
        );
    }

    let suggestion = hub
        .suggest_device(Some("code"), &["shell".into()])
        .await
        .unwrap();
    assert_eq!(
        suggestion.choice.as_ref().map(|pick| pick.id.as_str()),
        Some(pc.id.as_str())
    );
    assert!(
        suggestion
            .ranked
            .iter()
            .any(|row| row.id == mac.id && row.satisfies.iter().any(|need| need == "xcode"))
    );
    assert!(
        suggestion
            .ranked
            .iter()
            .any(|row| row.id == pc.id && row.satisfies.iter().any(|need| need == "gpu"))
    );

    let thread = start(&hub, &conv, "", Some("code"), &["shell"], "Shell")
        .await
        .unwrap();
    assert_eq!(thread.device_id, pc.id);
    let reason = thread.reason.unwrap();
    assert!(
        reason.contains("device Gaming PC:")
            && reason.contains("best code record")
            && reason.contains("3/3 succeeded")
            && reason.contains("grade 5.0")
            && reason.contains("needs shell"),
        "{reason}"
    );

    let thread = start(
        &hub,
        &conv,
        "",
        Some("code"),
        &["shell", "xcode"],
        "Mac build",
    )
    .await
    .unwrap();
    assert_eq!(
        thread.device_id, mac.id,
        "xcode rules out the PC even though its code record is better"
    );
    let reason = thread.reason.unwrap();
    assert!(
        reason.contains("below 50%") && reason.contains("needs shell, xcode"),
        "{reason}"
    );

    let thread = start(&hub, &conv, "", Some("code"), &["gpu"], "Train")
        .await
        .unwrap();
    assert_eq!(thread.device_id, pc.id);

    let thread = start(&hub, &conv, "", Some("code"), &["folder:projects"], "Notes")
        .await
        .unwrap();
    assert_eq!(thread.device_id, mac.id);

    let thread = start(&hub, &conv, "", Some("code"), &["agent:codex"], "Review")
        .await
        .unwrap();
    assert_eq!(thread.device_id, pc.id);

    let named = start(
        &hub,
        &conv,
        "Gaming PC",
        Some("code"),
        &["gpu"],
        "Named GPU",
    )
    .await
    .unwrap();
    assert_eq!(named.device_id, pc.id);
    assert!(
        !named.reason.unwrap().contains("needs gpu"),
        "a named device is not a suggestion"
    );

    let err = start(
        &hub,
        &conv,
        "MacBook",
        Some("code"),
        &["gpu"],
        "Wrong machine",
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("MacBook lacks gpu") && err.contains("Gaming PC (online)"),
        "{err}"
    );

    let err = start(&hub, &conv, "", Some("code"), &["gpu", "xcode"], "Both")
        .await
        .unwrap_err();
    assert!(
        err.contains("No online device satisfies gpu, xcode") && err.contains("file work"),
        "{err}"
    );

    drop(hub);
    let _ = std::fs::remove_dir_all(&dir);
}

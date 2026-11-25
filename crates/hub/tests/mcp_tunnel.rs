//! A real device (core-client) connects to a real hub over WebSocket: its tools reach
//! the registry over the MCP tunnel and `Hub::call_tool` runs them.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use core_client::core_protocol::{Capability, DeviceInfo, Permission, default_input_schema};
use core_client::{Events, Status, ToolHandler};
use hub::db::Db;
use hub::server::{self, CallError, Hub};
use serde_json::json;

struct Tools;
impl ToolHandler for Tools {
    fn call(&self, tool: &str, args: &str) -> Result<String, String> {
        match tool {
            "echo" => Ok(args.to_string()),
            "fail" => Err("it failed".into()),
            _ => Err(format!("unknown tool {tool}")),
        }
    }
}

struct Quiet;
impl Events for Quiet {
    fn status(&self, _status: Status) {}
    fn presence(&self, _devices: Vec<DeviceInfo>) {}
}

fn temp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("grouper-tunnel-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[tokio::test(flavor = "multi_thread")]
async fn device_tools_reach_the_registry_and_run_over_mcp() {
    let dir = temp("hub");
    let db = Db::open(&dir.join("data")).unwrap();
    let (device, token) = db.add_device("Laptop", "windows", false).unwrap();
    let hub = Hub::new(db, dir.join("data"));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = server::router(hub.clone());
    tokio::spawn(async move { axum::serve(listener, router).await });

    // One local MCP server on the device, no subagent-chat.
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../core-client/tests/fixtures/mcp_test_server.py");
    let python = if cfg!(windows) { "python" } else { "python3" };
    let mcp_config = dir.join("mcp.json");
    std::fs::write(
        &mcp_config,
        json!({"includeSubagentChat": false, "mcpServers": {"py": {"command": python, "args": [script]}}}).to_string(),
    )
    .unwrap();

    let config = core_client::Config {
        hub_url: format!("http://{addr}"),
        token,
        capabilities: vec![
            Capability {
                name: "echo".into(),
                description: "Returns its arguments".into(),
                input_schema: json!({"type": "object", "additionalProperties": true}),
            },
            Capability { name: "fail".into(), description: "Always fails".into(), input_schema: default_input_schema() },
        ],
        workspace_root: Some(dir.join("workspaces")),
        allow_shell: false,
        mcp_config: Some(mcp_config),
    };
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let client = tokio::spawn(core_client::run(config, Arc::new(Tools), Arc::new(Quiet), stopped));

    // The platform tools come with the first listing, the MCP server's after it starts.
    let names = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let names: Vec<String> = hub.db.tools(Some(&device.id)).into_iter().map(|t| t.name).collect();
            if names.iter().any(|n| n == "py__echo") {
                return names;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("the device's tools reached the registry");
    for expected in ["echo", "fail", "read_file", "write_file", "list_files", "py__echo", "py__sleep"] {
        assert!(names.iter().any(|n| n == expected), "{expected} missing from {names:?}");
    }
    assert!(!names.iter().any(|n| n == "run_shell"));
    let perms = hub.db.permissions(&device.id);
    assert_eq!(perms["read_file"], Permission::Auto);
    assert_eq!(perms["py__echo"], Permission::Manual);

    // Presence shows the tools, with their schemas.
    let devices = hub.devices().await;
    let me = devices.iter().find(|d| d.id == device.id).unwrap();
    assert!(me.online);
    let echo = me.capabilities.iter().find(|c| c.name == "echo").unwrap();
    assert_eq!(echo.input_schema["additionalProperties"], true);
    assert!(me.capabilities.iter().any(|c| c.name == "py__echo"));

    // Direct calls go through MCP tools/call.
    let r = hub.call_tool(&device.id, "echo", json!({"a": 1})).await.unwrap();
    assert!(r.ok);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&r.output).unwrap(), json!({"a": 1}));
    let r = hub.call_tool(&device.id, "fail", json!({})).await.unwrap();
    assert_eq!((r.ok, r.output.as_str()), (false, "it failed"));
    let r = hub.call_tool(&device.id, "py__echo", json!({"text": "through the tunnel"})).await.unwrap();
    assert_eq!((r.ok, r.output.as_str()), (true, "through the tunnel"));
    let r = hub.call_tool(&device.id, "read_file", json!({"path": "x"})).await.unwrap();
    assert!(!r.ok && r.output.contains("only runs inside a thread"), "{}", r.output);
    assert_eq!(hub.call_tool(&device.id, "nope", json!({})).await.unwrap_err(), CallError::UnknownTool);
    assert_eq!(hub.call_tool(&device.id, "echo", json!([1])).await.unwrap_err(), CallError::BadArguments);
    assert_eq!(hub.call_tool("someone-else", "echo", json!({})).await.unwrap_err(), CallError::Offline);

    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(15), client).await.expect("client stopped").unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while hub.is_online(&device.id).await {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("device went offline");
    drop(hub);
    std::fs::remove_dir_all(dir).ok();
}

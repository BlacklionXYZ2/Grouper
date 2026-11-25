//! The MCP host against a real stdio server (a stdlib-only Python script).

use std::path::PathBuf;
use std::time::Duration;

use core_client::mcp_host::{McpHost, ServerSpec};
use serde_json::json;

fn python() -> String {
    if cfg!(windows) { "python".into() } else { "python3".into() }
}

pub fn test_server() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures").join("mcp_test_server.py")
}

fn spec(name: &str, command: String, args: Vec<String>, startup_secs: u64) -> ServerSpec {
    ServerSpec {
        name: name.into(),
        command,
        args,
        env: Default::default(),
        cwd: None,
        startup_timeout: Duration::from_secs(startup_secs),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn stdio_server_lists_calls_hides_secrets_cancels_and_restarts() {
    // SAFETY: the only test in this binary touching the environment.
    unsafe { std::env::set_var("GROUPER_TOKEN", "dev.secret-for-mcp-test") };

    let started = std::time::Instant::now();
    let host = McpHost::start(vec![
        spec("py", python(), vec![test_server().display().to_string()], 30),
        // Never answers: must not hold up anything else.
        spec("slow", python(), vec!["-c".into(), "import time; time.sleep(60)".into()], 2),
        spec("missing", "grouper-no-such-command".into(), vec![], 5),
    ]);
    assert!(started.elapsed() < Duration::from_secs(1), "start must not wait for servers");

    let mut changes = host.subscribe();
    tokio::time::timeout(Duration::from_secs(30), async {
        while host.tools().is_empty() {
            changes.changed().await.unwrap();
        }
    })
    .await
    .expect("py listed its tools");
    let mut names: Vec<String> = host.tools().into_iter().map(|t| t.name).collect();
    names.sort();
    assert_eq!(names, ["py__cancelled", "py__crash", "py__echo", "py__env", "py__sleep"]);
    let echo = host.tools().into_iter().find(|t| t.name == "py__echo").unwrap();
    assert_eq!(echo.input_schema["properties"]["text"]["type"], "string");

    assert_eq!(host.call("py__echo", json!({"text": "hi"})).await, (true, "hi".to_string()));
    let (ok, env) = host.call("py__env", json!({})).await;
    assert!(ok);
    assert!(env.contains("GROUPER_TOKEN=absent"), "{env}");
    let pid = env.split("pid=").nth(1).unwrap().to_string();

    // Dropping an in-flight call (a thread being stopped) cancels it on the server.
    let sleeping = host.call("py__sleep", json!({"seconds": 20}));
    assert!(tokio::time::timeout(Duration::from_millis(500), sleeping).await.is_err());
    let cancelled = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let (_, list) = host.call("py__cancelled", json!({})).await;
            if list != "[]" {
                return list;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the server saw notifications/cancelled");
    assert!(cancelled.starts_with('['), "{cancelled}");

    // A crash fails the call; the next call restarts the server.
    let (ok, out) = host.call("py__crash", json!({})).await;
    assert!(!ok);
    assert!(out.contains("stopped"), "{out}");
    let (ok, env) = host.call("py__env", json!({})).await;
    assert!(ok, "{env}");
    assert_ne!(env.split("pid=").nth(1).unwrap(), pid, "a new process");
    assert_eq!(host.call("py__echo", json!({"text": "back"})).await, (true, "back".to_string()));

    assert!(!host.call("py__echo", json!([1])).await.0);
    assert!(!host.call("nope__x", json!({})).await.0);

    host.shutdown().await;
    let (ok, out) = host.call("py__echo", json!({"text": "after"})).await;
    assert!(!ok, "{out}");
}

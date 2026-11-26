//! The MCP host against a real stdio server (a stdlib-only Python script).

use std::path::PathBuf;
use std::time::Duration;

use core_client::mcp_host::{McpHost, ServerSpec, default_cwd};
use serde_json::json;

fn python() -> String {
    if cfg!(windows) { "python".into() } else { "python3".into() }
}

pub fn test_server() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures").join("mcp_test_server.py")
}

fn spec(name: &str, command: String, args: Vec<String>, cwd: Option<PathBuf>, startup_secs: u64) -> ServerSpec {
    ServerSpec {
        name: name.into(),
        command,
        args,
        env: Default::default(),
        cwd,
        startup_timeout: Duration::from_secs(startup_secs),
    }
}

fn pid_of(env: &str) -> String {
    env.split("pid=").nth(1).unwrap().to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn stdio_server_lists_calls_hides_secrets_cancels_and_restarts() {
    // SAFETY: the only test in this binary touching the environment.
    unsafe { std::env::set_var("GROUPER_TOKEN", "dev.secret-for-mcp-test") };

    let py_name = format!("py-test-{}", std::process::id());
    let expected_default_cwd = default_cwd(&py_name);
    if let Some(dir) = &expected_default_cwd {
        let _ = std::fs::remove_dir_all(dir);
    }
    let explicit_cwd = std::env::temp_dir().join(format!("grouper-mcp-explicit-cwd-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&explicit_cwd);
    std::fs::create_dir_all(&explicit_cwd).unwrap();

    let started = std::time::Instant::now();
    let host = McpHost::with_cancel_grace(
        vec![
            spec(&py_name, python(), vec![test_server().display().to_string()], None, 30),
            spec("py-cwd", python(), vec![test_server().display().to_string()], Some(explicit_cwd.clone()), 30),
            // Never answers: must not hold up anything else.
            spec("slow", python(), vec!["-c".into(), "import time; time.sleep(60)".into()], None, 2),
            spec("missing", "grouper-no-such-command".into(), vec![], None, 5),
        ],
        Duration::from_secs(1),
    );
    assert!(started.elapsed() < Duration::from_secs(1), "start must not wait for servers");

    let t = |tool: &str| format!("{py_name}__{tool}");
    let mut changes = host.subscribe();
    tokio::time::timeout(Duration::from_secs(30), async {
        while !host.has_tool(&t("echo")) || !host.has_tool("py-cwd__cwd") {
            changes.changed().await.unwrap();
        }
    })
    .await
    .expect("py servers listed their tools");

    let mut names: Vec<String> = host
        .tools()
        .into_iter()
        .filter(|d| d.name.starts_with(&format!("{py_name}__")))
        .map(|d| d.name)
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            t("cancelled"),
            t("crash"),
            t("cwd"),
            t("echo"),
            t("env"),
            t("set_tools"),
            t("sleep"),
            t("stubborn"),
        ]
    );
    let echo = host.tools().into_iter().find(|d| d.name == t("echo")).unwrap();
    assert_eq!(echo.input_schema["properties"]["text"]["type"], "string");

    // 1. Default working folder (created under data_local_dir) and explicit cwd override.
    let (ok, reported_cwd) = host.call(&t("cwd"), json!({})).await;
    assert!(ok, "{reported_cwd}");
    if let Some(expected) = &expected_default_cwd {
        assert!(expected.is_dir(), "default cwd should be created: {}", expected.display());
        assert_eq!(
            std::fs::canonicalize(&reported_cwd).unwrap(),
            std::fs::canonicalize(expected).unwrap(),
            "server with no cwd should run in its per-server data dir"
        );
    }
    let (ok, custom_cwd) = host.call("py-cwd__cwd", json!({})).await;
    assert!(ok, "{custom_cwd}");
    assert_eq!(
        std::fs::canonicalize(&custom_cwd).unwrap(),
        std::fs::canonicalize(&explicit_cwd).unwrap(),
        "server with explicit cwd should honour it"
    );

    assert_eq!(host.call(&t("echo"), json!({"text": "hi"})).await, (true, "hi".to_string()));
    let (ok, env) = host.call(&t("env"), json!({})).await;
    assert!(ok);
    assert!(env.contains("GROUPER_TOKEN=absent"), "{env}");
    let pid = pid_of(&env);

    // 2. Local server notifications/tools/list_changed updates host's tools.
    changes.borrow_and_update();
    let (ok, out) = host.call(&t("set_tools"), json!({"extra": ["bonus"]})).await;
    assert!(ok, "{out}");
    tokio::time::timeout(Duration::from_secs(5), async {
        while !host.has_tool(&t("bonus")) {
            changes.changed().await.unwrap();
        }
    })
    .await
    .expect("host re-listed tools after notifications/tools/list_changed");
    assert_eq!(host.call(&t("bonus"), json!({"text": "yay"})).await, (true, "yay".to_string()));

    let (ok, out) = host.call(&t("set_tools"), json!({"extra": []})).await;
    assert!(ok, "{out}");
    tokio::time::timeout(Duration::from_secs(5), async {
        while host.has_tool(&t("bonus")) {
            changes.changed().await.unwrap();
        }
    })
    .await
    .expect("host removed dropped tool after notifications/tools/list_changed");
    assert!(!host.call(&t("bonus"), json!({})).await.0);

    // Dropping an in-flight call (a thread being stopped) cancels it on the server.
    // A server that obeys cancellation is NOT restarted.
    let sleep_tool = t("sleep");
    let sleeping = host.call(&sleep_tool, json!({"seconds": 20}));
    assert!(tokio::time::timeout(Duration::from_millis(500), sleeping).await.is_err());
    let cancelled = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let (_, list) = host.call(&t("cancelled"), json!({})).await;
            if list != "[]" {
                return list;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the server saw notifications/cancelled");
    assert!(cancelled.starts_with('['), "{cancelled}");
    // Wait past the 1s cancel grace to verify a compliant server was NOT restarted.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let (ok, env_after_sleep) = host.call(&t("env"), json!({})).await;
    assert!(ok);
    assert_eq!(pid_of(&env_after_sleep), pid, "compliant server must stay on the same process");

    // 5. Cancel follow-up: a tool that ignores notifications/cancelled causes the host to
    // restart that server after the cancel grace period.
    let stubborn_tool = t("stubborn");
    let stubborn = host.call(&stubborn_tool, json!({"seconds": 30}));
    assert!(tokio::time::timeout(Duration::from_millis(400), stubborn).await.is_err());
    let new_pid = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let (ok, env) = host.call(&t("env"), json!({})).await;
            if ok {
                let current = pid_of(&env);
                if current != pid {
                    return current;
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("stubborn server should be restarted after cancel grace");
    assert_ne!(new_pid, pid);
    assert_eq!(host.call(&t("echo"), json!({"text": "after-restart"})).await, (true, "after-restart".to_string()));

    // A crash fails the call; the next call restarts the server.
    let (ok, out) = host.call(&t("crash"), json!({})).await;
    assert!(!ok);
    assert!(out.contains("stopped"), "{out}");
    let (ok, env) = host.call(&t("env"), json!({})).await;
    assert!(ok, "{env}");
    assert_ne!(pid_of(&env), new_pid, "a new process after crash");
    assert_eq!(host.call(&t("echo"), json!({"text": "back"})).await, (true, "back".to_string()));

    assert!(!host.call(&t("echo"), json!([1])).await.0);
    assert!(!host.call("nope__x", json!({})).await.0);

    host.shutdown().await;
    let (ok, out) = host.call(&t("echo"), json!({"text": "after"})).await;
    assert!(!ok, "{out}");

    if let Some(dir) = &expected_default_cwd {
        let _ = std::fs::remove_dir_all(dir);
    }
    if let Some(dir) = default_cwd("slow") {
        let _ = std::fs::remove_dir_all(dir);
    }
    if let Some(dir) = default_cwd("missing") {
        let _ = std::fs::remove_dir_all(dir);
    }
    let _ = std::fs::remove_dir_all(explicit_cwd);
}

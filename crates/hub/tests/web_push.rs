//! Web Push in process: a real hub served over HTTP, a hub thread that asks for approval, and
//! a fake push service on 127.0.0.1 that records what the hub sends.
//!
//! Verifies RFC 8291 encryption, RFC 8292 VAPID signing, random approval ids, no silent
//! `approval_cleared` pushes, live `dropped` events on stop, non-blocking per-subscription
//! delivery, and push subscription cleanup when sessions or devices are revoked.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::{Duration, Instant};

use aws_lc_rs::aead::{AES_128_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use aws_lc_rs::agreement::{self, ECDH_P256, UnparsedPublicKey};
use aws_lc_rs::hmac;
use aws_lc_rs::signature::{ECDSA_P256_SHA256_FIXED, UnparsedPublicKey as SigKey};
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use core_protocol::{ChatRequest, Event, EventBody, ModelEntry, Permission, ProviderConfig, ToolDef};
use futures_util::StreamExt;
use hub::db::Db;
use hub::server::{HUB_DEVICE, Hub};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMsg;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const WAIT: Duration = Duration::from_secs(60);
const ORCH: &str = "orchestrator of Grouper";
const THREAD: &str = "You are a subagent";
const MARKER: &str = "secret-marker-7f3a";

fn setup(tag: &str) -> (PathBuf, Arc<Hub>, String, String) {
    let dir = std::env::temp_dir().join(format!("grouper-push-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).unwrap();
    let script = json!({"rules": [
        {"system": ORCH, "last": "please", "reply": [{"type": "tool_use", "name": "start_thread",
            "input": {"title": "Look around", "device": "hub", "goal": "List the workspace."}}]},
        {"system": THREAD, "last": "# Brief", "reply": [{"type": "tool_use", "name": "list_files",
            "input": {"path": MARKER}}]}
    ], "default": [{"type": "text", "text": "Done."}]});
    db.upsert_provider(&ProviderConfig {
        id: "s".into(),
        kind: "scripted".into(),
        base_url: None,
        api_key: None,
        options: json!({ "inline": script }),
    })
    .unwrap();
    db.upsert_model(&ModelEntry {
        id: "fake".into(),
        provider_id: "s".into(),
        model: "fake-1".into(),
        input_price: 0.0,
        output_price: 0.0,
        cache_read_price: None,
        cache_write_price: None,
        context_window: 0,
        use_cases: String::new(),
        free: false,
        roles: vec!["orchestrator".into(), "standard".into(), "thread".into()],
        local: false,
        modalities: vec![],
    })
    .unwrap();
    db.upsert_tools(
        HUB_DEVICE,
        &[ToolDef {
            name: "list_files".into(),
            description: "List the workspace.".into(),
            input_schema: json!({"type": "object", "properties": {}}),
        }],
    );
    db.set_permission(HUB_DEVICE, "list_files", Permission::Manual).unwrap();
    let (_, admin) = db.add_device("Laptop", "windows", true).unwrap();
    let (_, plain) = db.add_device("Phone", "ios", false).unwrap();
    let hub = Hub::new(db, dir.clone());
    (dir, hub, admin, plain)
}

/// One request the fake push service received.
struct Received {
    path: String,
    headers: HeaderMap,
    body: Vec<u8>,
}

#[derive(Clone)]
struct Fake {
    tx: mpsc::UnboundedSender<Received>,
    status: Arc<AtomicU16>,
    slow_delay: Duration,
}

async fn fake_push(State(f): State<Fake>, Path(name): Path<String>, headers: HeaderMap, body: Bytes) -> StatusCode {
    if name == "slow" && !f.slow_delay.is_zero() {
        tokio::time::sleep(f.slow_delay).await;
    }
    let _ = f.tx.send(Received { path: name, headers, body: body.to_vec() });
    StatusCode::from_u16(f.status.load(Ordering::SeqCst)).unwrap()
}

async fn serve(router: axum::Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (base, task)
}

fn hmac256(key: &[u8], parts: &[&[u8]]) -> Vec<u8> {
    let mut ctx = hmac::Context::with_key(&hmac::Key::new(hmac::HMAC_SHA256, key));
    for p in parts {
        ctx.update(p);
    }
    ctx.sign().as_ref().to_vec()
}

/// RFC 8291 decryption on the subscriber's side, written independently of the hub's code.
fn decrypt(body: &[u8], ua_private: &agreement::PrivateKey, auth: &[u8]) -> Vec<u8> {
    let salt = &body[..16];
    let rs = u32::from_be_bytes(body[16..20].try_into().unwrap());
    assert_eq!(rs, 4096);
    let idlen = body[20] as usize;
    assert_eq!(idlen, 65);
    let as_public = &body[21..21 + idlen];
    let ciphertext = &body[21 + idlen..];
    let ua_public = ua_private.compute_public_key().unwrap();
    let ecdh =
        agreement::agree(ua_private, UnparsedPublicKey::new(&ECDH_P256, as_public), (), |s| Ok(s.to_vec())).unwrap();
    let prk_key = hmac256(auth, &[&ecdh]);
    let ikm = hmac256(&prk_key, &[b"WebPush: info\0", ua_public.as_ref(), as_public, &[1]]);
    let prk = hmac256(salt, &[&ikm]);
    let cek = &hmac256(&prk, &[b"Content-Encoding: aes128gcm\0", &[1]])[..16];
    let nonce: [u8; 12] = hmac256(&prk, &[b"Content-Encoding: nonce\0", &[1]])[..12].try_into().unwrap();
    let key = LessSafeKey::new(UnboundKey::new(&AES_128_GCM, cek).unwrap());
    let mut buf = ciphertext.to_vec();
    let plain = key.open_in_place(Nonce::assume_unique_for_key(nonce), Aad::empty(), &mut buf).unwrap();
    // Single record: plaintext, then the 0x02 delimiter, then zero padding.
    let end = plain.iter().rposition(|&b| b != 0).unwrap();
    assert_eq!(plain[end], 2, "last-record delimiter");
    plain[..end].to_vec()
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers.get(name).unwrap_or_else(|| panic!("missing header {name}")).to_str().unwrap()
}

/// Checks the VAPID `Authorization` header and returns the JWT claims.
fn check_vapid(headers: &HeaderMap, vapid_key: &str) -> Value {
    let auth = header(headers, "authorization");
    let rest = auth.strip_prefix("vapid t=").expect(auth);
    let (jwt, k) = rest.split_once(", k=").unwrap();
    assert_eq!(k, vapid_key);
    let parts: Vec<&str> = jwt.split('.').collect();
    assert_eq!(parts.len(), 3);
    let head: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).unwrap()).unwrap();
    assert_eq!(head["alg"], "ES256");
    let signature = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
    let public = URL_SAFE_NO_PAD.decode(vapid_key).unwrap();
    SigKey::new(&ECDSA_P256_SHA256_FIXED, &public)
        .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
        .expect("JWT verifies with the VAPID public key");
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap()
}

async fn get_json(http: &reqwest::Client, url: String, bearer: &str) -> Value {
    http.get(url).header("authorization", bearer).send().await.unwrap().json().await.unwrap()
}

async fn next(rx: &mut mpsc::UnboundedReceiver<Received>, what: &str) -> Received {
    tokio::time::timeout(WAIT, rx.recv()).await.unwrap_or_else(|_| panic!("timed out waiting for {what}")).unwrap()
}

fn sub_json(endpoint: String, ua_private: &agreement::PrivateKey, auth_secret: [u8; 16], label: &str) -> Value {
    let ua_public = URL_SAFE_NO_PAD.encode(ua_private.compute_public_key().unwrap().as_ref());
    json!({
        "endpoint": endpoint,
        "expirationTime": null,
        "keys": {"p256dh": ua_public, "auth": URL_SAFE_NO_PAD.encode(auth_secret)},
        "label": label,
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn approvals_are_pushed_encrypted_without_silent_clear_pushes() {
    let (dir, hub, admin, plain) = setup("flow");
    hub.set_push_allow_local_http(true);
    let (base, hub_task) = serve(hub::server::router(hub.clone())).await;

    let (tx, mut rx) = mpsc::unbounded_channel();
    let status = Arc::new(AtomicU16::new(201));
    let fake = Fake { tx, status: status.clone(), slow_delay: Duration::ZERO };
    let (push_base, push_task) =
        serve(axum::Router::new().route("/push/{name}", axum::routing::post(fake_push)).with_state(fake)).await;

    let http = reqwest::Client::new();
    let bearer = format!("Bearer {admin}");

    // Device tokens can't use push at all.
    let r = http.get(format!("{base}/v1/push/vapid-key")).header("authorization", format!("Bearer {plain}"));
    let r = r.send().await.unwrap();
    assert_eq!(r.status(), 403);

    let key = get_json(&http, format!("{base}/v1/push/vapid-key"), &bearer).await;
    let vapid_key = key["public_key"].as_str().unwrap().to_string();
    let decoded = URL_SAFE_NO_PAD.decode(&vapid_key).unwrap();
    assert_eq!((decoded.len(), decoded[0]), (65, 4));

    // The subscriber: our own key pair and auth secret, as a browser would make them.
    let ua_private = agreement::PrivateKey::generate(&ECDH_P256).unwrap();
    let auth_secret = [9u8; 16];
    let subscription = sub_json(format!("{push_base}/push/one"), &ua_private, auth_secret, "Test browser");
    let r = http
        .post(format!("{base}/v1/push/subscriptions"))
        .header("authorization", &bearer)
        .header("origin", "https://grouper.example.org")
        .json(&subscription)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let row: Value = r.json().await.unwrap();
    let sub_id = row["id"].as_str().unwrap().to_string();
    assert_eq!(row["label"], "Test browser");
    assert!(row["created"].as_u64().is_some());
    assert!(row["last_ok"].is_null());
    assert!(row.get("keys").is_none() && row.get("p256dh").is_none() && row.get("auth").is_none());

    // Subscribing again upserts by endpoint.
    let again: Value = http
        .post(format!("{base}/v1/push/subscriptions"))
        .header("authorization", &bearer)
        .json(&subscription)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(again["id"], sub_id.as_str());

    // An approval request is pushed with a random approval_id.
    let conv = hub
        .chat(ChatRequest { conversation_id: None, mode: Some("project".into()), text: "please".into(), model: None })
        .await
        .unwrap();
    let request = next(&mut rx, "the approval_requested push").await;
    assert_eq!(request.path, "one");
    let approval_id = hub
        .db
        .events(&conv, 0, None)
        .into_iter()
        .find_map(|e| match e.body {
            EventBody::ApprovalRequested { approval_id, .. } => Some(approval_id),
            _ => None,
        })
        .unwrap();
    assert!(
        approval_id.starts_with("a_") && approval_id.len() == 26 && approval_id[2..].bytes().all(|b| b.is_ascii_hexdigit()),
        "expected random 12-byte hex approval id, got {approval_id}"
    );
    let thread = hub.db.threads(Some(&conv)).remove(0);

    assert_eq!(header(&request.headers, "content-encoding"), "aes128gcm");
    assert_eq!(header(&request.headers, "content-type"), "application/octet-stream");
    assert_eq!(header(&request.headers, "ttl"), "86400");
    assert_eq!(header(&request.headers, "urgency"), "high");
    let topic = header(&request.headers, "topic").to_string();
    assert!(topic.len() <= 32 && topic.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'), "{topic}");
    let claims = check_vapid(&request.headers, &vapid_key);
    assert_eq!(claims["aud"], push_base.as_str());
    assert_eq!(claims["sub"], "https://grouper.example.org");
    let exp = claims["exp"].as_u64().unwrap();
    let now = hub::db::now();
    assert!(exp > now && exp <= now + 86_400);

    let plaintext = decrypt(&request.body, &ua_private, &auth_secret);
    let text = String::from_utf8(plaintext).unwrap();
    assert!(!text.contains(MARKER), "tool input leaked into the push: {text}");
    let payload: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(payload["type"], "approval_requested");
    assert_eq!(payload["approval_id"], approval_id.as_str());
    assert_eq!(payload["conversation_id"], conv.as_str());
    assert_eq!(payload["thread_id"], thread.id.as_str());
    assert_eq!(payload["tool"], "list_files");
    assert_eq!(payload["device"], "Hub");
    assert_eq!(payload["thread_title"], "Look around");
    assert_eq!(payload["chat_title"], "please");
    assert!(payload["ts"].as_u64().is_some());
    assert!(payload.get("input").is_none() && payload.get("reason").is_none());

    // Answering it must NOT send a silent `approval_cleared` push (violates userVisibleOnly).
    let r = http
        .post(format!("{base}/v1/approvals/{approval_id}"))
        .header("authorization", &bearer)
        .json(&json!({"approve": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(
        tokio::time::timeout(Duration::from_millis(250), rx.recv()).await.is_err(),
        "answering an approval must not send an approval_cleared push"
    );

    let list = get_json(&http, format!("{base}/v1/push/subscriptions"), &bearer).await;
    let rows = list.as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0]["last_ok"].as_u64().is_some(), "{list}");
    assert!(rows[0]["last_error"].is_null());

    // A client can unsubscribe itself by endpoint.
    let mut second = subscription.clone();
    second["endpoint"] = json!(format!("{push_base}/push/two"));
    let r = http.post(format!("{base}/v1/push/subscriptions")).header("authorization", &bearer).json(&second);
    let r = r.send().await.unwrap();
    assert_eq!(r.status(), 200);
    let r = http
        .delete(
            reqwest::Url::parse_with_params(
                &format!("{base}/v1/push/subscriptions"),
                &[("endpoint", format!("{push_base}/push/two"))],
            )
            .unwrap(),
        )
        .header("authorization", &bearer)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // The test push reaches the subscription; a 410 removes it.
    status.store(410, Ordering::SeqCst);
    let r: Value = http
        .post(format!("{base}/v1/push/test"))
        .header("authorization", &bearer)
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let test = next(&mut rx, "the test push").await;
    let payload: Value = serde_json::from_slice(&decrypt(&test.body, &ua_private, &auth_secret)).unwrap();
    assert_eq!(payload["type"], "test");
    let results = r["results"].as_array().unwrap();
    assert_eq!(results.len(), 1, "{r}");
    assert_eq!(results[0]["id"], sub_id.as_str());
    assert_eq!(results[0]["status"], 410);
    assert_eq!(results[0]["removed"], true);
    let list = get_json(&http, format!("{base}/v1/push/subscriptions"), &bearer).await;
    assert_eq!(list, json!([]));

    // Wait for the thread to finish before tearing down.
    tokio::time::timeout(WAIT, async {
        while hub.db.thread(&thread.id).is_some_and(|t| t.status == "running") {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    hub_task.abort();
    push_task.abort();
    drop(hub);
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_the_thread_emits_live_dropped_event_and_no_clear_push() {
    let (dir, hub, admin, _) = setup("stop");
    hub.set_push_allow_local_http(true);
    let (base, hub_task) = serve(hub::server::router(hub.clone())).await;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let fake = Fake { tx, status: Arc::new(AtomicU16::new(201)), slow_delay: Duration::ZERO };
    let (push_base, push_task) =
        serve(axum::Router::new().route("/push/{name}", axum::routing::post(fake_push)).with_state(fake)).await;
    let http = reqwest::Client::new();
    let bearer = format!("Bearer {admin}");

    let mut ws_req = format!("{}/v1/events", base.replacen("http://", "ws://", 1)).into_client_request().unwrap();
    ws_req.headers_mut().insert("authorization", bearer.parse().unwrap());
    let (mut ev_ws, _) = tokio_tungstenite::connect_async(ws_req).await.unwrap();

    let ua_private = agreement::PrivateKey::generate(&ECDH_P256).unwrap();
    let auth_secret = [3u8; 16];
    let r = http
        .post(format!("{base}/v1/push/subscriptions"))
        .header("authorization", &bearer)
        .header("origin", "http://localhost:8080")
        .json(&sub_json(format!("{push_base}/push/stop"), &ua_private, auth_secret, "Local dev"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let conv = hub
        .chat(ChatRequest { conversation_id: None, mode: Some("project".into()), text: "please".into(), model: None })
        .await
        .unwrap();
    let request = next(&mut rx, "the approval_requested push").await;
    // Loopback http:// subscribe origin is accepted as the VAPID sub claim.
    let key = get_json(&http, format!("{base}/v1/push/vapid-key"), &bearer).await;
    let claims = check_vapid(&request.headers, key["public_key"].as_str().unwrap());
    assert_eq!(claims["sub"], "http://localhost:8080");

    let payload: Value = serde_json::from_slice(&decrypt(&request.body, &ua_private, &auth_secret)).unwrap();
    let approval_id = payload["approval_id"].as_str().unwrap().to_string();
    let thread = hub.db.threads(Some(&conv)).remove(0);

    let r = http.post(format!("{base}/v1/threads/{}/stop", thread.id)).header("authorization", &bearer);
    let r = r.send().await.unwrap();
    assert_eq!(r.status(), 200);

    // Live clients receive a live-only (seq 0) `approval_resolved` with reason "dropped".
    let dropped_ev: Event = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(Ok(WsMsg::Text(t))) = ev_ws.next().await
                && let Ok(e) = serde_json::from_str::<Event>(&t)
                && matches!(
                    &e.body,
                    EventBody::ApprovalResolved { approval_id: id, approved: false, reason }
                        if id == &approval_id && reason.as_deref() == Some("dropped")
                )
            {
                break e;
            }
        }
    })
    .await
    .expect("live dropped approval event");
    assert_eq!(dropped_ev.seq, 0);
    assert_eq!(dropped_ev.conversation_id, conv);
    assert_eq!(dropped_ev.thread_id.as_deref(), Some(thread.id.as_str()));

    // And no silent `approval_cleared` push is sent.
    assert!(
        tokio::time::timeout(Duration::from_millis(250), rx.recv()).await.is_err(),
        "stopping a thread must not send an approval_cleared push"
    );

    let _ = ev_ws.close(None).await;
    hub_task.abort();
    push_task.abort();
    drop(hub);
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn slow_push_endpoint_does_not_block_healthy_endpoint() {
    let (dir, hub, admin, _) = setup("hol");
    hub.set_push_allow_local_http(true);
    let (base, hub_task) = serve(hub::server::router(hub.clone())).await;

    let (tx, mut rx) = mpsc::unbounded_channel();
    let fake = Fake { tx, status: Arc::new(AtomicU16::new(201)), slow_delay: Duration::from_secs(4) };
    let (push_base, push_task) =
        serve(axum::Router::new().route("/push/{name}", axum::routing::post(fake_push)).with_state(fake)).await;

    let http = reqwest::Client::new();
    let bearer = format!("Bearer {admin}");
    let ua_private = agreement::PrivateKey::generate(&ECDH_P256).unwrap();

    for name in ["slow", "fast"] {
        let r = http
            .post(format!("{base}/v1/push/subscriptions"))
            .header("authorization", &bearer)
            .json(&sub_json(format!("{push_base}/push/{name}"), &ua_private, [5u8; 16], name))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
    }

    let start = Instant::now();
    // Two separate approvals back-to-back.
    let _c1 = hub
        .chat(ChatRequest { conversation_id: None, mode: Some("project".into()), text: "please".into(), model: None })
        .await
        .unwrap();
    let _c2 = hub
        .chat(ChatRequest { conversation_id: None, mode: Some("project".into()), text: "please".into(), model: None })
        .await
        .unwrap();

    // The healthy endpoint must receive BOTH pushes long before the 4s slow endpoint responds once.
    let mut fast_count = 0;
    tokio::time::timeout(Duration::from_secs(2), async {
        while fast_count < 2 {
            let rec = rx.recv().await.unwrap();
            if rec.path == "fast" {
                fast_count += 1;
            }
        }
    })
    .await
    .expect("healthy endpoint must receive both pushes without waiting on slow endpoint");
    assert!(start.elapsed() < Duration::from_secs(3), "took {:?}", start.elapsed());

    hub_task.abort();
    push_task.abort();
    drop(hub);
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn revoking_session_or_device_deletes_push_subscriptions() {
    let (dir, hub, admin, _) = setup("revoke");
    hub.set_push_allow_local_http(true);
    let (base, hub_task) = serve(hub::server::router(hub.clone())).await;
    let http = reqwest::Client::new();
    let bearer = format!("Bearer {admin}");

    let login = || async {
        let r = http
            .post(format!("{base}/v1/session"))
            .header("x-grouper", "1")
            .header("origin", &base)
            .json(&json!({"token": admin}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        r.headers().get("set-cookie").unwrap().to_str().unwrap().split(';').next().unwrap().to_string()
    };

    let cookie1 = login().await;
    let cookie2 = login().await;
    let ua_private = agreement::PrivateKey::generate(&ECDH_P256).unwrap();

    let sub1: Value = http
        .post(format!("{base}/v1/push/subscriptions"))
        .header("cookie", &cookie1)
        .header("x-grouper", "1")
        .header("origin", &base)
        .json(&sub_json("http://127.0.0.1:9/push/s1".into(), &ua_private, [1u8; 16], "Browser 1"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let sub2: Value = http
        .post(format!("{base}/v1/push/subscriptions"))
        .header("cookie", &cookie2)
        .header("x-grouper", "1")
        .header("origin", &base)
        .json(&sub_json("http://127.0.0.1:9/push/s2".into(), &ua_private, [2u8; 16], "Browser 2"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(hub.db.push_subscription_count(), 2);

    // Find session 1's hash via GET /v1/sessions authenticated with cookie1.
    let sessions: Value =
        http.get(format!("{base}/v1/sessions")).header("cookie", &cookie1).send().await.unwrap().json().await.unwrap();
    let s1_id = sessions
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["current"] == true)
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Revoking session 1 deletes only Browser 1's push subscription.
    let r = http
        .delete(format!("{base}/v1/sessions/{s1_id}"))
        .header("authorization", &bearer)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let remaining = hub.db.push_subscription_list();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].id, sub2["id"].as_str().unwrap());
    assert_ne!(remaining[0].id, sub1["id"].as_str().unwrap());

    // Signing out session 2 deletes Browser 2's push subscription.
    let r = http
        .delete(format!("{base}/v1/session"))
        .header("cookie", &cookie2)
        .header("x-grouper", "1")
        .header("origin", &base)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(hub.db.push_subscription_count(), 0);

    // Removing admin rights or revoking a device deletes its push subscriptions.
    let r = http
        .post(format!("{base}/v1/push/subscriptions"))
        .header("authorization", &bearer)
        .json(&sub_json("http://127.0.0.1:9/push/dev".into(), &ua_private, [3u8; 16], "CLI"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(hub.db.push_subscription_count(), 1);
    let admin_id = admin.split('.').next().unwrap();
    hub.db.set_admin(admin_id, false).unwrap();
    assert_eq!(hub.db.push_subscription_count(), 0);

    hub.db.set_admin(admin_id, true).unwrap();
    let r = http
        .post(format!("{base}/v1/push/subscriptions"))
        .header("authorization", &bearer)
        .json(&sub_json("http://127.0.0.1:9/push/dev2".into(), &ua_private, [4u8; 16], "CLI 2"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(hub.db.push_subscription_count(), 1);
    hub.db.revoke_device(admin_id).unwrap();
    assert_eq!(hub.db.push_subscription_count(), 0);

    hub_task.abort();
    drop(hub);
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn plain_http_endpoints_need_the_test_flag() {
    let (dir, hub, admin, _) = setup("https");
    let (base, task) = serve(hub::server::router(hub.clone())).await;
    let ua_private = agreement::PrivateKey::generate(&ECDH_P256).unwrap();
    let body = json!({
        "endpoint": "http://127.0.0.1:9/push",
        "keys": {"p256dh": URL_SAFE_NO_PAD.encode(ua_private.compute_public_key().unwrap().as_ref()),
                 "auth": URL_SAFE_NO_PAD.encode([1u8; 16])},
    });
    let r = reqwest::Client::new()
        .post(format!("{base}/v1/push/subscriptions"))
        .header("authorization", format!("Bearer {admin}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let error: Value = r.json().await.unwrap();
    assert!(error["error"].as_str().unwrap().contains("https"), "{error}");
    assert_eq!(hub.db.push_subscription_count(), 0);
    task.abort();
    drop(hub);
    std::fs::remove_dir_all(dir).ok();
}

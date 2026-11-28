//! `POST /v1/devices` and `DELETE /v1/devices/{id}` over real HTTP: an admin registers a plain
//! device whose token connects as that device, device tokens are refused, cookie requests go
//! through the CSRF guard, and a revoked token stops working (its live connection is closed).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use core_protocol::{CONNECT_PATH, HubMessage, NodeMessage, PROTOCOL_VERSION};
use futures_util::{SinkExt, StreamExt};
use hub::db::Db;
use hub::server::{Hub, MAX_DEVICES, router};
use reqwest::StatusCode;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

struct Fixture {
    hub: Arc<Hub>,
    base: String,
    addr: std::net::SocketAddr,
    admin_id: String,
    admin: String,
    plain_id: String,
    plain: String,
    http: reqwest::Client,
    dir: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

async fn fixture(tag: &str) -> Fixture {
    let dir = std::env::temp_dir().join(format!("grouper-devreg-{tag}-{}", hub::db::random_hex(4)));
    let db = Db::open(&dir).unwrap();
    let (admin_device, admin) = db.add_device("Phone", "ios", true).unwrap();
    let (plain_device, plain) = db.add_device("Laptop", "windows", false).unwrap();
    let hub = Hub::new(db, dir.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = router(hub.clone());
    tokio::spawn(async move { axum::serve(listener, app).await });
    Fixture {
        hub,
        base: format!("http://{addr}"),
        addr,
        admin_id: admin_device.id,
        admin,
        plain_id: plain_device.id,
        plain,
        http: reqwest::Client::new(),
        dir,
    }
}

impl Fixture {
    async fn register(&self, token: &str, body: Value) -> (StatusCode, Value) {
        let url = format!("{}/v1/devices", self.base);
        let r = self.http.post(url).bearer_auth(token).json(&body).send().await.unwrap();
        let status = r.status();
        (status, r.json().await.unwrap_or(Value::Null))
    }

    async fn revoke(&self, token: &str, id: &str) -> (StatusCode, Value) {
        let r = self.http.delete(format!("{}/v1/devices/{id}", self.base)).bearer_auth(token).send().await.unwrap();
        let status = r.status();
        (status, r.json().await.unwrap_or(Value::Null))
    }

    async fn get_status(&self, token: &str, path: &str) -> StatusCode {
        self.http.get(format!("{}{path}", self.base)).bearer_auth(token).send().await.unwrap().status()
    }

    /// Open the device connection and say hello. `Err(status)` when the upgrade is refused.
    async fn connect(
        &self,
        token: &str,
    ) -> Result<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>, u16> {
        let mut req = format!("ws://{}{CONNECT_PATH}", self.addr).into_client_request().unwrap();
        req.headers_mut().insert("authorization", format!("Bearer {token}").parse().unwrap());
        let mut ws = match tokio_tungstenite::connect_async(req).await {
            Ok((ws, _)) => ws,
            Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => return Err(resp.status().as_u16()),
            Err(e) => panic!("connect failed: {e}"),
        };
        let hello = NodeMessage::Hello { protocol_version: PROTOCOL_VERSION, client_version: "test".into(), host: None };
        ws.send(Message::Text(serde_json::to_string(&hello).unwrap().into())).await.unwrap();
        Ok(ws)
    }
}

#[tokio::test]
async fn admin_registers_a_plain_device_whose_token_connects() {
    let f = fixture("create").await;
    let (status, body) = f.register(&f.admin, json!({"name": "  Oscar's MacBook Pro ", "platform": "macos"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let device = &body["device"];
    let id = device["id"].as_str().unwrap().to_string();
    assert_eq!(device["name"], "Oscar's MacBook Pro");
    assert_eq!(device["platform"], "macos");
    assert_eq!(device["online"], false);
    assert_eq!(device["capabilities"], json!([]));
    assert!(device["folders"].as_array().is_none_or(Vec::is_empty), "{device}");
    let token = body["token"].as_str().unwrap().to_string();
    assert_eq!(token.split_once('.').unwrap().0, id, "the token is <device id>.<secret>");

    // Same shape as GET /v1/devices, and it is listed there.
    let listed: Vec<Value> =
        f.http.get(format!("{}/v1/devices", f.base)).bearer_auth(&f.admin).send().await.unwrap().json().await.unwrap();
    assert_eq!(listed.iter().find(|d| d["id"] == id.as_str()), Some(device));

    // Not an admin: it can read but not touch admin routes, nor register or revoke devices.
    let auth = f.hub.db.authenticate(&token).unwrap();
    assert_eq!(auth.device_id, id);
    assert!(!auth.admin);
    assert_eq!(f.get_status(&token, "/v1/devices").await, StatusCode::OK);
    assert_eq!(f.get_status(&token, "/v1/providers").await, StatusCode::FORBIDDEN);
    assert_eq!(f.register(&token, json!({"name": "Sneaky"})).await.0, StatusCode::FORBIDDEN);

    // It connects as that device.
    let mut ws = f.connect(&token).await.unwrap();
    let welcome = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(Ok(Message::Text(t))) = ws.next().await {
            if let Ok(HubMessage::Welcome { device_id, device_name, .. }) = serde_json::from_str::<HubMessage>(&t) {
                return (device_id, device_name);
            }
        }
        panic!("no welcome");
    })
    .await
    .expect("welcomed");
    assert_eq!(welcome, (id.clone(), "Oscar's MacBook Pro".to_string()));

    // Asking for admin in the body is ignored: the device is still plain.
    let (status, body) = f.register(&f.admin, json!({"name": "Desk", "platform": "linux", "admin": true})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!f.hub.db.authenticate(body["token"].as_str().unwrap()).unwrap().admin);

    // Platform is optional.
    let (status, body) = f.register(&f.admin, json!({"name": "Box"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["device"]["platform"], "unknown");
}

#[tokio::test]
async fn device_tokens_and_strangers_are_refused() {
    let f = fixture("refuse").await;
    assert_eq!(f.register(&f.plain, json!({"name": "Mine", "platform": "macos"})).await.0, StatusCode::FORBIDDEN);
    // The admin check comes before the body is read.
    let r = f.http.post(format!("{}/v1/devices", f.base)).bearer_auth(&f.plain).body("not json").send().await.unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    assert_eq!(f.revoke(&f.plain, &f.admin_id).await.0, StatusCode::FORBIDDEN);
    assert_eq!(f.revoke(&f.plain, &f.plain_id).await.0, StatusCode::FORBIDDEN);
    assert_eq!(f.register("nope.nope", json!({"name": "X"})).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(f.revoke("nope.nope", &f.plain_id).await.0, StatusCode::UNAUTHORIZED);
    let r = f.http.post(format!("{}/v1/devices", f.base)).json(&json!({"name": "X"})).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    assert!(f.hub.db.authenticate(&f.admin).is_some() && f.hub.db.authenticate(&f.plain).is_some());
    assert_eq!(f.hub.db.devices().len(), 2, "nothing was registered");
}

#[tokio::test]
async fn bad_bodies_and_the_cap_are_refused() {
    let f = fixture("validate").await;
    for body in [
        json!({"name": ""}),
        json!({"name": "   "}),
        json!({"name": "x".repeat(65)}),
        json!({"name": "two\nlines"}),
        json!({"name": "hub"}),
        json!({"name": "Mac", "platform": "mac os"}),
        json!({"name": "Mac", "platform": "p".repeat(33)}),
        json!({"platform": "macos"}),
    ] {
        let (status, reply) = f.register(&f.admin, body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body} -> {reply}");
        assert!(reply["error"].is_string(), "{reply}");
    }
    let r = f.http.post(format!("{}/v1/devices", f.base)).bearer_auth(&f.admin).body("{").send().await.unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    assert_eq!(f.register(&f.admin, json!({"name": "é".repeat(64)})).await.0, StatusCode::OK, "64 characters fit");

    // Fill up to the cap (3 exist now); revoked devices don't count.
    let (spare, _) = f.hub.db.add_device("Spare", "linux", false).unwrap();
    f.hub.db.revoke_device(&spare.id).unwrap();
    for i in 3..MAX_DEVICES {
        f.hub.db.add_device(&format!("Filler {i}"), "linux", false).unwrap();
    }
    let (status, reply) = f.register(&f.admin, json!({"name": "One too many", "platform": "macos"})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{reply}");
    assert!(reply["error"].as_str().unwrap().contains("revoke one first"), "{reply}");
    assert_eq!(f.revoke(&f.admin, &f.plain_id).await.0, StatusCode::OK);
    assert_eq!(f.register(&f.admin, json!({"name": "Fits again", "platform": "macos"})).await.0, StatusCode::OK);
}

#[tokio::test]
async fn revoking_stops_the_token_and_closes_its_connection() {
    let f = fixture("revoke").await;
    let (_, body) = f.register(&f.admin, json!({"name": "MacBook", "platform": "macos"})).await;
    let id = body["device"]["id"].as_str().unwrap().to_string();
    let token = body["token"].as_str().unwrap().to_string();

    let mut ws = f.connect(&token).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !f.hub.is_online(&id).await {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("online");

    let (status, reply) = f.revoke(&f.admin, &id).await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    assert_eq!(reply, json!({"revoked": id}));

    // The live connection hears why and closes.
    let saw_error = tokio::time::timeout(Duration::from_secs(10), async {
        let mut saw = false;
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(t))) => {
                    if let Ok(HubMessage::Error { message }) = serde_json::from_str::<HubMessage>(&t) {
                        saw |= message.contains("revoked");
                    }
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break saw,
                _ => {}
            }
        }
    })
    .await
    .expect("connection closed");
    assert!(saw_error, "the device is told its token was revoked");
    tokio::time::timeout(Duration::from_secs(10), async {
        while f.hub.is_online(&id).await {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("offline");

    // The token no longer works anywhere.
    assert!(f.hub.db.authenticate(&token).is_none());
    assert_eq!(f.get_status(&token, "/v1/devices").await, StatusCode::UNAUTHORIZED);
    assert_eq!(f.connect(&token).await.err(), Some(401));
    let listed: Vec<Value> =
        f.http.get(format!("{}/v1/devices", f.base)).bearer_auth(&f.admin).send().await.unwrap().json().await.unwrap();
    assert!(listed.iter().all(|d| d["id"] != id.as_str()));

    // Already revoked, unknown, the hub itself: 404. The caller's own token: refused.
    assert_eq!(f.revoke(&f.admin, &id).await.0, StatusCode::NOT_FOUND);
    assert_eq!(f.revoke(&f.admin, "0123456789ab").await.0, StatusCode::NOT_FOUND);
    assert_eq!(f.revoke(&f.admin, "hub").await.0, StatusCode::NOT_FOUND);
    let (status, reply) = f.revoke(&f.admin, &f.admin_id).await;
    assert_eq!(status, StatusCode::CONFLICT, "{reply}");
    assert!(f.hub.db.authenticate(&f.admin).is_some());

    // An offline device can be revoked too.
    assert_eq!(f.revoke(&f.admin, &f.plain_id).await.0, StatusCode::OK);
    assert!(f.hub.db.authenticate(&f.plain).is_none());
}

#[tokio::test]
async fn cookie_requests_go_through_the_csrf_guard() {
    let f = fixture("cookie").await;
    let login = |token: String| {
        let http = f.http.clone();
        let base = f.base.clone();
        async move {
            let r = http
                .post(format!("{base}/v1/session"))
                .header("x-grouper", "1")
                .header("origin", &base)
                .json(&json!({"token": token}))
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::OK);
            r.headers()["set-cookie"].to_str().unwrap().split(';').next().unwrap().to_string()
        }
    };
    let admin_cookie = login(f.admin.clone()).await;
    let post = |cookie: &str, csrf: bool| {
        let mut req =
            f.http.post(format!("{}/v1/devices", f.base)).header("cookie", cookie).json(&json!({"name": "Web"}));
        if csrf {
            req = req.header("x-grouper", "1").header("origin", &f.base);
        }
        req.send()
    };
    assert_eq!(post(&admin_cookie, false).await.unwrap().status(), StatusCode::FORBIDDEN, "no X-Grouper header");
    let r = f
        .http
        .post(format!("{}/v1/devices", f.base))
        .header("cookie", &admin_cookie)
        .header("x-grouper", "1")
        .header("origin", "https://evil.example")
        .json(&json!({"name": "Web"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN, "cross-site origin");
    assert_eq!(f.hub.db.devices().len(), 2);
    let r = post(&admin_cookie, true).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let id = r.json::<Value>().await.unwrap()["device"]["id"].as_str().unwrap().to_string();

    let r = f
        .http
        .delete(format!("{}/v1/devices/{id}", f.base))
        .header("cookie", &admin_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN, "no X-Grouper header");
    let r = f
        .http
        .delete(format!("{}/v1/devices/{id}", f.base))
        .header("cookie", &admin_cookie)
        .header("x-grouper", "1")
        .header("origin", &f.base)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);

    // A web session can't revoke the device it belongs to, and a device's session isn't admin.
    let r = f
        .http
        .delete(format!("{}/v1/devices/{}", f.base, f.admin_id))
        .header("cookie", &admin_cookie)
        .header("x-grouper", "1")
        .header("origin", &f.base)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CONFLICT);
    let plain_cookie = login(f.plain.clone()).await;
    assert_eq!(post(&plain_cookie, true).await.unwrap().status(), StatusCode::FORBIDDEN);
}

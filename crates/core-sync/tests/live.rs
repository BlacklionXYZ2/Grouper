//! core-sync against a real in-process hub with a scripted model. A TCP proxy sits
//! between the client and the hub so the test can cut connections (a forced
//! disconnect, as when the hub restarts or the network drops) and refuse new ones.
//!
//! One run: sync, re-sync without duplicates, stream a live chat (deltas reach the
//! listener, only stored events reach the cache), fill a gap, reconnect after a
//! forced disconnect and catch up on what happened meanwhile (a new conversation
//! included) without duplicates, then read everything offline once the hub is gone.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use core_sync::core_protocol::{ChatRequest, Event, EventBody, ModelEntry, ProviderConfig};
use core_sync::{LiveStatus, Listener, Store, SyncClient, SyncError, TranscriptItem};
use hub::db::Db;
use hub::server::{self, Hub};
use serde_json::json;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// Generous: the first run after a build can be slow on Windows.
const WAIT: Duration = Duration::from_secs(90);

/// Forwards TCP to the hub; `cut` drops every open connection, `set_down` refuses new ones.
struct Proxy {
    url: String,
    down: Arc<AtomicBool>,
    conns: Arc<Mutex<Vec<JoinHandle<()>>>>,
    accept: JoinHandle<()>,
}

impl Proxy {
    async fn start(upstream: SocketAddr) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let down = Arc::new(AtomicBool::new(false));
        let conns: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::default();
        let (d, c) = (down.clone(), conns.clone());
        let accept = tokio::spawn(async move {
            while let Ok((mut inbound, _)) = listener.accept().await {
                if d.load(Ordering::SeqCst) {
                    drop(inbound);
                    continue;
                }
                c.lock().unwrap().push(tokio::spawn(async move {
                    if let Ok(mut out) = TcpStream::connect(upstream).await {
                        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut out).await;
                    }
                }));
            }
        });
        Self { url, down, conns, accept }
    }

    fn set_down(&self, down: bool) {
        self.down.store(down, Ordering::SeqCst);
    }

    fn cut(&self) {
        for h in self.conns.lock().unwrap().drain(..) {
            h.abort();
        }
    }
}

#[derive(Default)]
struct Recorder {
    statuses: Mutex<Vec<LiveStatus>>,
    live: Mutex<Vec<Event>>,
    stored: Mutex<Vec<Event>>,
}

impl Listener for Recorder {
    fn status(&self, status: &LiveStatus) {
        self.statuses.lock().unwrap().push(status.clone());
    }
    fn stored(&self, event: &Event) {
        self.stored.lock().unwrap().push(event.clone());
    }
    fn live(&self, event: &Event) {
        self.live.lock().unwrap().push(event.clone());
    }
}

impl Recorder {
    fn saw(&self, pred: impl Fn(&LiveStatus) -> bool) -> bool {
        self.statuses.lock().unwrap().iter().any(pred)
    }
    fn count(&self, pred: impl Fn(&LiveStatus) -> bool) -> usize {
        self.statuses.lock().unwrap().iter().filter(|s| pred(s)).count()
    }
}

async fn until<F: FnMut() -> bool>(what: &str, mut check: F) {
    tokio::time::timeout(WAIT, async {
        while !check() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting until {what}"));
}

fn script() -> serde_json::Value {
    let reply = |t: &str| json!([{"type": "text", "text": t}]);
    json!({"rules": [
        {"last": "hello one", "reply": reply("Reply one, streamed word by word.")},
        {"last": "hello two", "reply": reply("Reply two arrives live.")},
        {"last": "hello three", "reply": reply("Reply three happened while you were away.")},
        {"last": "hello four", "reply": reply("Reply four in a new chat.")},
    ]})
}

fn assistant_texts(store: &Store, conv: &str) -> Vec<String> {
    store
        .transcript(conv)
        .unwrap()
        .into_iter()
        .filter_map(|i| match i {
            TranscriptItem::Assistant { text, .. } => Some(text),
            _ => None,
        })
        .collect()
}

fn hub_seqs(hub: &Hub, conv: &str) -> Vec<i64> {
    hub.db.events(conv, 0, None).iter().map(|e| e.seq).collect()
}

fn cache_seqs(store: &Store, conv: &str) -> Vec<i64> {
    store.events(conv, 0, None).unwrap().iter().map(|e| e.seq).collect()
}

fn hub_said(hub: &Hub, conv: &str, prefix: &str) -> bool {
    hub.db.events(conv, 0, None).iter().any(|e| matches!(&e.body, EventBody::AssistantMessage { text } if text.starts_with(prefix)))
}

async fn chat(hub: &Arc<Hub>, conversation_id: Option<&str>, text: &str) -> String {
    hub.chat(ChatRequest {
        conversation_id: conversation_id.map(String::from),
        mode: Some("standard".into()),
        text: text.into(),
        model: None,
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn syncs_streams_reconnects_and_reads_offline() {
    let dir = std::env::temp_dir().join(format!("grouper-core-sync-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = Db::open(&dir.join("data")).unwrap();
    db.upsert_provider(&ProviderConfig {
        id: "s".into(),
        kind: "scripted".into(),
        base_url: None,
        api_key: None,
        options: json!({ "inline": script() }),
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
        free: true,
        roles: vec!["orchestrator".into(), "standard".into(), "thread".into()],
        local: false,
        modalities: vec![],
    })
    .unwrap();
    let (_, admin) = db.add_device("Phone", "ios", true).unwrap();
    let (_, reader) = db.add_device("Laptop", "windows", false).unwrap();
    db.add_memory("Likes green tea", &["food".into()], None).unwrap();
    let hub = Hub::new(db, dir.join("data"));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hub_addr = listener.local_addr().unwrap();
    let router = server::router(hub.clone());
    let serve = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let proxy = Proxy::start(hub_addr).await;

    // History from before this client existed.
    let c1 = chat(&hub, None, "hello one").await;
    until("the first reply is stored", || hub_said(&hub, &c1, "Reply one")).await;

    // ---- catch-up, twice
    let cache_path = dir.join("client").join("cache.db");
    let store = Arc::new(Store::open(&cache_path).unwrap());
    let client = Arc::new(SyncClient::new(&proxy.url, &admin, store.clone()).unwrap());
    let first = client.sync_once().await.unwrap();
    assert_eq!(first.conversations, 1);
    assert_eq!(first.new_events, hub_seqs(&hub, &c1).len());
    assert_eq!(first.memories, Some(1));
    assert_eq!(cache_seqs(&store, &c1), hub_seqs(&hub, &c1));
    let again = client.sync_once().await.unwrap();
    assert_eq!(again.new_events, 0, "re-running stores nothing new");
    assert_eq!(cache_seqs(&store, &c1), hub_seqs(&hub, &c1));
    assert_eq!(assistant_texts(&store, &c1), ["Reply one, streamed word by word."]);
    assert!(store.last_full_refresh().unwrap().is_some());

    // A non-admin token reads chats but not memories.
    let reader_store = Arc::new(Store::open_in_memory().unwrap());
    let reader_report = SyncClient::new(&proxy.url, &reader, reader_store.clone()).unwrap().sync_once().await.unwrap();
    assert_eq!((reader_report.conversations, reader_report.memories), (1, None));
    assert_eq!(reader_store.memories_synced().unwrap(), Some(false));

    // ---- live
    let rec = Arc::new(Recorder::default());
    let (stop, stopped) = watch::channel(false);
    let live = {
        let (client, rec) = (client.clone(), rec.clone());
        tokio::spawn(async move { client.run_live(rec, stopped).await })
    };
    until("the client is live", || rec.saw(|s| *s == LiveStatus::Live)).await;

    chat(&hub, Some(&c1), "hello two").await;
    until("the live reply is cached", || assistant_texts(&store, &c1).len() == 2).await;
    assert_eq!(assistant_texts(&store, &c1)[1], "Reply two arrives live.");
    {
        let live = rec.live.lock().unwrap();
        let streamed: String = live
            .iter()
            .filter_map(|e| match &e.body {
                EventBody::AssistantDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(streamed.contains("Reply two"), "deltas reach the listener: {streamed:?}");
        assert!(live.iter().all(|e| e.seq == 0));
    }
    assert!(rec.stored.lock().unwrap().iter().any(|e| matches!(&e.body, EventBody::UserMessage { text } if text == "hello two")));
    assert_eq!(cache_seqs(&store, &c1), hub_seqs(&hub, &c1), "deltas are never stored");

    // ---- a gap: an event stored on the hub but never broadcast, then one that is.
    let hidden = hub.db.add_event(&c1, None, EventBody::AssistantMessage { text: "Missed on the socket".into() });
    let shown = hub.emit(&c1, None, EventBody::Error { message: "after the gap".into() });
    until("the gap is filled", || cache_seqs(&store, &c1).contains(&shown.seq)).await;
    assert!(cache_seqs(&store, &c1).contains(&hidden.seq));
    assert!(rec.saw(|s| matches!(s, LiveStatus::GapFilled { seen, .. } if *seen == shown.seq)));
    assert_eq!(cache_seqs(&store, &c1), hub_seqs(&hub, &c1));

    // ---- forced disconnect: work happens while the client is cut off.
    let offline_before = rec.count(|s| matches!(s, LiveStatus::Offline { .. }));
    proxy.set_down(true);
    proxy.cut();
    until("the client notices", || rec.count(|s| matches!(s, LiveStatus::Offline { .. })) > offline_before).await;
    chat(&hub, Some(&c1), "hello three").await;
    let c2 = chat(&hub, None, "hello four").await;
    until("the hub answered both", || hub_said(&hub, &c1, "Reply three") && hub_said(&hub, &c2, "Reply four")).await;
    let lives_before = rec.count(|s| *s == LiveStatus::Live);
    proxy.set_down(false);
    until("the client reconnects", || rec.count(|s| *s == LiveStatus::Live) > lives_before).await;
    until("the catch-up has both chats", || {
        assistant_texts(&store, &c1).iter().any(|t| t.starts_with("Reply three"))
            && assistant_texts(&store, &c2).iter().any(|t| t.starts_with("Reply four"))
    })
    .await;
    assert_eq!(cache_seqs(&store, &c1), hub_seqs(&hub, &c1), "no duplicates or holes after reconnecting");
    assert_eq!(cache_seqs(&store, &c2), hub_seqs(&hub, &c2));
    assert!(store.has_conversation(&c2).unwrap());

    // And live again after the reconnect.
    chat(&hub, Some(&c2), "hello one").await;
    until("live after reconnect", || assistant_texts(&store, &c2).len() == 2).await;
    assert_eq!(cache_seqs(&store, &c2), hub_seqs(&hub, &c2));

    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(15), live).await.expect("run_live stops").unwrap().unwrap();

    // ---- the hub goes away
    let expected_c1 = hub_seqs(&hub, &c1);
    proxy.set_down(true);
    proxy.cut();
    serve.abort();
    proxy.accept.abort();
    drop(hub);
    match client.sync_once().await {
        Err(SyncError::Unreachable(_)) => {}
        other => panic!("expected unreachable, got {other:?}"),
    }
    drop(client);
    drop(store);

    // A fresh open of the same file, as after an app restart offline.
    let offline = Store::open_existing(&cache_path).unwrap().expect("cache file exists");
    let list = offline.conversation_summaries().unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].info.id, c2, "most recent first");
    assert_eq!(list[0].preview.as_ref().unwrap().text, "Reply one, streamed word by word.");
    assert!(list.iter().all(|c| c.unread > 0));
    assert_eq!(cache_seqs(&offline, &c1), expected_c1);
    assert_eq!(
        assistant_texts(&offline, &c1),
        [
            "Reply one, streamed word by word.",
            "Reply two arrives live.",
            "Missed on the socket",
            "Reply three happened while you were away."
        ]
    );
    let hits = offline.search("away", 10).unwrap();
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].conversation_id, c1);
    assert_eq!(offline.memories(Some("tea"), 10).unwrap().len(), 1);
    assert!(offline.pending_approvals(None).unwrap().is_empty());
    drop(offline);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rejected_token_stops_and_reports() {
    let dir = std::env::temp_dir().join(format!("grouper-core-sync-401-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir.join("data")).unwrap();
    let (device, _) = db.add_device("Phone", "ios", true).unwrap();
    let hub = Hub::new(db, dir.join("data"));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let router = server::router(hub.clone());
    let serve = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let store = Arc::new(Store::open_in_memory().unwrap());
    let client = SyncClient::new(&url, &format!("{}.wrong", device.id), store).unwrap();
    assert!(matches!(client.sync_once().await, Err(SyncError::Unauthorized)));
    let rec = Arc::new(Recorder::default());
    let (_stop, stopped) = watch::channel(false);
    let result = tokio::time::timeout(Duration::from_secs(15), client.run_live(rec.clone(), stopped)).await;
    assert!(matches!(result, Ok(Err(SyncError::Unauthorized))), "{result:?}");
    assert!(rec.saw(|s| *s == LiveStatus::Unauthorized));

    serve.abort();
    drop(hub);
    let _ = std::fs::remove_dir_all(&dir);
}

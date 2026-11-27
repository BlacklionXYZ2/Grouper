#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod autostart;
mod node;
mod oauth;
mod settings;
mod toasts;

use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{Context, Result};
use hub::host::HubHandle;
use node::{MoveTargetContext, NodeHandle};
use serde::Serialize;
use settings::{DEFAULT_REMOTE_HUB_URL, DesktopSettings, HostMode, LOCAL_HUB_URL, machine_device_name};
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::webview::PageLoadEvent;
use tauri::{AppHandle, Manager, State, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_notification::NotificationExt;

const LOCAL_HUB_PORT: u16 = 47600;
const DATA_DIR_ENV: &str = "GROUPER_DESKTOP_DATA_DIR";
const MAIN_WINDOW_LABEL: &str = "main";
const TRAY_OPEN_ID: &str = "open";
const TRAY_SETUP_ID: &str = "setup";
const TRAY_QUIT_ID: &str = "quit";
const PRE_MOVE_RETENTION_SECS: u64 = 7 * 24 * 60 * 60;
const PRE_MOVE_PRUNE_INTERVAL: Duration = Duration::from_secs(60 * 60);
const STEPDOWN_TOAST_TITLE: &str = "Grouper";
const STEPDOWN_TOAST_BODY: &str = "This computer is no longer the hub";

struct DesktopInner {
    data_dir: PathBuf,
    local_hub_bind: SocketAddr,
    settings: Mutex<DesktopSettings>,
    hub: Mutex<Option<Arc<HubHandle>>>,
    node: Mutex<Option<NodeHandle>>,
    /// One-time `(origin, admin_token)` traded into a session cookie via `POST /v1/session` once
    /// the WebView finishes loading that exact origin.
    pending_session: Mutex<Option<(String, String)>>,
}

#[derive(Clone)]
pub(crate) struct DesktopState {
    inner: Arc<DesktopInner>,
}

impl DesktopState {
    fn new(
        data_dir: PathBuf,
        settings: DesktopSettings,
        hub: Option<Arc<HubHandle>>,
        node: Option<NodeHandle>,
    ) -> Self {
        Self::new_with_bind(
            data_dir,
            SocketAddr::from((Ipv4Addr::LOCALHOST, LOCAL_HUB_PORT)),
            settings,
            hub,
            node,
        )
    }

    fn new_with_bind(
        data_dir: PathBuf,
        local_hub_bind: SocketAddr,
        settings: DesktopSettings,
        hub: Option<Arc<HubHandle>>,
        node: Option<NodeHandle>,
    ) -> Self {
        Self {
            inner: Arc::new(DesktopInner {
                data_dir,
                local_hub_bind,
                settings: Mutex::new(settings),
                hub: Mutex::new(hub),
                node: Mutex::new(node),
                pending_session: Mutex::new(None),
            }),
        }
    }

    fn settings_snapshot(&self) -> DesktopSettings {
        self.inner.settings.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn host_mode(&self) -> Option<HostMode> {
        self.settings_snapshot().mode
    }

    pub(crate) fn data_dir(&self) -> &Path {
        &self.inner.data_dir
    }

    pub(crate) fn local_hub_bind(&self) -> SocketAddr {
        self.inner.local_hub_bind
    }

    pub(crate) fn hub_handle(&self) -> Option<Arc<HubHandle>> {
        self.inner.hub.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn set_hub(&self, next: Option<Arc<HubHandle>>) -> Option<Arc<HubHandle>> {
        let mut guard = self.inner.hub.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::replace(&mut *guard, next)
    }

    fn take_hub(&self) -> Option<Arc<HubHandle>> {
        self.inner.hub.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    fn take_hub_if_same(&self, target: &Arc<HubHandle>) -> bool {
        let mut guard = self.inner.hub.lock().unwrap_or_else(|e| e.into_inner());
        if guard.as_ref().is_some_and(|cur| Arc::ptr_eq(cur, target)) {
            *guard = None;
            true
        } else {
            false
        }
    }

    fn replace_node(&self, next: Option<NodeHandle>) -> Option<NodeHandle> {
        let mut guard = self.inner.node.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::replace(&mut *guard, next)
    }

    fn take_node(&self) -> Option<NodeHandle> {
        self.inner.node.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    fn arm_session_handoff(&self, hub_url: &str, token: String) {
        if let Ok(parsed) = url::Url::parse(hub_url) {
            let origin = parsed.origin().ascii_serialization();
            if origin != "null" {
                *self.inner.pending_session.lock().unwrap_or_else(|e| e.into_inner()) = Some((origin, token));
            }
        }
    }

    fn take_session_handoff_for(&self, page_url: &url::Url) -> Option<String> {
        let page_origin = page_url.origin().ascii_serialization();
        let mut guard = self.inner.pending_session.lock().unwrap_or_else(|e| e.into_inner());
        if guard.as_ref().is_some_and(|(origin, _)| *origin == page_origin) {
            return guard.take().map(|(_, tok)| tok);
        }
        None
    }
}

#[derive(Serialize)]
struct SetupStateView {
    mode: Option<HostMode>,
    remote_hub_url: String,
    default_remote_hub_url: &'static str,
    local_hub_url: &'static str,
    google_desktop_configured: bool,
    /// False until this PC's hub has an owner. "Continue" on This computer stays hidden until then.
    local_hub_claimed: bool,
}

#[tauri::command]
fn setup(state: State<'_, DesktopState>) -> SetupStateView {
    let s = state.settings_snapshot();
    SetupStateView {
        mode: s.mode,
        remote_hub_url: s.resolved_remote_hub_url(),
        default_remote_hub_url: DEFAULT_REMOTE_HUB_URL,
        local_hub_url: LOCAL_HUB_URL,
        google_desktop_configured: oauth::resolve_oauth_config(&s).is_some(),
        local_hub_claimed: local_hub_claimed(state.inner()),
    }
}

#[tauri::command]
async fn choose(
    app: AppHandle,
    state: State<'_, DesktopState>,
    mode: HostMode,
    remote_hub_url: Option<String>,
    sign_in_with_google: bool,
) -> std::result::Result<(), String> {
    apply_choice(&app, &state, mode, remote_hub_url.as_deref(), sign_in_with_google)
        .await
        .map_err(|e| format!("{e:#}"))
}

async fn apply_choice(
    app: &AppHandle,
    state: &DesktopState,
    mode: HostMode,
    remote_hub_url: Option<&str>,
    sign_in_with_google: bool,
) -> Result<()> {
    if mode == HostMode::ThisComputer && !sign_in_with_google && !local_hub_claimed(state) {
        anyhow::bail!(
            "This computer has no owner yet. Sign in with Google to claim it before continuing."
        );
    }

    let mut updated = state.settings_snapshot();
    updated.set_choice(mode, remote_hub_url)?;
    let target_url = updated.active_hub_url().expect("mode was just set");

    if let Some(prev_node) = state.take_node() {
        prev_node.stop().await;
    }

    // Google's return listens on its own loopback port, so sign-in leaves the hub running.
    // Stop the embedded hub only when this PC is no longer the one hosting it.
    if mode == HostMode::AnotherComputer
        && let Some(prev) = state.take_hub()
    {
        prev.stop().await;
    }

    let next_node = match mode {
        HostMode::ThisComputer => {
            let hub = if sign_in_with_google {
                let oauth_cfg = oauth::resolve_oauth_config(&updated).context(
                    "GROUPER_GOOGLE_DESKTOP_CLIENT_ID is not set yet (see TODO in src/oauth.rs)",
                )?;
                let nonce = hub::db::random_hex(16);
                let id_token = oauth::obtain_google_id_token(&oauth_cfg, &nonce).await?;
                let hub = ensure_embedded_hub(Some(app), state).await?;
                let device_name = machine_device_name();
                let (_owner, issued) = hub
                    .claim_local_client(&id_token, "windows", Some(&device_name), Some(&updated.install_id))
                    .await
                    .context("claiming local hub with Google account")?;
                updated.set_admin_token(&target_url, &issued.token)?;
                state.arm_session_handoff(&target_url, issued.token);
                hub
            } else {
                let hub = ensure_embedded_hub(Some(app), state).await?;
                if let Ok(Some(saved_tok)) = updated.admin_token_for(&target_url) {
                    state.arm_session_handoff(&target_url, saved_tok);
                }
                hub
            };
            let node_token = node::ensure_local_node_token(&hub, &target_url, &mut updated)?;
            Some(NodeHandle::start(&target_url, node_token, state.data_dir()))
        }
        HostMode::AnotherComputer => {
            if sign_in_with_google {
                let oauth_cfg = oauth::resolve_oauth_config(&updated).context(
                    "GROUPER_GOOGLE_DESKTOP_CLIENT_ID is not set yet (see TODO in src/oauth.rs)",
                )?;
                let device_name = machine_device_name();
                let token =
                    oauth::sign_in_remote_hub(&target_url, &oauth_cfg, &device_name, &updated.install_id).await?;
                updated.set_admin_token(&target_url, &token)?;
                state.arm_session_handoff(&target_url, token);
            } else if let Ok(Some(saved_tok)) = updated.admin_token_for(&target_url) {
                state.arm_session_handoff(&target_url, saved_tok);
            }
            match node::resolve_remote_node_token(&target_url, &mut updated).await {
                Ok(Some(node_token)) => {
                    let confirm_addr = updated.resolved_remote_hub_url();
                    Some(start_client_node(
                        state,
                        Some(app.clone()),
                        &target_url,
                        node_token,
                        confirm_addr,
                    ))
                }
                Ok(None) => None,
                Err(err) => {
                    tracing::warn!("could not start remote desktop node yet: {err:#}");
                    None
                }
            }
        }
    };

    if let Some(prev) = state.replace_node(next_node) {
        prev.stop().await;
    }

    updated.save(state.data_dir())?;
    *state.inner.settings.lock().unwrap_or_else(|e| e.into_inner()) = updated;

    autostart::apply_default(app);

    open_target_in_main_window(app, WebviewUrl::External(target_url.parse()?))?;
    Ok(())
}

/// Whether the local hub database already has an owner. A missing database is unclaimed.
/// Uses the running hub when this process holds it, so a second open does not run migrations.
fn local_hub_claimed(state: &DesktopState) -> bool {
    if let Some(handle) = state.hub_handle().as_ref()
        && let Some(hub) = handle.hub()
    {
        return hub.db.owner().is_some();
    }
    let dir = hub_data_dir(state.data_dir());
    if !dir.join("grouper.db").is_file() {
        return false;
    }
    match hub::db::Db::open(&dir) {
        Ok(db) => db.owner().is_some(),
        Err(err) => {
            tracing::warn!("could not read local hub owner: {err:#}");
            false
        }
    }
}

async fn ensure_embedded_hub(app: Option<&AppHandle>, state: &DesktopState) -> Result<Arc<HubHandle>> {
    if let Some(existing) = state.hub_handle() {
        return Ok(existing);
    }
    let hub_dir = hub_data_dir(state.data_dir());
    let bind = state.local_hub_bind();
    let install_id = state.settings_snapshot().install_id;
    let handle = hub::host::start_with_install(&hub_dir, &[bind], Some(&install_id))
        .await
        .with_context(|| format!("starting embedded hub on {bind}"))?;
    tracing::info!(
        client_version = core_client::CLIENT_VERSION,
        data_dir = %hub_dir.display(),
        "embedded local hub started"
    );
    let arc = Arc::new(handle);
    if let Some(prev) = state.set_hub(Some(arc.clone())) {
        prev.stop().await;
    }
    spawn_stepdown_watcher(state.clone(), app.cloned(), arc.clone());
    Ok(arc)
}

/// Watch a running embedded hub for step-down (either at the end of an outgoing move or on seeing
/// a higher epoch at the public address) and switch this desktop install to client mode.
fn spawn_stepdown_watcher(state: DesktopState, app: Option<AppHandle>, hub_handle: Arc<HubHandle>) {
    tauri::async_runtime::spawn(async move {
        let Some(hub) = hub_handle.hub() else {
            return;
        };
        let mut rx = hub.stepdown().subscribe();
        let info = if let Some(existing) = rx.borrow_and_update().clone() {
            existing
        } else {
            match rx.wait_for(|v| v.is_some()).await {
                Ok(guard) => match guard.clone() {
                    Some(info) => info,
                    None => return,
                },
                Err(_) => return,
            }
        };

        if hub.migrate().status().state.is_active() {
            let mut move_rx = hub.migrate().subscribe();
            let _ = tokio::time::timeout(
                Duration::from_secs(6),
                move_rx.wait_for(|s| !s.state.is_active()),
            )
            .await;
        }

        let public_addr = if info.address.trim().is_empty() {
            hub.db.public_address()
        } else {
            info.address
        };
        drop(rx);
        drop(hub);

        if let Err(err) = step_down_hub_to_client(&state, app.as_ref(), hub_handle, &public_addr).await {
            tracing::error!("failed switching stepped-down hub to client mode: {err:#}");
        }
    });
}

async fn step_down_hub_to_client(
    state: &DesktopState,
    app: Option<&AppHandle>,
    stepped_hub: Arc<HubHandle>,
    public_address: &str,
) -> Result<Option<String>> {
    if !state.take_hub_if_same(&stepped_hub) {
        return Ok(None);
    }

    if let Some(prev_node) = state.take_node() {
        prev_node.stop().await;
    }

    stepped_hub.stop().await;
    drop(stepped_hub);

    let now_secs = hub::db::now();
    if let Err(err) = archive_stepped_down_hub_data(state.data_dir(), now_secs).await {
        tracing::warn!("could not archive stepped-down hub data folder: {err:#}");
    }
    let _ = prune_expired_pre_move_dirs(state.data_dir(), now_secs);

    let mut updated = state.settings_snapshot();
    let remote_url = updated.persist_client_mode(state.data_dir(), Some(public_address))?;

    let next_node = match node::resolve_remote_node_token(&remote_url, &mut updated).await {
        Ok(Some(node_token)) => {
            let confirm_addr = updated.resolved_remote_hub_url();
            Some(start_client_node(
                state,
                app.cloned(),
                &remote_url,
                node_token,
                confirm_addr,
            ))
        }
        Ok(None) => None,
        Err(err) => {
            tracing::warn!("could not start remote desktop node after step-down: {err:#}");
            None
        }
    };
    updated.save(state.data_dir())?;
    if let Ok(Some(saved_tok)) = updated.admin_token_for(&remote_url) {
        state.arm_session_handoff(&remote_url, saved_tok);
    }
    *state.inner.settings.lock().unwrap_or_else(|e| e.into_inner()) = updated;

    if let Some(prev) = state.replace_node(next_node) {
        prev.stop().await;
    }

    tracing::info!(
        remote_hub_url = %remote_url,
        "embedded hub stepped down; switched desktop app to client mode"
    );

    if let Some(app) = app {
        show_stepdown_toast(app);
        if let Ok(url) = remote_url.parse() {
            let _ = open_target_in_main_window(app, WebviewUrl::External(url));
        }
    }
    Ok(Some(remote_url))
}

fn start_client_node(
    state: &DesktopState,
    app: Option<AppHandle>,
    remote_url: &str,
    node_token: String,
    confirm_address: String,
) -> NodeHandle {
    let ctx = build_move_target_context(state, app, remote_url, &node_token, confirm_address);
    NodeHandle::start_client(remote_url, node_token, state.data_dir(), ctx)
}

fn build_move_target_context(
    state: &DesktopState,
    app: Option<AppHandle>,
    remote_url: &str,
    node_token: &str,
    confirm_address: String,
) -> MoveTargetContext {
    let state_for_promote = state.clone();
    MoveTargetContext::new(
        remote_url.to_string(),
        node_token.to_string(),
        target_hub_install_dir(state.data_dir()),
        state.local_hub_bind(),
        confirm_address,
        Arc::new(move |new_hub: HubHandle| {
            let state = state_for_promote.clone();
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(err) = promote_target_to_hub(&state, app.as_ref(), Arc::new(new_hub)).await {
                    tracing::error!("failed switching desktop app to hub mode after move: {err:#}");
                }
            });
        }),
    )
}

/// Switch this desktop install from client mode (`AnotherComputer`) to hub mode (`ThisComputer`)
/// once `execute_target_move` has installed `%LOCALAPPDATA%\Grouper\hub`, started the embedded hub,
/// and confirmed serving.
async fn promote_target_to_hub(
    state: &DesktopState,
    app: Option<&AppHandle>,
    new_hub: Arc<HubHandle>,
) -> Result<String> {
    let bound = new_hub.binds().first().copied();
    let local_url = match bound {
        Some(addr) if addr.port() == LOCAL_HUB_PORT => LOCAL_HUB_URL.to_string(),
        Some(addr) => format!("http://{addr}"),
        None => LOCAL_HUB_URL.to_string(),
    };

    if let Some(prev_hub) = state.set_hub(Some(Arc::clone(&new_hub))) {
        prev_hub.stop().await;
    }

    let mut updated = state.settings_snapshot();
    updated.persist_hub_mode(state.data_dir(), &local_url)?;
    let node_tok = node::ensure_local_node_token(&new_hub, &local_url, &mut updated)?;
    updated.save(state.data_dir())?;
    if let Ok(Some(saved_tok)) = updated.admin_token_for(&local_url) {
        state.arm_session_handoff(&local_url, saved_tok);
    }
    *state.inner.settings.lock().unwrap_or_else(|e| e.into_inner()) = updated;

    let next_node = Some(NodeHandle::start(&local_url, node_tok, state.data_dir()));
    if let Some(prev_node) = state.replace_node(next_node) {
        prev_node.stop().await;
    }

    spawn_stepdown_watcher(state.clone(), app.cloned(), new_hub);

    if let Some(app) = app {
        autostart::apply_default(app);
        if let Ok(url) = local_url.parse() {
            let _ = open_target_in_main_window(app, WebviewUrl::External(url));
        }
    }
    Ok(local_url)
}

fn target_hub_install_dir(app_data: &Path) -> PathBuf {
    app_data.join("hub")
}

fn show_stepdown_toast(app: &AppHandle) {
    if let Err(err) = app
        .notification()
        .builder()
        .title(STEPDOWN_TOAST_TITLE)
        .body(STEPDOWN_TOAST_BODY)
        .show()
    {
        tracing::warn!("could not show step-down toast: {err}");
    }
}

fn unique_pre_move_dir(app_data: &Path, now_secs: u64) -> PathBuf {
    let candidate = app_data.join(format!("pre-move-{now_secs}"));
    if !candidate.exists() {
        candidate
    } else {
        app_data.join(format!("pre-move-{now_secs}-{}", hub::db::random_hex(2)))
    }
}

/// Move the stepped-down hub's data and secrets aside into `pre-move-<time>` for 7-day retention.
async fn archive_stepped_down_hub_data(app_data: &Path, now_secs: u64) -> Result<Option<PathBuf>> {
    let sub_hub = app_data.join("hub");
    if sub_hub.exists() {
        let dest = unique_pre_move_dir(app_data, now_secs);
        let mut last_err = None;
        for attempt in 0..10 {
            match std::fs::rename(&sub_hub, &dest) {
                Ok(()) => return Ok(Some(dest)),
                Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied && attempt < 9 => {
                    last_err = Some(err);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(err) => {
                    return Err(err).with_context(|| {
                        format!("renaming {} to {}", sub_hub.display(), dest.display())
                    });
                }
            }
        }
        if let Some(err) = last_err {
            return Err(err).with_context(|| {
                format!("renaming {} to {}", sub_hub.display(), dest.display())
            });
        }
    } else if app_data.join("grouper.db").is_file() {
        let dest = unique_pre_move_dir(app_data, now_secs);
        std::fs::create_dir_all(&dest)
            .with_context(|| format!("creating {}", dest.display()))?;
        for entry in std::fs::read_dir(app_data)
            .with_context(|| format!("reading {}", app_data.display()))?
            .flatten()
        {
            let name = entry.file_name();
            let s = name.to_string_lossy();
            let is_hub_artifact = matches!(
                s.as_ref(),
                "grouper.db"
                    | "grouper.db-wal"
                    | "grouper.db-shm"
                    | "grouper.lock"
                    | "master.key"
                    | "master.key.dpapi"
                    | "identity.key"
                    | "workspaces"
            ) || s.ends_with(".marker");
            if is_hub_artifact {
                let _ = std::fs::rename(entry.path(), dest.join(name));
            }
        }
        return Ok(Some(dest));
    }
    Ok(None)
}

/// Delete `pre-move-<time>` folders in `app_data` that have reached the 7-day retention limit.
fn prune_expired_pre_move_dirs(app_data: &Path, now_secs: u64) -> usize {
    let Ok(entries) = std::fs::read_dir(app_data) else {
        return 0;
    };
    let mut deleted = 0usize;
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if !ft.is_dir() {
            continue;
        }
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        let Some(rest) = name.strip_prefix("pre-move-") else {
            continue;
        };
        let parsed_ts = rest
            .split('-')
            .next()
            .and_then(|digits| digits.parse::<u64>().ok())
            .or_else(|| {
                entry
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
            });
        if let Some(created_secs) = parsed_ts
            && now_secs.saturating_sub(created_secs) >= PRE_MOVE_RETENTION_SECS
            && std::fs::remove_dir_all(entry.path()).is_ok()
        {
            deleted += 1;
        }
    }
    deleted
}

fn app_data_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os(DATA_DIR_ENV).filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    let local = dirs::data_local_dir().context("Windows LOCALAPPDATA directory is unavailable")?;
    Ok(local.join("Grouper"))
}

fn hub_data_dir(app_data: &Path) -> PathBuf {
    if app_data.join("grouper.db").is_file() {
        app_data.to_path_buf()
    } else {
        app_data.join("hub")
    }
}

fn initial_webview_url(settings: &DesktopSettings) -> WebviewUrl {
    match settings.active_hub_url().and_then(|u| u.parse::<url::Url>().ok()) {
        Some(url) => WebviewUrl::External(url),
        None => WebviewUrl::App("index.html".into()),
    }
}

fn open_target_in_main_window(app: &AppHandle, target: WebviewUrl) -> tauri::Result<()> {
    // Recreate when switching between local Tauri setup (`WebviewUrl::App`) and external hub URLs
    // so Tauri's `local: true` capability boundary applies cleanly only to `index.html`.
    if let Some(existing) = app.get_webview_window(MAIN_WINDOW_LABEL) {
        if let WebviewUrl::External(url) = &target {
            let current_is_app = existing
                .url()
                .ok()
                .is_some_and(|u| u.scheme() == "tauri" || u.host_str() == Some("tauri.localhost"));
            if !current_is_app {
                let _ = existing.navigate(url.clone());
                let _ = existing.unminimize();
                existing.show()?;
                let _ = existing.set_focus();
                return Ok(());
            }
        }
        let _ = existing.destroy();
    }
    build_main_window(app, target)
}

fn show_main_window(app: &AppHandle) -> tauri::Result<()> {
    if let Some(win) = app.get_webview_window(MAIN_WINDOW_LABEL) {
        let _ = win.unminimize();
        win.show()?;
        let _ = win.set_focus();
        return Ok(());
    }
    let target = app
        .try_state::<DesktopState>()
        .map(|s| initial_webview_url(&s.settings_snapshot()))
        .unwrap_or_else(|| WebviewUrl::App("index.html".into()));
    build_main_window(app, target)
}

fn build_main_window(app: &AppHandle, target: WebviewUrl) -> tauri::Result<()> {
    let app_for_load = app.clone();
    let win = WebviewWindowBuilder::new(app, MAIN_WINDOW_LABEL, target)
        .title("Grouper")
        .inner_size(1180.0, 800.0)
        .min_inner_size(420.0, 520.0)
        .on_page_load(move |webview, payload| {
            if payload.event() != PageLoadEvent::Finished {
                return;
            }
            let Some(state) = app_for_load.try_state::<DesktopState>() else {
                return;
            };
            let Some(token) = state.take_session_handoff_for(payload.url()) else {
                return;
            };
            if let Ok(token_json) = serde_json::to_string(&token) {
                let js = format!(
                    "(async()=>{{try{{const s=await fetch('/v1/session',{{credentials:'same-origin'}});\
                     if(s.ok){{const j=await s.json();if(j&&j.signed_in)return;}}\
                     const r=await fetch('/v1/session',{{method:'POST',credentials:'same-origin',\
                     headers:{{'Content-Type':'application/json','X-Grouper':'1'}},\
                     body:JSON.stringify({{token:{token_json}}})}});\
                     if(r.ok)location.reload();}}catch(_){{}}}})();"
                );
                let _ = webview.eval(&js);
            }
        })
        .build()?;
    let win_for_close = win.clone();
    win.on_window_event(move |event| {
        if let WindowEvent::CloseRequested { api, .. } = event {
            api.prevent_close();
            let _ = win_for_close.hide();
        }
    });
    Ok(())
}

fn install_tray(app: &AppHandle) -> tauri::Result<()> {
    let open_item = MenuItem::with_id(app, TRAY_OPEN_ID, "Open", true, None::<&str>)?;
    let setup_item = MenuItem::with_id(app, TRAY_SETUP_ID, "Where Grouper runs…", true, None::<&str>)?;
    let login_item = autostart::menu_item(app)?;
    let quit_item = MenuItem::with_id(app, TRAY_QUIT_ID, "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open_item, &setup_item, &login_item, &quit_item])?;
    let icon = Image::from_bytes(include_bytes!("../icons/32x32.png"))?;

    TrayIconBuilder::with_id("grouper-tray")
        .icon(icon)
        .tooltip("Grouper")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            TRAY_OPEN_ID => {
                let _ = show_main_window(app);
            }
            TRAY_SETUP_ID => {
                let _ = open_target_in_main_window(app, WebviewUrl::App("index.html".into()));
            }
            autostart::TRAY_ID => autostart::toggle(app),
            TRAY_QUIT_ID => {
                let handle = app.clone();
                tauri::async_runtime::spawn(async move {
                    if let Some(state) = handle.try_state::<DesktopState>() {
                        if let Some(node) = state.take_node() {
                            node.stop().await;
                        }
                        if let Some(hub) = state.take_hub() {
                            hub.stop().await;
                        }
                    }
                    handle.exit(0);
                });
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let _ = show_main_window(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    tauri::Builder::default()
        // First plugin: a second launch never starts another process. It focuses this window.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            let _ = show_main_window(app);
        }))
        .plugin(autostart::plugin())
        .plugin(tauri_plugin_notification::init())
        .invoke_handler(tauri::generate_handler![setup, choose])
        .setup(|app| {
            let data_dir = app_data_dir()?;
            std::fs::create_dir_all(&data_dir)
                .with_context(|| format!("creating {}", data_dir.display()))?;
            let _ = prune_expired_pre_move_dirs(&data_dir, hub::db::now());
            let mut settings = DesktopSettings::load(&data_dir)?;

            let (initial_hub, initial_local_node, initial_remote_node) = match settings.mode {
                Some(HostMode::ThisComputer) => {
                    let hub_dir = hub_data_dir(&data_dir);
                    let bind = SocketAddr::from((Ipv4Addr::LOCALHOST, LOCAL_HUB_PORT));
                    let handle = tauri::async_runtime::block_on(hub::host::start_with_install(
                        &hub_dir,
                        &[bind],
                        Some(&settings.install_id),
                    ))
                    .with_context(|| format!("starting embedded hub on {bind}"))?;
                    tracing::info!(
                        client_version = core_client::CLIENT_VERSION,
                        data_dir = %hub_dir.display(),
                        "embedded local hub started"
                    );
                    let node_tok = node::ensure_local_node_token(&handle, LOCAL_HUB_URL, &mut settings)?;
                    settings.save(&data_dir)?;
                    let node_handle = NodeHandle::start(LOCAL_HUB_URL, node_tok, &data_dir);
                    (Some(Arc::new(handle)), Some(node_handle), None)
                }
                Some(HostMode::AnotherComputer) => {
                    let remote_url = settings.resolved_remote_hub_url();
                    let remote_tok = match tauri::async_runtime::block_on(node::resolve_remote_node_token(
                        &remote_url,
                        &mut settings,
                    )) {
                        Ok(Some(tok)) => {
                            let _ = settings.save(&data_dir);
                            Some((remote_url, tok))
                        }
                        Ok(None) => None,
                        Err(err) => {
                            tracing::warn!("could not start remote desktop node on launch: {err:#}");
                            None
                        }
                    };
                    (None, None, remote_tok)
                }
                None => (None, None, None),
            };

            let target = initial_webview_url(&settings);
            let initial_hub_for_watch = initial_hub.clone();
            let desktop_state =
                DesktopState::new(data_dir.clone(), settings.clone(), initial_hub, initial_local_node);
            if let Some((remote_url, tok)) = initial_remote_node {
                let confirm_addr = settings.resolved_remote_hub_url();
                let node_handle = start_client_node(
                    &desktop_state,
                    Some(app.handle().clone()),
                    &remote_url,
                    tok,
                    confirm_addr,
                );
                let _ = desktop_state.replace_node(Some(node_handle));
            }
            if let Some(hub_url) = settings.active_hub_url()
                && let Ok(Some(saved_tok)) = settings.admin_token_for(&hub_url)
            {
                desktop_state.arm_session_handoff(&hub_url, saved_tok);
            }
            app.manage(desktop_state.clone());

            if let Some(hub_arc) = initial_hub_for_watch {
                spawn_stepdown_watcher(desktop_state, Some(app.handle().clone()), hub_arc);
            }
            tauri::async_runtime::spawn(async move {
                let mut ticker = tokio::time::interval(PRE_MOVE_PRUNE_INTERVAL);
                ticker.tick().await;
                loop {
                    ticker.tick().await;
                    let _ = prune_expired_pre_move_dirs(&data_dir, hub::db::now());
                }
            });

            autostart::apply_default(app.handle());
            install_tray(app.handle())?;
            toasts::start(app.handle());
            build_main_window(app.handle(), target)?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running Grouper desktop app");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stepdown_switches_to_client_mode_persists_reconnects_node_and_keeps_old_data_seven_days() {
        let root = std::env::temp_dir().join(format!("grouper-desktop-stepdown-{}", hub::db::random_hex(4)));
        let app_dir = root.join("app");
        let hub_dir = app_dir.join("hub");
        let peer_dir = root.join("peer-hub");
        std::fs::create_dir_all(&app_dir).unwrap();

        let loopback = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
        let mut settings = DesktopSettings::load(&app_dir).unwrap();
        settings.set_choice(HostMode::ThisComputer, None).unwrap();

        // Start embedded local hub in `<app_dir>/hub`.
        let local_handle = Arc::new(
            hub::host::start_with_install(&hub_dir, &[loopback], Some(&settings.install_id))
                .await
                .unwrap(),
        );
        let local_url = format!("http://{}", local_handle.binds()[0]);
        let node_tok = node::ensure_local_node_token(&local_handle, &local_url, &mut settings).unwrap();

        // Start replacement hub at `peer_url` with an admin token saved on this install so
        // step-down carries tokens across to `peer_url` and reconnects the desktop node there.
        let peer_handle = hub::host::start(&peer_dir, &[loopback]).await.unwrap();
        let peer_hub = peer_handle.hub().unwrap();
        let (_peer_admin, peer_admin_tok) = peer_hub.db.add_device("Oscar PC UI", "windows", true).unwrap();
        settings.set_admin_token(&local_url, &peer_admin_tok).unwrap();
        settings.save(&app_dir).unwrap();

        let peer_url = format!("http://{}", peer_handle.binds()[0]);
        let initial_node = NodeHandle::start(&local_url, node_tok, &app_dir);
        let state = DesktopState::new_with_bind(
            app_dir.clone(),
            loopback,
            settings,
            Some(local_handle.clone()),
            Some(initial_node),
        );

        let switched_to = step_down_hub_to_client(&state, None, local_handle, &peer_url)
            .await
            .unwrap();
        assert_eq!(switched_to.as_deref(), Some(peer_url.as_str()));
        assert_eq!(state.host_mode(), Some(HostMode::AnotherComputer));
        assert!(state.hub_handle().is_none(), "embedded hub must be stopped after step-down");
        assert_eq!(STEPDOWN_TOAST_BODY, "This computer is no longer the hub");

        // Verify persistence on disk and session handoff arming.
        let persisted = DesktopSettings::load(&app_dir).unwrap();
        assert_eq!(persisted.mode, Some(HostMode::AnotherComputer));
        assert_eq!(persisted.active_hub_url().as_deref(), Some(peer_url.as_str()));
        assert_eq!(
            persisted.admin_token_for(&peer_url).unwrap().as_deref(),
            Some(peer_admin_tok.as_str())
        );
        assert_eq!(
            state.take_session_handoff_for(&peer_url.parse().unwrap()).as_deref(),
            Some(peer_admin_tok.as_str())
        );

        // Verify the desktop node reconnected to the public hub address.
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if peer_hub.devices().await.iter().any(|d| d.online) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
        })
        .await
        .expect("stepped-down desktop node connected to replacement hub");

        // Verify old hub data folder was moved aside as `pre-move-<time>` and kept for 7 days.
        assert!(!hub_dir.exists(), "active hub folder should be moved aside to pre-move-*");
        let pre_move_dirs: Vec<PathBuf> = std::fs::read_dir(&app_dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("pre-move-")))
            .collect();
        assert_eq!(pre_move_dirs.len(), 1);
        assert!(pre_move_dirs[0].join("grouper.db").is_file());

        let archived_name = pre_move_dirs[0].file_name().unwrap().to_string_lossy().to_string();
        let archived_ts: u64 = archived_name
            .strip_prefix("pre-move-")
            .unwrap()
            .split('-')
            .next()
            .unwrap()
            .parse()
            .unwrap();

        // Kept before 7 days; pruned at >= 7 days.
        assert_eq!(prune_expired_pre_move_dirs(&app_dir, archived_ts + 6 * 24 * 60 * 60), 0);
        assert!(pre_move_dirs[0].exists());
        assert_eq!(prune_expired_pre_move_dirs(&app_dir, archived_ts + PRE_MOVE_RETENTION_SECS), 1);
        assert!(!pre_move_dirs[0].exists());

        if let Some(n) = state.take_node() {
            n.stop().await;
        }
        peer_handle.stop().await;
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn promote_target_to_hub_switches_client_to_hub_mode_persists_and_connects_local_node() {
        let root = std::env::temp_dir().join(format!("grouper-desktop-promote-{}", hub::db::random_hex(4)));
        let app_dir = root.join("app");
        let old_hub_dir = root.join("old-hub");
        let target_hub_dir = target_hub_install_dir(&app_dir);
        std::fs::create_dir_all(&app_dir).unwrap();

        let loopback = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
        let mut settings = DesktopSettings::load(&app_dir).unwrap();

        // Old remote hub with this PC's admin and node device rows already registered.
        let old_handle = hub::host::start(&old_hub_dir, &[loopback]).await.unwrap();
        let old_hub = old_handle.hub().unwrap();
        let old_url = format!("http://{}", old_handle.binds()[0]);
        let admin_issued = old_hub
            .db
            .issue_device(
                "Oscar PC",
                "windows",
                true,
                Some(hub::db::Install { id: &settings.install_id, kind: "windows" }),
                None,
            )
            .unwrap();
        let node_issued = old_hub
            .db
            .issue_device(
                "Oscar PC",
                "windows",
                false,
                Some(hub::db::Install { id: &settings.install_id, kind: hub::db::NODE_INSTALL }),
                None,
            )
            .unwrap();

        settings.set_choice(HostMode::AnotherComputer, Some(&old_url)).unwrap();
        settings.set_admin_token(&old_url, &admin_issued.token).unwrap();
        settings.set_node_token(&old_url, &node_issued.token).unwrap();
        settings.save(&app_dir).unwrap();

        let state = DesktopState::new_with_bind(app_dir.clone(), loopback, settings.clone(), None, None);
        let confirm_addr = settings.resolved_remote_hub_url();
        let ctx = build_move_target_context(&state, None, &old_url, &node_issued.token, confirm_addr.clone());
        assert_eq!(ctx.confirm_address, confirm_addr);
        assert_eq!(ctx.hub_data_dir, app_dir.join("hub"));
        let remote_node = NodeHandle::start_client(&old_url, node_issued.token.clone(), &app_dir, ctx);
        let _ = state.replace_node(Some(remote_node));

        // Simulate `execute_target_move` completing installation into `<app_dir>/hub` and starting the new hub.
        let snap = root.join("move.tar.gz");
        hub::backup::backup(&old_hub_dir, &snap, true).unwrap();
        // Copy old hub's master key via backup restore into target_hub_dir:
        let new_handle = Arc::new(
            hub::host::start_with_install(&target_hub_dir, &[loopback], Some(&settings.install_id))
                .await
                .unwrap(),
        );
        let new_hub = new_handle.hub().unwrap();
        let new_admin = new_hub
            .db
            .issue_device(
                "Oscar PC",
                "windows",
                true,
                Some(hub::db::Install { id: &settings.install_id, kind: "windows" }),
                None,
            )
            .unwrap();
        {
            let mut s = state.settings_snapshot();
            s.set_admin_token(&old_url, &new_admin.token).unwrap();
            s.save(&app_dir).unwrap();
            *state.inner.settings.lock().unwrap() = s;
        }

        let local_url = promote_target_to_hub(&state, None, new_handle.clone()).await.unwrap();
        assert_eq!(state.host_mode(), Some(HostMode::ThisComputer));
        assert!(state.hub_handle().is_some());

        // Verify desktop.json persisted ThisComputer mode and carried tokens to local_url.
        let reloaded = DesktopSettings::load(&app_dir).unwrap();
        assert_eq!(reloaded.mode, Some(HostMode::ThisComputer));
        assert_eq!(reloaded.active_hub_url().as_deref(), Some(LOCAL_HUB_URL));
        assert_eq!(
            state.take_session_handoff_for(&local_url.parse().unwrap()).as_deref(),
            Some(new_admin.token.as_str())
        );

        // Verify local node switched from old remote hub to the new embedded hub.
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if new_hub.devices().await.iter().any(|d| d.online) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
        })
        .await
        .expect("local node connected to newly promoted embedded hub");

        if let Some(n) = state.take_node() {
            n.stop().await;
        }
        if let Some(h) = state.take_hub() {
            h.stop().await;
        }
        old_handle.stop().await;
        let _ = std::fs::remove_dir_all(&root);
    }
}

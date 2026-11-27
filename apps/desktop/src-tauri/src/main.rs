#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod node;
mod oauth;
mod settings;

use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use hub::host::HubHandle;
use node::NodeHandle;
use serde::Serialize;
use settings::{DEFAULT_REMOTE_HUB_URL, DesktopSettings, HostMode, LOCAL_HUB_URL, machine_device_name};
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::webview::PageLoadEvent;
use tauri::{AppHandle, Manager, State, WebviewUrl, WebviewWindowBuilder, WindowEvent};

const LOCAL_HUB_PORT: u16 = 47600;
const DATA_DIR_ENV: &str = "GROUPER_DESKTOP_DATA_DIR";
const MAIN_WINDOW_LABEL: &str = "main";
const TRAY_OPEN_ID: &str = "open";
const TRAY_SETUP_ID: &str = "setup";
const TRAY_QUIT_ID: &str = "quit";

struct DesktopState {
    data_dir: PathBuf,
    settings: Mutex<DesktopSettings>,
    hub: Mutex<Option<Arc<HubHandle>>>,
    node: Mutex<Option<NodeHandle>>,
    /// One-time `(origin, admin_token)` traded into a session cookie via `POST /v1/session` once
    /// the WebView finishes loading that exact origin.
    pending_session: Mutex<Option<(String, String)>>,
}

impl DesktopState {
    fn new(
        data_dir: PathBuf,
        settings: DesktopSettings,
        hub: Option<Arc<HubHandle>>,
        node: Option<NodeHandle>,
    ) -> Self {
        Self {
            data_dir,
            settings: Mutex::new(settings),
            hub: Mutex::new(hub),
            node: Mutex::new(node),
            pending_session: Mutex::new(None),
        }
    }

    fn settings_snapshot(&self) -> DesktopSettings {
        self.settings.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn set_hub(&self, next: Option<Arc<HubHandle>>) -> Option<Arc<HubHandle>> {
        let mut guard = self.hub.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::replace(&mut *guard, next)
    }

    fn take_hub(&self) -> Option<Arc<HubHandle>> {
        self.hub.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    fn replace_node(&self, next: Option<NodeHandle>) -> Option<NodeHandle> {
        let mut guard = self.node.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::replace(&mut *guard, next)
    }

    fn take_node(&self) -> Option<NodeHandle> {
        self.node.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    fn arm_session_handoff(&self, hub_url: &str, token: String) {
        if let Ok(parsed) = url::Url::parse(hub_url) {
            let origin = parsed.origin().ascii_serialization();
            if origin != "null" {
                *self.pending_session.lock().unwrap_or_else(|e| e.into_inner()) = Some((origin, token));
            }
        }
    }

    fn take_session_handoff_for(&self, page_url: &url::Url) -> Option<String> {
        let page_origin = page_url.origin().ascii_serialization();
        let mut guard = self.pending_session.lock().unwrap_or_else(|e| e.into_inner());
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
                let hub = ensure_embedded_hub(state).await?;
                let device_name = machine_device_name();
                let (_owner, issued) = hub
                    .claim_local_client(&id_token, "windows", Some(&device_name), Some(&updated.install_id))
                    .await
                    .context("claiming local hub with Google account")?;
                updated.set_admin_token(&target_url, &issued.token)?;
                state.arm_session_handoff(&target_url, issued.token);
                hub
            } else {
                let hub = ensure_embedded_hub(state).await?;
                if let Ok(Some(saved_tok)) = updated.admin_token_for(&target_url) {
                    state.arm_session_handoff(&target_url, saved_tok);
                }
                hub
            };
            let node_token = node::ensure_local_node_token(&hub, &target_url, &mut updated)?;
            Some(NodeHandle::start(&target_url, node_token, &state.data_dir))
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
                Ok(Some(node_token)) => Some(NodeHandle::start(&target_url, node_token, &state.data_dir)),
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

    updated.save(&state.data_dir)?;
    *state.settings.lock().unwrap_or_else(|e| e.into_inner()) = updated;

    open_target_in_main_window(app, WebviewUrl::External(target_url.parse()?))?;
    Ok(())
}

/// Whether the local hub database already has an owner. A missing database is unclaimed.
/// Uses the running hub when this process holds it, so a second open does not run migrations.
fn local_hub_claimed(state: &DesktopState) -> bool {
    if let Some(handle) = state.hub.lock().unwrap_or_else(|e| e.into_inner()).as_ref()
        && let Some(hub) = handle.hub()
    {
        return hub.db.owner().is_some();
    }
    let dir = hub_data_dir(&state.data_dir);
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

async fn ensure_embedded_hub(state: &DesktopState) -> Result<Arc<HubHandle>> {
    if let Some(existing) = state.hub.lock().unwrap_or_else(|e| e.into_inner()).clone() {
        return Ok(existing);
    }
    let hub_dir = hub_data_dir(&state.data_dir);
    let bind = SocketAddr::from((Ipv4Addr::LOCALHOST, LOCAL_HUB_PORT));
    let handle = hub::host::start(&hub_dir, &[bind])
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
    Ok(arc)
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
    let quit_item = MenuItem::with_id(app, TRAY_QUIT_ID, "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open_item, &setup_item, &quit_item])?;
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
        .invoke_handler(tauri::generate_handler![setup, choose])
        .setup(|app| {
            let data_dir = app_data_dir()?;
            std::fs::create_dir_all(&data_dir)
                .with_context(|| format!("creating {}", data_dir.display()))?;
            let mut settings = DesktopSettings::load(&data_dir)?;

            let (initial_hub, initial_node) = match settings.mode {
                Some(HostMode::ThisComputer) => {
                    let hub_dir = hub_data_dir(&data_dir);
                    let bind = SocketAddr::from((Ipv4Addr::LOCALHOST, LOCAL_HUB_PORT));
                    let handle = tauri::async_runtime::block_on(hub::host::start(&hub_dir, &[bind]))
                        .with_context(|| format!("starting embedded hub on {bind}"))?;
                    tracing::info!(
                        client_version = core_client::CLIENT_VERSION,
                        data_dir = %hub_dir.display(),
                        "embedded local hub started"
                    );
                    let node_tok = node::ensure_local_node_token(&handle, LOCAL_HUB_URL, &mut settings)?;
                    settings.save(&data_dir)?;
                    let node_handle = NodeHandle::start(LOCAL_HUB_URL, node_tok, &data_dir);
                    (Some(Arc::new(handle)), Some(node_handle))
                }
                Some(HostMode::AnotherComputer) => {
                    let remote_url = settings.resolved_remote_hub_url();
                    let node_handle = match tauri::async_runtime::block_on(node::resolve_remote_node_token(
                        &remote_url,
                        &mut settings,
                    )) {
                        Ok(Some(tok)) => {
                            let _ = settings.save(&data_dir);
                            Some(NodeHandle::start(&remote_url, tok, &data_dir))
                        }
                        Ok(None) => None,
                        Err(err) => {
                            tracing::warn!("could not start remote desktop node on launch: {err:#}");
                            None
                        }
                    };
                    (None, node_handle)
                }
                None => (None, None),
            };

            let target = initial_webview_url(&settings);
            let desktop_state = DesktopState::new(data_dir, settings.clone(), initial_hub, initial_node);
            if let Some(hub_url) = settings.active_hub_url()
                && let Ok(Some(saved_tok)) = settings.admin_token_for(&hub_url)
            {
                desktop_state.arm_session_handoff(&hub_url, saved_tok);
            }
            app.manage(desktop_state);

            install_tray(app.handle())?;
            build_main_window(app.handle(), target)?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running Grouper desktop app");
}

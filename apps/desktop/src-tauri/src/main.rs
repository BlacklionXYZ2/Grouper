#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use hub::host::HubHandle;
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent};

const LOCAL_HUB_PORT: u16 = 47600;
const LOCAL_HUB_URL: &str = "http://127.0.0.1:47600";
const DATA_DIR_ENV: &str = "GROUPER_DESKTOP_DATA_DIR";
const MAIN_WINDOW_LABEL: &str = "main";
const TRAY_OPEN_ID: &str = "open";
const TRAY_QUIT_ID: &str = "quit";

struct DesktopState {
    hub: Mutex<Option<Arc<HubHandle>>>,
}

impl DesktopState {
    fn new(hub: Option<Arc<HubHandle>>) -> Self {
        Self { hub: Mutex::new(hub) }
    }

    fn take_hub(&self) -> Option<Arc<HubHandle>> {
        self.hub.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
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

fn show_or_create_main_window(app: &AppHandle, target_url: &str) -> tauri::Result<()> {
    if let Some(win) = app.get_webview_window(MAIN_WINDOW_LABEL) {
        let _ = win.unminimize();
        win.show()?;
        let _ = win.set_focus();
        return Ok(());
    }
    let parsed: url::Url = target_url.parse().expect("valid hub URL");
    let win = WebviewWindowBuilder::new(app, MAIN_WINDOW_LABEL, WebviewUrl::External(parsed))
        .title("Grouper")
        .inner_size(1180.0, 800.0)
        .min_inner_size(420.0, 520.0)
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
    let quit_item = MenuItem::with_id(app, TRAY_QUIT_ID, "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open_item, &quit_item])?;
    let icon = Image::from_bytes(include_bytes!("../icons/32x32.png"))?;

    TrayIconBuilder::with_id("grouper-tray")
        .icon(icon)
        .tooltip("Grouper")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            TRAY_OPEN_ID => {
                let _ = show_or_create_main_window(app, LOCAL_HUB_URL);
            }
            TRAY_QUIT_ID => {
                let handle = app.clone();
                tauri::async_runtime::spawn(async move {
                    if let Some(state) = handle.try_state::<DesktopState>()
                        && let Some(hub) = state.take_hub()
                    {
                        hub.stop().await;
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
                let _ = show_or_create_main_window(tray.app_handle(), LOCAL_HUB_URL);
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
        .setup(|app| {
            let data_dir = app_data_dir()?;
            std::fs::create_dir_all(&data_dir)
                .with_context(|| format!("creating {}", data_dir.display()))?;
            let hub_dir = hub_data_dir(&data_dir);
            let bind = SocketAddr::from((Ipv4Addr::LOCALHOST, LOCAL_HUB_PORT));

            let handle = tauri::async_runtime::block_on(hub::host::start(&hub_dir, &[bind]))
                .with_context(|| format!("starting embedded hub on {bind}"))?;
            tracing::info!(
                client_version = core_client::CLIENT_VERSION,
                data_dir = %hub_dir.display(),
                "embedded local hub started"
            );

            app.manage(DesktopState::new(Some(Arc::new(handle))));
            install_tray(app.handle())?;
            show_or_create_main_window(app.handle(), LOCAL_HUB_URL)?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running Grouper desktop app");
}

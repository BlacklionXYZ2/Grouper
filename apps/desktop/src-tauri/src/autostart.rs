//! Start Grouper at Windows login.
//!
//! When "This computer" is the hub and the user has not chosen yet, login start is turned on.
//! The tray item "Start Grouper at login" records the choice in `login-start.json` so a later
//! launch does not override it. The plugin writes the current-user Run key.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tauri::menu::CheckMenuItem;
use tauri::{AppHandle, Manager};
use tauri_plugin_autostart::ManagerExt;

use crate::settings::HostMode;

pub const TRAY_ID: &str = "start-at-login";
pub const LABEL: &str = "Start Grouper at login";
const CHOICE_FILE: &str = "login-start.json";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct LoginChoice {
    enabled: bool,
}

/// Held so a toggle can update the check mark on the tray item already in the menu.
pub(crate) struct LoginMenuItem(CheckMenuItem);

pub fn plugin<R: tauri::Runtime>() -> tauri::plugin::TauriPlugin<R> {
    tauri_plugin_autostart::Builder::new().build()
}

/// Enable login start when this PC is the hub and the tray has never been used.
pub fn apply_default(app: &AppHandle) {
    let Some((mode, data_dir)) = desktop_view(app) else {
        return;
    };
    if !should_enable_by_default(mode, read_choice(&data_dir)) {
        return;
    }
    match app.autolaunch().enable() {
        Ok(()) => {
            tracing::info!("start at login enabled (this computer is the hub)");
            set_menu_checked(app, true);
        }
        Err(err) => tracing::warn!("could not enable start at login: {err}"),
    }
}

pub fn is_enabled(app: &AppHandle) -> bool {
    match app.autolaunch().is_enabled() {
        Ok(enabled) => enabled,
        Err(err) => {
            tracing::warn!("could not read start at login: {err}");
            false
        }
    }
}

/// Tray item. Call once, after [`apply_default`], so the check matches the Run key.
pub fn menu_item(app: &AppHandle) -> tauri::Result<CheckMenuItem> {
    let item = CheckMenuItem::with_id(app, TRAY_ID, LABEL, true, is_enabled(app), None::<&str>)?;
    app.manage(LoginMenuItem(item.clone()));
    Ok(item)
}

/// Flip the Run key and remember the choice.
pub fn toggle(app: &AppHandle) {
    let Some((_, data_dir)) = desktop_view(app) else {
        return;
    };
    let manager = app.autolaunch();
    let currently = manager.is_enabled().unwrap_or(false);
    let next = !currently;
    let result = if next { manager.enable() } else { manager.disable() };
    if let Err(err) = result {
        tracing::warn!("could not change start at login: {err}");
        return;
    }
    if let Err(err) = write_choice(&data_dir, next) {
        tracing::warn!("could not remember start-at-login choice: {err:#}");
    }
    set_menu_checked(app, next);
}

/// `chosen` is `None` until the tray toggle has been used.
pub(crate) fn should_enable_by_default(mode: Option<HostMode>, chosen: Option<bool>) -> bool {
    mode == Some(HostMode::ThisComputer) && chosen.is_none()
}

fn desktop_view(app: &AppHandle) -> Option<(Option<HostMode>, PathBuf)> {
    let state = app.try_state::<crate::DesktopState>()?;
    Some((state.host_mode(), state.data_dir().to_path_buf()))
}

fn choice_path(data_dir: &Path) -> PathBuf {
    data_dir.join(CHOICE_FILE)
}

fn read_choice(data_dir: &Path) -> Option<bool> {
    let text = std::fs::read_to_string(choice_path(data_dir)).ok()?;
    serde_json::from_str::<LoginChoice>(&text).ok().map(|choice| choice.enabled)
}

fn write_choice(data_dir: &Path, enabled: bool) -> Result<()> {
    std::fs::create_dir_all(data_dir).with_context(|| format!("creating {}", data_dir.display()))?;
    let mut bytes = serde_json::to_vec_pretty(&LoginChoice { enabled })?;
    bytes.push(b'\n');
    let path = choice_path(data_dir);
    std::fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn set_menu_checked(app: &AppHandle, checked: bool) {
    if let Some(item) = app.try_state::<LoginMenuItem>() {
        let _ = item.0.set_checked(checked);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_computer_enables_login_start_until_the_user_chooses() {
        assert!(should_enable_by_default(Some(HostMode::ThisComputer), None));
        assert!(!should_enable_by_default(Some(HostMode::ThisComputer), Some(true)));
        assert!(!should_enable_by_default(Some(HostMode::ThisComputer), Some(false)));
        assert!(!should_enable_by_default(Some(HostMode::AnotherComputer), None));
        assert!(!should_enable_by_default(None, None));
    }

    #[test]
    fn the_tray_choice_is_remembered() {
        let dir = std::env::temp_dir().join(format!("grouper-login-start-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(read_choice(&dir).is_none());
        write_choice(&dir, false).unwrap();
        assert_eq!(read_choice(&dir), Some(false));
        write_choice(&dir, true).unwrap();
        assert_eq!(read_choice(&dir), Some(true));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

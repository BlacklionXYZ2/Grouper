//! Tray controls for "Read subscription usage on this device".
//!
//! Subscription usage belongs to the whole account, so reading is on by default wherever a harness
//! is signed in. This submenu is the local off switch (the whole device, or one provider). It is
//! stored in `desktop.json`; changing it restarts this PC's node so the hub sees the new
//! advertisement. There is deliberately no remote way to flip it.

use tauri::menu::{CheckMenuItem, IsMenuItem, MenuItem, PredefinedMenuItem, Submenu};
use tauri::{AppHandle, Manager};

use crate::DesktopState;
use crate::node;
use crate::settings::DesktopSettings;

pub const MASTER_ID: &str = "usage-reading";
const PROVIDER_PREFIX: &str = "usage-provider-";
const SUBMENU_LABEL: &str = "Subscription usage";
const MASTER_LABEL: &str = "Read subscription usage";
pub const EXPLANATION: &str = "Lets this PC read how much of your Claude, ChatGPT, Grok and Antigravity plans is used, from the sign-ins those apps keep here.";
pub const PRIVACY_NOTE: &str = "Only percentages and reset times go to your hub.";

const PROVIDERS: [(&str, &str); 4] = [
    ("claude", "Claude"),
    ("codex", "ChatGPT (Codex)"),
    ("grok", "Grok"),
    ("antigravity", "Antigravity"),
];

/// Held so a change can update the check marks already in the menu.
struct UsageMenuItems {
    master: CheckMenuItem<tauri::Wry>,
    providers: Vec<(&'static str, CheckMenuItem<tauri::Wry>)>,
}

pub fn menu_item(app: &AppHandle) -> tauri::Result<Submenu<tauri::Wry>> {
    let settings = app
        .try_state::<DesktopState>()
        .map(|s| s.settings_snapshot())
        .unwrap_or_default();
    let master = CheckMenuItem::with_id(app, MASTER_ID, MASTER_LABEL, true, settings.usage_probes, None::<&str>)?;
    let explanation = MenuItem::new(app, EXPLANATION, false, None::<&str>)?;
    let privacy = MenuItem::new(app, PRIVACY_NOTE, false, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let mut providers = Vec::new();
    for (id, label) in PROVIDERS {
        let item = CheckMenuItem::with_id(
            app,
            format!("{PROVIDER_PREFIX}{id}"),
            label,
            true,
            settings.usage_provider_on(id),
            None::<&str>,
        )?;
        providers.push((id, item));
    }
    let mut items: Vec<&dyn IsMenuItem<tauri::Wry>> = vec![&master, &explanation, &privacy, &sep];
    for (_, item) in &providers {
        items.push(item);
    }
    let submenu = Submenu::with_items(app, SUBMENU_LABEL, true, &items)?;
    app.manage(UsageMenuItems { master, providers });
    Ok(submenu)
}

/// Handle a tray click; returns false when the id is not one of ours.
pub fn handle_event(app: &AppHandle, id: &str) -> bool {
    if id == MASTER_ID {
        change(app, None);
        return true;
    }
    if let Some(provider) = id.strip_prefix(PROVIDER_PREFIX)
        && let Some((name, _)) = PROVIDERS.iter().find(|(p, _)| *p == provider)
    {
        change(app, Some(name));
        return true;
    }
    false
}

/// Flip the whole-device switch (`None`) or one provider, save it, and reconnect the node.
fn change(app: &AppHandle, provider: Option<&'static str>) {
    let Some(state) = app.try_state::<DesktopState>() else {
        return;
    };
    let state = state.inner().clone();
    let updated = {
        let mut guard = state.inner.settings.lock().unwrap_or_else(|e| e.into_inner());
        match provider {
            None => guard.usage_probes = !guard.usage_probes,
            Some(p) => {
                let on = guard.usage_provider_on(p);
                guard.set_usage_provider(p, !on);
            }
        }
        guard.clone()
    };
    if let Err(err) = updated.save(state.data_dir()) {
        tracing::warn!("could not save the usage reading setting: {err:#}");
    }
    sync_checks(app, &updated);
    node::set_usage_setting(updated.usage_probe_setting());
    tauri::async_runtime::spawn(async move {
        if let Some(prev) = state.take_node() {
            let next = prev.restart().await;
            if let Some(stale) = state.replace_node(Some(next)) {
                stale.stop().await;
            }
        }
    });
}

fn sync_checks(app: &AppHandle, settings: &DesktopSettings) {
    let Some(items) = app.try_state::<UsageMenuItems>() else {
        return;
    };
    let _ = items.master.set_checked(settings.usage_probes);
    for (id, item) in &items.providers {
        let _ = item.set_checked(settings.usage_provider_on(id));
    }
}

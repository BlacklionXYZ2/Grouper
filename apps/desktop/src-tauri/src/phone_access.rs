//! "Turn on phone and web access": link the embedded hub to the Grouper relay from the tray.
//!
//! In "This computer" mode the app asks its own hub for a relay nonce (`POST /v1/relay/nonce`, with
//! the app's admin token), runs the desktop Google sign-in with the domain-separated hash of that
//! nonce (`hub::google_auth::relay_google_nonce`, so a relay can never be handed a token minted for a
//! hub nonce), and posts `{id_token, relay_nonce}` to `POST /v1/relay/link`. "Turn off" is
//! `DELETE /v1/relay`. The tray item's text follows the hub's link state.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tauri::menu::MenuItem;
use tauri::{AppHandle, Manager};
use tauri_plugin_notification::NotificationExt;

use crate::DesktopState;
use crate::oauth;
use crate::settings::HostMode;

pub const TRAY_ID: &str = "phone-access";
pub const ON_LABEL: &str = "Turn on phone and web access";
pub const OFF_LABEL: &str = "Turn off phone and web access";
const NOT_HERE_LABEL: &str = "Phone and web access (needs This computer mode)";
const TOAST_TITLE: &str = "Grouper";
const OFFER_FILE: &str = "phone-access-offered";
const REFRESH_EVERY: Duration = Duration::from_secs(10);
const OFFER_AFTER: Duration = Duration::from_secs(20);
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Held so the item's text can follow the link state.
struct PhoneAccessItem(MenuItem<tauri::Wry>);

/// Set while a link or unlink is running, so a second click does nothing.
static BUSY: AtomicBool = AtomicBool::new(false);

/// What the tray item offers, from the hub's link state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Offer {
    TurnOn,
    TurnOff,
    Unavailable,
}

pub(crate) fn offer_for(mode: Option<HostMode>, hub_running: bool, linked: bool) -> Offer {
    match (mode, hub_running) {
        (Some(HostMode::ThisComputer), true) if linked => Offer::TurnOff,
        (Some(HostMode::ThisComputer), true) => Offer::TurnOn,
        _ => Offer::Unavailable,
    }
}

fn label_for(offer: Offer) -> &'static str {
    match offer {
        Offer::TurnOn => ON_LABEL,
        Offer::TurnOff => OFF_LABEL,
        Offer::Unavailable => NOT_HERE_LABEL,
    }
}

/// The offer right now: mode, and whether the embedded hub holds a relay link.
fn current_offer(app: &AppHandle) -> Offer {
    let Some(state) = app.try_state::<DesktopState>() else { return Offer::Unavailable };
    let hub = state.hub_handle().and_then(|h| h.hub());
    offer_for(state.host_mode(), hub.is_some(), hub.is_some_and(|h| h.db.has_relay_secret()))
}

/// The tray item. Call once while building the menu.
pub fn menu_item(app: &AppHandle) -> tauri::Result<MenuItem<tauri::Wry>> {
    let offer = current_offer(app);
    let item = MenuItem::with_id(app, TRAY_ID, label_for(offer), offer != Offer::Unavailable, None::<&str>)?;
    app.manage(PhoneAccessItem(item.clone()));
    Ok(item)
}

fn refresh_item(app: &AppHandle) {
    let Some(item) = app.try_state::<PhoneAccessItem>() else { return };
    let offer = current_offer(app);
    let busy = BUSY.load(Ordering::Relaxed);
    let _ = item.0.set_text(label_for(offer));
    let _ = item.0.set_enabled(offer != Offer::Unavailable && !busy);
}

/// Keep the item's text current, and offer phone access once to someone who has claimed the hub.
pub fn start(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(OFFER_AFTER).await;
        offer_once(&app);
        loop {
            refresh_item(&app);
            tokio::time::sleep(REFRESH_EVERY).await;
        }
    });
}

/// After the hub is claimed, say once that phone and web access exists (a toast is enough).
fn offer_once(app: &AppHandle) {
    let Some(state) = app.try_state::<DesktopState>() else { return };
    let marker = state.data_dir().join(OFFER_FILE);
    if marker.exists() || current_offer(app) != Offer::TurnOn {
        return;
    }
    let claimed = state.hub_handle().and_then(|h| h.hub()).is_some_and(|h| h.db.owner().is_some());
    if !claimed {
        return;
    }
    toast(app, "Use Grouper from your phone and the web: choose “Turn on phone and web access” in the tray menu.");
    if let Err(err) = std::fs::write(&marker, b"offered\n") {
        tracing::warn!("could not remember the phone access offer: {err}");
    }
}

fn toast(app: &AppHandle, body: &str) {
    if let Err(err) = app.notification().builder().title(TOAST_TITLE).body(body).show() {
        tracing::warn!("could not show a notification: {err}");
    }
}

/// Handle a tray click. True when `id` was this item's.
pub fn handle_event(app: &AppHandle, id: &str) -> bool {
    if id != TRAY_ID {
        return false;
    }
    if BUSY.swap(true, Ordering::SeqCst) {
        return true;
    }
    refresh_item(app);
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let turn_off = current_offer(&app) == Offer::TurnOff;
        let result = if turn_off { turn_off_access(&app).await } else { turn_on_access(&app).await };
        BUSY.store(false, Ordering::SeqCst);
        match result {
            Ok(message) => toast(&app, &message),
            Err(err) => {
                tracing::warn!("phone and web access: {err:#}");
                toast(&app, &format!("Couldn't {} phone and web access: {err:#}", if turn_off { "turn off" } else { "turn on" }));
            }
        }
        refresh_item(&app);
    });
    true
}

/// The embedded hub's address and the app's admin token for it.
fn local_hub(app: &AppHandle) -> Result<(String, String)> {
    let state = app.try_state::<DesktopState>().context("Grouper isn't ready yet")?;
    if state.host_mode() != Some(HostMode::ThisComputer) {
        bail!("this needs Grouper to run the hub on this computer");
    }
    let settings = state.settings_snapshot();
    let url = settings.active_hub_url().context("no hub is chosen")?;
    let token = settings
        .admin_token_for(&url)?
        .context("this app has no admin sign-in for the hub; choose “Where Grouper runs…” and sign in again")?;
    Ok((url, token))
}

fn http() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder().timeout(CALL_TIMEOUT).build()?)
}

/// A hub error body (`{"error": …}`) as text.
async fn failure(what: &str, resp: reqwest::Response) -> anyhow::Error {
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let message = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| v["error"].as_str().map(str::to_string))
        .unwrap_or_else(|| text.chars().take(200).collect());
    anyhow::anyhow!("{what} failed ({status}): {message}")
}

async fn turn_on_access(app: &AppHandle) -> Result<String> {
    let (hub_url, admin) = local_hub(app)?;
    let state = app.try_state::<DesktopState>().context("Grouper isn't ready yet")?;
    let oauth_cfg =
        oauth::resolve_oauth_config(&state.settings_snapshot()).context("no Google sign-in is set up for this app")?;
    let http = http()?;

    let resp = http
        .post(format!("{hub_url}/v1/relay/nonce"))
        .bearer_auth(&admin)
        .json(&json!({}))
        .send()
        .await
        .context("asking the hub for a relay nonce")?;
    if !resp.status().is_success() {
        return Err(failure("getting a relay nonce", resp).await);
    }
    let relay_nonce = resp.json::<Value>().await?["nonce"].as_str().context("the hub sent no nonce")?.to_string();

    // Google gets only the hash of the relay nonce.
    let id_token = oauth::obtain_google_id_token(&oauth_cfg, &hub::google_auth::relay_google_nonce(&relay_nonce)).await?;

    let resp = http
        .post(format!("{hub_url}/v1/relay/link"))
        .bearer_auth(&admin)
        .json(&json!({"id_token": id_token, "relay_nonce": relay_nonce}))
        .send()
        .await
        .context("linking the hub")?;
    if !resp.status().is_success() {
        return Err(failure("linking", resp).await);
    }
    let status = resp.json::<Value>().await.unwrap_or(Value::Null);
    Ok(match status["account_email"].as_str().filter(|e| !e.is_empty()) {
        Some(email) => format!("Phone and web access is on for {email}."),
        None => "Phone and web access is on.".to_string(),
    })
}

async fn turn_off_access(app: &AppHandle) -> Result<String> {
    let (hub_url, admin) = local_hub(app)?;
    let resp = http()?
        .delete(format!("{hub_url}/v1/relay"))
        .bearer_auth(&admin)
        .send()
        .await
        .context("unlinking the hub")?;
    if !resp.status().is_success() {
        return Err(failure("turning off", resp).await);
    }
    Ok("Phone and web access is off.".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tray_item_follows_mode_and_link_state() {
        let here = Some(HostMode::ThisComputer);
        assert_eq!(offer_for(here, true, false), Offer::TurnOn);
        assert_eq!(offer_for(here, true, true), Offer::TurnOff);
        assert_eq!(offer_for(here, false, false), Offer::Unavailable, "hub not running yet");
        assert_eq!(offer_for(Some(HostMode::AnotherComputer), false, false), Offer::Unavailable);
        assert_eq!(offer_for(None, false, false), Offer::Unavailable);
        assert_eq!(label_for(Offer::TurnOn), "Turn on phone and web access");
        assert!(label_for(Offer::TurnOff).starts_with("Turn off"));
    }

    #[test]
    fn google_gets_the_domain_separated_hash_of_the_relay_nonce() {
        use base64::Engine as _;
        use sha2::{Digest, Sha256};
        let want = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(b"grouper-relay-v1:rn.a.1.b"));
        let got = hub::google_auth::relay_google_nonce("rn.a.1.b");
        assert_eq!(got, want);
        assert_eq!(got.len(), 43);
        assert!(!got.contains("rn."), "never the raw nonce");
    }
}

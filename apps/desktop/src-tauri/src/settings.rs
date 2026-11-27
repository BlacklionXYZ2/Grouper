//! Persistent desktop configuration stored in `%LOCALAPPDATA%\Grouper\desktop.json`.
//!
//! Stores the user's first-run choice ("Where should Grouper run?"), any custom remote hub URL,
//! this install's `install_id`, optional Desktop OAuth client configuration, and DPAPI-protected
//! admin/node device tokens on Windows.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const SETTINGS_FILE: &str = "desktop.json";
pub const LOCAL_HUB_URL: &str = "http://127.0.0.1:47600";
pub const DEFAULT_REMOTE_HUB_URL: &str = "https://grouper.clockmillbarn.uk";

/// Where Grouper's hub runs for this desktop install.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostMode {
    /// Run the embedded hub in-process on `127.0.0.1:47600`.
    ThisComputer,
    /// Connect to a hub running on another machine (defaults to `https://grouper.clockmillbarn.uk`).
    AnotherComputer,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DesktopSettings {
    /// `None` before the user completes the first-run screen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<HostMode>,
    /// Custom remote hub address when `mode == Some(AnotherComputer)` (`None` means `DEFAULT_REMOTE_HUB_URL`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_hub_url: Option<String>,
    /// Stable per-install identifier (`[a-f0-9]{32}`) used to reuse this PC's client and node rows.
    #[serde(default)]
    pub install_id: String,
    /// Optional Google Desktop OAuth client ID (overridden by `GROUPER_GOOGLE_DESKTOP_CLIENT_ID`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub google_desktop_client_id: Option<String>,
    /// DPAPI-protected Google Desktop OAuth client secret (`dpapi:<base64>`). Never plaintext.
    /// Absent means the caller falls back to `GROUPER_GOOGLE_DESKTOP_CLIENT_SECRET`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    google_desktop_client_secret: Option<String>,
    /// Hub URL that `admin_token` belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admin_hub_url: Option<String>,
    /// DPAPI-protected admin device token (Windows) for trading into a WebView session cookie.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    admin_token: Option<String>,
    /// Hub URL that `node_token` belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_hub_url: Option<String>,
    /// DPAPI-protected non-admin node token (Windows) for in-process `core-client`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    node_token: Option<String>,
}

impl DesktopSettings {
    pub fn settings_path(data_dir: &Path) -> PathBuf {
        data_dir.join(SETTINGS_FILE)
    }

    /// Load `<data_dir>/desktop.json`, generating and persisting a fresh `install_id` if missing.
    pub fn load(data_dir: &Path) -> Result<Self> {
        let path = Self::settings_path(data_dir);
        let mut settings: Self = match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("parsing {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let mut changed = false;
        if !hub::db::valid_install_id(&settings.install_id) {
            settings.install_id = new_install_id();
            changed = true;
        }
        if seal_legacy_client_secret(&mut settings)? {
            changed = true;
        }
        if changed {
            settings.save(data_dir)?;
        }
        Ok(settings)
    }

    pub fn save(&self, data_dir: &Path) -> Result<()> {
        if let Some(secret) = &self.google_desktop_client_secret
            && !secret.starts_with("dpapi:")
        {
            bail!("refusing to write a plaintext Google client secret to desktop.json");
        }
        fs::create_dir_all(data_dir)
            .with_context(|| format!("creating {}", data_dir.display()))?;
        let path = Self::settings_path(data_dir);
        let mut bytes = serde_json::to_vec_pretty(self)?;
        bytes.push(b'\n');
        let mut opts = OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        file.write_all(&bytes)
            .with_context(|| format!("writing {}", path.display()))?;
        file.sync_all()
            .with_context(|| format!("syncing {}", path.display()))?;
        Ok(())
    }

    /// Effective remote hub URL (`DEFAULT_REMOTE_HUB_URL` unless overridden).
    pub fn resolved_remote_hub_url(&self) -> String {
        self.remote_hub_url
            .as_deref()
            .and_then(|u| validate_hub_url(u).ok())
            .unwrap_or_else(|| DEFAULT_REMOTE_HUB_URL.to_string())
    }

    /// Active hub URL for the chosen mode, or `None` on first run before a choice is made.
    pub fn active_hub_url(&self) -> Option<String> {
        match self.mode? {
            HostMode::ThisComputer => Some(LOCAL_HUB_URL.to_string()),
            HostMode::AnotherComputer => Some(self.resolved_remote_hub_url()),
        }
    }

    /// Companion `install_id` for the non-admin node row (distinct from the admin UI row).
    pub fn node_install_id(&self) -> String {
        let base: String = self.install_id.chars().take(48).collect();
        format!("{base}-node")
    }

    pub fn set_choice(&mut self, mode: HostMode, remote_hub_url: Option<&str>) -> Result<()> {
        if let Some(raw) = remote_hub_url.map(str::trim).filter(|s| !s.is_empty()) {
            let normalized = validate_hub_url(raw)?;
            if normalized == DEFAULT_REMOTE_HUB_URL {
                self.remote_hub_url = None;
            } else {
                self.remote_hub_url = Some(normalized);
            }
        } else if mode == HostMode::AnotherComputer && self.remote_hub_url.is_none() {
            self.remote_hub_url = None;
        }
        self.mode = Some(mode);
        Ok(())
    }

    /// Store the Desktop OAuth client secret sealed with DPAPI. An empty secret clears it.
    pub fn set_google_desktop_client_secret(&mut self, secret: &str) -> Result<()> {
        let trimmed = secret.trim();
        if trimmed.is_empty() {
            self.google_desktop_client_secret = None;
            return Ok(());
        }
        let sealed = seal_token(trimmed)?;
        if !sealed.starts_with("dpapi:") {
            bail!("refusing to store the Google client secret without Windows DPAPI");
        }
        self.google_desktop_client_secret = Some(sealed);
        Ok(())
    }

    /// The Desktop OAuth client secret, if one was stored with DPAPI.
    pub fn google_desktop_client_secret(&self) -> Result<Option<String>> {
        let Some(stored) = &self.google_desktop_client_secret else {
            return Ok(None);
        };
        if !stored.starts_with("dpapi:") {
            bail!("stored Google client secret is not DPAPI-protected");
        }
        let plain = open_token(stored)?;
        Ok((!plain.is_empty()).then_some(plain))
    }

    pub fn set_admin_token(&mut self, hub_url: &str, token: &str) -> Result<()> {
        self.admin_hub_url = Some(normalize_url_key(hub_url));
        self.admin_token = Some(seal_token(token)?);
        Ok(())
    }

    pub fn admin_token_for(&self, hub_url: &str) -> Result<Option<String>> {
        let Some(saved_hub) = &self.admin_hub_url else {
            return Ok(None);
        };
        if normalize_url_key(saved_hub) != normalize_url_key(hub_url) {
            return Ok(None);
        }
        let Some(sealed) = &self.admin_token else {
            return Ok(None);
        };
        Ok(Some(open_token(sealed)?))
    }

    pub fn set_node_token(&mut self, hub_url: &str, token: &str) -> Result<()> {
        self.node_hub_url = Some(normalize_url_key(hub_url));
        self.node_token = Some(seal_token(token)?);
        Ok(())
    }

    pub fn node_token_for(&self, hub_url: &str) -> Result<Option<String>> {
        let Some(saved_hub) = &self.node_hub_url else {
            return Ok(None);
        };
        if normalize_url_key(saved_hub) != normalize_url_key(hub_url) {
            return Ok(None);
        }
        let Some(sealed) = &self.node_token else {
            return Ok(None);
        };
        Ok(Some(open_token(sealed)?))
    }
}

/// Validate a hub base URL: `https://` anywhere, or `http://` on loopback / `.local` / RFC1918 LAN.
pub fn validate_hub_url(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("enter a hub address such as {DEFAULT_REMOTE_HUB_URL}");
    }
    let parsed = url::Url::parse(trimmed)
        .context("hub address must be a full URL such as https://grouper.clockmillbarn.uk")?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        bail!("hub address must not include a username or password");
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        bail!("hub address must not include a query string or #fragment");
    }
    let Some(host) = parsed.host() else {
        bail!("hub address has no host");
    };
    match parsed.scheme() {
        "https" => {}
        "http" => {
            if !is_allowed_plain_http_host(&host) {
                bail!("remote hub addresses must use https:// (plain http:// is only allowed for localhost or LAN)");
            }
        }
        _ => bail!("hub address must start with https:// (or http:// for localhost)"),
    }
    Ok(parsed.as_str().trim_end_matches('/').to_string())
}

fn is_allowed_plain_http_host(host: &url::Host<&str>) -> bool {
    match host {
        url::Host::Domain(d) => {
            let lower = d.to_ascii_lowercase();
            lower == "localhost" || lower.ends_with(".localhost") || lower.ends_with(".local")
        }
        url::Host::Ipv4(v4) => is_local_ipv4(*v4),
        url::Host::Ipv6(v6) => is_local_ipv6(*v6),
    }
}

fn is_local_ipv4(ip: Ipv4Addr) -> bool {
    ip.is_loopback() || ip.is_private() || ip.is_link_local()
}

fn is_local_ipv6(ip: Ipv6Addr) -> bool {
    if ip.is_loopback() || (ip.segments()[0] & 0xfe00) == 0xfc00 || (ip.segments()[0] & 0xffc0) == 0xfe80 {
        return true;
    }
    ip.to_ipv4_mapped().is_some_and(is_local_ipv4)
}

pub fn normalize_url_key(raw: &str) -> String {
    url::Url::parse(raw.trim())
        .map(|u| u.as_str().trim_end_matches('/').to_string())
        .unwrap_or_else(|_| raw.trim().trim_end_matches('/').to_ascii_lowercase())
}

pub fn machine_device_name() -> String {
    let raw = std::env::var("COMPUTERNAME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
        })
        .unwrap_or_default();
    let cleaned: String = raw.trim().chars().filter(|c| !c.is_control()).take(64).collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() || cleaned.eq_ignore_ascii_case("hub") {
        "Windows PC".into()
    } else {
        cleaned.to_string()
    }
}

/// An older `desktop.json` may hold the secret in the clear. Seal it before the next save.
fn seal_legacy_client_secret(settings: &mut DesktopSettings) -> Result<bool> {
    let Some(stored) = settings.google_desktop_client_secret.clone() else {
        return Ok(false);
    };
    if stored.is_empty() {
        settings.google_desktop_client_secret = None;
        return Ok(true);
    }
    if stored.starts_with("dpapi:") {
        return Ok(false);
    }
    settings.set_google_desktop_client_secret(&stored)?;
    Ok(true)
}

fn new_install_id() -> String {
    let mut buf = [0u8; 16];
    getrandom::fill(&mut buf).expect("system randomness");
    let _ = IpAddr::V4(Ipv4Addr::LOCALHOST);
    hex::encode(buf)
}

#[cfg(windows)]
fn seal_token(token: &str) -> Result<String> {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    Ok(format!("dpapi:{}", STANDARD.encode(dpapi::protect(token.as_bytes())?)))
}

#[cfg(windows)]
fn open_token(stored: &str) -> Result<String> {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    let encoded = stored
        .strip_prefix("dpapi:")
        .context("stored desktop token is missing dpapi: prefix")?;
    let encrypted = STANDARD
        .decode(encoded)
        .context("stored desktop token is not valid base64")?;
    let plain = dpapi::unprotect(&encrypted)?;
    String::from_utf8(plain).context("stored desktop token is not valid UTF-8")
}

#[cfg(not(windows))]
fn seal_token(token: &str) -> Result<String> {
    Ok(token.to_string())
}

#[cfg(not(windows))]
fn open_token(stored: &str) -> Result<String> {
    Ok(stored.to_string())
}

#[cfg(windows)]
mod dpapi {
    use std::ptr;

    use anyhow::{Context, Result, bail};
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    };

    pub fn protect(input: &[u8]) -> Result<Vec<u8>> {
        crypt(input, true)
    }

    pub fn unprotect(input: &[u8]) -> Result<Vec<u8>> {
        crypt(input, false)
    }

    fn crypt(input: &[u8], protect: bool) -> Result<Vec<u8>> {
        let len = u32::try_from(input.len()).context("secret too large for DPAPI")?;
        let in_blob = CRYPT_INTEGER_BLOB { cbData: len, pbData: input.as_ptr().cast_mut() };
        let mut out_blob = CRYPT_INTEGER_BLOB { cbData: 0, pbData: ptr::null_mut() };
        let ok = unsafe {
            if protect {
                CryptProtectData(
                    &in_blob,
                    ptr::null(),
                    ptr::null(),
                    ptr::null_mut(),
                    ptr::null(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut out_blob,
                )
            } else {
                CryptUnprotectData(
                    &in_blob,
                    ptr::null_mut(),
                    ptr::null(),
                    ptr::null_mut(),
                    ptr::null(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut out_blob,
                )
            }
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error()).context("Windows DPAPI token protection failed");
        }
        if out_blob.pbData.is_null() {
            bail!("Windows DPAPI returned null buffer");
        }
        let out_len = out_blob.cbData as usize;
        let out = unsafe { std::slice::from_raw_parts(out_blob.pbData, out_len).to_vec() };
        unsafe {
            for i in 0..out_len {
                ptr::write_volatile(out_blob.pbData.add(i), 0);
            }
            std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
            let _ = LocalFree(out_blob.pbData.cast());
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("grouper-desktop-{tag}-{}", hub::db::random_hex(4)))
    }

    #[test]
    fn first_run_defaults_have_no_mode_and_default_domain() {
        let dir = temp_dir("first-run");
        let s = DesktopSettings::load(&dir).unwrap();
        assert_eq!(s.mode, None);
        assert_eq!(s.active_hub_url(), None);
        assert_eq!(s.resolved_remote_hub_url(), DEFAULT_REMOTE_HUB_URL);
        assert!(hub::db::valid_install_id(&s.install_id));
        assert!(hub::db::valid_install_id(&s.node_install_id()));
        assert_ne!(s.install_id, s.node_install_id());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn choice_and_tokens_persist_across_reloads() {
        let dir = temp_dir("persist");
        let mut s = DesktopSettings::load(&dir).unwrap();
        let install_id = s.install_id.clone();

        s.set_choice(HostMode::ThisComputer, None).unwrap();
        s.save(&dir).unwrap();
        let loaded = DesktopSettings::load(&dir).unwrap();
        assert_eq!(loaded.mode, Some(HostMode::ThisComputer));
        assert_eq!(loaded.active_hub_url().as_deref(), Some(LOCAL_HUB_URL));
        assert_eq!(loaded.install_id, install_id);

        let mut s2 = loaded;
        s2.set_choice(HostMode::AnotherComputer, None).unwrap();
        s2.save(&dir).unwrap();
        let loaded2 = DesktopSettings::load(&dir).unwrap();
        assert_eq!(loaded2.mode, Some(HostMode::AnotherComputer));
        assert_eq!(loaded2.active_hub_url().as_deref(), Some(DEFAULT_REMOTE_HUB_URL));

        let mut s3 = loaded2;
        s3.set_choice(HostMode::AnotherComputer, Some("https://hub.example.org/")).unwrap();
        s3.set_admin_token("https://hub.example.org", "dev123.secret456").unwrap();
        s3.save(&dir).unwrap();
        let raw_json = fs::read_to_string(DesktopSettings::settings_path(&dir)).unwrap();
        #[cfg(windows)]
        assert!(!raw_json.contains("secret456"), "raw secret must be DPAPI-sealed on Windows");

        let loaded3 = DesktopSettings::load(&dir).unwrap();
        assert_eq!(loaded3.active_hub_url().as_deref(), Some("https://hub.example.org"));
        assert_eq!(
            loaded3.admin_token_for("https://hub.example.org/").unwrap().as_deref(),
            Some("dev123.secret456")
        );
        assert_eq!(loaded3.admin_token_for("https://other.example.org").unwrap(), None);

        assert!(validate_hub_url("http://remote.example.com").is_err());
        assert_eq!(validate_hub_url("http://localhost:47600/").unwrap(), "http://localhost:47600");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn google_client_secret_never_lands_in_desktop_json() {
        let dir = temp_dir("oauth-secret");
        let secret = "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ut-of-json";
        let mut s = DesktopSettings::load(&dir).unwrap();
        let set = s.set_google_desktop_client_secret(secret);
        #[cfg(windows)]
        {
            set.unwrap();
            s.save(&dir).unwrap();
            let raw = fs::read_to_string(DesktopSettings::settings_path(&dir)).unwrap();
            assert!(!raw.contains(secret), "client secret must not appear in desktop.json");
            assert!(raw.contains("dpapi:"), "secret should be stored as a DPAPI blob");
            let loaded = DesktopSettings::load(&dir).unwrap();
            assert_eq!(loaded.google_desktop_client_secret().unwrap().as_deref(), Some(secret));

            let install_id = loaded.install_id.clone();
            let forged = format!(
                "{{\n  \"install_id\": \"{install_id}\",\n  \"google_desktop_client_secret\": \"{secret}\"\n}}\n"
            );
            fs::write(DesktopSettings::settings_path(&dir), &forged).unwrap();
            let migrated = DesktopSettings::load(&dir).unwrap();
            let raw_after = fs::read_to_string(DesktopSettings::settings_path(&dir)).unwrap();
            assert!(!raw_after.contains(secret), "legacy plaintext secret must be resealed on load");
            assert!(raw_after.contains("dpapi:"));
            assert_eq!(migrated.google_desktop_client_secret().unwrap().as_deref(), Some(secret));
        }
        #[cfg(not(windows))]
        {
            assert!(set.is_err(), "without DPAPI the secret must not be stored");
            s.save(&dir).unwrap();
            let raw = fs::read_to_string(DesktopSettings::settings_path(&dir)).unwrap();
            assert!(!raw.contains(secret));
            assert!(!raw.contains("google_desktop_client_secret"));
        }
        let _ = fs::remove_dir_all(&dir);
    }
}

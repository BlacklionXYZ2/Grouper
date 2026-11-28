//! Per-device hub credentials.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[cfg(windows)]
use base64::{Engine as _, engine::general_purpose::STANDARD};

const FILE_NAME: &str = "credentials.json";
pub const THREAD_SHELL_ENV: &str = "GROUPER_THREAD_SHELL";
/// Overrides where the credentials file is (default `<config dir>/grouper/credentials.json`).
const PATH_ENV: &str = "GROUPER_CREDENTIALS_FILE";
/// Hub address when the process environment names one. `--hub` still wins.
const HUB_ENV: &str = "GROUPER_HUB";
/// Hub address when `--hub`, `GROUPER_HUB` and the credentials file give none.
/// `grouper --help` documents this same string.
pub const DEFAULT_HUB: &str = "https://grouper.clockmillbarn.uk";
#[cfg(target_os = "macos")]
const KEYCHAIN_SERVICE: &str = "dev.grouper.cli";
#[cfg(target_os = "macos")]
const KEYCHAIN_ACCOUNT: &str = "device-token";
#[cfg(target_os = "macos")]
const KEYCHAIN_RELAY_ACCOUNT: &str = "relay-token";
/// Relay token for the process, instead of the saved one (automation).
const RELAY_TOKEN_ENV: &str = "GROUPER_RELAY_TOKEN";

pub struct Credentials {
    pub hub: String,
    pub token: Option<String>,
    /// The relay sign-in token, when the hub address is a relay. Loaded with the token.
    #[allow(dead_code)] // read by tests; commands use relay_token_for(hub)
    pub relay_token: Option<String>,
}

/// `<config dir>/grouper/credentials.json`. The token is DPAPI-protected on Windows, in the
/// Keychain on macOS, and plain in an owner-only (0600) file elsewhere. `install_id` is this CLI
/// install's random id: the hub reuses its device when it signs in again. Logging out keeps it.
#[derive(Default, Deserialize, Serialize)]
struct StoredCredentials {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hub: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    install_id: Option<String>,
    /// The relay token, protected like `token` (DPAPI / Keychain / 0600 file).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    relay_token: Option<String>,
}

/// A fresh install id: 32 random hex characters.
fn new_install_id() -> String {
    let mut buf = [0u8; 16];
    getrandom::fill(&mut buf).expect("system randomness");
    hex::encode(buf)
}

fn read_stored() -> Result<Option<StoredCredentials>> {
    let path = credentials_path()?;
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("reading saved credentials from {}", path.display()));
        }
    };
    let stored = serde_json::from_str(&text).with_context(|| format!("parsing saved credentials from {}", path.display()))?;
    Ok(Some(stored))
}

/// This install's id, made and saved on first use.
pub fn install_id() -> Result<String> {
    if std::env::var_os(THREAD_SHELL_ENV).is_some() {
        bail!("signing in isn't available inside a Grouper thread shell");
    }
    let mut stored = read_stored()?.unwrap_or_default();
    if let Some(id) = stored.install_id.as_deref().filter(|id| valid_install_id(id)) {
        return Ok(id.to_string());
    }
    let id = new_install_id();
    stored.install_id = Some(id.clone());
    write_stored(&stored)?;
    Ok(id)
}

/// The hub's rule for install ids: 16 to 64 characters of `[A-Za-z0-9_-]`.
fn valid_install_id(id: &str) -> bool {
    (16..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Where the credentials file is (for messages).
pub fn path() -> Result<PathBuf> {
    credentials_path()
}

pub fn normalize_hub(url: &str) -> String {
    let trimmed = url.trim();
    if let Ok(parsed) = reqwest::Url::parse(trimmed) {
        return parsed.as_str().trim_end_matches('/').to_string();
    }
    let stripped = trimmed.trim_end_matches('/');
    if let Some((scheme, rest)) = stripped.split_once("://") {
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let (host, tail) = rest.split_at(end);
        format!("{}://{}{}", scheme.to_ascii_lowercase(), host.to_ascii_lowercase(), tail)
    } else {
        stripped.to_string()
    }
}

pub fn resolve(
    cli_hub: Option<String>,
    cli_token: Option<String>,
    default_hub: &str,
) -> Result<(String, String)> {
    // Fold `GROUPER_HUB` in here so a caller that did not already merge the env
    // (clap does, when the flag is absent) still keeps flag, then env, then saved.
    resolve_with(explicit_hub(cli_hub.as_deref()), cli_token, default_hub, load)
}

/// Hub address for `grouper login` and anything else that does not need a token yet.
/// Order: `--hub`, then `GROUPER_HUB`, then the hub saved in the credentials file,
/// then [`DEFAULT_HUB`].
pub fn resolve_hub(cli_hub: Option<&str>) -> Result<String> {
    if let Some(hub) = explicit_hub(cli_hub) {
        return Ok(hub);
    }
    if let Some(saved) = load(false)? {
        return Ok(saved.hub);
    }
    Ok(DEFAULT_HUB.to_string())
}

/// `--hub` when it is non-empty after trimming, otherwise a non-empty `GROUPER_HUB`.
fn explicit_hub(cli_hub: Option<&str>) -> Option<String> {
    cli_hub
        .map(str::trim)
        .filter(|hub| !hub.is_empty())
        .map(str::to_string)
        .or_else(|| {
            std::env::var(HUB_ENV)
                .ok()
                .map(|hub| hub.trim().to_string())
                .filter(|hub| !hub.is_empty())
        })
}

fn resolve_with(
    cli_hub: Option<String>,
    cli_token: Option<String>,
    default_hub: &str,
    mut loader: impl FnMut(bool) -> Result<Option<Credentials>>,
) -> Result<(String, String)> {
    match (cli_hub, cli_token) {
        (Some(hub), Some(token)) => Ok((hub, token)),
        (None, Some(token)) => {
            let hub = loader(false)?
                .map(|credentials| credentials.hub)
                .unwrap_or_else(|| default_hub.to_string());
            Ok((hub, token))
        }
        (Some(hub), None) => {
            let Some(saved) = loader(false)? else {
                bail!("no token: pass --token, set GROUPER_TOKEN, or run `grouper login`");
            };
            if normalize_hub(&hub) != normalize_hub(&saved.hub) {
                bail!(
                    "saved credentials are for {}, not {hub}: pass --token or set GROUPER_TOKEN",
                    saved.hub
                );
            }
            let token = loader(true)?
                .and_then(|credentials| credentials.token)
                .context("saved credentials do not contain a device token")?;
            Ok((hub, token))
        }
        (None, None) => {
            let saved = loader(true)?;
            let hub = saved
                .as_ref()
                .map(|credentials| credentials.hub.clone())
                .unwrap_or_else(|| default_hub.to_string());
            let token = saved
                .and_then(|credentials| credentials.token)
                .context("no token: pass --token, set GROUPER_TOKEN, or run `grouper login`")?;
            Ok((hub, token))
        }
    }
}

pub fn load(with_token: bool) -> Result<Option<Credentials>> {
    if std::env::var_os(THREAD_SHELL_ENV).is_some() {
        bail!("stored credentials are not available inside a Grouper thread shell");
    }
    let Some(stored) = read_stored()? else { return Ok(None) };
    let Some(hub) = stored.hub.clone() else { return Ok(None) };
    let (token, relay_token) = if with_token {
        let token = load_token(&stored)?.context("saved credentials do not contain a device token")?;
        (Some(token), load_relay_token(&stored)?)
    } else {
        (None, None)
    };
    Ok(Some(Credentials { hub, token, relay_token }))
}

/// The relay token to send to `hub`: `GROUPER_RELAY_TOKEN`, else the saved one when the saved hub
/// is this hub. `None` for a hub that isn't behind a relay (or no relay sign-in yet).
pub fn relay_token_for(hub: &str) -> Result<Option<String>> {
    if let Some(t) = std::env::var(RELAY_TOKEN_ENV).ok().map(|t| t.trim().to_string()).filter(|t| !t.is_empty()) {
        return Ok(Some(t));
    }
    if std::env::var_os(THREAD_SHELL_ENV).is_some() {
        return Ok(None);
    }
    let Some(stored) = read_stored()? else { return Ok(None) };
    if stored.hub.as_deref().map(normalize_hub) != Some(normalize_hub(hub)) {
        return Ok(None);
    }
    load_relay_token(&stored)
}

/// Save (or clear, with `None`) the relay token for `hub`. The relay step of `grouper login` runs
/// before the device is approved, so this may be the first thing saved for the hub.
pub fn save_relay_token(hub: &str, relay_token: Option<&str>) -> Result<()> {
    let mut stored = read_stored().ok().flatten().unwrap_or_default();
    if stored.hub.as_deref().map(normalize_hub) != Some(normalize_hub(hub)) {
        // A different hub's credentials don't carry over.
        stored = StoredCredentials { install_id: stored.install_id, ..Default::default() };
        stored.hub = Some(hub.to_string());
    }
    stored.relay_token = match relay_token {
        Some(t) => save_secret(RELAY_ACCOUNT, t)?,
        None => {
            remove_secret(RELAY_ACCOUNT)?;
            None
        }
    };
    write_stored(&stored)
}

fn load_relay_token(stored: &StoredCredentials) -> Result<Option<String>> {
    match &stored.relay_token {
        None => load_secret_keychain_only(RELAY_ACCOUNT),
        Some(v) => load_secret(RELAY_ACCOUNT, Some(v)),
    }
}

/// Save the hub and token from `grouper login`, keeping this install's id.
pub fn save(hub: &str, token: &str, install_id: &str) -> Result<()> {
    // Keep the relay token when it's for this same hub (login saves it first).
    let relay_token = read_stored()
        .ok()
        .flatten()
        .filter(|s| s.hub.as_deref().map(normalize_hub) == Some(normalize_hub(hub)))
        .and_then(|s| s.relay_token);
    let stored = StoredCredentials {
        hub: Some(hub.to_string()),
        token: save_token(token)?,
        install_id: Some(install_id.to_string()),
        relay_token,
    };
    write_stored(&stored)
}

fn write_stored(stored: &StoredCredentials) -> Result<()> {
    let path = credentials_path()?;
    let parent = path
        .parent()
        .context("credentials path has no parent directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("creating config directory {}", parent.display()))?;
    let contents = serde_json::to_vec_pretty(stored)?;
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .with_context(|| format!("opening credentials file {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .with_context(|| format!("securing credentials file {}", path.display()))?;
    }
    file.write_all(&contents)
        .with_context(|| format!("writing credentials file {}", path.display()))?;
    file.write_all(b"\n")
        .with_context(|| format!("writing credentials file {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("syncing credentials file {}", path.display()))?;
    Ok(())
}

/// Forget the hub and token. The install id stays, so signing in again reuses this device.
/// Returns the hub it was signed in to, if any.
pub fn remove() -> Result<Option<String>> {
    remove_token()?;
    remove_secret(RELAY_ACCOUNT)?;
    // An unreadable file is removed outright.
    let stored = read_stored().unwrap_or_default();
    let hub = stored.as_ref().and_then(|s| s.hub.clone());
    match stored.and_then(|s| s.install_id) {
        Some(install_id) => write_stored(&StoredCredentials { install_id: Some(install_id), ..Default::default() })?,
        None => {
            let path = credentials_path()?;
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| format!("removing credentials file {}", path.display()));
                }
            }
        }
    }
    Ok(hub)
}

fn credentials_path() -> Result<PathBuf> {
    // For tests and second profiles: keep these credentials somewhere else.
    if let Some(path) = std::env::var_os(PATH_ENV).filter(|p| !p.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    let config = dirs::config_dir()
        .context("the operating system did not provide a user config directory")?;
    Ok(config.join("grouper").join(FILE_NAME))
}

// Secrets (the device token and the relay token) are stored per "account": `TOKEN_ACCOUNT` and
// `RELAY_ACCOUNT`. Windows: DPAPI in the file. macOS: Keychain, one item per account. Elsewhere:
// plain in the 0600 file.
#[cfg(target_os = "macos")]
const TOKEN_ACCOUNT: &str = KEYCHAIN_ACCOUNT;
#[cfg(target_os = "macos")]
const RELAY_ACCOUNT: &str = KEYCHAIN_RELAY_ACCOUNT;
#[cfg(not(target_os = "macos"))]
const TOKEN_ACCOUNT: &str = "device-token";
#[cfg(not(target_os = "macos"))]
const RELAY_ACCOUNT: &str = "relay-token";

fn save_token(token: &str) -> Result<Option<String>> {
    save_secret(TOKEN_ACCOUNT, token)
}

fn load_token(stored: &StoredCredentials) -> Result<Option<String>> {
    load_secret(TOKEN_ACCOUNT, stored.token.as_ref())
}

fn remove_token() -> Result<()> {
    remove_secret(TOKEN_ACCOUNT)
}

#[cfg(windows)]
fn save_secret(_account: &str, token: &str) -> Result<Option<String>> {
    Ok(Some(format!(
        "dpapi:{}",
        STANDARD.encode(dpapi::protect(token.as_bytes())?)
    )))
}

#[cfg(windows)]
fn load_secret_keychain_only(_account: &str) -> Result<Option<String>> {
    Ok(None)
}

#[cfg(windows)]
fn load_secret(_account: &str, value: Option<&String>) -> Result<Option<String>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let encoded = value
        .strip_prefix("dpapi:")
        .context("saved token is not protected with Windows DPAPI")?;
    let encrypted = STANDARD
        .decode(encoded)
        .context("saved DPAPI token is not valid base64")?;
    let bytes = dpapi::unprotect(&encrypted)?;
    Ok(Some(
        String::from_utf8(bytes).context("saved token is not valid UTF-8")?,
    ))
}

#[cfg(windows)]
fn remove_secret(_account: &str) -> Result<()> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn save_secret(account: &str, token: &str) -> Result<Option<String>> {
    security_framework::passwords::set_generic_password(KEYCHAIN_SERVICE, account, token.as_bytes())
        .context("saving token in macOS Keychain")?;
    Ok(None)
}

#[cfg(target_os = "macos")]
fn load_secret_keychain_only(account: &str) -> Result<Option<String>> {
    load_secret(account, None)
}

#[cfg(target_os = "macos")]
fn load_secret(account: &str, _value: Option<&String>) -> Result<Option<String>> {
    match security_framework::passwords::get_generic_password(KEYCHAIN_SERVICE, account) {
        Ok(bytes) => Ok(Some(String::from_utf8(bytes).context("saved token is not valid UTF-8")?)),
        // errSecItemNotFound: the relay token is optional.
        Err(error) if error.code() == -25300 && account == KEYCHAIN_RELAY_ACCOUNT => Ok(None),
        Err(error) => Err(error).context("reading token from macOS Keychain"),
    }
}

#[cfg(target_os = "macos")]
fn remove_secret(account: &str) -> Result<()> {
    match security_framework::passwords::delete_generic_password(KEYCHAIN_SERVICE, account) {
        Ok(()) => Ok(()),
        Err(error) if error.code() == -25300 => Ok(()),
        Err(error) => Err(error).context("removing token from macOS Keychain"),
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn save_secret(_account: &str, token: &str) -> Result<Option<String>> {
    // There is no universal OS key store on Unix. The token is not encrypted;
    // access is restricted with the credentials file's 0600 mode instead.
    Ok(Some(token.to_string()))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn load_secret_keychain_only(_account: &str) -> Result<Option<String>> {
    Ok(None)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn load_secret(_account: &str, value: Option<&String>) -> Result<Option<String>> {
    Ok(value.cloned())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn remove_secret(_account: &str) -> Result<()> {
    Ok(())
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
        let input_len =
            u32::try_from(input.len()).context("device token is too large for DPAPI")?;
        let input_blob = CRYPT_INTEGER_BLOB {
            cbData: input_len,
            pbData: input.as_ptr().cast_mut(),
        };
        let mut output_blob = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: ptr::null_mut(),
        };
        let ok = unsafe {
            if protect {
                CryptProtectData(
                    &input_blob,
                    ptr::null(),
                    ptr::null(),
                    ptr::null_mut(),
                    ptr::null(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut output_blob,
                )
            } else {
                CryptUnprotectData(
                    &input_blob,
                    ptr::null_mut(),
                    ptr::null(),
                    ptr::null_mut(),
                    ptr::null(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut output_blob,
                )
            }
        };
        if ok == 0 {
            let operation = if protect {
                "protecting"
            } else {
                "unprotecting"
            };
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("{operation} device token with Windows DPAPI"));
        }
        if output_blob.pbData.is_null() {
            bail!("Windows DPAPI returned an empty output buffer");
        }
        let len = output_blob.cbData as usize;
        let output = unsafe { std::slice::from_raw_parts(output_blob.pbData, len).to_vec() };
        unsafe {
            for i in 0..len {
                ptr::write_volatile(output_blob.pbData.add(i), 0);
            }
            std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
            let _ = LocalFree(output_blob.pbData.cast());
        }
        Ok(output)
    }

    #[test]
    fn round_trip() {
        let protected = protect(b"device-token").unwrap();
        assert_ne!(protected, b"device-token");
        assert_eq!(unprotect(&protected).unwrap(), b"device-token");
    }
}

/// Process-environment lock for tests. `GROUPER_HUB` and `GROUPER_CREDENTIALS_FILE`
/// are process-wide, and `cargo test` runs these tests in parallel.
#[cfg(test)]
pub(crate) mod test_env {
    use std::ffi::{OsStr, OsString};
    use std::sync::{Mutex, MutexGuard};

    static LOCK: Mutex<()> = Mutex::new(());

    pub fn lock() -> MutexGuard<'static, ()> {
        LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Restores one environment variable when dropped. Hold [`lock`] first.
    pub struct VarGuard {
        key: &'static str,
        prev: Option<OsString>,
    }

    impl VarGuard {
        pub fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
            let prev = std::env::var_os(key);
            // SAFETY: the caller holds `lock()` until this guard drops, so no other
            // grouper-cli test mutates the process environment at the same time.
            unsafe { std::env::set_var(key, value) }
            Self { key, prev }
        }

        pub fn unset(key: &'static str) -> Self {
            let prev = std::env::var_os(key);
            // SAFETY: same as `set`.
            unsafe { std::env::remove_var(key) }
            Self { key, prev }
        }
    }

    impl Drop for VarGuard {
        fn drop(&mut self) {
            // SAFETY: same as `set`. Drop runs before the lock is released when the
            // guard is declared after `lock()`.
            unsafe {
                match self.prev.take() {
                    Some(value) => std::env::set_var(self.key, value),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_hub_handles_case_and_trailing_slashes() {
        assert_eq!(normalize_hub("HTTPS://Hub.Example.COM///"), "https://hub.example.com");
        assert_eq!(normalize_hub("http://LOCALHOST:8080/"), "http://localhost:8080");
        assert_eq!(normalize_hub("https://Example.COM/Api/"), "https://example.com/Api");
        assert_eq!(normalize_hub("https://example.com:443/"), "https://example.com");
    }

    #[test]
    fn resolve_only_uses_saved_token_for_matching_hub() {
        let saved_loader = |with_token: bool| {
            Ok(Some(Credentials {
                hub: "https://hub.example.com".into(),
                token: with_token.then(|| "saved.tok".into()),
                relay_token: None,
            }))
        };

        // No --hub, no --token: uses saved hub and token.
        let (hub, tok) = resolve_with(None, None, "http://127.0.0.1:8080", saved_loader).unwrap();
        assert_eq!((hub.as_str(), tok.as_str()), ("https://hub.example.com", "saved.tok"));

        // Matching --hub (different scheme/host case and trailing slash): uses saved token.
        let (hub, tok) =
            resolve_with(Some("HTTPS://HUB.EXAMPLE.COM/".into()), None, "http://127.0.0.1:8080", saved_loader)
                .unwrap();
        assert_eq!((hub.as_str(), tok.as_str()), ("HTTPS://HUB.EXAMPLE.COM/", "saved.tok"));

        // Overridden --hub without --token: refuses and never loads the token.
        let err = resolve_with(Some("https://evil.example.com".into()), None, "http://127.0.0.1:8080", |with_token| {
            assert!(!with_token, "must not load saved token for a mismatched hub");
            saved_loader(false)
        })
        .unwrap_err();
        assert!(err.to_string().contains("--token"), "{err}");

        // Overridden --hub with explicit --token: does not touch saved credentials.
        let (hub, tok) = resolve_with(
            Some("https://other.example.com".into()),
            Some("explicit.tok".into()),
            "http://127.0.0.1:8080",
            |_| panic!("should not load saved credentials when both hub and token are given"),
        )
        .unwrap();
        assert_eq!((hub.as_str(), tok.as_str()), ("https://other.example.com", "explicit.tok"));
    }

    #[test]
    fn relay_token_is_saved_per_hub_and_kept_through_login_save() {
        let _lock = super::test_env::lock();
        let path = std::env::temp_dir().join(format!(
            "grouper-cli-relay-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let _creds = super::test_env::VarGuard::set(PATH_ENV, &path);
        let _env = super::test_env::VarGuard::unset(RELAY_TOKEN_ENV);
        struct RemoveFile(std::path::PathBuf);
        impl Drop for RemoveFile {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let _remove = RemoveFile(path.clone());
        let hub = "https://relay.example";

        assert_eq!(relay_token_for(hub).unwrap(), None);
        // The relay step comes first: only the relay token is saved so far.
        save_relay_token(hub, Some("rt_one")).unwrap();
        assert_eq!(relay_token_for(hub).unwrap().as_deref(), Some("rt_one"));
        assert_eq!(relay_token_for("HTTPS://Relay.Example/").unwrap().as_deref(), Some("rt_one"));
        assert_eq!(relay_token_for("https://other.example").unwrap(), None);

        // The device token arrives later and must not drop the relay token.
        save(hub, "dev.secret", &"a".repeat(32)).unwrap();
        let creds = load(true).unwrap().unwrap();
        assert_eq!(creds.token.as_deref(), Some("dev.secret"));
        assert_eq!(creds.relay_token.as_deref(), Some("rt_one"));

        // Windows protects it with DPAPI, so it is not readable in the file.
        let text = std::fs::read_to_string(&path).unwrap();
        if cfg!(windows) {
            assert!(!text.contains("rt_one"), "{text}");
        }

        // The environment override wins.
        {
            let _e = super::test_env::VarGuard::set(RELAY_TOKEN_ENV, " rt_env ");
            assert_eq!(relay_token_for("https://anything.example").unwrap().as_deref(), Some("rt_env"));
        }

        // Saving for a different hub starts clean; logout clears it.
        save_relay_token("https://elsewhere.example", Some("rt_two")).unwrap();
        assert_eq!(relay_token_for(hub).unwrap(), None);
        assert_eq!(relay_token_for("https://elsewhere.example").unwrap().as_deref(), Some("rt_two"));
        assert_eq!(remove().unwrap().as_deref(), Some("https://elsewhere.example"));
        assert_eq!(relay_token_for("https://elsewhere.example").unwrap(), None);
    }

    #[test]
    fn load_refuses_inside_thread_shell() {
        let _lock = super::test_env::lock();
        let _shell = super::test_env::VarGuard::set(THREAD_SHELL_ENV, "1");
        let err_with_token = load(true).err().unwrap();
        let err_without_token = load(false).err().unwrap();
        let err_resolve = resolve(None, None, "http://127.0.0.1:8080").err().unwrap();
        for err in [err_with_token, err_without_token, err_resolve] {
            assert_eq!(err.to_string(), "stored credentials are not available inside a Grouper thread shell");
        }
    }

    #[test]
    fn hub_address_resolution_order() {
        let _lock = super::test_env::lock();
        let path = std::env::temp_dir().join(format!(
            "grouper-cli-credentials-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        // Set the override before any credentials read, and never the real file.
        let _creds = super::test_env::VarGuard::set(PATH_ENV, &path);
        struct RemoveFile(std::path::PathBuf);
        impl Drop for RemoveFile {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let _remove = RemoveFile(path.clone());

        {
            let _hub = super::test_env::VarGuard::unset(HUB_ENV);
            assert!(
                !path.exists(),
                "the temp credentials file must not already exist at {}",
                path.display()
            );
            assert_eq!(resolve_hub(None).unwrap(), DEFAULT_HUB);
            let (hub, token) = resolve(None, Some("cli-token".into()), DEFAULT_HUB).unwrap();
            assert_eq!((hub.as_str(), token.as_str()), (DEFAULT_HUB, "cli-token"));

            // A credentials file with no hub is the same as no saved hub.
            std::fs::write(&path, "{}\n").unwrap();
            assert_eq!(resolve_hub(None).unwrap(), DEFAULT_HUB);

            save("https://saved.example", "saved-token", &"a".repeat(32)).unwrap();
            assert_eq!(resolve_hub(None).unwrap(), "https://saved.example");
            let (hub, token) = resolve(None, None, DEFAULT_HUB).unwrap();
            assert_eq!((hub.as_str(), token.as_str()), ("https://saved.example", "saved-token"));
            // An explicit token still uses the saved hub when no flag or env is set.
            let (hub, token) = resolve(None, Some("cli-token".into()), DEFAULT_HUB).unwrap();
            assert_eq!((hub.as_str(), token.as_str()), ("https://saved.example", "cli-token"));
        }

        {
            let _hub = super::test_env::VarGuard::set(HUB_ENV, "https://env.example");
            assert_eq!(resolve_hub(None).unwrap(), "https://env.example");
            let message = resolve(None, None, DEFAULT_HUB).unwrap_err().to_string();
            assert!(message.contains("https://saved.example"), "{message}");
            assert!(message.contains("https://env.example"), "{message}");
            assert!(message.contains("--token"), "{message}");
        }

        {
            // Same hub as the file, different case and a trailing slash: saved token is used.
            let _hub = super::test_env::VarGuard::set(HUB_ENV, "HTTPS://SAVED.EXAMPLE/");
            assert_eq!(resolve_hub(None).unwrap(), "HTTPS://SAVED.EXAMPLE/");
            let (hub, token) = resolve(None, None, DEFAULT_HUB).unwrap();
            assert_eq!((hub.as_str(), token.as_str()), ("HTTPS://SAVED.EXAMPLE/", "saved-token"));
        }

        {
            // Blank flag and blank env are absent, so the saved hub is next.
            let _hub = super::test_env::VarGuard::set(HUB_ENV, "   ");
            assert_eq!(resolve_hub(Some("  ")).unwrap(), "https://saved.example");
        }

        {
            let _hub = super::test_env::VarGuard::set(HUB_ENV, "https://env.example");
            assert_eq!(resolve_hub(Some(" https://flag.example/ ")).unwrap(), "https://flag.example/");
            // Flag plus token must not read the credentials file. This file does not parse,
            // so a read would fail the call above.
            let bad = path.with_file_name(format!(
                "grouper-cli-credentials-bad-{}-{}.json",
                std::process::id(),
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
            ));
            std::fs::write(&bad, "not json\n").unwrap();
            let _bad_file = RemoveFile(bad.clone());
            let _bad_path = super::test_env::VarGuard::set(PATH_ENV, &bad);
            let (hub, token) = resolve(Some("https://flag.example".into()), Some("flag-token".into()), DEFAULT_HUB).unwrap();
            assert_eq!((hub.as_str(), token.as_str()), ("https://flag.example", "flag-token"));
            let message = resolve(None, None, DEFAULT_HUB).unwrap_err().to_string();
            assert!(message.contains("parsing saved credentials"), "{message}");
        }
    }
}

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
#[cfg(target_os = "macos")]
const KEYCHAIN_SERVICE: &str = "dev.grouper.cli";
#[cfg(target_os = "macos")]
const KEYCHAIN_ACCOUNT: &str = "device-token";

pub struct Credentials {
    pub hub: String,
    pub token: Option<String>,
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
    resolve_with(cli_hub, cli_token, default_hub, load)
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
                bail!("no token: pass --token, set GROUPER_TOKEN, or run `grouper login --hub <url>`");
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
                .context("no token: pass --token, set GROUPER_TOKEN, or run `grouper login --hub <url>`")?;
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
    let token = if with_token {
        Some(load_token(&stored)?.context("saved credentials do not contain a device token")?)
    } else {
        None
    };
    Ok(Some(Credentials { hub, token }))
}

/// Save the hub and token from `grouper login`, keeping this install's id.
pub fn save(hub: &str, token: &str, install_id: &str) -> Result<()> {
    let stored = StoredCredentials {
        hub: Some(hub.to_string()),
        token: save_token(token)?,
        install_id: Some(install_id.to_string()),
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

#[cfg(windows)]
fn save_token(token: &str) -> Result<Option<String>> {
    Ok(Some(format!(
        "dpapi:{}",
        STANDARD.encode(dpapi::protect(token.as_bytes())?)
    )))
}

#[cfg(windows)]
fn load_token(stored: &StoredCredentials) -> Result<Option<String>> {
    let Some(value) = &stored.token else {
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
fn remove_token() -> Result<()> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn save_token(token: &str) -> Result<Option<String>> {
    security_framework::passwords::set_generic_password(
        KEYCHAIN_SERVICE,
        KEYCHAIN_ACCOUNT,
        token.as_bytes(),
    )
    .context("saving device token in macOS Keychain")?;
    Ok(None)
}

#[cfg(target_os = "macos")]
fn load_token(_stored: &StoredCredentials) -> Result<Option<String>> {
    let bytes =
        security_framework::passwords::get_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
            .context("reading device token from macOS Keychain")?;
    Ok(Some(
        String::from_utf8(bytes).context("saved token is not valid UTF-8")?,
    ))
}

#[cfg(target_os = "macos")]
fn remove_token() -> Result<()> {
    match security_framework::passwords::delete_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
    {
        Ok(()) => Ok(()),
        Err(error) if error.code() == -25300 => Ok(()),
        Err(error) => Err(error).context("removing device token from macOS Keychain"),
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn save_token(token: &str) -> Result<Option<String>> {
    // There is no universal OS key store on Unix. The token is not encrypted;
    // access is restricted with the credentials file's 0600 mode instead.
    Ok(Some(token.to_string()))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn load_token(stored: &StoredCredentials) -> Result<Option<String>> {
    Ok(stored.token.clone())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn remove_token() -> Result<()> {
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
    fn load_refuses_inside_thread_shell() {
        let prev = std::env::var_os(THREAD_SHELL_ENV);
        // SAFETY: only this unit test touches GROUPER_THREAD_SHELL in grouper-cli.
        unsafe {
            std::env::set_var(THREAD_SHELL_ENV, "1");
        }
        let err_with_token = load(true).err().unwrap();
        let err_without_token = load(false).err().unwrap();
        let err_resolve = resolve(None, None, "http://127.0.0.1:8080").err().unwrap();
        unsafe {
            match prev {
                Some(v) => std::env::set_var(THREAD_SHELL_ENV, v),
                None => std::env::remove_var(THREAD_SHELL_ENV),
            }
        }
        for err in [err_with_token, err_without_token, err_resolve] {
            assert_eq!(err.to_string(), "stored credentials are not available inside a Grouper thread shell");
        }
    }
}

//! Per-device hub credentials.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[cfg(windows)]
use base64::{Engine as _, engine::general_purpose::STANDARD};

const FILE_NAME: &str = "credentials.json";
#[cfg(target_os = "macos")]
const KEYCHAIN_SERVICE: &str = "dev.grouper.cli";
#[cfg(target_os = "macos")]
const KEYCHAIN_ACCOUNT: &str = "device-token";

pub struct Credentials {
    pub hub: String,
    pub token: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct StoredCredentials {
    hub: String,
    token: Option<String>,
}

pub fn load(with_token: bool) -> Result<Option<Credentials>> {
    let path = credentials_path()?;
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("reading saved credentials from {}", path.display()));
        }
    };
    let stored: StoredCredentials = serde_json::from_str(&text)
        .with_context(|| format!("parsing saved credentials from {}", path.display()))?;
    let token = if with_token {
        Some(load_token(&stored)?.context("saved credentials do not contain a device token")?)
    } else {
        None
    };
    Ok(Some(Credentials {
        hub: stored.hub,
        token,
    }))
}

pub fn save(hub: &str, token: &str) -> Result<()> {
    let path = credentials_path()?;
    let parent = path
        .parent()
        .context("credentials path has no parent directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("creating config directory {}", parent.display()))?;

    let stored = StoredCredentials {
        hub: hub.to_string(),
        token: save_token(token)?,
    };
    let contents = serde_json::to_vec_pretty(&stored)?;
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

pub fn remove() -> Result<()> {
    remove_token()?;
    let path = credentials_path()?;
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("removing credentials file {}", path.display()))
        }
    }
}

fn credentials_path() -> Result<PathBuf> {
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
        let output = unsafe {
            std::slice::from_raw_parts(output_blob.pbData, output_blob.cbData as usize).to_vec()
        };
        unsafe {
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

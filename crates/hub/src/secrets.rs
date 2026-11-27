//! Encryption for secrets stored by the hub.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit},
};

const ENV_KEY: &str = "GROUPER_MASTER_KEY";
const ENV_KEY_FILE: &str = "GROUPER_MASTER_KEY_FILE";
pub(crate) const PLAIN_KEY_FILE: &str = "master.key";
#[cfg(any(windows, test))]
pub(crate) const DPAPI_KEY_FILE: &str = "master.key.dpapi";
#[cfg(windows)]
const DPAPI_PREFIX: &str = "dpapi:";
#[cfg(target_os = "macos")]
const KEYCHAIN_SERVICE: &str = "uk.clockmillbarn.grouper.hub";
const PREFIX: &str = "enc:v1:";
const KEY_BYTES: usize = 32;
const NONCE_BYTES: usize = 24;

pub struct Secrets {
    cipher: XChaCha20Poly1305,
}

impl Secrets {
    pub fn load(data_dir: &Path) -> Result<Self> {
        Self::load_with_create(data_dir, true)
    }

    pub(crate) fn load_with_create(data_dir: &Path, allow_create: bool) -> Result<Self> {
        let key = load_key_bytes(data_dir, allow_create, true)?;
        Ok(Self::from_key(key))
    }

    pub(crate) fn from_key(key: [u8; KEY_BYTES]) -> Self {
        Self {
            cipher: XChaCha20Poly1305::new((&key).into()),
        }
    }

    pub fn is_sealed(value: &str) -> bool {
        value.starts_with(PREFIX)
    }

    pub fn seal(&self, plaintext: &str) -> Result<String> {
        let mut nonce = [0u8; NONCE_BYTES];
        getrandom::fill(&mut nonce).context("generating a secret nonce")?;
        let ciphertext = self
            .cipher
            .encrypt(XNonce::from_slice(&nonce), plaintext.as_bytes())
            .map_err(|_| anyhow::anyhow!("encrypting secret failed"))?;
        let mut payload = Vec::with_capacity(nonce.len() + ciphertext.len());
        payload.extend_from_slice(&nonce);
        payload.extend_from_slice(&ciphertext);
        Ok(format!("{PREFIX}{}", STANDARD.encode(payload)))
    }

    pub fn open(&self, sealed: &str) -> Result<String> {
        let encoded = sealed
            .strip_prefix(PREFIX)
            .context("secret is not in the supported enc:v1 format")?;
        let payload = STANDARD
            .decode(encoded)
            .context("encrypted secret payload is not valid base64")?;
        if payload.len() < NONCE_BYTES + 16 {
            bail!("encrypted secret payload is too short");
        }
        let (nonce, ciphertext) = payload.split_at(NONCE_BYTES);
        let plaintext = self
            .cipher
            .decrypt(XNonce::from_slice(nonce), ciphertext)
            .map_err(|_| {
                anyhow::anyhow!(
                    "cannot decrypt secret: the master key is wrong or the data was modified"
                )
            })?;
        String::from_utf8(plaintext).context("decrypted secret is not valid UTF-8")
    }
}

/// Read the active master key (base64-encoded 32 bytes) for `data_dir`, respecting
/// `GROUPER_MASTER_KEY`, `GROUPER_MASTER_KEY_FILE`, OS protected storage, and plain `master.key`.
pub(crate) fn read_master_key_b64(data_dir: &Path) -> Result<String> {
    let key = load_key_bytes(data_dir, false, false)?;
    Ok(STANDARD.encode(key))
}

/// Store a base64-encoded 32-byte master key into `data_dir`'s OS-appropriate store
/// (DPAPI `master.key.dpapi` on Windows, Keychain on macOS, `0600` `master.key` on Linux).
pub(crate) fn write_master_key_b64(data_dir: &Path, encoded: &str) -> Result<()> {
    let trimmed = encoded.trim();
    let key = decode_key(trimmed)?;
    #[cfg(any(windows, target_os = "macos"))]
    {
        write_protected_key(data_dir, trimmed)?;
        let read_back = read_protected_key(data_dir)?
            .context("protected master key missing after write")?;
        let read_back_key = decode_key(read_back.trim())
            .context("protected master key invalid after write")?;
        if read_back.trim() != trimmed || read_back_key != key {
            bail!("protected master key read-back mismatch after write");
        }
        Ok(())
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = key;
        write_plain_key_file(&data_dir.join(PLAIN_KEY_FILE), trimmed)
    }
}

/// Remove any stored master key for `data_dir` (used to clean up temporary move-staging stores).
pub(crate) fn remove_master_key(data_dir: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        match fs::remove_file(data_dir.join(DPAPI_KEY_FILE)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing {}", data_dir.join(DPAPI_KEY_FILE).display())),
        }
    }
    #[cfg(target_os = "macos")]
    {
        remove_keychain_key(data_dir)?;
    }
    match fs::remove_file(data_dir.join(PLAIN_KEY_FILE)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", data_dir.join(PLAIN_KEY_FILE).display())),
    }
}

fn decode_key(encoded: &str) -> Result<[u8; KEY_BYTES]> {
    let decoded = STANDARD
        .decode(encoded)
        .context("master key is not valid base64")?;
    decoded
        .try_into()
        .map_err(|_| anyhow::anyhow!("master key must decode to exactly {KEY_BYTES} bytes"))
}

fn load_key_bytes(data_dir: &Path, allow_create: bool, allow_migrate: bool) -> Result<[u8; KEY_BYTES]> {
    if let Some(encoded) = std::env::var_os(ENV_KEY) {
        let encoded = encoded
            .into_string()
            .map_err(|_| anyhow::anyhow!("{ENV_KEY} is not valid UTF-8"))?;
        return decode_key(encoded.trim()).with_context(|| format!("invalid {ENV_KEY}"));
    }

    if let Some(explicit_file) = std::env::var_os(ENV_KEY_FILE) {
        let path = PathBuf::from(explicit_file);
        return load_or_create_plain_file(&path, data_dir, allow_create);
    }

    #[cfg(any(windows, target_os = "macos"))]
    {
        if let Some(encoded) = read_protected_key(data_dir)? {
            return decode_key(encoded.trim())
                .with_context(|| format!("invalid protected master key for {}", data_dir.display()));
        }

        let plain_path = data_dir.join(PLAIN_KEY_FILE);
        match fs::metadata(&plain_path) {
            Ok(_) => {
                if allow_migrate {
                    return migrate_plain_file_with(
                        &plain_path,
                        |trimmed| write_protected_key(data_dir, trimmed),
                        || read_protected_key(data_dir),
                    );
                }
                let raw = fs::read_to_string(&plain_path)
                    .with_context(|| format!("reading master key file {}", plain_path.display()))?;
                return decode_key(raw.trim())
                    .with_context(|| format!("invalid master key file {}", plain_path.display()));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("checking master key file {}", plain_path.display()));
            }
        }

        if !allow_create {
            bail!(
                "master key missing ({}); restore it or set {ENV_KEY}",
                missing_key_location(data_dir)
            );
        }

        let encoded = create_protected_key(data_dir)?;
        decode_key(encoded.trim())
            .with_context(|| format!("invalid protected master key for {}", data_dir.display()))
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = allow_migrate;
        let path = data_dir.join(PLAIN_KEY_FILE);
        load_or_create_plain_file(&path, data_dir, allow_create)
    }
}

fn load_or_create_plain_file(path: &Path, data_dir: &Path, allow_create: bool) -> Result<[u8; KEY_BYTES]> {
    let encoded = match fs::read_to_string(path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if !allow_create {
                bail!("master key missing ({}); restore it or set {ENV_KEY}", path.display());
            }
            create_key_file(path)?
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("reading master key file {}", path.display()));
        }
    };
    if file_is_inside(path, data_dir) {
        tracing::warn!(
            path = %path.display(),
            "master key is inside the hub data directory; copy it separately from data backups"
        );
    }
    decode_key(encoded.trim()).with_context(|| format!("invalid master key file {}", path.display()))
}

#[cfg(any(windows, target_os = "macos", test))]
fn migrate_plain_file_with<W, R>(
    plain_path: &Path,
    mut write_protected: W,
    mut read_protected: R,
) -> Result<[u8; KEY_BYTES]>
where
    W: FnMut(&str) -> Result<()>,
    R: FnMut() -> Result<Option<String>>,
{
    let raw = fs::read_to_string(plain_path)
        .with_context(|| format!("reading master key file {}", plain_path.display()))?;
    let trimmed = raw.trim();
    let key = decode_key(trimmed)
        .with_context(|| format!("invalid master key file {}", plain_path.display()))?;
    write_protected(trimmed)?;
    let read_back = read_protected()?
        .context("protected master key missing during migration read-back verification")?;
    let read_back_key = decode_key(read_back.trim())
        .context("protected master key invalid during migration read-back verification")?;
    if read_back.trim() != trimmed || read_back_key != key {
        bail!(
            "protected master key read-back did not match plain master.key; keeping {}",
            plain_path.display()
        );
    }
    fs::remove_file(plain_path)
        .with_context(|| format!("removing migrated plain master key {}", plain_path.display()))?;
    tracing::info!(
        path = %plain_path.display(),
        "migrated plain master.key to OS protected store and removed plain file"
    );
    Ok(key)
}

#[cfg(windows)]
fn missing_key_location(data_dir: &Path) -> String {
    data_dir.join(DPAPI_KEY_FILE).display().to_string()
}

#[cfg(target_os = "macos")]
fn missing_key_location(data_dir: &Path) -> String {
    format!("Keychain {KEYCHAIN_SERVICE} ({})", data_dir.display())
}

#[cfg(windows)]
fn read_protected_key(data_dir: &Path) -> Result<Option<String>> {
    let path = data_dir.join(DPAPI_KEY_FILE);
    let raw = match fs::read_to_string(&path) {
        Ok(v) => v,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("reading protected master key {}", path.display()));
        }
    };
    let encoded = raw
        .trim()
        .strip_prefix(DPAPI_PREFIX)
        .with_context(|| format!("protected master key {} is missing dpapi: prefix", path.display()))?;
    let encrypted = STANDARD
        .decode(encoded)
        .with_context(|| format!("protected master key {} is not valid base64", path.display()))?;
    let bytes = dpapi::unprotect(&encrypted)
        .with_context(|| format!("unprotecting master key {}", path.display()))?;
    let text = String::from_utf8(bytes)
        .with_context(|| format!("protected master key {} is not valid UTF-8", path.display()))?;
    Ok(Some(text))
}

#[cfg(windows)]
fn write_protected_key(data_dir: &Path, encoded: &str) -> Result<()> {
    fs::create_dir_all(data_dir)
        .with_context(|| format!("creating master key directory {}", data_dir.display()))?;
    let sealed = format!(
        "{DPAPI_PREFIX}{}",
        STANDARD.encode(dpapi::protect(encoded.trim().as_bytes())?)
    );
    write_plain_key_file(&data_dir.join(DPAPI_KEY_FILE), &sealed)
}

#[cfg(windows)]
fn create_protected_key(data_dir: &Path) -> Result<String> {
    fs::create_dir_all(data_dir)
        .with_context(|| format!("creating master key directory {}", data_dir.display()))?;
    let mut key = [0u8; KEY_BYTES];
    getrandom::fill(&mut key).context("generating the master key")?;
    let encoded = STANDARD.encode(key);
    let sealed = format!(
        "{DPAPI_PREFIX}{}",
        STANDARD.encode(dpapi::protect(encoded.as_bytes())?)
    );
    let path = data_dir.join(DPAPI_KEY_FILE);
    let tmp_path = temp_sibling_path(&path)?;
    write_new_temp_file(&tmp_path, &sealed)?;
    let link_res = fs::hard_link(&tmp_path, &path);
    let _ = fs::remove_file(&tmp_path);
    match link_res {
        Ok(()) => Ok(encoded),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => read_protected_key(data_dir)?
            .with_context(|| format!("reading protected master key {}", path.display())),
        Err(error) => {
            Err(error).with_context(|| format!("creating protected master key {}", path.display()))
        }
    }
}

#[cfg(target_os = "macos")]
fn read_protected_key(data_dir: &Path) -> Result<Option<String>> {
    let account = data_dir.to_string_lossy();
    match security_framework::passwords::get_generic_password(KEYCHAIN_SERVICE, &account) {
        Ok(bytes) => {
            let text = String::from_utf8(bytes)
                .context("master key in macOS Keychain is not valid UTF-8")?;
            Ok(Some(text))
        }
        Err(error) if error.code() == -25300 => Ok(None),
        Err(error) => Err(error).context("reading master key from macOS Keychain"),
    }
}

#[cfg(target_os = "macos")]
fn write_protected_key(data_dir: &Path, encoded: &str) -> Result<()> {
    fs::create_dir_all(data_dir)
        .with_context(|| format!("creating master key directory {}", data_dir.display()))?;
    let account = data_dir.to_string_lossy();
    security_framework::passwords::set_generic_password(
        KEYCHAIN_SERVICE,
        &account,
        encoded.trim().as_bytes(),
    )
    .context("saving master key in macOS Keychain")
}

#[cfg(target_os = "macos")]
fn create_protected_key(data_dir: &Path) -> Result<String> {
    if let Some(existing) = read_protected_key(data_dir)? {
        return Ok(existing);
    }
    let mut key = [0u8; KEY_BYTES];
    getrandom::fill(&mut key).context("generating the master key")?;
    let encoded = STANDARD.encode(key);
    write_protected_key(data_dir, &encoded)?;
    Ok(encoded)
}

#[cfg(target_os = "macos")]
fn remove_keychain_key(data_dir: &Path) -> Result<()> {
    let account = data_dir.to_string_lossy();
    match security_framework::passwords::delete_generic_password(KEYCHAIN_SERVICE, &account) {
        Ok(()) => Ok(()),
        Err(error) if error.code() == -25300 => Ok(()),
        Err(error) => Err(error).context("removing master key from macOS Keychain"),
    }
}

fn temp_sibling_path(path: &Path) -> Result<PathBuf> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut suffix = [0u8; 8];
    getrandom::fill(&mut suffix).context("generating a temporary key filename")?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(PLAIN_KEY_FILE);
    Ok(parent.join(format!(".{file_name}.tmp.{}", hex::encode(suffix))))
}

fn write_new_temp_file(tmp_path: &Path, contents: &str) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let write_res = (|| -> Result<()> {
        let mut file = options
            .open(tmp_path)
            .with_context(|| format!("creating temporary master key file {}", tmp_path.display()))?;
        writeln!(file, "{}", contents.trim())
            .with_context(|| format!("writing temporary master key file {}", tmp_path.display()))?;
        file.sync_all()
            .with_context(|| format!("syncing temporary master key file {}", tmp_path.display()))?;
        Ok(())
    })();
    if let Err(error) = write_res {
        let _ = fs::remove_file(tmp_path);
        return Err(error);
    }
    Ok(())
}

#[cfg(any(windows, not(target_os = "macos")))]
fn write_plain_key_file(path: &Path, contents: &str) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("creating master key directory {}", parent.display()))?;
    let tmp_path = temp_sibling_path(path)?;
    write_new_temp_file(&tmp_path, contents)?;
    if let Err(error) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(error).with_context(|| format!("replacing master key file {}", path.display()));
    }
    Ok(())
}

fn create_key_file(path: &Path) -> Result<String> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("creating master key directory {}", parent.display()))?;
    let mut key = [0u8; KEY_BYTES];
    getrandom::fill(&mut key).context("generating the master key")?;
    let encoded = STANDARD.encode(key);

    let tmp_path = temp_sibling_path(path)?;
    write_new_temp_file(&tmp_path, &encoded)?;

    let link_res = fs::hard_link(&tmp_path, path);
    let _ = fs::remove_file(&tmp_path);
    match link_res {
        Ok(()) => Ok(encoded),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => fs::read_to_string(path)
            .with_context(|| format!("reading master key file {}", path.display())),
        Err(error) => {
            Err(error).with_context(|| format!("creating master key file {}", path.display()))
        }
    }
}

fn file_is_inside(path: &Path, directory: &Path) -> bool {
    let Ok(path) = fs::canonicalize(path) else {
        return false;
    };
    let Ok(directory) = fs::canonicalize(directory) else {
        return false;
    };
    path.starts_with(directory)
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
            u32::try_from(input.len()).context("master key is too large for DPAPI")?;
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
                .with_context(|| format!("{operation} master key with Windows DPAPI"));
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_uses_fresh_nonces() {
        let secrets = Secrets::from_key([7; KEY_BYTES]);
        let first = secrets.seal("provider-secret").unwrap();
        let second = secrets.seal("provider-secret").unwrap();
        assert_ne!(first, second);
        assert!(Secrets::is_sealed(&first));
        assert_eq!(secrets.open(&first).unwrap(), "provider-secret");
    }

    #[test]
    fn wrong_key_and_tampering_are_rejected() {
        let secrets = Secrets::from_key([1; KEY_BYTES]);
        let sealed = secrets.seal("provider-secret").unwrap();
        let error = Secrets::from_key([2; KEY_BYTES]).open(&sealed).unwrap_err();
        assert!(error.to_string().contains("master key is wrong"));

        let mut payload = STANDARD
            .decode(sealed.strip_prefix(PREFIX).unwrap())
            .unwrap();
        *payload.last_mut().unwrap() ^= 1;
        let tampered = format!("{PREFIX}{}", STANDARD.encode(payload));
        assert!(secrets.open(&tampered).is_err());
    }

    #[test]
    fn create_key_file_is_atomic_and_does_not_overwrite() {
        let dir = std::env::temp_dir().join(format!("grouper-secrets-test-{}", crate::db::random_hex(4)));
        let path = dir.join("master.key");
        let first = create_key_file(&path).unwrap();
        let entries: Vec<_> = fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(entries.len(), 1);

        // Calling again must keep the existing key and return its contents.
        let second = create_key_file(&path).unwrap();
        assert_eq!(second.trim(), first.trim());
        assert_eq!(fs::read_to_string(&path).unwrap().trim(), first.trim());
        fs::remove_dir_all(dir).ok();
    }
}

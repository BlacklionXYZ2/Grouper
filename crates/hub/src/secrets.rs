//! Encryption for secrets stored by the hub.

use std::cell::Cell;
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
pub(crate) const DPAPI_KEY_FILE: &str = "master.key.dpapi";
#[cfg(windows)]
const DPAPI_PREFIX: &str = "dpapi:";
#[cfg(target_os = "macos")]
const KEYCHAIN_SERVICE: &str = "uk.clockmillbarn.grouper.hub";
const PREFIX: &str = "enc:v1:";
const KEY_BYTES: usize = 32;
const NONCE_BYTES: usize = 24;

thread_local! {
    static SUPPRESS_MIGRATION: Cell<bool> = const { Cell::new(false) };
}

/// Run `f` without migrating/deleting a plain `master.key` file (used by `backup::restore`
/// so a plain `master.key` placed before `restore` stays in place until the next hub start).
pub(crate) fn without_migration<T>(f: impl FnOnce() -> T) -> T {
    struct Guard(bool);
    impl Drop for Guard {
        fn drop(&mut self) {
            SUPPRESS_MIGRATION.with(|flag| flag.set(self.0));
        }
    }
    let prev = SUPPRESS_MIGRATION.with(|flag| {
        let old = flag.get();
        flag.set(true);
        old
    });
    let _guard = Guard(prev);
    f()
}

pub struct Secrets {
    cipher: XChaCha20Poly1305,
}

impl Secrets {
    pub fn load(data_dir: &Path) -> Result<Self> {
        Self::load_with_create(data_dir, true)
    }

    pub(crate) fn load_with_create(data_dir: &Path, allow_create: bool) -> Result<Self> {
        let allow_migrate = !SUPPRESS_MIGRATION.with(Cell::get);
        let key = load_key_bytes(data_dir, allow_create, allow_migrate)?;
        Ok(Self::from_key(key))
    }

    /// Load an existing master key for `backup::restore` verification without creating a new key
    /// and without migrating/deleting a plain `master.key`. If a plain `master.key` was placed in
    /// `data_dir` for `--force` restore, prefer it over a previous hub's protected key.
    pub(crate) fn load_for_restore(data_dir: &Path) -> Result<Self> {
        if std::env::var_os(ENV_KEY).is_some() {
            let key = load_key_bytes(data_dir, false, false).context("reading GROUPER_MASTER_KEY")?;
            return Ok(Self::from_key(key));
        }
        if let Some(explicit) = std::env::var_os(ENV_KEY_FILE) {
            let path = PathBuf::from(explicit);
            if !path.is_file() {
                bail!(
                    "master key missing ({}). This backup does not contain the master key. Restore the same \
                     key with {ENV_KEY} or {ENV_KEY_FILE} (Docker: /secrets/master.key) \
                     before restoring, or provider keys cannot be decrypted. Nothing was restored",
                    path.display()
                );
            }
            let key = load_or_create_plain_file(&path, data_dir, false)
                .with_context(|| format!("reading master key {}", path.display()))?;
            return Ok(Self::from_key(key));
        }
        let plain_path = data_dir.join(PLAIN_KEY_FILE);
        if plain_path.is_file() {
            let key = load_or_create_plain_file(&plain_path, data_dir, false)
                .with_context(|| format!("reading master key {}", plain_path.display()))?;
            return Ok(Self::from_key(key));
        }
        #[cfg(any(windows, target_os = "macos"))]
        if let Some(encoded) = read_protected_key(data_dir)? {
            let key = decode_key(encoded.trim())
                .with_context(|| format!("invalid protected master key for {}", data_dir.display()))?;
            return Ok(Self::from_key(key));
        }
        bail!(
            "master key missing ({}). This backup does not contain the master key. Restore the same \
             key with {ENV_KEY} or {ENV_KEY_FILE} (Docker: /secrets/master.key) \
             before restoring, or provider keys cannot be decrypted. Nothing was restored",
            plain_path.display()
        )
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
        let plain_path = data_dir.join(PLAIN_KEY_FILE);
        if !allow_migrate && plain_path.is_file() {
            let raw = fs::read_to_string(&plain_path)
                .with_context(|| format!("reading master key file {}", plain_path.display()))?;
            return decode_key(raw.trim())
                .with_context(|| format!("invalid master key file {}", plain_path.display()));
        }

        if let Some(encoded) = read_protected_key(data_dir)? {
            return decode_key(encoded.trim())
                .with_context(|| format!("invalid protected master key for {}", data_dir.display()));
        }

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

    #[cfg(windows)]
    #[test]
    fn windows_dpapi_protected_round_trip() {
        let dir = std::env::temp_dir().join(format!("grouper-secrets-dpapi-{}", crate::db::random_hex(6)));
        let first = Secrets::load(&dir).unwrap();
        let dpapi_path = dir.join(DPAPI_KEY_FILE);
        let plain_path = dir.join(PLAIN_KEY_FILE);
        assert!(dpapi_path.is_file(), "expected {}", dpapi_path.display());
        assert!(!plain_path.exists(), "plain master.key must not be created on Windows");

        let on_disk = fs::read_to_string(&dpapi_path).unwrap();
        assert!(on_disk.starts_with(DPAPI_PREFIX));
        let key_b64 = read_master_key_b64(&dir).unwrap();
        assert!(!on_disk.contains(&key_b64), "raw key must not appear in master.key.dpapi");

        let sealed = first.seal("provider-key-win").unwrap();
        let second = Secrets::load(&dir).unwrap();
        assert_eq!(second.open(&sealed).unwrap(), "provider-key-win");

        // write_master_key_b64 lands DPAPI-protected in a target directory without plain master.key.
        let target_dir = dir.join("moved-target");
        write_master_key_b64(&target_dir, &key_b64).unwrap();
        assert!(target_dir.join(DPAPI_KEY_FILE).is_file());
        assert!(!target_dir.join(PLAIN_KEY_FILE).exists());
        assert_eq!(Secrets::load(&target_dir).unwrap().open(&sealed).unwrap(), "provider-key-win");

        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn migration_deletes_plain_file_only_after_verified_read_back() {
        let dir = std::env::temp_dir().join(format!("grouper-secrets-migrate-{}", crate::db::random_hex(6)));
        fs::create_dir_all(&dir).unwrap();
        let plain_path = dir.join(PLAIN_KEY_FILE);
        let raw_key = [0x5Au8; KEY_BYTES];
        let encoded = STANDARD.encode(raw_key);
        fs::write(&plain_path, format!("{encoded}\n")).unwrap();

        // Write failure -> plain file stays intact.
        assert!(
            migrate_plain_file_with(&plain_path, |_| bail!("simulated write failure"), || unreachable!())
                .is_err()
        );
        assert!(plain_path.is_file());

        // Missing read-back -> plain file stays intact.
        assert!(migrate_plain_file_with(&plain_path, |_| Ok(()), || Ok(None)).is_err());
        assert!(plain_path.is_file());

        // Mismatched read-back -> plain file stays intact.
        let wrong_b64 = STANDARD.encode([0x11u8; KEY_BYTES]);
        assert!(migrate_plain_file_with(&plain_path, |_| Ok(()), || Ok(Some(wrong_b64.clone()))).is_err());
        assert!(plain_path.is_file());
        assert_eq!(fs::read_to_string(&plain_path).unwrap().trim(), encoded);

        // Identical verified read-back -> deletes plain file and returns the exact key.
        let stored = std::cell::RefCell::new(None);
        let migrated = migrate_plain_file_with(
            &plain_path,
            |v| {
                *stored.borrow_mut() = Some(v.to_string());
                Ok(())
            },
            || Ok(stored.borrow().clone()),
        )
        .unwrap();
        assert_eq!(migrated, raw_key);
        assert!(!plain_path.exists(), "plain master.key must be deleted after verified read-back");

        #[cfg(windows)]
        {
            // Real DPAPI migration on Windows: plain master.key -> master.key.dpapi, plain deleted.
            let win_dir = dir.join("real-win-migrate");
            fs::create_dir_all(&win_dir).unwrap();
            let win_plain = win_dir.join(PLAIN_KEY_FILE);
            fs::write(&win_plain, format!("{encoded}\n")).unwrap();
            let pre_sealed = Secrets::from_key(raw_key).seal("migrated-secret").unwrap();

            let loaded = Secrets::load(&win_dir).unwrap();
            assert_eq!(loaded.open(&pre_sealed).unwrap(), "migrated-secret");
            assert!(win_dir.join(DPAPI_KEY_FILE).is_file());
            assert!(!win_plain.exists());

            // Subsequent load reads from DPAPI and still decrypts.
            let loaded_again = Secrets::load(&win_dir).unwrap();
            assert_eq!(loaded_again.open(&pre_sealed).unwrap(), "migrated-secret");
        }

        fs::remove_dir_all(dir).ok();
    }

    const CHILD_MODE_ENV: &str = "GROUPER_SECRETS_TEST_CHILD";

    #[test]
    fn env_var_wins_and_server_mode_key_file_is_unchanged() {
        if let Ok(mode) = std::env::var(CHILD_MODE_ENV) {
            let root = PathBuf::from(std::env::var("GROUPER_SECRETS_TEST_DIR").unwrap());
            match mode.as_str() {
                "env_wins" => {
                    let data_dir = root.join("data");
                    let env_expected = [0x33u8; KEY_BYTES];
                    let sealed_by_env = Secrets::from_key(env_expected).seal("from-env").unwrap();
                    let loaded = Secrets::load(&data_dir).unwrap();
                    assert_eq!(loaded.open(&sealed_by_env).unwrap(), "from-env");
                    assert_eq!(read_master_key_b64(&data_dir).unwrap(), STANDARD.encode(env_expected));
                    // Plain master.key in data_dir must remain untouched when GROUPER_MASTER_KEY is set.
                    assert!(data_dir.join(PLAIN_KEY_FILE).is_file());
                }
                "server_file" => {
                    let data_dir = root.join("server-data");
                    fs::create_dir_all(&data_dir).unwrap();
                    let key_file = PathBuf::from(std::env::var(ENV_KEY_FILE).unwrap());
                    assert!(!key_file.exists());

                    let first = Secrets::load(&data_dir).unwrap();
                    assert!(key_file.is_file(), "server mode must create GROUPER_MASTER_KEY_FILE");
                    let raw = fs::read_to_string(&key_file).unwrap();
                    let decoded = decode_key(raw.trim()).unwrap();
                    assert_eq!(STANDARD.encode(decoded), raw.trim());
                    assert!(!data_dir.join(PLAIN_KEY_FILE).exists());
                    assert!(!data_dir.join(DPAPI_KEY_FILE).exists());

                    let sealed = first.seal("docker-secret").unwrap();
                    let second = Secrets::load(&data_dir).unwrap();
                    assert_eq!(second.open(&sealed).unwrap(), "docker-secret");
                    assert!(key_file.is_file(), "server mode must never migrate/delete GROUPER_MASTER_KEY_FILE");
                    assert_eq!(read_master_key_b64(&data_dir).unwrap(), raw.trim());
                }
                other => panic!("unknown child mode {other}"),
            }
            return;
        }

        let root = std::env::temp_dir().join(format!("grouper-secrets-env-{}", crate::db::random_hex(6)));
        let data_dir = root.join("data");
        fs::create_dir_all(&data_dir).unwrap();

        // Populate both protected store (on desktop) and a plain master.key, plus a key file, all with different keys.
        write_master_key_b64(&data_dir, &STANDARD.encode([0x11u8; KEY_BYTES])).unwrap();
        fs::write(data_dir.join(PLAIN_KEY_FILE), format!("{}\n", STANDARD.encode([0x22u8; KEY_BYTES]))).unwrap();
        let other_file = root.join("other-master.key");
        fs::write(&other_file, format!("{}\n", STANDARD.encode([0x44u8; KEY_BYTES]))).unwrap();

        let exe = std::env::current_exe().unwrap();

        // 1. GROUPER_MASTER_KEY wins over GROUPER_MASTER_KEY_FILE, protected store, and plain master.key.
        let status = std::process::Command::new(&exe)
            .args(["--exact", "secrets::tests::env_var_wins_and_server_mode_key_file_is_unchanged", "--nocapture"])
            .env(CHILD_MODE_ENV, "env_wins")
            .env("GROUPER_SECRETS_TEST_DIR", &root)
            .env(ENV_KEY, STANDARD.encode([0x33u8; KEY_BYTES]))
            .env(ENV_KEY_FILE, &other_file)
            .status()
            .unwrap();
        assert!(status.success(), "env_wins child failed");

        // 2. Server mode with GROUPER_MASTER_KEY_FILE creates/reads plain file and never uses DPAPI/Keychain.
        let server_key_file = root.join("secrets-vol").join("master.key");
        let status = std::process::Command::new(&exe)
            .args(["--exact", "secrets::tests::env_var_wins_and_server_mode_key_file_is_unchanged", "--nocapture"])
            .env(CHILD_MODE_ENV, "server_file")
            .env("GROUPER_SECRETS_TEST_DIR", &root)
            .env_remove(ENV_KEY)
            .env(ENV_KEY_FILE, &server_key_file)
            .status()
            .unwrap();
        assert!(status.success(), "server_file child failed");

        let _ = remove_master_key(&data_dir);
        fs::remove_dir_all(root).ok();
    }
}

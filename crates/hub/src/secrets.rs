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
const PREFIX: &str = "enc:v1:";
const KEY_BYTES: usize = 32;
const NONCE_BYTES: usize = 24;

pub struct Secrets {
    cipher: XChaCha20Poly1305,
}

impl Secrets {
    pub fn load(data_dir: &Path) -> Result<Self> {
        if let Some(encoded) = std::env::var_os(ENV_KEY) {
            let encoded = encoded
                .into_string()
                .map_err(|_| anyhow::anyhow!("{ENV_KEY} is not valid UTF-8"))?;
            return Self::from_base64(encoded.trim()).with_context(|| format!("invalid {ENV_KEY}"));
        }

        let path = std::env::var_os(ENV_KEY_FILE)
            .map(PathBuf::from)
            .unwrap_or_else(|| data_dir.join("master.key"));
        let encoded = match fs::read_to_string(&path) {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => create_key_file(&path)?,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("reading master key file {}", path.display()));
            }
        };
        if file_is_inside(&path, data_dir) {
            tracing::warn!(
                path = %path.display(),
                "master key is inside the hub data directory; copy it separately from data backups"
            );
        }
        Self::from_base64(encoded.trim())
            .with_context(|| format!("invalid master key file {}", path.display()))
    }

    pub(crate) fn from_key(key: [u8; KEY_BYTES]) -> Self {
        Self {
            cipher: XChaCha20Poly1305::new((&key).into()),
        }
    }

    fn from_base64(encoded: &str) -> Result<Self> {
        let decoded = STANDARD
            .decode(encoded)
            .context("master key is not valid base64")?;
        let key: [u8; KEY_BYTES] = decoded
            .try_into()
            .map_err(|_| anyhow::anyhow!("master key must decode to exactly {KEY_BYTES} bytes"))?;
        Ok(Self::from_key(key))
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

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(mut file) => {
            writeln!(file, "{encoded}")
                .with_context(|| format!("writing master key file {}", path.display()))?;
            file.sync_all()
                .with_context(|| format!("syncing master key file {}", path.display()))?;
            Ok(encoded)
        }
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
}

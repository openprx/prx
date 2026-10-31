//! Per-account data keys for the ramflux channel.
//!
//! Each account has a 32-byte DEK wrapped by [`crate::security::SecretStore`].
//! The DEK stays in memory, is zeroized on lock, and is never formatted into
//! logs. Ciphertext is ChaCha20-Poly1305 with associated data
//! `agent_account_id || conversation_id`.

use chacha20poly1305::aead::rand_core::RngCore;
use chacha20poly1305::aead::{Aead, KeyInit, OsRng, Payload};
use chacha20poly1305::{AeadCore, ChaCha20Poly1305, Key, Nonce};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

const DEK_LEN: usize = 32;
const NONCE_LEN: usize = 12;
const MAGIC: &[u8] = b"RF1";

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("ramflux data key is locked")]
    Locked,
    #[error("ramflux ciphertext was sealed for a different conversation")]
    AadMismatch,
    #[error("ramflux ciphertext is truncated")]
    Truncated,
    #[error("{0}")]
    Store(String),
}

/// Account data keys and the AEAD that uses them.
pub struct AccountCrypto {
    secrets_dir: PathBuf,
    key_dir: PathBuf,
    enabled: bool,
    deks: Mutex<HashMap<String, Zeroizing<[u8; DEK_LEN]>>>,
}

impl std::fmt::Debug for AccountCrypto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let dek_count = self.deks.lock().len();
        let deks = format!("[redacted; count={dek_count}]");
        f.debug_struct("AccountCrypto")
            .field("secrets_dir", &self.secrets_dir)
            .field("key_dir", &self.key_dir)
            .field("enabled", &self.enabled)
            .field("deks", &deks)
            .finish()
    }
}

impl AccountCrypto {
    /// Build a key manager. `secrets_dir` is the prx secret-store root.
    pub fn new(secrets_dir: impl Into<PathBuf>, data_dir: &Path, enabled: bool) -> Self {
        Self {
            secrets_dir: secrets_dir.into(),
            key_dir: data_dir.join("keys"),
            enabled,
            deks: Mutex::new(HashMap::new()),
        }
    }

    /// Unwrap the account DEK into memory, creating one on first use.
    ///
    /// # Errors
    ///
    /// Returns [`CryptoError`] when the wrapped key cannot be read or unwrapped.
    pub fn unlock(&self, account_id: &str) -> Result<(), CryptoError> {
        if !self.enabled {
            return Ok(());
        }
        if self.deks.lock().contains_key(account_id) {
            return Ok(());
        }
        let dek = match self.read_wrapped(account_id)? {
            Some(dek) => dek,
            None => {
                let dek = random_dek();
                self.write_wrapped(account_id, &dek)?;
                dek
            }
        };
        self.deks.lock().insert(account_id.to_string(), dek);
        Ok(())
    }

    /// Drop the in-memory DEK. Wrapped key material on disk stays.
    pub fn lock(&self, account_id: &str) {
        self.deks.lock().remove(account_id);
    }

    /// Drop every in-memory DEK.
    pub fn lock_all(&self) {
        self.deks.lock().clear();
    }

    /// Delete the wrapped key and the in-memory copy.
    ///
    /// # Errors
    ///
    /// Returns [`CryptoError`] when the key file cannot be overwritten.
    pub fn destroy(&self, account_id: &str) -> Result<(), CryptoError> {
        self.lock(account_id);
        let path = self.key_path(account_id);
        if path.exists() {
            overwrite_and_remove(&path).map_err(|error| CryptoError::Store(error.to_string()))?;
        }
        Ok(())
    }

    /// Seal `plaintext`. Empty input returns an empty blob when encryption is off.
    ///
    /// # Errors
    ///
    /// Returns [`CryptoError::Locked`] when the account DEK is not in memory.
    pub fn seal(&self, account_id: &str, conversation_id: &str, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        if !self.enabled {
            return Ok(plaintext.to_vec());
        }
        let deks = self.deks.lock();
        let dek = deks.get(account_id).ok_or(CryptoError::Locked)?;
        let cipher = ChaCha20Poly1305::new(Key::from_slice(dek.as_ref()));
        let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
        let aad = associated_data(account_id, conversation_id);
        let ciphertext = cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: plaintext,
                    aad: &aad,
                },
            )
            .map_err(|_| CryptoError::AadMismatch)?;
        let mut out = Vec::with_capacity(MAGIC.len() + NONCE_LEN + ciphertext.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }

    /// Open a blob sealed by [`Self::seal`].
    ///
    /// # Errors
    ///
    /// Returns [`CryptoError::Locked`] or [`CryptoError::AadMismatch`].
    pub fn open(&self, account_id: &str, conversation_id: &str, blob: &[u8]) -> Result<Vec<u8>, CryptoError> {
        if !self.enabled {
            return Ok(blob.to_vec());
        }
        let rest = blob.strip_prefix(MAGIC).ok_or(CryptoError::Truncated)?;
        if rest.len() < NONCE_LEN {
            return Err(CryptoError::Truncated);
        }
        let (nonce_bytes, ciphertext) = rest.split_at_checked(NONCE_LEN).ok_or(CryptoError::Truncated)?;
        let deks = self.deks.lock();
        let dek = deks.get(account_id).ok_or(CryptoError::Locked)?;
        let cipher = ChaCha20Poly1305::new(Key::from_slice(dek.as_ref()));
        let nonce = Nonce::from_slice(nonce_bytes);
        let aad = associated_data(account_id, conversation_id);
        cipher
            .decrypt(
                nonce,
                Payload {
                    msg: ciphertext,
                    aad: &aad,
                },
            )
            .map_err(|_| CryptoError::AadMismatch)
    }

    fn key_path(&self, account_id: &str) -> PathBuf {
        self.key_dir.join(format!("{}.dek", sanitize_id(account_id)))
    }

    fn read_wrapped(&self, account_id: &str) -> Result<Option<Zeroizing<[u8; DEK_LEN]>>, CryptoError> {
        let path = self.key_path(account_id);
        if !path.exists() {
            return Ok(None);
        }
        let wrapped = std::fs::read_to_string(&path).map_err(|error| CryptoError::Store(error.to_string()))?;
        let store = crate::security::SecretStore::new(&self.secrets_dir, true);
        let hex_dek = store
            .decrypt(wrapped.trim())
            .map_err(|error| CryptoError::Store(error.to_string()))?;
        let bytes = hex::decode(hex_dek.trim()).map_err(|error| CryptoError::Store(error.to_string()))?;
        if bytes.len() != DEK_LEN {
            return Err(CryptoError::Store("wrapped data key has the wrong length".to_string()));
        }
        let mut dek = Zeroizing::new([0u8; DEK_LEN]);
        dek.copy_from_slice(&bytes);
        Ok(Some(dek))
    }

    fn write_wrapped(&self, account_id: &str, dek: &Zeroizing<[u8; DEK_LEN]>) -> Result<(), CryptoError> {
        std::fs::create_dir_all(&self.key_dir).map_err(|error| CryptoError::Store(error.to_string()))?;
        let store = crate::security::SecretStore::new(&self.secrets_dir, true);
        let wrapped = store
            .encrypt(&hex::encode(dek.as_ref()))
            .map_err(|error| CryptoError::Store(error.to_string()))?;
        let path = self.key_path(account_id);
        std::fs::write(&path, wrapped).map_err(|error| CryptoError::Store(error.to_string()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .map_err(|error| CryptoError::Store(error.to_string()))?;
        }
        Ok(())
    }
}

fn random_dek() -> Zeroizing<[u8; DEK_LEN]> {
    let mut dek = Zeroizing::new([0u8; DEK_LEN]);
    OsRng.fill_bytes(dek.as_mut());
    dek
}

fn associated_data(account_id: &str, conversation_id: &str) -> Vec<u8> {
    let mut out = Vec::new();
    push_len(&mut out, account_id.as_bytes());
    push_len(&mut out, conversation_id.as_bytes());
    out
}

fn push_len(out: &mut Vec<u8>, bytes: &[u8]) {
    let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
}

fn sanitize_id(account_id: &str) -> String {
    account_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

pub(crate) fn overwrite_and_remove(path: &Path) -> std::io::Result<()> {
    if let Ok(meta) = std::fs::metadata(path) {
        let len = usize::try_from(meta.len()).unwrap_or(0);
        let zeros = vec![0u8; len];
        std::fs::write(path, zeros)?;
    }
    std::fs::remove_file(path)
}

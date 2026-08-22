//! API key storage at rest.
//!
//! Each provider key is encrypted with AES-256-GCM using a per-install master
//! key, then written to `<data_dir>/keychain/<id>.bin`. The master key is a
//! random 32-byte secret generated on first launch and stored next to the
//! ciphertext at `<data_dir>/keychain/master.bin` with restrictive perms.
//!
//! Security caveat: the master key sits next to the ciphertext, so this is
//! defense-in-depth rather than OS-bound crypto. It protects against casual
//! disk access and shoulder-surfing; a determined attacker with file-system
//! access can decrypt all keys. That's the accepted tradeoff for "works
//! anywhere without an OS credential service" — same posture as browsers
//! storing cookies at rest.

use crate::db::data_dir;
use crate::error::{AppError, AppResult};
use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use rand::RngCore;
use std::path::{Path, PathBuf};

const KEY_DIR: &str = "keychain";
const MASTER_FILENAME: &str = "master.bin";
const NONCE_LEN: usize = 12;

fn key_dir() -> AppResult<PathBuf> {
    // Override mirroring the NESTRA_HOME_DIR pattern: point the keychain at a
    // temp dir so unit tests (and portable setups) never touch the real one.
    if let Ok(p) = std::env::var("NESTRA_KEYCHAIN_DIR") {
        if !p.is_empty() {
            return Ok(PathBuf::from(p));
        }
    }
    Ok(data_dir()?.join(KEY_DIR))
}

fn master_key_path() -> AppResult<PathBuf> {
    Ok(key_dir()?.join(MASTER_FILENAME))
}

fn provider_key_path(id: &str) -> AppResult<PathBuf> {
    // Sanitize: id is already constrained to [a-z0-9-]+ by validate_id, but
    // defend against a future relaxation.
    if id.is_empty()
        || id == MASTER_FILENAME.trim_end_matches(".bin")
        || id == "master"
        || !id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    {
        return Err(AppError::Internal(format!(
            "provider id '{id}' not safe for filesystem storage"
        )));
    }
    Ok(key_dir()?.join(format!("{id}.bin")))
}

fn load_or_create_master_key() -> AppResult<[u8; 32]> {
    let dir = key_dir()?;
    std::fs::create_dir_all(&dir)?;

    let path = master_key_path()?;
    if path.exists() {
        let bytes = std::fs::read(&path)?;
        if bytes.len() != 32 {
            return Err(AppError::Internal("corrupt master key file".into()));
        }
        let mut key = [0u8; 32];
        key.copy_from_slice(&bytes);
        return Ok(key);
    }

    // No master.bin but OTHER provider key files exist: the master key was
    // lost/deleted. Regenerating here would silently make every stored
    // credential undecryptable — indistinguishable from corruption, and
    // irreversible. Refuse loudly instead.
    let has_stored_keys = std::fs::read_dir(&dir)
        .map(|it| {
            it.flatten().any(|e| {
                let name = e.file_name();
                name.to_string_lossy().ends_with(".bin") && name != std::ffi::OsStr::new(MASTER_FILENAME)
            })
        })
        .unwrap_or(false);
    if has_stored_keys {
        return Err(AppError::Internal(
            "master key missing but stored provider keys exist — refusing to regenerate (credentials would be unrecoverable)".into(),
        ));
    }

    let mut key = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut key);
    atomic_write(&path, &key)?;
    Ok(key)
}

/// Atomic write with restrictive perms: the temp file is created 0600
/// (umask-independent) and fsynced before the rename, so a crash can never
/// leave a zero-length or world-readable key file behind. `rename` atomically
/// replaces the target on both Unix and Windows.
fn atomic_write(path: &Path, bytes: &[u8]) -> AppResult<()> {
    use std::io::Write;
    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("keychain-entry");
    static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = parent.join(format!(".{file_name}.tmp.{}.{seq}", std::process::id()));

    let res = (|| -> AppResult<()> {
        #[cfg(unix)]
        let mut f = {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)?
        };
        #[cfg(not(unix))]
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path).map_err(AppError::Io)?;
        Ok(())
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

pub fn set(provider_id: &str, key: &str) -> AppResult<()> {
    let master = load_or_create_master_key()?;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&master));

    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, key.as_bytes())
        .map_err(|e| AppError::Internal(format!("encrypt: {e}")))?;

    let mut blob = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    blob.extend_from_slice(&nonce_bytes);
    blob.extend_from_slice(&ciphertext);

    atomic_write(&provider_key_path(provider_id)?, &blob)
}

pub fn get(provider_id: &str) -> AppResult<Option<String>> {
    let path = provider_key_path(provider_id)?;
    if !path.exists() {
        return Ok(None);
    }
    let blob = std::fs::read(&path)?;
    if blob.len() < NONCE_LEN + 16 {
        return Err(AppError::Internal("corrupt key file".into()));
    }
    let (nonce_bytes, ciphertext) = blob.split_at(NONCE_LEN);

    let master = load_or_create_master_key()?;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&master));
    let nonce = Nonce::from_slice(nonce_bytes);

    let plaintext = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| AppError::Internal(format!("decrypt: {e}")))?;
    String::from_utf8(plaintext)
        .map(Some)
        .map_err(|e| AppError::Internal(format!("utf8: {e}")))
}

pub fn delete(provider_id: &str) -> AppResult<()> {
    let path = provider_key_path(provider_id)?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(AppError::Io(e)),
    }
}

#[cfg(test)]
mod tests;
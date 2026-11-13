//! Credential encryption for the SQLite store (Issue #1189, review rule 1/2).
//!
//! Account credentials (API key / secret / passphrase / private key) MUST be
//! recoverable at signing time, so a one-way hash (bcrypt etc.) is not an
//! option — they are encrypted with AES-256-GCM, one random 96-bit nonce per
//! record, and stored as `nonce (12 bytes) || ciphertext || tag (16 bytes)`.
//!
//! The master key comes from `BLITZKRIEG_DB_KEY` (base64, 32 raw bytes). When
//! it is missing or does not decode to exactly 32 bytes, decryption is
//! REFUSED with a clear error — never a silent plaintext fallback (review
//! rule 2). The only plaintext outside the database is the `.env` fallback
//! (handoff §2): an account with no row in the DB reads the venue's declared
//! env var, the same source the pre-SQLite kernel used.
//!
//! SECRETS: this module never logs, prints, or echoes credential VALUES.
//! Error messages name keys and lengths, never material. Callers that log
//! report key NAMES only (the same hard line as `crate::env_file`).

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use rand::RngCore;
use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::model::{CoreError, CoreErrorCode, CoreResult};

/// AES-256-GCM nonce length in bytes (the 96-bit standard: the size that
/// keeps GCM's birthday bound comfortable under a random per-record nonce).
pub const NONCE_LEN: usize = 12;
/// AES-256-GCM tag length in bytes (128-bit authentication tag).
pub const TAG_LEN: usize = 16;
/// Minimum stored blob length: nonce + tag, i.e. an empty plaintext still
/// leaves 28 bytes on the wire.
pub const MIN_BLOB_LEN: usize = NONCE_LEN + TAG_LEN;
/// The master-key env var. Base64 of exactly 32 raw bytes (AES-256).
pub const MASTER_KEY_ENV: &str = "BLITZKRIEG_DB_KEY";

/// `credential_values` table rows behind the DB half of [`CredentialsManager`].
///
/// The migration (#1189) lands the SQLite schema separately; this trait is the
/// read face the manager needs NOW so the encryption logic is testable without
/// dragging `rusqlite` into the core crate before the schema exists. The
/// production implementation (`sqlite` feature, schema wave) will pass
/// `&[(account_id, field, blob)]`; tests use a map. Values are BLOBs — never
/// parsed as UTF-8 here, never logged.
pub trait CredentialRowSource {
    /// Every `(account_id, field, blob)` row the DB currently holds. The
    /// manager filters; the source only reads.
    fn credential_rows(&self) -> Vec<(String, String, Vec<u8>)>;
}

impl CredentialRowSource for BTreeMap<(String, String), Vec<u8>> {
    fn credential_rows(&self) -> Vec<(String, String, Vec<u8>)> {
        self.iter()
            .map(|((a, f), v)| (a.clone(), f.clone(), v.clone()))
            .collect()
    }
}

impl<F: Fn() -> Vec<(String, String, Vec<u8>)>> CredentialRowSource for F {
    fn credential_rows(&self) -> Vec<(String, String, Vec<u8>)> {
        self()
    }
}

/// The one credential failure mode: a wrong or missing master key. Kept as a
/// distinct constructor from other `CoreError`s so callers can match on it
/// (the launcher refuses to boot a decrypt path on it, the IPC layer maps it
/// to `NotAuthenticated` — an operator error, not a bug).
fn key_error(message: impl Into<String>) -> CoreError {
    CoreError::new(CoreErrorCode::NotAuthenticated, message)
}

/// Decrypts stored credential blobs and reads the `.env` fallback.
///
/// Construction validates the master key ONCE (missing / malformed / wrong
/// length are `Err` — the operator must fix `BLITZKRIEG_DB_KEY` before any
/// credential path runs). Every `decrypt` after that uses the same cipher;
/// a per-record nonce is generated fresh on each `encrypt`.
#[derive(Debug)]
pub struct CredentialsManager {
    cipher: Aes256Gcm,
    /// `(account_id, field) → blob` snapshot. `Mutex` keeps the manager
    /// `Sync` so the IPC/service layers can share one instance; the swap is
    /// only done by the schema wave's import path, never mid-signing.
    rows: Mutex<BTreeMap<(String, String), Vec<u8>>>,
}

impl CredentialsManager {
    /// Build a manager from the base64 master key (`BLITZKRIEG_DB_KEY`).
    ///
    /// Errors (never a panic, never a plaintext fallback):
    /// - missing / empty → "master key … not set"
    /// - invalid base64 → names the env var, not the value
    /// - wrong length → states the required 32 bytes and the actual length
    pub fn new(master_key_b64: &str) -> CoreResult<Self> {
        let raw = STANDARD
            .decode(master_key_b64.trim())
            .map_err(|_| key_error(format!("{MASTER_KEY_ENV} is not valid base64")))?;
        if raw.len() != 32 {
            return Err(key_error(format!(
                "{MASTER_KEY_ENV} must decode to 32 bytes (AES-256), got {}",
                raw.len()
            )));
        }
        let key = Key::<Aes256Gcm>::try_from(&raw[..])
            .map_err(|_| key_error("internal: master key length check failed"))?;
        Ok(Self {
            cipher: Aes256Gcm::new(&key),
            rows: Mutex::new(BTreeMap::new()),
        })
    }

    /// Encrypt a plaintext credential. Returns `nonce || ciphertext || tag`
    /// (12 + n + 16 bytes), ready for a SQLite BLOB column.
    ///
    /// The nonce is a fresh CSPRNG draw per record — never derived from the
    /// plaintext or the account id (review rule 1: independent random nonce
    /// per credential record).
    pub fn encrypt(&self, plaintext: &[u8]) -> CoreResult<Vec<u8>> {
        let mut nonce_bytes = [0u8; NONCE_LEN];
        rand::rng().fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::try_from(&nonce_bytes[..])
            .map_err(|_| key_error("internal: nonce length check failed"))?;
        let ciphertext = self
            .cipher
            .encrypt(&nonce, plaintext)
            // map_err, NOT expect: a hostile length must not take the core
            // down (panic = "unwind" posture, Cargo.toml §release).
            .map_err(|_| key_error("credential encryption failed"))?;
        let mut blob = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        blob.extend_from_slice(&nonce_bytes);
        blob.extend_from_slice(&ciphertext);
        Ok(blob)
    }

    /// Decrypt a stored blob. Errors, in order of the shape check:
    /// - shorter than nonce+tag → "truncated" (a corrupt row, not a key problem)
    /// - authentication failure → "wrong key or corrupted" (a WRONG master key
    ///   lands here: GCM's tag check is what catches it — the error is clear,
    ///   not a panic, and carries no plaintext fragment)
    pub fn decrypt(&self, data: &[u8]) -> CoreResult<Vec<u8>> {
        if data.len() < MIN_BLOB_LEN {
            return Err(key_error(format!(
                "credential blob is truncated: {} bytes (need at least {MIN_BLOB_LEN} \
                 = nonce {NONCE_LEN} + tag {TAG_LEN})",
                data.len()
            )));
        }
        let (nonce_bytes, ciphertext) = data.split_at(NONCE_LEN);
        let nonce = Nonce::try_from(nonce_bytes)
            .map_err(|_| key_error("internal: nonce length check failed"))?;
        self.cipher
            .decrypt(&nonce, ciphertext)
            .map_err(|_| key_error("decryption failed: wrong master key or corrupted record"))
    }

    /// DB first, then `.env` fallback (handoff §2/§3.1).
    ///
    /// `field` is the credential field name as the DB row spells it
    /// (`api_key` / `secret` / `passphrase` / `private_key`); the env fallback
    /// maps it to the venue's declared variable name (see
    /// [`env_var_for_field`]). `None` = the account has no credential for that
    /// field anywhere — the caller decides what that means (dry-run OK, live
    /// refusal).
    ///
    /// Returns plaintext bytes; the CALLER must not log them. The `tracing`
    /// line this emits on the env-fallback path names the VARIABLE, never the
    /// value — the same rule `crate::env_file` runs.
    pub fn get_credential(&self, account_id: &str, field: &str) -> CoreResult<Option<Vec<u8>>> {
        // DB first: a stored blob always wins over the env fallback.
        let blob = {
            let rows = self.rows.lock().unwrap_or_else(|p| p.into_inner());
            rows.get(&(account_id.to_string(), field.to_string()))
                .cloned()
        };
        if let Some(blob) = blob {
            return self.decrypt(&blob).map(Some);
        }
        // `.env` fallback: no DB row for this (account, field).
        let Some(var_name) = env_var_for_field(field) else {
            // Unknown field: no env spelling exists — a DB-only field.
            return Ok(None);
        };
        match std::env::var(var_name) {
            Ok(v) if !v.is_empty() => {
                tracing::info!(
                    account_id,
                    field,
                    env = var_name,
                    "credential served from .env fallback (no DB row)"
                );
                Ok(Some(v.into_bytes()))
            }
            // A missing/empty env var is the NORMAL dry-run case, not an error.
            _ => Ok(None),
        }
    }

    /// Install the DB rows (the schema wave's import path calls this once at
    /// boot, inside the migration transaction). Replaces any prior snapshot;
    /// rows are already-encrypted BLOBs, never plaintext.
    pub fn set_rows(&self, rows: BTreeMap<(String, String), Vec<u8>>) {
        *self.rows.lock().unwrap_or_else(|p| p.into_inner()) = rows;
    }
}

/// Map a credential field name to the venue env var the `.env` fallback
/// reads. Only the fields the four-slot `CredentialKeys` declares have an env
/// spelling; anything else is DB-only.
///
/// The mapping is by field name, not by account/venue — the DB row's
/// `(account_id, field)` names the slot, and the env vars are the SAME global
/// names the plugins read directly (`POLYMARKET_PRIVATE_KEY` etc.), exactly
/// as the pre-SQLite kernel did. Per-account env scoping (`ACCOUNT_<ID>_…`)
/// is account bookkeeping (venue/strategies/rounds), not the credential
/// values — those stayed in the global vars even before #1189.
fn env_var_for_field(field: &str) -> Option<&'static str> {
    match field {
        "api_key" => Some("POLYMARKET_API_KEY"),
        "secret" => Some("POLYMARKET_SECRET"),
        "passphrase" => Some("POLYMARKET_PASSPHRASE"),
        "private_key" => Some("POLYMARKET_PRIVATE_KEY"),
        // PredictFun names its two slots API_KEY / PRIVATE_KEY.
        "predict_api_key" => Some("PREDICT_API_KEY"),
        "predict_private_key" => Some("PREDICT_PRIVATE_KEY"),
        // Kalshi's two-slot credential.
        "kalshi_api_key_id" => Some("KALSHI_API_KEY_ID"),
        "kalshi_rsa_private_key" => Some("KALSHI_RSA_PRIVATE_KEY"),
        // Polysimulator.
        "polysim_api_key" => Some("POLYSIM_API_KEY"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    // Rust 2024: env mutation is `unsafe` and process-global. The lock keeps
    // these tests from racing each other (same pattern as
    // tests/pair_book_check.rs); the restore guard keeps the process env
    // clean for the sibling tests in this binary.
    static ENV_LOCK: StdMutex<()> = StdMutex::new(());

    struct EnvRestore(&'static str);
    impl Drop for EnvRestore {
        fn drop(&mut self) {
            // SAFETY: single-threaded test binary, lock held by the caller.
            unsafe { std::env::remove_var(self.0) };
        }
    }

    /// A valid 32-byte key, base64-encoded (all zeros — never a real key).
    const KEY_B64: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

    fn manager() -> CredentialsManager {
        CredentialsManager::new(KEY_B64).unwrap()
    }

    #[test]
    fn missing_master_key_is_a_clear_error() {
        // 31 raw bytes, base64-encoded (right alphabet, wrong length).
        const KEY_31: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHg==";
        for (input, label) in [
            ("", "empty"),
            ("   ", "whitespace"),
            ("not base64!!", "not base64"),
            // 31 bytes: right alphabet, wrong length.
            (KEY_31, "31 bytes"),
        ] {
            let err = CredentialsManager::new(input).expect_err(label);
            assert!(
                err.message.contains(MASTER_KEY_ENV),
                "{label}: message must name the env var, got {:?}",
                err.message
            );
        }
        // Wrong length is called out with BOTH numbers.
        let err = CredentialsManager::new(KEY_31).unwrap_err();
        assert!(err.message.contains("32 bytes"), "{}", err.message);
        assert!(err.message.contains("got 31"), "{}", err.message);
    }

    #[test]
    fn roundtrip_yields_the_original_plaintext() {
        let m = manager();
        let plaintext = b"0xdeadbeefcafe private key material";
        let blob = m.encrypt(plaintext).unwrap();
        assert_eq!(blob.len(), NONCE_LEN + plaintext.len() + TAG_LEN);
        // Shape check: the first 12 bytes differ from a second encryption —
        // a random nonce per record (review rule 1).
        let blob2 = m.encrypt(plaintext).unwrap();
        assert_ne!(&blob[..NONCE_LEN], &blob2[..NONCE_LEN]);
        assert_eq!(m.decrypt(&blob).unwrap(), plaintext);
        assert_eq!(m.decrypt(&blob2).unwrap(), plaintext);
        // Ciphertext does not carry the plaintext (a grep for the material
        // must fail — the blob is not the plaintext in disguise).
        assert!(!blob.windows(plaintext.len()).any(|w| w == &plaintext[..]));
    }

    #[test]
    fn wrong_master_key_fails_cleanly_never_panics() {
        let blob = manager().encrypt(b"secret material").unwrap();
        // A different 32-byte key: GCM tag check must reject, as an Err.
        let other =
            CredentialsManager::new("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=").unwrap();
        let err = other.decrypt(&blob).unwrap_err();
        assert!(
            err.message.contains("wrong master key") || err.message.contains("corrupted"),
            "{}",
            err.message
        );
        // And the correct manager still decrypts it (the failure did not
        // corrupt state).
        assert_eq!(manager().decrypt(&blob).unwrap(), b"secret material");
    }

    #[test]
    fn truncated_blob_is_a_clear_error() {
        let m = manager();
        let blob = m.encrypt(b"abc").unwrap();
        for cut in [0usize, 1, NONCE_LEN - 1, NONCE_LEN, NONCE_LEN + 1] {
            let err = m.decrypt(&blob[..cut]).unwrap_err();
            assert!(
                err.message.contains("truncated"),
                "cut {cut}: {}",
                err.message
            );
        }
    }

    #[test]
    fn db_row_wins_and_decrypts() {
        let m = manager();
        let blob = m.encrypt(b"db-stored value").unwrap();
        let mut rows = BTreeMap::new();
        rows.insert(
            ("acct-a".to_string(), "private_key".to_string()),
            blob.clone(),
        );
        m.set_rows(rows);
        // DB row present → served (decrypted).
        assert_eq!(
            m.get_credential("acct-a", "private_key").unwrap().unwrap(),
            b"db-stored value"
        );
        // DB row absent for another account → falls through. Use a field with
        // NO env spelling so the result is deterministic: the env fallback is
        // process-global and a sibling test may have set POLYMARKET_PRIVATE_KEY.
        assert_eq!(m.get_credential("acct-b", "made_up_field").unwrap(), None);
    }

    #[test]
    fn env_fallback_serves_when_db_has_no_row() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _restore = EnvRestore("POLYMARKET_PRIVATE_KEY");
        // SAFETY: env lock held; guard restores on drop.
        unsafe { std::env::set_var("POLYMARKET_PRIVATE_KEY", "env-fallback pk") };

        let m = manager();
        let got = m.get_credential("acct-x", "private_key").unwrap().unwrap();
        assert_eq!(got, b"env-fallback pk");

        // A DB row for the same field now WINS over the env var.
        let blob = m.encrypt(b"db value wins").unwrap();
        let mut rows = BTreeMap::new();
        rows.insert(("acct-x".to_string(), "private_key".to_string()), blob);
        m.set_rows(rows);
        assert_eq!(
            m.get_credential("acct-x", "private_key").unwrap().unwrap(),
            b"db value wins"
        );
    }

    #[test]
    fn empty_env_var_is_not_a_credential() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _restore = EnvRestore("KALSHI_API_KEY_ID");
        // SAFETY: env lock held; guard restores on drop.
        unsafe { std::env::set_var("KALSHI_API_KEY_ID", "") };

        let m = manager();
        assert_eq!(m.get_credential("a", "kalshi_api_key_id").unwrap(), None);
    }

    #[test]
    fn unknown_field_has_no_env_spelling() {
        let m = manager();
        assert_eq!(m.get_credential("a", "made_up_field").unwrap(), None);
    }

    #[test]
    fn plaintext_never_appears_in_error_messages() {
        // Error text must not leak the plaintext (or key material) it fails
        // on: decrypt garbage, encrypt, and check both messages.
        let m = manager();
        let blob = m.encrypt(b"super secret value").unwrap();
        let err = m.decrypt(&blob[..10]).unwrap_err();
        assert!(!err.message.contains("secret"), "{}", err.message);
        let err =
            CredentialsManager::new("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHg==").unwrap_err();
        assert!(!err.message.contains("AAECAw"), "{}", err.message);
    }
}

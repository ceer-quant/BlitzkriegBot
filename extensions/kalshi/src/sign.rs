//! Kalshi request authentication (#423).
//!
//! Kalshi API-key auth: the request timestamp is covered by an RSA private
//! key; the key id travels alongside it in headers. Credentials come from the
//! process environment only; tests generate a throwaway key locally — no real
//! credential ever appears in this repository. The cryptography is plain
//! in-process `rsa`-crate work on the caller's thread.

use base64::Engine as _;
use rand::thread_rng;
use rsa::Pkcs1v15Sign;
use rsa::RsaPrivateKey;
use rsa::pkcs1::DecodeRsaPrivateKey;
use sha2::{Digest, Sha256};

/// A loaded key pair, reusable across requests. Cheap to clone.
#[derive(Clone)]
pub struct RequestSigner {
    key_id: String,
    key: RsaPrivateKey,
}

impl std::fmt::Debug for RequestSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The key material must never reach a log line.
        f.debug_struct("RequestSigner")
            .field("key_id", &self.key_id)
            .finish_non_exhaustive()
    }
}

impl RequestSigner {
    /// Build from a key id and a PKCS#1 PEM key body — the format Kalshi's
    /// dashboard hands out for API-key auth.
    pub fn from_pem(key_id: String, key_text: &str) -> Result<Self, crate::KalshiError> {
        Ok(Self {
            key_id,
            key: load_key(key_text)?,
        })
    }

    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    /// Cover one timestamp. The header pair
    /// `(KALSHI-ACCESS-KEY, KALSHI-ACCESS-SIGNATURE)` plus the timestamp
    /// header is the whole auth.
    pub fn timestamp_token(&self, timestamp_ms: u64) -> Result<String, crate::KalshiError> {
        let timestamp_text = timestamp_ms.to_string();
        let digest = Sha256::digest(timestamp_text.as_bytes());
        let scheme = Pkcs1v15Sign::new::<Sha256>();
        let mut rng = thread_rng();
        let covered = self
            .key
            .sign_with_rng(&mut rng, scheme, &digest)
            .map_err(|_| crate::KalshiError::AuthConfig(String::from("RSA 签名失败")))?;
        Ok(base64::engine::general_purpose::STANDARD.encode(covered))
    }
}

fn load_key(key_text: &str) -> Result<RsaPrivateKey, crate::KalshiError> {
    RsaPrivateKey::from_pkcs1_pem(key_text)
        .map_err(|_| crate::KalshiError::AuthConfig(String::from("RSA 私钥解析失败")))
}

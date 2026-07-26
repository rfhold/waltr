use std::{collections::BTreeMap, fmt};

use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit, Payload},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use p256::{
    SecretKey,
    elliptic_curve::sec1::ToEncodedPoint as _,
    pkcs8::{DecodePrivateKey as _, EncodePrivateKey as _},
};
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

use super::{McpOAuthEntropy, OAuthAuthorizationStoreError, OAuthAuthorizationStoreResult};

/// Encrypted PKCS#8 private material. Debug output intentionally omits all secret fields.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthEncryptedPrivateKey {
    /// Versioned wrapping-key identifier.
    pub wrapping_key_id: String,
    /// Unique authenticated-encryption nonce.
    pub nonce: Vec<u8>,
    /// Authenticated ciphertext.
    pub ciphertext: Vec<u8>,
}

impl fmt::Debug for OAuthEncryptedPrivateKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OAuthEncryptedPrivateKey")
            .field("wrapping_key_id", &self.wrapping_key_id)
            .finish_non_exhaustive()
    }
}

/// Deployment-provided private-key wrapping interface.
pub trait OAuthKeyEncryption: Send + Sync {
    /// Encrypt PKCS#8 bytes using `algorithm` and `key_id` as associated data.
    ///
    /// # Errors
    ///
    /// Returns an error when entropy or authenticated encryption is unavailable.
    fn encrypt(
        &self,
        algorithm: &str,
        key_id: &str,
        plaintext: &[u8],
        entropy: &dyn McpOAuthEntropy,
    ) -> OAuthAuthorizationStoreResult<OAuthEncryptedPrivateKey>;

    /// Decrypt PKCS#8 bytes, failing for unknown wrapping keys or corrupt ciphertext.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown wrapping key or failed authentication.
    fn decrypt(
        &self,
        algorithm: &str,
        key_id: &str,
        encrypted: &OAuthEncryptedPrivateKey,
    ) -> OAuthAuthorizationStoreResult<Zeroizing<Vec<u8>>>;
}

/// Versioned AES-256-GCM wrapping-key ring with one current encryption key.
pub struct VersionedOAuthWrappingKeyring {
    current_key_id: String,
    keys: BTreeMap<String, Zeroizing<Vec<u8>>>,
}

impl fmt::Debug for VersionedOAuthWrappingKeyring {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VersionedOAuthWrappingKeyring")
            .field("current_key_id", &self.current_key_id)
            .field("key_ids", &self.keys.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl VersionedOAuthWrappingKeyring {
    /// Build a key ring. Every key must contain exactly 32 bytes.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty identifier, invalid key length, or unknown current key.
    pub fn new(
        current_key_id: impl Into<String>,
        keys: impl IntoIterator<Item = (String, Vec<u8>)>,
    ) -> OAuthAuthorizationStoreResult<Self> {
        let current_key_id = current_key_id.into();
        let mut validated = BTreeMap::new();
        for (id, bytes) in keys {
            if id.is_empty() || bytes.len() != 32 || validated.contains_key(&id) {
                return Err(OAuthAuthorizationStoreError::InvalidData);
            }
            validated.insert(id, Zeroizing::new(bytes));
        }
        let keys = validated;
        if !keys.contains_key(&current_key_id) {
            return Err(OAuthAuthorizationStoreError::InvalidData);
        }
        Ok(Self {
            current_key_id,
            keys,
        })
    }

    /// Return the current key identifier used for new ciphertext.
    #[must_use]
    pub fn current_key_id(&self) -> &str {
        &self.current_key_id
    }
}

/// Decrypt and parse a persisted ES256 signer, then require its exact public JWK.
///
/// # Errors
///
/// Returns an error when private material is absent, undecryptable, malformed, or does not
/// correspond exactly to the stored public JWK.
pub fn validate_es256_signing_key_readiness(
    key: &super::OAuthSigningKey,
    encryption: &dyn OAuthKeyEncryption,
) -> OAuthAuthorizationStoreResult<()> {
    if key.algorithm != "ES256" {
        return Err(OAuthAuthorizationStoreError::InvalidData);
    }
    let encrypted = key
        .encrypted_private_key
        .as_ref()
        .ok_or(OAuthAuthorizationStoreError::KeyUnavailable)?;
    let private = encryption.decrypt("ES256", &key.key_id, encrypted)?;
    let secret = SecretKey::from_pkcs8_der(&private)
        .map_err(|_| OAuthAuthorizationStoreError::KeyUnavailable)?;
    if public_jwk(&secret, &key.key_id)? != key.public_jwk {
        return Err(OAuthAuthorizationStoreError::InvalidData);
    }
    Ok(())
}

impl OAuthKeyEncryption for VersionedOAuthWrappingKeyring {
    fn encrypt(
        &self,
        algorithm: &str,
        key_id: &str,
        plaintext: &[u8],
        entropy: &dyn McpOAuthEntropy,
    ) -> OAuthAuthorizationStoreResult<OAuthEncryptedPrivateKey> {
        let key = self
            .keys
            .get(&self.current_key_id)
            .ok_or(OAuthAuthorizationStoreError::KeyUnavailable)?;
        let cipher = Aes256Gcm::new_from_slice(key)
            .map_err(|_| OAuthAuthorizationStoreError::InvalidData)?;
        let mut nonce = vec![0_u8; 12];
        entropy
            .fill_bytes(&mut nonce)
            .map_err(|_| OAuthAuthorizationStoreError::KeyUnavailable)?;
        let aad = associated_data(&self.current_key_id, algorithm, key_id);
        let ciphertext = cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| OAuthAuthorizationStoreError::KeyUnavailable)?;
        Ok(OAuthEncryptedPrivateKey {
            wrapping_key_id: self.current_key_id.clone(),
            nonce,
            ciphertext,
        })
    }

    fn decrypt(
        &self,
        algorithm: &str,
        key_id: &str,
        encrypted: &OAuthEncryptedPrivateKey,
    ) -> OAuthAuthorizationStoreResult<Zeroizing<Vec<u8>>> {
        if encrypted.nonce.len() != 12 {
            return Err(OAuthAuthorizationStoreError::InvalidData);
        }
        let key = self
            .keys
            .get(&encrypted.wrapping_key_id)
            .ok_or(OAuthAuthorizationStoreError::KeyUnavailable)?;
        let cipher = Aes256Gcm::new_from_slice(key)
            .map_err(|_| OAuthAuthorizationStoreError::InvalidData)?;
        let aad = associated_data(&encrypted.wrapping_key_id, algorithm, key_id);
        cipher
            .decrypt(
                Nonce::from_slice(&encrypted.nonce),
                Payload {
                    msg: &encrypted.ciphertext,
                    aad: aad.as_bytes(),
                },
            )
            .map(Zeroizing::new)
            .map_err(|_| OAuthAuthorizationStoreError::KeyUnavailable)
    }
}

/// Generate an ES256 key candidate outside the authorization-store transaction.
///
/// # Errors
///
/// Returns an error when input, entropy, key encoding, or private-key encryption fails.
pub fn generate_es256_signing_key_candidate(
    issuer: impl Into<String>,
    key_id: impl Into<String>,
    now_ms: i64,
    encryption: &dyn OAuthKeyEncryption,
    entropy: &dyn McpOAuthEntropy,
) -> OAuthAuthorizationStoreResult<super::OAuthSigningKeyCandidate> {
    let issuer = issuer.into();
    let key_id = key_id.into();
    if issuer.is_empty() || key_id.is_empty() || now_ms < 0 {
        return Err(OAuthAuthorizationStoreError::InvalidData);
    }
    let mut secret = None;
    for _ in 0..128 {
        let mut bytes = Zeroizing::new(vec![0_u8; 32]);
        entropy
            .fill_bytes(&mut bytes)
            .map_err(|_| OAuthAuthorizationStoreError::KeyUnavailable)?;
        if let Ok(value) = SecretKey::from_slice(&bytes) {
            secret = Some(value);
            break;
        }
        bytes.zeroize();
    }
    let secret = secret.ok_or(OAuthAuthorizationStoreError::KeyUnavailable)?;
    let document = secret
        .to_pkcs8_der()
        .map_err(|_| OAuthAuthorizationStoreError::KeyUnavailable)?;
    let encrypted_private_key =
        encryption.encrypt("ES256", &key_id, document.as_bytes(), entropy)?;
    let public_jwk = public_jwk(&secret, &key_id)?;

    Ok(super::OAuthSigningKeyCandidate {
        issuer,
        key_id,
        algorithm: "ES256".to_owned(),
        public_jwk,
        encrypted_private_key,
        created_at_ms: now_ms,
    })
}

fn public_jwk(
    secret: &SecretKey,
    key_id: &str,
) -> OAuthAuthorizationStoreResult<serde_json::Value> {
    let point = secret.public_key().to_encoded_point(false);
    let x = point.x().ok_or(OAuthAuthorizationStoreError::InvalidData)?;
    let y = point.y().ok_or(OAuthAuthorizationStoreError::InvalidData)?;
    Ok(serde_json::json!({
        "kty": "EC",
        "crv": "P-256",
        "use": "sig",
        "alg": "ES256",
        "kid": key_id,
        "x": URL_SAFE_NO_PAD.encode(x),
        "y": URL_SAFE_NO_PAD.encode(y),
    }))
}

fn associated_data(wrapping_key_id: &str, algorithm: &str, key_id: &str) -> String {
    format!("mcp-oauth-signing-key:v1\0{wrapping_key_id}\0{algorithm}\0{key_id}")
}

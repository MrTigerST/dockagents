//! Ed25519 publisher signing.
//!
//! Layout:
//!   * `~/.dockagents/keys/private.key` — base64-encoded 32-byte Ed25519 seed.
//!   * `~/.dockagents/keys/public.key`  — base64-encoded 32-byte verifying key.
//!
//! `dockagents keygen` creates both. `dockagents publish` signs the gzipped
//! tarball bytes (sha256 of the bytes for stability across encodings) with
//! the private seed, ships `(signature_b64, pubkey_b64)` alongside the
//! tarball, and the registry persists both.
//!
//! `dockagents install` re-derives the sha256, fetches the signature + pubkey
//! the registry returns, and rejects the install if verification fails — a
//! direct realization of the trust model in dockagents.md §10.
//!
//! NB: this gives us *publisher signing*, not a CA-style PKI. A future
//! milestone can layer a registry-issued attestation on top so users can pin
//! "trust this publisher" decisions.

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey, SECRET_KEY_LENGTH};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::paths;

const B64: base64::engine::general_purpose::GeneralPurpose = base64::engine::general_purpose::STANDARD;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublisherKey {
    pub created_at: String,
    pub public_key_b64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedArtifact {
    pub digest_sha256_hex: String,
    pub signature_b64: String,
    pub public_key_b64: String,
}

pub fn keys_dir() -> Result<PathBuf> {
    Ok(paths::home()?.join("keys"))
}

pub fn private_key_path() -> Result<PathBuf> {
    Ok(keys_dir()?.join("private.key"))
}

pub fn public_key_path() -> Result<PathBuf> {
    Ok(keys_dir()?.join("public.key"))
}

/// Generate a new keypair in `~/.dockagents/keys/`. Refuses to overwrite an
/// existing key — operators must move the old key out first to avoid
/// accidentally rotating publisher identity.
pub fn generate_keypair(force: bool) -> Result<PublisherKey> {
    let priv_path = private_key_path()?;
    let pub_path = public_key_path()?;
    if priv_path.exists() && !force {
        return Err(anyhow!(
            "private key already exists at {}; pass --force to overwrite",
            priv_path.display()
        ));
    }
    std::fs::create_dir_all(keys_dir()?)?;

    let mut rng = rand::rngs::OsRng;
    let signing = SigningKey::generate(&mut rng);
    let verifying = signing.verifying_key();

    let priv_b64 = B64.encode(signing.to_bytes());
    let pub_b64 = B64.encode(verifying.to_bytes());
    std::fs::write(&priv_path, &priv_b64)?;
    std::fs::write(&pub_path, &pub_b64)?;
    restrict_permissions(&priv_path)?;

    Ok(PublisherKey {
        created_at: chrono::Utc::now().to_rfc3339(),
        public_key_b64: pub_b64,
    })
}

#[cfg(unix)]
fn restrict_permissions(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o600);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &std::path::Path) -> Result<()> {
    Ok(())
}

fn load_signing_key() -> Result<SigningKey> {
    let path = private_key_path()?;
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading private key at {}", path.display()))?;
    let bytes = B64
        .decode(raw.trim())
        .with_context(|| format!("decoding base64 private key at {}", path.display()))?;
    if bytes.len() != SECRET_KEY_LENGTH {
        return Err(anyhow!(
            "private key has length {} (expected {SECRET_KEY_LENGTH})",
            bytes.len()
        ));
    }
    let mut seed = [0u8; SECRET_KEY_LENGTH];
    seed.copy_from_slice(&bytes);
    Ok(SigningKey::from_bytes(&seed))
}

/// Sha256 of the bytes; returned as lowercase hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// Sign a tarball. Produces the artifact metadata the registry stores.
pub fn sign_bytes(bytes: &[u8]) -> Result<SignedArtifact> {
    let key = load_signing_key()?;
    let digest = sha256_hex(bytes);
    let sig: Signature = key.sign(digest.as_bytes());
    Ok(SignedArtifact {
        digest_sha256_hex: digest,
        signature_b64: B64.encode(sig.to_bytes()),
        public_key_b64: B64.encode(key.verifying_key().to_bytes()),
    })
}

/// Verify a tarball against an attached signature.
pub fn verify(bytes: &[u8], artifact: &SignedArtifact) -> Result<()> {
    let actual = sha256_hex(bytes);
    if actual != artifact.digest_sha256_hex {
        return Err(anyhow!(
            "sha256 mismatch (claimed {}, computed {})",
            artifact.digest_sha256_hex,
            actual
        ));
    }
    let pub_bytes = B64
        .decode(&artifact.public_key_b64)
        .context("decoding public_key_b64")?;
    if pub_bytes.len() != 32 {
        return Err(anyhow!("public key must be 32 bytes, got {}", pub_bytes.len()));
    }
    let mut pkb = [0u8; 32];
    pkb.copy_from_slice(&pub_bytes);
    let verifying = VerifyingKey::from_bytes(&pkb).context("loading verifying key")?;

    let sig_bytes = B64
        .decode(&artifact.signature_b64)
        .context("decoding signature_b64")?;
    if sig_bytes.len() != 64 {
        return Err(anyhow!("signature must be 64 bytes, got {}", sig_bytes.len()));
    }
    let mut sb = [0u8; 64];
    sb.copy_from_slice(&sig_bytes);
    let signature = Signature::from_bytes(&sb);

    verifying
        .verify(actual.as_bytes(), &signature)
        .map_err(|e| anyhow!("signature did not verify: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        // Doesn't write to disk — uses in-memory keys to avoid clobbering.
        let mut rng = rand::rngs::OsRng;
        let key = SigningKey::generate(&mut rng);
        let bytes = b"hello dockagents".to_vec();
        let digest = sha256_hex(&bytes);
        let sig = key.sign(digest.as_bytes());
        let artifact = SignedArtifact {
            digest_sha256_hex: digest,
            signature_b64: B64.encode(sig.to_bytes()),
            public_key_b64: B64.encode(key.verifying_key().to_bytes()),
        };
        verify(&bytes, &artifact).unwrap();

        let mut tampered = bytes.clone();
        tampered[0] ^= 0x01;
        verify(&tampered, &artifact).unwrap_err();
    }
}

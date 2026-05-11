//! HTTP client for the centralized DockAgents registry.
//!
//! Mirrors the API exposed by `dockagents-registry`:
//!   * `GET  /packages/:name`               → metadata + version list
//!   * `GET  /packages/:name/:version`      → manifest for a specific version
//!   * `GET  /packages/:name/:version/pull` → gzipped tarball
//!   * `GET  /search?q=...`                 → ranked package summaries
//!   * `POST /publish`                      → upload a `.tar.gz`
//!
//! Tarball pack / unpack lives here too so the only place that knows the
//! on-the-wire format is this module.

use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::manifest::Manifest;

const PACK_SKIP_DIRS: &[&str] = &["node_modules", ".git", "target", "data", ".dockagents"];

#[derive(Debug, Clone)]
pub struct RemoteRegistry {
    base: String,
    token: Option<String>,
    agent: ureq::Agent,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct PackageSummary {
    pub name: String,
    pub description: String,
    pub latest: String,
    pub versions: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct VersionRecord {
    pub version: String,
    #[serde(default)]
    pub description: String,
    pub sha256: String,
    #[serde(rename = "byteLength")]
    pub byte_length: u64,
    #[serde(rename = "publishedAt")]
    pub published_at: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct PackageDetail {
    pub name: String,
    pub description: String,
    pub latest: String,
    pub versions: Vec<String>,
    #[serde(rename = "versionRecords", default)]
    pub version_records: Vec<VersionRecord>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ResolveDetail {
    pub name: String,
    pub range: String,
    pub resolved: String,
    pub sha256: String,
    #[serde(rename = "byteLength")]
    pub byte_length: u64,
    #[serde(rename = "publishedAt")]
    pub published_at: String,
    #[serde(rename = "manifestYaml")]
    pub manifest_yaml: String,
    pub manifest: serde_json::Value,
    #[serde(default, rename = "signatureB64")]
    pub signature_b64: Option<String>,
    #[serde(default, rename = "publicKeyB64")]
    pub public_key_b64: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct VersionDetail {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    pub sha256: String,
    #[serde(rename = "byteLength")]
    pub byte_length: u64,
    #[serde(rename = "publishedAt")]
    pub published_at: String,
    #[serde(rename = "manifestYaml")]
    pub manifest_yaml: String,
    pub manifest: serde_json::Value,
    /// Ed25519 signature of the tarball's sha256 hex digest, base64-encoded.
    /// `None` means the package was published without a signature.
    #[serde(default, rename = "signatureB64")]
    pub signature_b64: Option<String>,
    /// Publisher's Ed25519 verifying key, base64-encoded.
    #[serde(default, rename = "publicKeyB64")]
    pub public_key_b64: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct SearchResponse {
    pub query: String,
    pub count: usize,
    pub results: Vec<PackageSummary>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct PublishAck {
    pub ok: bool,
    pub name: String,
    pub version: String,
    pub sha256: String,
    #[serde(rename = "byteLength")]
    pub byte_length: u64,
    #[serde(rename = "publishedAt")]
    pub published_at: String,
    #[serde(default)]
    pub signed: bool,
}

/// How `publish` should handle signing.
#[derive(Debug, Clone, Copy)]
pub enum SignMode {
    /// Skip signing entirely.
    None,
    /// Sign with the publisher key in `~/.dockagents/keys/`. Error if missing.
    Required,
    /// Sign if a publisher key is available; otherwise publish unsigned.
    IfAvailable,
}

impl RemoteRegistry {
    pub fn has_token(&self) -> bool {
        self.token.as_deref().map(str::trim).is_some_and(|s| !s.is_empty())
    }

    pub fn new(base: impl Into<String>, token: Option<String>) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(15))
            .timeout(Duration::from_secs(120))
            .build();
        Self {
            base: base.into().trim_end_matches('/').to_string(),
            token,
            agent,
        }
    }

    /// Resolve URL from CLI flag or `DOCKAGENTS_REGISTRY_URL` env. Returns
    /// `None` when neither is set — caller should fall back to the local file
    /// registry in that case.
    pub fn from_flag_or_env(flag: Option<&str>) -> Option<Self> {
        let url = flag
            .map(str::to_string)
            .or_else(|| std::env::var("DOCKAGENTS_REGISTRY_URL").ok())?;
        if url.trim().is_empty() {
            return None;
        }
        let token = std::env::var("DOCKAGENTS_REGISTRY_TOKEN").ok();
        Some(Self::new(url, token))
    }

    pub fn search(&self, q: &str) -> Result<SearchResponse> {
        let url = format!("{}/search", self.base);
        let resp = self
            .agent
            .get(&url)
            .query("q", q)
            .call()
            .map_err(|e| anyhow!("GET /search failed: {e}"))?;
        Ok(resp.into_json()?)
    }

    pub fn get_package(&self, name: &str) -> Result<PackageDetail> {
        let url = format!("{}/packages/{}", self.base, urlenc(name));
        let resp = self
            .agent
            .get(&url)
            .call()
            .map_err(|e| anyhow!("GET {url}: {e}"))?;
        Ok(resp.into_json()?)
    }

    /// Ask the registry to resolve a semver range to a concrete version.
    pub fn resolve(&self, name: &str, range: &str) -> Result<ResolveDetail> {
        let url = format!(
            "{}/packages/{}/resolve",
            self.base,
            urlenc(name)
        );
        let resp = self
            .agent
            .get(&url)
            .query("range", range)
            .call()
            .map_err(|e| anyhow!("GET {url}?range={range}: {e}"))?;
        Ok(resp.into_json()?)
    }

    pub fn get_version(&self, name: &str, version: &str) -> Result<VersionDetail> {
        let url = format!(
            "{}/packages/{}/{}",
            self.base,
            urlenc(name),
            urlenc(version)
        );
        let resp = self
            .agent
            .get(&url)
            .call()
            .map_err(|e| anyhow!("GET {url}: {e}"))?;
        Ok(resp.into_json()?)
    }

    /// Download the gzipped tarball bytes for a published version. Verifies
    /// the sha256 reported by the registry matches what we received, and
    /// (when present) verifies the Ed25519 signature.
    pub fn pull(&self, name: &str, version: &str) -> Result<Vec<u8>> {
        let detail = self.get_version(name, version)?;
        let url = format!(
            "{}/packages/{}/{}/pull",
            self.base,
            urlenc(name),
            urlenc(version)
        );
        let resp = self
            .agent
            .get(&url)
            .call()
            .map_err(|e| anyhow!("GET {url}: {e}"))?;
        let mut buf = Vec::new();
        resp.into_reader().read_to_end(&mut buf)?;
        let actual = sha256_hex(&buf);
        if actual != detail.sha256 {
            return Err(anyhow!(
                "sha256 mismatch for {name}@{version}: registry={} actual={}",
                detail.sha256,
                actual
            ));
        }
        if let (Some(sig), Some(pk)) = (&detail.signature_b64, &detail.public_key_b64) {
            let artifact = crate::signing::SignedArtifact {
                digest_sha256_hex: actual.clone(),
                signature_b64: sig.clone(),
                public_key_b64: pk.clone(),
            };
            crate::signing::verify(&buf, &artifact)
                .with_context(|| format!("signature verification failed for {name}@{version}"))?;
            tracing::info!("signature OK for {name}@{version}");
        } else {
            tracing::warn!(
                "{name}@{version} is unsigned — proceeding (set DOCKAGENTS_REQUIRE_SIGNED=1 to refuse)"
            );
            if std::env::var("DOCKAGENTS_REQUIRE_SIGNED").is_ok() {
                return Err(anyhow!(
                    "{name}@{version} has no signature and DOCKAGENTS_REQUIRE_SIGNED is set"
                ));
            }
        }
        Ok(buf)
    }

    /// Upload a sandbox source directory.
    pub fn publish(&self, source: &Path, sign: SignMode) -> Result<PublishAck> {
        let manifest_path = source.join("manifest.yaml");
        Manifest::load(&manifest_path)
            .with_context(|| format!("validating manifest at {}", manifest_path.display()))?;

        let tarball = pack_dir(source).context("packing tarball")?;

        let signature = match sign {
            SignMode::None => None,
            SignMode::Required => Some(crate::signing::sign_bytes(&tarball)?),
            SignMode::IfAvailable => match crate::signing::sign_bytes(&tarball) {
                Ok(s) => Some(s),
                Err(e) => {
                    tracing::warn!("publishing unsigned ({e})");
                    None
                }
            },
        };

        let url = format!("{}/publish", self.base);
        let mut req = self
            .agent
            .post(&url)
            .set("content-type", "application/gzip");
        if let Some(tok) = &self.token {
            req = req.set("authorization", &format!("Bearer {tok}"));
        }
        if let Some(s) = &signature {
            req = req
                .set("x-dockagents-signature", &s.signature_b64)
                .set("x-dockagents-public-key", &s.public_key_b64)
                .set("x-dockagents-digest", &s.digest_sha256_hex);
        }

        let resp = req
            .send_bytes(&tarball)
            .map_err(|e| anyhow!("POST /publish failed: {e}"))?;
        Ok(resp.into_json()?)
    }
}

fn urlenc(s: &str) -> String {
    // Tiny URL encoder — only escapes the characters we'd plausibly hit in a
    // package name or semver string.
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' | '+' => out.push(c),
            _ => out.push_str(&format!("%{:02X}", c as u32)),
        }
    }
    out
}

pub fn sha256_hex(buf: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(buf);
    hex::encode(h.finalize())
}

/// Walk `source`, build a gzipped tarball in memory. Mirrors the JS
/// `scripts/publish.ts` in `dockagents-registry`.
pub fn pack_dir(source: &Path) -> Result<Vec<u8>> {
    let mut gz = GzEncoder::new(Vec::new(), Compression::default());
    {
        let mut tar = tar::Builder::new(&mut gz);
        tar.follow_symlinks(false);
        for entry in walkdir::WalkDir::new(source).follow_links(false) {
            let entry = entry?;
            let path = entry.path();
            if path == source {
                continue;
            }
            if entry
                .path()
                .components()
                .any(|c| match c {
                    std::path::Component::Normal(name) => {
                        let s = name.to_string_lossy();
                        PACK_SKIP_DIRS.iter().any(|skip| skip == &s)
                    }
                    _ => false,
                })
            {
                continue;
            }
            let rel = path.strip_prefix(source).unwrap();
            // tar.rs needs forward-slash names; on Windows strip_prefix yields
            // backslashes.
            let archive_name = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            if entry.file_type().is_dir() {
                let mut header = tar::Header::new_gnu();
                header.set_path(&format!("{archive_name}/"))?;
                header.set_size(0);
                header.set_entry_type(tar::EntryType::Directory);
                header.set_mode(0o755);
                header.set_cksum();
                tar.append(&header, std::io::empty())?;
            } else if entry.file_type().is_file() {
                let mut f = std::fs::File::open(path)
                    .with_context(|| format!("opening {}", path.display()))?;
                tar.append_file(&archive_name, &mut f)?;
            }
        }
        tar.finish()?;
    }
    Ok(gz.finish()?)
}

/// Inverse of [`pack_dir`] — extract a tarball into `dest`. Used by the
/// remote `install` flow.
pub fn unpack_into(tarball: &[u8], dest: &Path) -> Result<()> {
    if dest.exists() {
        std::fs::remove_dir_all(dest).ok();
    }
    std::fs::create_dir_all(dest)?;
    let gz = GzDecoder::new(tarball);
    let mut archive = tar::Archive::new(gz);
    archive.set_overwrite(true);
    archive.set_preserve_permissions(false);
    archive.unpack(dest)?;
    Ok(())
}

/// Convenience helper: write a tarball to disk for inspection / manual
/// publish.
pub fn save_tarball(bytes: &[u8], dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::File::create(dest)?;
    f.write_all(bytes)?;
    Ok(())
}

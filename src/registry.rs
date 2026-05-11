//! Local file-backed registry stub.
//!
//! Phase 2 of the roadmap calls for a centralized HTTPS registry with semver,
//! semantic search, and signed packages. Until that exists, we resolve
//! sandboxes against:
//!
//!   1. an explicit path (if the install/run argument is a directory),
//!   2. the local registry at `~/.dockagents/registry/<name>/`,
//!   3. an already-installed sandbox at `~/.dockagents/sandboxes/<name>/`.
//!
//! `publish` here just copies the source tree into the local registry. The
//! HTTPS protocol is described in `dockagents.md` §6 and will replace this.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use fs_extra::dir::{CopyOptions, copy as copy_dir};

use crate::manifest::Manifest;
use crate::paths;

pub struct Registry;

impl Registry {
    /// Find a sandbox source by name in the local registry.
    pub fn locate(name: &str) -> Result<PathBuf> {
        let candidate = paths::registry_dir()?.join(name);
        if candidate.join("manifest.yaml").exists() {
            return Ok(candidate);
        }
        Err(anyhow!(
            "sandbox '{name}' not found in local registry ({}). \
             Use `dockagents publish <path>` to add it, or pass a directory path directly.",
            paths::registry_dir()?.display()
        ))
    }

    /// Resolve a user-provided spec — either a directory containing a
    /// manifest, or a registered sandbox name.
    pub fn resolve_source(spec: &str) -> Result<PathBuf> {
        let as_path = Path::new(spec);
        if as_path.is_dir() && as_path.join("manifest.yaml").exists() {
            return Ok(as_path.to_path_buf());
        }
        Self::locate(spec)
    }

    /// Publish (copy) a sandbox source tree into the local registry,
    /// validating its manifest first.
    pub fn publish(source: &Path) -> Result<Manifest> {
        let manifest_path = source.join("manifest.yaml");
        let manifest = Manifest::load(&manifest_path)
            .with_context(|| format!("loading manifest at {}", manifest_path.display()))?;
        let dest = paths::registry_dir()?.join(&manifest.name);
        if dest.exists() {
            std::fs::remove_dir_all(&dest)
                .with_context(|| format!("removing previous registry entry at {}", dest.display()))?;
        }
        std::fs::create_dir_all(dest.parent().unwrap())?;
        std::fs::create_dir_all(&dest)?;
        let mut opts = CopyOptions::new();
        opts.copy_inside = true;
        opts.overwrite = true;
        copy_dir(source, &dest, &opts)
            .with_context(|| format!("copying {} → {}", source.display(), dest.display()))?;
        Ok(manifest)
    }

    /// Iterate over published packages.
    pub fn list_published() -> Result<Vec<Manifest>> {
        let dir = paths::registry_dir()?;
        if !dir.exists() {
            return Ok(vec![]);
        }
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let m = entry.path().join("manifest.yaml");
            if m.exists() {
                if let Ok(manifest) = Manifest::load(&m) {
                    out.push(manifest);
                }
            }
        }
        Ok(out)
    }

    /// Substring search over `name` and `description`. Real implementation
    /// (Phase 2) does embedding-based semantic matching.
    pub fn search(query: &str) -> Result<Vec<Manifest>> {
        let q = query.to_lowercase();
        Ok(Self::list_published()?
            .into_iter()
            .filter(|m| {
                m.name.to_lowercase().contains(&q)
                    || m.description.to_lowercase().contains(&q)
            })
            .collect())
    }
}

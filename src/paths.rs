//! Host paths under `~/.dockagents/`.
//!
//! Layout (cf. §9 of the spec):
//!
//! ```text
//! ~/.dockagents/
//!   sandboxes/<name>/        installed sandboxes (one per name; multi-version
//!                            support is left for Phase 2)
//!   cache/<name>/<version>/  pulled but unowned ephemeral sandboxes (SIP)
//!   registry/                local file-backed registry (publish target)
//!   state/                   process pidfiles, run logs
//! ```

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};

/// Resolves the DockAgents home. Honors `DOCKAGENTS_HOME` for testing, otherwise
/// `~/.dockagents`.
pub fn home() -> Result<PathBuf> {
    if let Ok(custom) = std::env::var("DOCKAGENTS_HOME") {
        return Ok(PathBuf::from(custom));
    }
    let base = dirs::home_dir().ok_or_else(|| anyhow!("could not resolve home directory"))?;
    Ok(base.join(".dockagents"))
}

pub fn sandboxes_dir() -> Result<PathBuf> {
    Ok(home()?.join("sandboxes"))
}

pub fn sandbox_dir(name: &str) -> Result<PathBuf> {
    Ok(sandboxes_dir()?.join(name))
}

pub fn cache_dir() -> Result<PathBuf> {
    Ok(home()?.join("cache"))
}

pub fn registry_dir() -> Result<PathBuf> {
    Ok(home()?.join("registry"))
}

pub fn state_dir() -> Result<PathBuf> {
    Ok(home()?.join("state"))
}

/// Expand `~` and environment variables in a manifest-supplied path.
pub fn expand(path: &Path) -> Result<PathBuf> {
    let s = path.to_string_lossy();
    let expanded = shellexpand::full(&s)
        .map_err(|e| anyhow!("expanding path {}: {}", path.display(), e))?;
    Ok(PathBuf::from(expanded.into_owned()))
}

/// Ensure all standard DockAgents directories exist.
pub fn ensure_layout() -> Result<()> {
    for d in [sandboxes_dir()?, cache_dir()?, registry_dir()?, state_dir()?] {
        std::fs::create_dir_all(&d)
            .map_err(|e| anyhow!("creating {}: {}", d.display(), e))?;
    }
    Ok(())
}

//! User-level configuration at `~/.dockagents/config.yaml`.
//!
//! Stores the user's preferred LLM provider and API key reference so an
//! installed sandbox whose manifest points at a provider the user does not
//! have credentials for can still run. The runtime falls back to
//! [`Config::default_llm`] when an agent's configured API key env var is
//! unset and no literal `api_key` is in the manifest.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths;
use crate::updater::UpdateConfig;

/// Top-level user configuration.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Config {
    /// Provider/key the runtime should use when an agent's manifest does not
    /// resolve to a usable API key. Optional — when absent and the manifest's
    /// env var is unset, the agent fails as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_llm: Option<DefaultLlm>,
    /// Named registries, keyed by alias. Populated by `dockagents registry add`.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub registries: HashMap<String, String>,
    /// Alias of the registry to use when no `--registry` flag and no
    /// `DOCKAGENTS_REGISTRY_URL` env var are set. Set by `dockagents registry use`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_registry: Option<String>,
    /// Auth tokens for the registries the user has logged in to, keyed by
    /// registry alias OR by URL (matching whichever the user used). Set by
    /// `dockagents login`.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub auth_tokens: HashMap<String, String>,
    /// GitHub self-update behavior.
    #[serde(default, skip_serializing_if = "UpdateConfig::is_default")]
    pub updates: UpdateConfig,
}

/// User-supplied default LLM. Mirrors the manifest [`crate::manifest::Llm`]
/// struct, but with `provider` and `api_key_env` required — that's the whole
/// point of having a default.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DefaultLlm {
    pub provider: String,
    pub api_key_env: String,
    /// Optional preferred model. When set, overrides each agent's `model:` at
    /// resolve time only if the agent's provider would otherwise be unusable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub extra_headers: HashMap<String, String>,
}

pub fn config_path() -> Result<PathBuf> {
    Ok(paths::home()?.join("config.yaml"))
}

impl Config {
    /// Load the config from disk. A missing file yields `Config::default()`.
    pub fn load() -> Result<Self> {
        let path = config_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading config at {}", path.display()))?;
        if raw.trim().is_empty() {
            return Ok(Self::default());
        }
        let cfg: Config = serde_yaml::from_str(&raw)
            .with_context(|| format!("parsing config at {}", path.display()))?;
        Ok(cfg)
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let yaml = serde_yaml::to_string(self).context("serializing config")?;
        std::fs::write(&path, yaml)
            .with_context(|| format!("writing config to {}", path.display()))?;
        Ok(())
    }
}

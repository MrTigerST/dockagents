//! YAML manifest parsing and validation.
//!
//! The manifest is the central contract of a sandbox. It is consumed by the
//! CLI, the runtime, and any LLM orchestrator that wants to invoke a sandbox
//! through SIP, MCP or REST. Field-level docs reference the matching section
//! of `dockagents.md`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

/// Top-level manifest, mirroring the YAML shown in §4 of the spec.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Manifest {
    pub name: String,
    pub version: semver::Version,
    #[serde(default)]
    pub description: String,
    pub lifecycle: Lifecycle,
    pub execution: Execution,
    pub agents: Vec<AgentSpec>,
    #[serde(default)]
    pub mounts: Vec<Mount>,
    #[serde(default)]
    pub capabilities: Capabilities,
    #[serde(default)]
    pub message_bus: MessageBus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Lifecycle {
    Persistent,
    Ephemeral,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExecutionMode {
    /// Caller blocks until the sandbox produces output.
    Sync,
    /// Caller continues; output is delivered via callback.
    Async,
    /// Caller delegates and forgets; output is written to the workspace/mount.
    FireAndForget,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Execution {
    pub mode: ExecutionMode,
    /// Wall-clock cap for the entire sandbox. Parsed from `humantime` strings
    /// like `120s`, `5m`.
    #[serde(with = "humantime_serde")]
    pub timeout: Duration,
    #[serde(default)]
    pub input: Vec<IoSpec>,
    #[serde(default)]
    pub output: Vec<IoSpec>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IoSpec {
    Directory {
        #[serde(default)]
        accepts: Vec<String>,
    },
    File {
        #[serde(default)]
        accepts: Vec<String>,
    },
    Text,
    StructuredJson {
        /// Path to a JSON schema, relative to the manifest.
        schema: PathBuf,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AgentSpec {
    pub id: String,
    pub model: String,
    /// Path to a markdown skill file, relative to the manifest.
    pub skill: PathBuf,
    /// Per-agent workspace directory, relative to the manifest install root.
    pub workspace: PathBuf,
    /// Optional override of the system temperature; defaults to provider default.
    #[serde(default)]
    pub temperature: Option<f32>,
    /// Optional declared inbound topics for the message bus (directed topology).
    #[serde(default)]
    pub subscribes: Vec<String>,
    /// LLM provider, endpoint, and credential reference. All fields are
    /// optional; the runtime infers sensible defaults from the model name
    /// (e.g. `claude-*` ⇒ Anthropic Messages API).
    #[serde(default)]
    pub llm: Option<Llm>,
}

/// Per-agent LLM configuration. Manifests are expected to keep credentials
/// out of source control by referencing an environment variable through
/// `api_key_env`; the literal `api_key` is supported for local testing.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Llm {
    /// `anthropic`, `openai`, `openai-compatible`, or any custom string the
    /// agent runner knows how to dispatch.
    #[serde(default)]
    pub provider: Option<String>,
    /// Full HTTPS endpoint to POST against. Defaults are provider-specific.
    #[serde(default)]
    pub endpoint: Option<String>,
    /// Name of the environment variable holding the API key. Preferred over
    /// `api_key`.
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Literal API key. Discouraged — use `api_key_env` instead.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Provider-specific API version header (e.g. Anthropic `2023-06-01`).
    #[serde(default)]
    pub api_version: Option<String>,
    /// Output token cap. Defaults to 2048.
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Extra static headers (e.g. for proxies, OpenRouter app routing).
    #[serde(default)]
    pub extra_headers: std::collections::HashMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Mount {
    /// Host path. Tilde and env vars are expanded at install/run time.
    pub host: PathBuf,
    /// Sandbox-side mount point (an absolute-looking path inside the sandbox).
    pub sandbox: PathBuf,
    #[serde(default = "Mount::default_mode")]
    pub mode: MountMode,
}

impl Mount {
    fn default_mode() -> MountMode {
        MountMode::Readonly
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MountMode {
    Readonly,
    Readwrite,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Capabilities {
    /// Sandboxes (with optional semver range) that this sandbox may invoke
    /// via SIP. Format: `name` or `name@^1.0`.
    #[serde(default)]
    pub invoke: Vec<String>,
    /// Whether the sandbox needs network access for its agent processes.
    /// Defaults to `false`; ephemeral SIP sandboxes always default to false
    /// regardless of this flag (zero-trust).
    #[serde(default)]
    pub network: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MessageBus {
    #[serde(default)]
    pub topology: Topology,
    #[serde(default)]
    pub visibility: Visibility,
}

impl Default for MessageBus {
    fn default() -> Self {
        Self {
            topology: Topology::default(),
            visibility: Visibility::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Topology {
    #[default]
    None,
    Broadcast,
    Directed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    /// Agents only see other agents' messages after writing their own output.
    #[default]
    PostOutput,
    /// Agents see each other's messages live as they are emitted.
    Live,
}

impl Manifest {
    /// Parse a manifest file. Performs schema-level validation and a few
    /// semantic checks (unique agent IDs, declared `invoke` references parse
    /// as `name[@range]`, etc.).
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading manifest at {}", path.display()))?;
        let manifest: Manifest = serde_yaml::from_str(&raw)
            .with_context(|| format!("parsing manifest at {}", path.display()))?;
        manifest.validate()?;
        Ok(manifest)
    }

    fn validate(&self) -> Result<()> {
        if self.agents.is_empty() {
            return Err(anyhow!("manifest must declare at least one agent"));
        }
        let mut seen = std::collections::HashSet::new();
        for agent in &self.agents {
            if !seen.insert(&agent.id) {
                return Err(anyhow!("duplicate agent id: {}", agent.id));
            }
            if agent.id.is_empty() {
                return Err(anyhow!("agent id cannot be empty"));
            }
        }
        for invocation in &self.capabilities.invoke {
            parse_invoke_target(invocation)?;
        }
        Ok(())
    }
}

/// Parses a SIP invocation target like `cve-lookup` or `translator@^1.0`
/// into `(name, optional VersionReq)`.
pub fn parse_invoke_target(spec: &str) -> Result<(String, Option<semver::VersionReq>)> {
    match spec.split_once('@') {
        None => Ok((spec.to_string(), None)),
        Some((name, range)) => {
            let req = semver::VersionReq::parse(range)
                .with_context(|| format!("invalid semver range in invoke target: {spec}"))?;
            Ok((name.to_string(), Some(req)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_indie_devteam_example() {
        let yaml = r#"
name:        indie-devteam
version:     1.0.0
description: Code review team for indie developers
lifecycle:   persistent

execution:
  mode:      sync
  timeout:   120s
  input:
    - type: directory
      accepts: [py, ts, rs]

agents:
  - id:        senior-reviewer
    model:     claude-sonnet-4-20250514
    skill:     ./skills/senior-reviewer.md
    workspace: ./workspaces/reviewer/
  - id:        security-auditor
    model:     claude-sonnet-4-20250514
    skill:     ./skills/security-auditor.md
    workspace: ./workspaces/security/

mounts:
  - host:    ~/Desktop/indie-devteam/
    sandbox: /output/
    mode:    readwrite

capabilities:
  invoke:
    - cve-lookup
    - translator@^1.0
"#;
        let m: Manifest = serde_yaml::from_str(yaml).unwrap();
        m.validate().unwrap();
        assert_eq!(m.name, "indie-devteam");
        assert_eq!(m.lifecycle, Lifecycle::Persistent);
        assert_eq!(m.execution.mode, ExecutionMode::Sync);
        assert_eq!(m.execution.timeout, Duration::from_secs(120));
        assert_eq!(m.agents.len(), 2);
    }

    #[test]
    fn rejects_duplicate_agent_ids() {
        let yaml = r#"
name: x
version: 1.0.0
lifecycle: ephemeral
execution: { mode: sync, timeout: 10s }
agents:
  - { id: a, model: m, skill: ./a.md, workspace: ./a/ }
  - { id: a, model: m, skill: ./a.md, workspace: ./a/ }
"#;
        let m: Manifest = serde_yaml::from_str(yaml).unwrap();
        assert!(m.validate().is_err());
    }

    #[test]
    fn parses_invoke_targets() {
        let (n, r) = parse_invoke_target("cve-lookup").unwrap();
        assert_eq!(n, "cve-lookup");
        assert!(r.is_none());

        let (n, r) = parse_invoke_target("translator@^1.0").unwrap();
        assert_eq!(n, "translator");
        assert!(r.unwrap().matches(&semver::Version::new(1, 4, 0)));
    }
}

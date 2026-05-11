//! Process manager: spawns each agent as its own OS process and wires its
//! stdin/stdout to the message bus and the SIP dispatcher.
//!
//! Each spawned process is `dockagents __agent`. OS-level isolation lives in
//! [`crate::isolation`] and is applied to the [`Command`] before spawn.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use crossbeam_channel::{Receiver, Sender};
use serde::{Deserialize, Serialize};

use crate::manifest::{AgentSpec, Manifest};
use crate::sip;

use super::bus::Envelope;
use super::workspace::SandboxLayout;

/// Configuration handed to a freshly spawned agent process via stdin.
#[derive(Debug, Serialize, Deserialize)]
pub struct AgentConfig {
    pub sandbox_name: String,
    pub agent_id: String,
    pub model: String,
    pub temperature: Option<f32>,
    pub skill_path: PathBuf,
    pub workspace: PathBuf,
    pub input_dir: PathBuf,
    pub output_file: PathBuf,
    pub log_file: PathBuf,
    pub bus_topology: String,
    pub bus_visibility: String,
    pub subscribes: Vec<String>,
    pub timeout_secs: u64,
    /// Resolved LLM endpoint configuration.
    pub llm: LlmEndpoint,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct LlmEndpoint {
    pub provider: String,
    pub endpoint: String,
    pub api_key: String,
    pub api_version: Option<String>,
    pub max_tokens: u32,
    pub extra_headers: std::collections::HashMap<String, String>,
}

pub struct AgentHandle {
    pub agent_id: String,
    pub child: Arc<Mutex<Child>>,
    pub stdout_pump: Option<JoinHandle<()>>,
    pub stderr_pump: Option<JoinHandle<()>>,
    /// Per-agent Win32 Job Object on Windows; `()` everywhere else. Held in
    /// the handle so that dropping the handle closes the job and (per
    /// `KILL_ON_JOB_CLOSE`) kills the agent process tree.
    #[cfg(windows)]
    pub _job: Option<crate::isolation::JobHandle>,
}

impl AgentHandle {
    pub fn wait_with_timeout(&mut self, dur: Duration) -> Result<i32> {
        let deadline = Instant::now() + dur;
        loop {
            {
                let mut child = self.child.lock().unwrap();
                if let Some(status) = child.try_wait()? {
                    if let Some(h) = self.stdout_pump.take() {
                        let _ = h.join();
                    }
                    if let Some(h) = self.stderr_pump.take() {
                        let _ = h.join();
                    }
                    return Ok(status.code().unwrap_or(-1));
                }
            }
            if Instant::now() >= deadline {
                return Err(anyhow!("agent {} timed out", self.agent_id));
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    pub fn kill(&self) {
        let mut child = self.child.lock().unwrap();
        let _ = child.kill();
    }
}

pub fn spawn_agent(
    manifest: &Manifest,
    agent: &AgentSpec,
    layout: &SandboxLayout,
    bus_tx: Sender<Envelope>,
    bus_rx: Receiver<Envelope>,
) -> Result<AgentHandle> {
    let workspace = layout
        .workspaces
        .get(&agent.id)
        .ok_or_else(|| anyhow!("workspace missing for {}", agent.id))?
        .clone();
    let skill_path = layout.run_root.join(&agent.skill);
    let input_dir = workspace.join("input");
    let output_file = layout.agent_output_file(&agent.id);
    let log_file = layout.log_file(&agent.id);

    let resolved = resolve_llm(agent)?;
    let model = resolved.model.clone();
    let llm = resolved.endpoint;

    let cfg = AgentConfig {
        sandbox_name: manifest.name.clone(),
        agent_id: agent.id.clone(),
        model,
        temperature: agent.temperature,
        skill_path,
        workspace: workspace.clone(),
        input_dir,
        output_file: output_file.clone(),
        log_file: log_file.clone(),
        bus_topology: super::topology_label(manifest.message_bus.topology).to_string(),
        bus_visibility: super::visibility_label(manifest.message_bus.visibility).to_string(),
        subscribes: agent.subscribes.clone(),
        timeout_secs: manifest.execution.timeout.as_secs(),
        llm,
    };

    if let Some(parent) = output_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if let Some(parent) = log_file.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let exe = std::env::current_exe().context("locating dockagents executable")?;
    let mut command = Command::new(&exe);
    command
        .arg("__agent")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Apply OS-level isolation. On Linux/macOS this rewrites the command
    // (e.g. via bwrap or sandbox-exec). On Windows we get process-tree-level
    // containment via a Job Object after spawn; see below.
    let readonly_owned: Vec<std::path::PathBuf> = manifest
        .mounts
        .iter()
        .filter(|m| matches!(m.mode, crate::manifest::MountMode::Readonly))
        .filter_map(|m| crate::paths::expand(&m.host).ok())
        .collect();
    let readwrite_owned: Vec<std::path::PathBuf> = manifest
        .mounts
        .iter()
        .filter(|m| matches!(m.mode, crate::manifest::MountMode::Readwrite))
        .filter_map(|m| crate::paths::expand(&m.host).ok())
        .collect();
    let readonly_refs: Vec<&Path> = readonly_owned.iter().map(|p| p.as_path()).collect();
    let readwrite_refs: Vec<&Path> = readwrite_owned.iter().map(|p| p.as_path()).collect();

    let _ = crate::isolation::wrap(
        &mut command,
        &crate::isolation::Sandbox {
            agent_id: &agent.id,
            workspace: &workspace,
            readonly_paths: &readonly_refs,
            readwrite_paths: &readwrite_refs,
            allow_network: manifest.capabilities.network,
        },
    );

    // Windows: build the Job Object up front so we can fail-fast on any
    // SetInformationJobObject error before launching anything.
    #[cfg(windows)]
    let job = match crate::isolation::windows::create_job(&crate::isolation::Sandbox {
        agent_id: &agent.id,
        workspace: &workspace,
        readonly_paths: &readonly_refs,
        readwrite_paths: &readwrite_refs,
        allow_network: manifest.capabilities.network,
    }) {
        Ok(j) => Some(j),
        Err(e) => {
            tracing::warn!("could not create Job Object for {}: {e}", agent.id);
            None
        }
    };

    let mut child = command
        .spawn()
        .with_context(|| format!("spawning agent process for {}", agent.id))?;

    // Windows: assign the child to its Job Object so the runtime kills the
    // whole subtree atomically and we cap the active-process count.
    #[cfg(windows)]
    if let Some(j) = job.as_ref() {
        if let Err(e) = crate::isolation::windows::attach(j, &child) {
            tracing::warn!("Job Object attach failed for {}: {e}", agent.id);
        }
    }

    // Write the resolved config to stdin and close it.
    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow!("child stdin missing"))?;
        let bytes = serde_json::to_vec(&cfg)?;
        stdin.write_all(&bytes)?;
        stdin.write_all(b"\n")?;
    }
    drop(child.stdin.take());

    // Save the pid for `dockagents status` / `dockagents stop`.
    std::fs::write(layout.pid_file(&agent.id), child.id().to_string())?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("child stdout missing"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("child stderr missing"))?;

    let id_clone = agent.id.clone();
    let log_path = log_file.clone();
    let bus_tx_clone = bus_tx.clone();
    let caller_manifest = manifest.clone();
    let workspace_for_sip = workspace.clone();
    let stdout_pump = thread::spawn(move || {
        if let Ok(mut log) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
        {
            let reader = BufReader::new(stdout);
            for line in reader.lines().flatten() {
                let _ = writeln!(log, "[stdout] {line}");
                if let Some(rest) = line.strip_prefix("@@BUS@@ ") {
                    if let Ok(env) = serde_json::from_str::<Envelope>(rest) {
                        let _ = bus_tx_clone.send(env);
                    }
                    continue;
                }
                if let Some(rest) = line.strip_prefix(sip::SIP_PREFIX) {
                    let _ = writeln!(log, "[runtime] dispatching SIP: {rest}");
                    let response = sip::dispatch(&caller_manifest, rest);
                    let _ = writeln!(
                        log,
                        "[runtime] SIP response ok={} target={} elapsed={}ms",
                        response.ok, response.sandbox, response.execution_time_ms
                    );
                    if let Err(e) =
                        sip::deliver_to_inbox(&workspace_for_sip, &id_clone, &response)
                    {
                        let _ = writeln!(log, "[runtime] failed to deliver SIP response: {e}");
                    }
                }
            }
        }
    });

    let id_clone = agent.id.clone();
    let log_path = log_file.clone();
    let stderr_pump = thread::spawn(move || {
        if let Ok(mut log) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
        {
            let reader = BufReader::new(stderr);
            for line in reader.lines().flatten() {
                let _ = writeln!(log, "[stderr] {line}");
            }
        }
        let _ = id_clone;
    });

    // A delivery thread that forwards bus envelopes back into the agent
    // process. We can't write to the child's stdin after we closed it, so we
    // append envelopes to a per-agent inbox file the agent runner polls.
    let inbox = workspace.join(".inbox.jsonl");
    let agent_id = agent.id.clone();
    thread::spawn(move || {
        while let Ok(env) = bus_rx.recv() {
            if let Some(ref to) = env.to {
                if to != &agent_id {
                    continue;
                }
            }
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&inbox)
            {
                if let Ok(line) = serde_json::to_string(&env) {
                    let _ = writeln!(f, "{line}");
                }
            }
        }
    });

    Ok(AgentHandle {
        agent_id: agent.id.clone(),
        child: Arc::new(Mutex::new(child)),
        stdout_pump: Some(stdout_pump),
        stderr_pump: Some(stderr_pump),
        #[cfg(windows)]
        _job: job,
    })
}

/// What `resolve_llm` returns: the endpoint plus the model the agent will
/// actually call (the manifest's `model:` may have been swapped for the
/// user's global default if the manifest's provider was unusable).
pub struct ResolvedLlm {
    pub endpoint: LlmEndpoint,
    pub model: String,
}

/// Resolve the LLM configuration for an agent, applying provider defaults and
/// expanding the API-key reference. If the manifest's declared key isn't
/// available in the environment, falls back to the user's global default
/// from `~/.dockagents/config.yaml`.
fn resolve_llm(agent: &AgentSpec) -> Result<ResolvedLlm> {
    let llm = agent.llm.clone().unwrap_or_default();

    let provider = llm
        .provider
        .clone()
        .unwrap_or_else(|| infer_provider(&agent.model));

    let endpoint = llm.endpoint.clone().unwrap_or_else(|| default_endpoint(&provider));

    if endpoint.is_empty() {
        return Err(anyhow!(
            "agent '{}' has no `llm.endpoint` and provider '{}' has no default",
            agent.id,
            provider
        ));
    }

    // First, try the manifest-declared key. If it's missing, fall back to the
    // global default LLM (so a sandbox published for, say, OpenAI can still be
    // run by a user who only has an Anthropic key).
    let manifest_key = if let Some(env_var) = &llm.api_key_env {
        std::env::var(env_var).ok()
    } else if let Some(key) = &llm.api_key {
        Some(key.clone())
    } else {
        let fallback = match provider.as_str() {
            "anthropic" => "ANTHROPIC_API_KEY",
            "openai" | "openai-compatible" => "OPENAI_API_KEY",
            _ => "",
        };
        if fallback.is_empty() {
            None
        } else {
            std::env::var(fallback).ok()
        }
    };

    if let Some(api_key) = manifest_key {
        let api_version = llm.api_version.clone().or_else(|| {
            if provider == "anthropic" {
                Some("2023-06-01".to_string())
            } else {
                None
            }
        });
        return Ok(ResolvedLlm {
            endpoint: LlmEndpoint {
                provider,
                endpoint,
                api_key,
                api_version,
                max_tokens: llm.max_tokens.unwrap_or(2048),
                extra_headers: llm.extra_headers.clone(),
            },
            model: agent.model.clone(),
        });
    }

    // Fall back to the user's global default.
    let user_cfg = crate::config::Config::load().unwrap_or_default();
    if let Some(def) = user_cfg.default_llm {
        let api_key = std::env::var(&def.api_key_env).map_err(|_| {
            anyhow!(
                "agent '{}': manifest credentials missing and global default LLM env var `{}` is also unset. \
                 Either set the manifest's API key env var, or run `dockagents config set-default-llm` and \
                 export `{}`.",
                agent.id, def.api_key_env, def.api_key_env
            )
        })?;

        let provider = def.provider.clone();
        let endpoint = def
            .endpoint
            .clone()
            .unwrap_or_else(|| default_endpoint(&provider));
        if endpoint.is_empty() {
            return Err(anyhow!(
                "agent '{}': global default LLM has no endpoint and provider '{}' has no default",
                agent.id, provider
            ));
        }

        let api_version = def.api_version.clone().or_else(|| {
            if provider == "anthropic" {
                Some("2023-06-01".to_string())
            } else {
                None
            }
        });

        // Pick a model: the user's preferred default if set, otherwise keep
        // whatever the manifest asked for (and hope it works for the new
        // provider — typical for openai-compatible aggregators).
        let model = def.model.clone().unwrap_or_else(|| agent.model.clone());

        tracing::info!(
            "agent '{}': manifest LLM unavailable, falling back to global default ({} via {})",
            agent.id,
            model,
            provider
        );

        return Ok(ResolvedLlm {
            endpoint: LlmEndpoint {
                provider,
                endpoint,
                api_key,
                api_version,
                max_tokens: def.max_tokens.or(llm.max_tokens).unwrap_or(2048),
                extra_headers: def.extra_headers.clone(),
            },
            model,
        });
    }

    // No manifest credentials, no global default — fail with a helpful pointer.
    Err(anyhow!(
        "agent '{}' has no usable LLM credentials. Either:\n  \
         (a) set the env var the manifest references (`{}`),\n  \
         (b) run `dockagents install --override-llm provider=...,api_key_env=...` to rewrite the manifest, or\n  \
         (c) run `dockagents config set-default-llm --provider <p> --api-key-env <ENV>` to configure a global default.",
        agent.id,
        llm.api_key_env.as_deref().unwrap_or_else(|| match provider.as_str() {
            "anthropic" => "ANTHROPIC_API_KEY",
            "openai" | "openai-compatible" => "OPENAI_API_KEY",
            _ => "(none)",
        })
    ))
}

fn default_endpoint(provider: &str) -> String {
    match provider {
        "anthropic" => "https://api.anthropic.com/v1/messages".to_string(),
        "openai" | "openai-compatible" => {
            "https://api.openai.com/v1/chat/completions".to_string()
        }
        _ => String::new(),
    }
}

fn infer_provider(model: &str) -> String {
    let m = model.to_lowercase();
    if m.starts_with("claude") {
        "anthropic".into()
    } else if m.starts_with("gpt") || m.starts_with("o1") || m.starts_with("o3") {
        "openai".into()
    } else {
        "openai-compatible".into()
    }
}


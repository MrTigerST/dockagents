//! Runtime: takes an installed sandbox and runs it.
//!
//! The runtime is responsible for:
//!   * preparing each agent's isolated workspace,
//!   * bridging declared mounts onto the host filesystem,
//!   * spawning each agent as its own OS process,
//!   * brokering inter-agent messages on a bus,
//!   * collecting outputs and writing the final synthesized report.

pub mod bus;
pub mod process;
pub mod workspace;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

use crate::manifest::{ExecutionMode, Lifecycle, Manifest, Topology, Visibility};
use crate::paths;

use self::bus::{Bus, Envelope};
use self::process::AgentHandle;
use self::workspace::SandboxLayout;

/// One end-to-end sandbox execution.
pub struct Run {
    pub manifest: Manifest,
    pub layout: SandboxLayout,
    pub input: Input,
}

#[derive(Debug, Clone)]
pub struct Input {
    /// Optional file/directory the user passed via `--input`.
    pub path: Option<PathBuf>,
    /// Optional inline string input (used by SIP / API callers).
    pub text: Option<String>,
}

/// What the runtime returns to the CLI / API layer once all agents have
/// produced their output.
#[derive(Debug, Serialize, Deserialize)]
pub struct RunReport {
    pub sandbox: String,
    pub version: String,
    pub execution_time_ms: u128,
    pub agent_outputs: HashMap<String, AgentOutput>,
    pub output_path: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AgentOutput {
    pub status: AgentStatus,
    pub output_file: PathBuf,
    pub log_file: PathBuf,
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentStatus {
    Ok,
    Failed,
    Timeout,
}

/// Live progress events emitted while a sandbox runs. Consumers (SSE
/// streaming in the REST API, future TUIs, log forwarders) subscribe via
/// the optional channel handed to [`run_sandbox_with_progress`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum RunEvent {
    RunStarted {
        sandbox: String,
        version: String,
        agents: Vec<String>,
    },
    AgentSpawned {
        agent: String,
    },
    AgentFinished {
        agent: String,
        status: AgentStatus,
        exit_code: Option<i32>,
        output_file: PathBuf,
    },
    RunFinished {
        execution_time_ms: u128,
        report_path: PathBuf,
    },
}

pub type ProgressSink = crossbeam_channel::Sender<RunEvent>;

pub fn run_sandbox(
    install_root: &Path,
    manifest: Manifest,
    input: Input,
    cancel: Arc<AtomicBool>,
) -> Result<RunReport> {
    run_sandbox_with_progress(install_root, manifest, input, cancel, None)
}

pub fn run_sandbox_with_progress(
    install_root: &Path,
    manifest: Manifest,
    input: Input,
    cancel: Arc<AtomicBool>,
    progress: Option<ProgressSink>,
) -> Result<RunReport> {
    paths::ensure_layout()?;
    let started = Instant::now();
    let layout = SandboxLayout::prepare(install_root, &manifest)?;
    layout.bridge_mounts(&manifest)?;

    emit(&progress, RunEvent::RunStarted {
        sandbox: manifest.name.clone(),
        version: manifest.version.to_string(),
        agents: manifest.agents.iter().map(|a| a.id.clone()).collect(),
    });

    let bus = Bus::new(&manifest.message_bus);
    let mut handles: Vec<AgentHandle> = Vec::with_capacity(manifest.agents.len());

    // Make the input available to every agent under <workspace>/input/.
    layout.distribute_input(&manifest, &input)?;

    for agent in &manifest.agents {
        let receiver = bus.subscribe(&agent.id);
        let sender = bus.sender();
        let handle = process::spawn_agent(
            &manifest,
            agent,
            &layout,
            sender,
            receiver,
        )
        .with_context(|| format!("spawning agent '{}'", agent.id))?;
        emit(&progress, RunEvent::AgentSpawned { agent: agent.id.clone() });
        handles.push(handle);
    }

    // Drive the bus on a background thread so messages flow while agents run.
    let bus_thread = bus.spawn_router();

    let mut outputs = HashMap::new();
    let timeout = manifest.execution.timeout;
    let deadline = started + timeout;

    for mut h in handles.drain(..) {
        let now = Instant::now();
        let remaining = if now >= deadline {
            std::time::Duration::ZERO
        } else {
            deadline - now
        };

        let cancelled = cancel.load(Ordering::Relaxed);
        let result = if cancelled {
            h.kill();
            Err(anyhow!("cancelled"))
        } else {
            h.wait_with_timeout(remaining)
        };

        let log_file = layout.log_file(&h.agent_id);
        let output_file = layout.agent_output_file(&h.agent_id);
        let (status, exit_code) = match result {
            Ok(code) if code == 0 => (AgentStatus::Ok, Some(code)),
            Ok(code) => (AgentStatus::Failed, Some(code)),
            Err(e) => {
                tracing::warn!("agent {} failed/timed out: {e}", h.agent_id);
                if e.to_string().contains("timed out") {
                    h.kill();
                    (AgentStatus::Timeout, None)
                } else {
                    (AgentStatus::Failed, None)
                }
            }
        };
        emit(&progress, RunEvent::AgentFinished {
            agent: h.agent_id.clone(),
            status,
            exit_code,
            output_file: output_file.clone(),
        });
        outputs.insert(
            h.agent_id.clone(),
            AgentOutput {
                status,
                output_file,
                log_file,
                exit_code,
            },
        );
    }

    drop(bus_thread); // router exits when senders drop
    let final_path = layout.write_synthesized_report(&manifest, &outputs)?;

    if matches!(manifest.lifecycle, Lifecycle::Ephemeral) {
        layout.tear_down()?;
    }

    let elapsed = started.elapsed().as_millis();
    emit(&progress, RunEvent::RunFinished {
        execution_time_ms: elapsed,
        report_path: final_path.clone(),
    });

    Ok(RunReport {
        sandbox: manifest.name.clone(),
        version: manifest.version.to_string(),
        execution_time_ms: elapsed,
        agent_outputs: outputs,
        output_path: final_path,
    })
}

fn emit(sink: &Option<ProgressSink>, event: RunEvent) {
    if let Some(s) = sink {
        let _ = s.send(event);
    }
}

/// SIP marker: re-exported from the [`crate::sip`] module so existing
/// callers in `runtime::process` can keep their import path.
pub use crate::sip::SIP_PREFIX;

/// Convenience for tests / external callers.
pub fn execution_mode_label(mode: ExecutionMode) -> &'static str {
    match mode {
        ExecutionMode::Sync => "sync",
        ExecutionMode::Async => "async",
        ExecutionMode::FireAndForget => "fire-and-forget",
    }
}

pub fn topology_label(t: Topology) -> &'static str {
    match t {
        Topology::None => "none",
        Topology::Broadcast => "broadcast",
        Topology::Directed => "directed",
    }
}

pub fn visibility_label(v: Visibility) -> &'static str {
    match v {
        Visibility::Live => "live",
        Visibility::PostOutput => "post_output",
    }
}

/// What gets serialized to disk as `<sandbox>/.state/last_envelope.json`.
#[derive(Debug, Serialize, Deserialize)]
pub struct LastEnvelopeRecord {
    pub envelopes: Vec<Envelope>,
}

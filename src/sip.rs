//! Sandbox Invocation Protocol (SIP).
//!
//! When an agent on stdout emits a line of the form
//!
//!   `@@SIP@@ <json>`
//!
//! the runtime parses the JSON as a [`SipRequest`], checks the calling
//! sandbox's `capabilities.invoke` allow-list, resolves the target sandbox
//! (local install first, then HTTP registry if `DOCKAGENTS_REGISTRY_URL` is
//! set), runs it as an ephemeral sandbox, and writes a [`SipResponse`] back
//! to the calling agent's `.inbox.jsonl` so the agent can read it on its
//! next turn.
//!
//! Zero-trust per dockagents.md §10:
//!   * the ephemeral sandbox sees only the `input` field — never the caller's
//!     workspace,
//!   * mounts declared by the ephemeral sandbox are bridged onto a fresh
//!     run-root under `~/.dockagents/cache/`,
//!   * `lifecycle: ephemeral` causes the run-root to be torn down after the
//!     response is delivered.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

use crate::manifest::{parse_invoke_target, Lifecycle, Manifest};
use crate::paths;
use crate::registry::Registry;
use crate::remote::{self, RemoteRegistry};
use crate::runtime::{self, Input};

pub const SIP_PREFIX: &str = "@@SIP@@ ";

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SipRequest {
    /// Correlation ID echoed back in the response.
    #[serde(default)]
    pub id: Option<String>,
    pub sandbox: String,
    /// Optional semver range (e.g. `^1.0`).
    #[serde(default)]
    pub version: Option<String>,
    /// Optional explicit timeout (overrides the target manifest's value if
    /// shorter — never longer, for safety).
    #[serde(default)]
    pub timeout: Option<String>,
    /// Either an inline string (passed as `--text`) or a path on the host.
    #[serde(default)]
    pub input: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SipResponse {
    pub id: Option<String>,
    pub sandbox: String,
    pub version: String,
    pub ok: bool,
    pub error: Option<String>,
    pub output: Option<String>,
    pub execution_time_ms: u128,
}

/// Validate + dispatch a SIP call. Returns the response (which the runtime
/// writes to the caller's inbox).
pub fn dispatch(caller: &Manifest, raw: &str) -> SipResponse {
    let started = std::time::Instant::now();
    let req: SipRequest = match serde_json::from_str(raw) {
        Ok(r) => r,
        Err(e) => {
            return SipResponse {
                id: None,
                sandbox: String::new(),
                version: String::new(),
                ok: false,
                error: Some(format!("invalid SIP payload: {e}")),
                output: None,
                execution_time_ms: started.elapsed().as_millis(),
            }
        }
    };

    match dispatch_inner(caller, &req) {
        Ok(out) => SipResponse {
            id: req.id.clone(),
            sandbox: out.sandbox,
            version: out.version,
            ok: true,
            error: None,
            output: Some(out.report_md),
            execution_time_ms: started.elapsed().as_millis(),
        },
        Err(e) => SipResponse {
            id: req.id.clone(),
            sandbox: req.sandbox.clone(),
            version: req.version.clone().unwrap_or_default(),
            ok: false,
            error: Some(format!("{e:#}")),
            output: None,
            execution_time_ms: started.elapsed().as_millis(),
        },
    }
}

struct DispatchOk {
    sandbox: String,
    version: String,
    report_md: String,
}

fn dispatch_inner(caller: &Manifest, req: &SipRequest) -> Result<DispatchOk> {
    enforce_capability(caller, &req.sandbox)?;

    let install_root = resolve_target(&req.sandbox, req.version.as_deref())?;
    let mut target_manifest = Manifest::load(&install_root.join("manifest.yaml"))?;

    // Force ephemeral lifecycle for SIP-invoked sandboxes regardless of how
    // the package itself declares lifecycle. dockagents.md §10.
    target_manifest.lifecycle = Lifecycle::Ephemeral;

    if let Some(t) = &req.timeout {
        if let Ok(dur) = humantime::parse_duration(t) {
            if dur < target_manifest.execution.timeout {
                target_manifest.execution.timeout = dur;
            }
        }
    }

    let input = build_input(&req.input)?;
    let cancel = Arc::new(AtomicBool::new(false));
    let report = runtime::run_sandbox(&install_root, target_manifest.clone(), input, cancel)?;
    let report_md = std::fs::read_to_string(&report.output_path).unwrap_or_default();

    Ok(DispatchOk {
        sandbox: target_manifest.name,
        version: target_manifest.version.to_string(),
        report_md,
    })
}

fn enforce_capability(caller: &Manifest, target_name: &str) -> Result<()> {
    let allowed = caller.capabilities.invoke.iter().any(|spec| {
        match parse_invoke_target(spec) {
            Ok((name, _)) => name == target_name,
            Err(_) => false,
        }
    });
    if !allowed {
        return Err(anyhow!(
            "sandbox '{}' tried to invoke '{}' but it is not declared in capabilities.invoke",
            caller.name,
            target_name
        ));
    }
    Ok(())
}

fn resolve_target(name: &str, _version: Option<&str>) -> Result<std::path::PathBuf> {
    // 1. Already installed?
    let installed = paths::sandbox_dir(name)?;
    if installed.join("manifest.yaml").exists() {
        return Ok(installed);
    }

    // 2. Local file registry?
    if let Ok(p) = Registry::locate(name) {
        return Ok(p);
    }

    // 3. Remote registry, if configured.
    if let Some(remote) = RemoteRegistry::from_flag_or_env(None) {
        let detail = remote
            .get_package(name)
            .with_context(|| format!("resolving SIP target '{name}' on remote registry"))?;
        let bytes = remote.pull(&detail.name, &detail.latest)?;
        let cache = paths::cache_dir()?
            .join(&detail.name)
            .join(&detail.latest);
        remote::unpack_into(&bytes, &cache)?;
        return Ok(cache);
    }

    Err(anyhow!(
        "could not resolve SIP target '{name}' (not installed, not in local registry, and no DOCKAGENTS_REGISTRY_URL)"
    ))
}

fn build_input(value: &serde_json::Value) -> Result<Input> {
    match value {
        serde_json::Value::Null => Ok(Input { path: None, text: None }),
        serde_json::Value::String(s) => Ok(Input {
            path: None,
            text: Some(s.clone()),
        }),
        serde_json::Value::Object(map) => {
            let path = map
                .get("path")
                .and_then(|v| v.as_str())
                .map(std::path::PathBuf::from);
            let text = map
                .get("text")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            // For maps without `path`/`text`, fall back to JSON-stringifying
            // so the called agent at least receives the structured payload
            // verbatim.
            let text = text.or_else(|| Some(serde_json::to_string(value).unwrap_or_default()));
            Ok(Input { path, text })
        }
        other => Ok(Input {
            path: None,
            text: Some(other.to_string()),
        }),
    }
}

/// Format a [`SipResponse`] as a single JSONL line for the caller's inbox.
pub fn response_to_inbox_line(target: &str, response: &SipResponse) -> Result<String> {
    let env = serde_json::json!({
        "from": "__sip__",
        "to": target,
        "topic": "sip.response",
        "body": serde_json::to_string(response)?,
        "output_ready": false,
    });
    Ok(env.to_string())
}

/// Append a SIP envelope to an agent's inbox file.
pub fn deliver_to_inbox(workspace: &Path, target: &str, response: &SipResponse) -> Result<()> {
    let inbox = workspace.join(".inbox.jsonl");
    let line = response_to_inbox_line(target, response)?;
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(inbox)?;
    writeln!(f, "{line}")?;
    Ok(())
}

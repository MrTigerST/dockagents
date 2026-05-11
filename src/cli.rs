//! `dockagents` CLI surface.
//!
//! Mirrors §8.1 of the spec. Many commands are thin wrappers over the
//! [`crate::registry`] and [`crate::runtime`] modules. The hidden `__agent`
//! subcommand is what the runtime spawns for each agent process.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};

use crate::manifest::Manifest;
use crate::paths;
use crate::registry::Registry;
use crate::remote::{RemoteRegistry, SignMode};
use crate::runtime::{self, Input};
use crate::signing;

#[derive(Debug, Parser)]
#[command(
    name = "dockagents",
    version,
    about = "DockAgents — distributable multi-agent sandboxes",
    propagate_version = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Install a sandbox by name or natural-language description.
    Install {
        /// Sandbox name, registry path, or natural language query (in quotes).
        target: String,
        /// HTTP registry URL. Falls back to `DOCKAGENTS_REGISTRY_URL`, then
        /// the local file registry under `~/.dockagents/registry/`.
        #[arg(long)]
        registry: Option<String>,
        /// Specific version to install. Defaults to the registry's `latest`.
        #[arg(long)]
        version: Option<String>,
        /// Rewrite every agent's `llm:` block in the installed manifest.
        /// Comma-separated key=value pairs. Recognized keys:
        /// `provider`, `api_key_env`, `model`, `endpoint`, `api_version`, `max_tokens`.
        /// Example: --override-llm provider=anthropic,api_key_env=ANTHROPIC_API_KEY,model=claude-sonnet-4-6
        #[arg(long, value_name = "KEY=VAL,...")]
        override_llm: Option<String>,
    },
    /// Run a sandbox.
    Run {
        /// Sandbox name (must already be installed) or path to a sandbox source.
        target: String,
        /// Path to input file or directory passed to every agent.
        #[arg(long)]
        input: Option<PathBuf>,
        /// Inline string input. Overrides --input if both provided.
        #[arg(long)]
        text: Option<String>,
        /// Path on the host where the synthesized report should also be copied.
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// List installed sandboxes.
    List,
    /// Show installed-sandbox status (running pids, last run, etc.).
    Status {
        target: String,
    },
    /// Stop any agent processes belonging to a running sandbox.
    Stop {
        target: String,
    },
    /// Publish a sandbox source directory to a registry.
    Publish {
        /// Path to a directory containing a `manifest.yaml`.
        path: PathBuf,
        /// HTTP registry URL. Falls back to `DOCKAGENTS_REGISTRY_URL`. When
        /// neither is set, publishes to the local file registry.
        #[arg(long)]
        registry: Option<String>,
        /// Require signing (fails if no key). Default: sign if a key is
        /// present, otherwise publish unsigned.
        #[arg(long)]
        sign: bool,
        /// Skip signing entirely.
        #[arg(long, conflicts_with = "sign")]
        no_sign: bool,
    },
    /// Generate a publisher Ed25519 keypair under `~/.dockagents/keys/`.
    Keygen {
        /// Overwrite an existing key.
        #[arg(long)]
        force: bool,
    },
    /// Run the MCP server (JSON-RPC over stdio).
    Mcp,
    /// Run the REST API for orchestrators.
    Serve {
        #[arg(long, default_value_t = 8989)]
        port: u16,
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
    },
    /// Watch the host mount input/ and re-run the sandbox on changes.
    Watch {
        target: String,
        /// Debounce window before re-running.
        #[arg(long, default_value = "1500ms")]
        debounce: String,
    },
    /// Search the registry.
    Search {
        query: String,
        /// HTTP registry URL. Falls back to `DOCKAGENTS_REGISTRY_URL`, then
        /// the local file registry.
        #[arg(long)]
        registry: Option<String>,
    },
    /// Pull a sandbox into the install root without running it.
    Pull {
        target: String,
        #[arg(long)]
        registry: Option<String>,
        #[arg(long)]
        version: Option<String>,
    },
    /// Show the manifest for an installed sandbox.
    Manifest {
        target: String,
    },
    /// Inspect or modify `~/.dockagents/config.yaml`.
    Config {
        #[command(subcommand)]
        action: ConfigCmd,
    },
    /// Manage named registries stored in `~/.dockagents/config.yaml`.
    Registry {
        #[command(subcommand)]
        action: RegistryCmd,
    },
    /// Save an auth token for `dockagents publish` (mint one at <website>/me).
    Login {
        /// The token shown on the website's account page (starts with `dgkp_`).
        #[arg(long)]
        token: String,
        /// Registry alias or URL this token is for. Defaults to the current
        /// default registry.
        #[arg(long)]
        registry: Option<String>,
    },
    /// Forget the saved auth token for a registry.
    Logout {
        /// Registry alias or URL to forget. Defaults to the current default registry.
        #[arg(long)]
        registry: Option<String>,
    },
    /// Internal: agent subprocess entry point. Reads its config from stdin.
    #[command(name = "__agent", hide = true)]
    Agent,
}

#[derive(Debug, Subcommand)]
pub enum RegistryCmd {
    /// Add or overwrite a named registry.
    Add {
        /// Short alias, e.g. `local` or `prod`.
        name: String,
        /// Full URL, e.g. `http://localhost:8787`.
        url: String,
    },
    /// Remove a named registry.
    Remove {
        name: String,
    },
    /// Set the default registry by name. Used when no `--registry` flag and no
    /// `DOCKAGENTS_REGISTRY_URL` env var are set.
    Use {
        name: String,
    },
    /// List configured registries and which one is default.
    List,
    /// Clear the default registry (commands fall back to env or local file registry).
    ClearDefault,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCmd {
    /// Print the current config.
    Show,
    /// Print the path to the config file.
    Path,
    /// Set the global default LLM. Used as a fallback when an installed
    /// sandbox declares an LLM provider/key the user does not have.
    SetDefaultLlm {
        /// Provider: `anthropic`, `openai`, or `openai-compatible`.
        #[arg(long)]
        provider: String,
        /// Name of the env var that holds the API key.
        #[arg(long, value_name = "ENV")]
        api_key_env: String,
        /// Preferred model. Used in place of the manifest's `model:` when
        /// this fallback applies.
        #[arg(long)]
        model: Option<String>,
        /// Override the default endpoint URL.
        #[arg(long)]
        endpoint: Option<String>,
        /// Anthropic API version header (default `2023-06-01`).
        #[arg(long)]
        api_version: Option<String>,
        /// Output token cap.
        #[arg(long)]
        max_tokens: Option<u32>,
    },
    /// Remove the configured default LLM.
    ClearDefaultLlm,
}

pub fn dispatch(cli: Cli) -> Result<ExitCode> {
    paths::ensure_layout()?;

    match cli.cmd {
        Cmd::Agent => match crate::agent::run() {
            Ok(()) => Ok(ExitCode::from(0)),
            Err(e) => {
                eprintln!("agent error: {e:#}");
                Ok(ExitCode::from(1))
            }
        },
        Cmd::Install {
            target,
            registry,
            version,
            override_llm,
        } => cmd_install(
            &target,
            registry.as_deref(),
            version.as_deref(),
            override_llm.as_deref(),
        )
        .map(|_| ExitCode::from(0)),
        Cmd::Run {
            target,
            input,
            text,
            output,
        } => cmd_run(&target, input, text, output),
        Cmd::List => cmd_list().map(|_| ExitCode::from(0)),
        Cmd::Status { target } => cmd_status(&target).map(|_| ExitCode::from(0)),
        Cmd::Stop { target } => cmd_stop(&target).map(|_| ExitCode::from(0)),
        Cmd::Publish {
            path,
            registry,
            sign,
            no_sign,
        } => {
            let mode = if no_sign {
                SignMode::None
            } else if sign {
                SignMode::Required
            } else {
                SignMode::IfAvailable
            };
            cmd_publish(&path, registry.as_deref(), mode).map(|_| ExitCode::from(0))
        }
        Cmd::Search { query, registry } => {
            cmd_search(&query, registry.as_deref()).map(|_| ExitCode::from(0))
        }
        Cmd::Pull {
            target,
            registry,
            version,
        } => cmd_pull(&target, registry.as_deref(), version.as_deref())
            .map(|_| ExitCode::from(0)),
        Cmd::Manifest { target } => cmd_manifest(&target).map(|_| ExitCode::from(0)),
        Cmd::Config { action } => cmd_config(action).map(|_| ExitCode::from(0)),
        Cmd::Registry { action } => cmd_registry(action).map(|_| ExitCode::from(0)),
        Cmd::Login { token, registry } => {
            cmd_login(&token, registry.as_deref()).map(|_| ExitCode::from(0))
        }
        Cmd::Logout { registry } => cmd_logout(registry.as_deref()).map(|_| ExitCode::from(0)),
        Cmd::Keygen { force } => cmd_keygen(force).map(|_| ExitCode::from(0)),
        Cmd::Mcp => crate::mcp::run().map(|_| ExitCode::from(0)),
        Cmd::Serve { port, host } => crate::api::serve(&host, port).map(|_| ExitCode::from(0)),
        Cmd::Watch { target, debounce } => {
            crate::watcher::watch(&target, &debounce).map(|_| ExitCode::from(0))
        }
    }
}

fn cmd_install(
    target: &str,
    registry_flag: Option<&str>,
    version: Option<&str>,
    override_llm: Option<&str>,
) -> Result<()> {
    let override_parsed = match override_llm {
        Some(s) => Some(parse_llm_override(s)?),
        None => None,
    };

    if let Some(remote) = resolve_registry(registry_flag)? {
        return install_from_remote(&remote, target, version, override_parsed.as_ref());
    }

    if version.is_some() {
        tracing::warn!("--version is ignored for the local file registry");
    }

    let source = match Registry::resolve_source(target) {
        Ok(p) => p,
        Err(_) => {
            // Fall through to substring-search the local registry.
            let hits = Registry::search(target)?;
            if hits.is_empty() {
                return Err(anyhow!(
                    "no sandbox found for '{target}'. \
                     Pass a directory path, configure --registry, or `dockagents publish` it first."
                ));
            }
            let pick = &hits[0];
            tracing::info!(
                "best match: {}@{}  ({})",
                pick.name,
                pick.version,
                pick.description
            );
            paths::registry_dir()?.join(&pick.name)
        }
    };

    let manifest = Manifest::load(&source.join("manifest.yaml"))?;
    install_dir_to_sandboxes(&source, &manifest)?;
    let install_root = paths::sandbox_dir(&manifest.name)?;
    if let Some(ovr) = override_parsed.as_ref() {
        apply_llm_override(&install_root.join("manifest.yaml"), ovr)?;
        println!(
            "Installed {}@{} → {}  (llm overridden: {})",
            manifest.name,
            manifest.version,
            install_root.display(),
            ovr.summary()
        );
    } else {
        println!(
            "Installed {}@{} → {}",
            manifest.name,
            manifest.version,
            install_root.display()
        );
    }
    Ok(())
}

fn install_from_remote(
    remote: &RemoteRegistry,
    target: &str,
    version: Option<&str>,
    override_llm: Option<&LlmOverride>,
) -> Result<()> {
    // Resolve `target` to a (name, version). If the target isn't a known
    // package, fall back to searching for it.
    let (name, version) = match remote.get_package(target) {
        Ok(detail) => {
            let v = match version {
                Some(v) if is_range(v) => {
                    let resolved = remote.resolve(&detail.name, v)?;
                    tracing::info!("range '{v}' → resolved {}", resolved.resolved);
                    resolved.resolved
                }
                Some(v) => v.to_string(),
                None => detail.latest.clone(),
            };
            (detail.name, v)
        }
        Err(_) => {
            let search = remote.search(target)?;
            let pick = search
                .results
                .first()
                .ok_or_else(|| anyhow!("no remote match for '{target}'"))?;
            tracing::info!(
                "remote best match: {}@{} ({})",
                pick.name,
                pick.latest,
                pick.description
            );
            let v = match version {
                Some(v) if is_range(v) => {
                    let resolved = remote.resolve(&pick.name, v)?;
                    resolved.resolved
                }
                Some(v) => v.to_string(),
                None => pick.latest.clone(),
            };
            (pick.name.clone(), v)
        }
    };

    tracing::info!("pulling {name}@{version}");
    let bytes = remote.pull(&name, &version)?;
    let install_to = paths::sandbox_dir(&name)?;
    crate::remote::unpack_into(&bytes, &install_to)?;
    let manifest = Manifest::load(&install_to.join("manifest.yaml"))
        .context("loading manifest after extracting tarball")?;
    if let Some(ovr) = override_llm {
        apply_llm_override(&install_to.join("manifest.yaml"), ovr)?;
        println!(
            "Installed {}@{} from {} → {}  (llm overridden: {})",
            manifest.name,
            manifest.version,
            registry_url_label(remote),
            install_to.display(),
            ovr.summary()
        );
    } else {
        println!(
            "Installed {}@{} from {} → {}",
            manifest.name,
            manifest.version,
            registry_url_label(remote),
            install_to.display()
        );
    }
    Ok(())
}

fn is_range(spec: &str) -> bool {
    // Anything with a semver operator or an `x` placeholder is a range, not
    // a literal version.
    let s = spec.trim();
    s.starts_with('^')
        || s.starts_with('~')
        || s.starts_with('>')
        || s.starts_with('<')
        || s.starts_with('=')
        || s.contains('*')
        || s.contains(' ')
        || s.contains('x')
        || s.contains('X')
}

fn install_dir_to_sandboxes(source: &std::path::Path, manifest: &Manifest) -> Result<()> {
    let install_to = paths::sandbox_dir(&manifest.name)?;
    if install_to.exists() {
        tracing::info!("removing prior install at {}", install_to.display());
        std::fs::remove_dir_all(&install_to)?;
    }
    std::fs::create_dir_all(install_to.parent().unwrap())?;
    std::fs::create_dir_all(&install_to)?;
    let mut opts = fs_extra::dir::CopyOptions::new();
    opts.copy_inside = true;
    opts.overwrite = true;
    fs_extra::dir::copy(source, &install_to, &opts)?;
    Ok(())
}

fn registry_url_label(_remote: &RemoteRegistry) -> String {
    std::env::var("DOCKAGENTS_REGISTRY_URL").unwrap_or_else(|_| "remote".to_string())
}

fn cmd_run(
    target: &str,
    input: Option<PathBuf>,
    text: Option<String>,
    output_override: Option<PathBuf>,
) -> Result<ExitCode> {
    let install_root = match Registry::resolve_source(target) {
        Ok(p) => p,
        Err(_) => paths::sandbox_dir(target)?,
    };
    let manifest_path = install_root.join("manifest.yaml");
    let manifest = Manifest::load(&manifest_path).with_context(|| {
        format!(
            "loading manifest from {} (have you `dockagents install`-ed this sandbox?)",
            manifest_path.display()
        )
    })?;

    let input = Input {
        path: input,
        text,
    };

    let cancel = Arc::new(AtomicBool::new(false));
    {
        let c = cancel.clone();
        let _ = ctrlc::set_handler(move || {
            tracing::warn!("interrupt received, stopping sandbox");
            c.store(true, Ordering::Relaxed);
        });
    }

    let report = runtime::run_sandbox(&install_root, manifest, input, cancel)?;

    println!("\n══ Run report ══");
    println!("sandbox:      {}@{}", report.sandbox, report.version);
    println!("elapsed:      {} ms", report.execution_time_ms);
    println!("output:       {}", report.output_path.display());
    println!("agent results:");
    for (id, out) in &report.agent_outputs {
        println!(
            "  - {:<24}  {:?}  exit={:?}  log={}",
            id,
            out.status,
            out.exit_code,
            out.log_file.display()
        );
    }

    if let Some(extra) = output_override {
        std::fs::create_dir_all(&extra)?;
        let dest = extra.join("report.md");
        std::fs::copy(&report.output_path, &dest)?;
        println!("copied report → {}", dest.display());
    }

    let any_failed = report
        .agent_outputs
        .values()
        .any(|o| !matches!(o.status, runtime::AgentStatus::Ok));
    Ok(if any_failed {
        ExitCode::from(2)
    } else {
        ExitCode::from(0)
    })
}

fn cmd_list() -> Result<()> {
    let dir = paths::sandboxes_dir()?;
    if !dir.exists() {
        println!("(no sandboxes installed)");
        return Ok(());
    }
    let mut found = false;
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let manifest_path = entry.path().join("manifest.yaml");
        if !manifest_path.exists() {
            continue;
        }
        if let Ok(m) = Manifest::load(&manifest_path) {
            found = true;
            println!(
                "{:<32}  {:>8}  {:?}  agents={}",
                m.name,
                m.version,
                m.lifecycle,
                m.agents.len()
            );
        }
    }
    if !found {
        println!("(no sandboxes installed)");
    }
    Ok(())
}

fn cmd_status(target: &str) -> Result<()> {
    let install_root = paths::sandbox_dir(target)?;
    if !install_root.exists() {
        return Err(anyhow!("sandbox '{target}' is not installed"));
    }
    let manifest = Manifest::load(&install_root.join("manifest.yaml"))?;
    println!("sandbox: {}@{}", manifest.name, manifest.version);
    println!("install: {}", install_root.display());
    println!("agents:");
    let state = install_root.join(".state");
    for agent in &manifest.agents {
        let pid_file = state.join(format!("{}.pid", agent.id));
        let pid = std::fs::read_to_string(&pid_file).ok().map(|s| s.trim().to_string());
        println!(
            "  - {:<24}  model={:<32}  pid={}",
            agent.id,
            agent.model,
            pid.as_deref().unwrap_or("-")
        );
    }
    Ok(())
}

fn cmd_stop(target: &str) -> Result<()> {
    let install_root = paths::sandbox_dir(target)?;
    if !install_root.exists() {
        return Err(anyhow!("sandbox '{target}' is not installed"));
    }
    let manifest = Manifest::load(&install_root.join("manifest.yaml"))?;
    let state = install_root.join(".state");
    for agent in &manifest.agents {
        let pid_file = state.join(format!("{}.pid", agent.id));
        if let Ok(raw) = std::fs::read_to_string(&pid_file) {
            if let Ok(pid) = raw.trim().parse::<u32>() {
                kill_pid(pid);
                println!("stopped {} (pid {pid})", agent.id);
            }
        }
        let _ = std::fs::remove_file(pid_file);
    }
    Ok(())
}

fn cmd_publish(
    path: &std::path::Path,
    registry_flag: Option<&str>,
    sign: SignMode,
) -> Result<()> {
    if let Some(remote) = resolve_registry(registry_flag)? {
        if !remote.has_token() {
            return Err(anyhow!(
                "no auth token is configured for this registry. \
                 Mint one at <website>/me and run `dockagents login --token <token>` first."
            ));
        }
        let ack = remote.publish(path, sign)?;
        println!(
            "published {}@{}  sha256={}  ({} bytes)  signed={}  to {}",
            ack.name,
            ack.version,
            ack.sha256,
            ack.byte_length,
            ack.signed,
            registry_url_label(&remote)
        );
        return Ok(());
    }

    let manifest = Registry::publish(path)?;
    println!(
        "published {}@{} to {}",
        manifest.name,
        manifest.version,
        paths::registry_dir()?.join(&manifest.name).display()
    );
    Ok(())
}

fn cmd_keygen(force: bool) -> Result<()> {
    let key = signing::generate_keypair(force)?;
    println!("Generated publisher key.");
    println!("  private: {}", signing::private_key_path()?.display());
    println!("  public:  {}", signing::public_key_path()?.display());
    println!("  pubkey_b64: {}", key.public_key_b64);
    println!("  created_at: {}", key.created_at);
    println!();
    println!("Keep the private key secret — it identifies this publisher.");
    Ok(())
}

fn cmd_search(query: &str, registry_flag: Option<&str>) -> Result<()> {
    if let Some(remote) = resolve_registry(registry_flag)? {
        let resp = remote.search(query)?;
        if resp.results.is_empty() {
            println!("(no matches)");
            return Ok(());
        }
        for r in resp.results {
            println!(
                "{:<32} {:>8}  {}",
                r.name,
                r.latest,
                if r.description.is_empty() {
                    "(no description)"
                } else {
                    &r.description
                }
            );
        }
        return Ok(());
    }

    let hits = Registry::search(query)?;
    if hits.is_empty() {
        println!("(no matches)");
        return Ok(());
    }
    for m in hits {
        println!(
            "{:<32} {:>8}  {}",
            m.name,
            m.version,
            if m.description.is_empty() {
                "(no description)"
            } else {
                &m.description
            }
        );
    }
    Ok(())
}

fn cmd_pull(
    target: &str,
    registry_flag: Option<&str>,
    version: Option<&str>,
) -> Result<()> {
    cmd_install(target, registry_flag, version, None)
}

/// Parsed `--override-llm key=val,key=val` argument.
#[derive(Debug, Default, Clone)]
struct LlmOverride {
    provider: Option<String>,
    api_key_env: Option<String>,
    model: Option<String>,
    endpoint: Option<String>,
    api_version: Option<String>,
    max_tokens: Option<u32>,
}

impl LlmOverride {
    fn summary(&self) -> String {
        let mut parts = Vec::new();
        if let Some(p) = &self.provider {
            parts.push(format!("provider={p}"));
        }
        if let Some(k) = &self.api_key_env {
            parts.push(format!("api_key_env={k}"));
        }
        if let Some(m) = &self.model {
            parts.push(format!("model={m}"));
        }
        if let Some(e) = &self.endpoint {
            parts.push(format!("endpoint={e}"));
        }
        parts.join(", ")
    }
}

fn parse_llm_override(raw: &str) -> Result<LlmOverride> {
    let mut out = LlmOverride::default();
    for pair in raw.split(',') {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| anyhow!("--override-llm pair '{pair}' must be key=value"))?;
        let key = key.trim();
        let value = value.trim();
        match key {
            "provider" => out.provider = Some(value.to_string()),
            "api_key_env" => out.api_key_env = Some(value.to_string()),
            "model" => out.model = Some(value.to_string()),
            "endpoint" => out.endpoint = Some(value.to_string()),
            "api_version" => out.api_version = Some(value.to_string()),
            "max_tokens" => {
                out.max_tokens = Some(
                    value
                        .parse()
                        .with_context(|| format!("max_tokens must be a u32, got '{value}'"))?,
                )
            }
            other => {
                return Err(anyhow!(
                    "unknown --override-llm key '{other}' (recognized: provider, api_key_env, model, endpoint, api_version, max_tokens)"
                ))
            }
        }
    }
    Ok(out)
}

/// Rewrite every agent's `llm:` block in an installed manifest with the
/// override values. Round-trips through serde_yaml so unknown fields would be
/// dropped — that's acceptable here because the manifest schema is fully
/// covered by [`crate::manifest::Manifest`].
fn apply_llm_override(manifest_path: &std::path::Path, ovr: &LlmOverride) -> Result<()> {
    let mut manifest = Manifest::load(manifest_path)?;
    for agent in &mut manifest.agents {
        let mut llm = agent.llm.clone().unwrap_or_default();
        if let Some(p) = &ovr.provider {
            llm.provider = Some(p.clone());
        }
        if let Some(k) = &ovr.api_key_env {
            llm.api_key_env = Some(k.clone());
            // Drop any literal api_key — env var takes precedence and we want
            // to be explicit about where credentials come from.
            llm.api_key = None;
        }
        if let Some(e) = &ovr.endpoint {
            llm.endpoint = Some(e.clone());
        }
        if let Some(v) = &ovr.api_version {
            llm.api_version = Some(v.clone());
        }
        if let Some(t) = ovr.max_tokens {
            llm.max_tokens = Some(t);
        }
        agent.llm = Some(llm);
        if let Some(m) = &ovr.model {
            agent.model = m.clone();
        }
    }
    let yaml = serde_yaml::to_string(&manifest).context("serializing overridden manifest")?;
    std::fs::write(manifest_path, yaml)
        .with_context(|| format!("writing overridden manifest to {}", manifest_path.display()))?;
    Ok(())
}

fn cmd_config(action: ConfigCmd) -> Result<()> {
    use crate::config::{Config, DefaultLlm};
    match action {
        ConfigCmd::Path => {
            println!("{}", crate::config::config_path()?.display());
        }
        ConfigCmd::Show => {
            let cfg = Config::load()?;
            let yaml = serde_yaml::to_string(&cfg).context("serializing config")?;
            print!("{}", yaml);
        }
        ConfigCmd::SetDefaultLlm {
            provider,
            api_key_env,
            model,
            endpoint,
            api_version,
            max_tokens,
        } => {
            let mut cfg = Config::load()?;
            cfg.default_llm = Some(DefaultLlm {
                provider,
                api_key_env,
                model,
                endpoint,
                api_version,
                max_tokens,
                extra_headers: Default::default(),
            });
            cfg.save()?;
            println!("Wrote {}", crate::config::config_path()?.display());
            if let Some(def) = &cfg.default_llm {
                if std::env::var(&def.api_key_env).is_err() {
                    println!(
                        "note: env var `{}` is not set in this shell — export it before running a sandbox.",
                        def.api_key_env
                    );
                }
            }
        }
        ConfigCmd::ClearDefaultLlm => {
            let mut cfg = Config::load()?;
            cfg.default_llm = None;
            cfg.save()?;
            println!("Cleared default_llm");
        }
    }
    Ok(())
}

/// Resolve a registry from a `--registry` flag, falling back through
/// `DOCKAGENTS_REGISTRY_URL` and the configured default registry. The flag may
/// be either a URL or the alias of an entry under `registries:` in
/// `~/.dockagents/config.yaml`. Auth token is taken from
/// `DOCKAGENTS_REGISTRY_TOKEN` if set, else from `auth_tokens[<alias or url>]`
/// in the config (populated by `dockagents login`).
fn resolve_registry(flag: Option<&str>) -> Result<Option<RemoteRegistry>> {
    let env_token = std::env::var("DOCKAGENTS_REGISTRY_TOKEN").ok();
    let cfg = crate::config::Config::load()?;

    if let Some(f) = flag {
        let f = f.trim();
        if f.is_empty() {
            return Ok(None);
        }
        if looks_like_url(f) {
            let token = env_token.or_else(|| cfg.auth_tokens.get(f).cloned());
            return Ok(Some(RemoteRegistry::new(f.to_string(), token)));
        }
        if let Some(url) = cfg.registries.get(f) {
            let token = env_token
                .or_else(|| cfg.auth_tokens.get(f).cloned())
                .or_else(|| cfg.auth_tokens.get(url).cloned());
            return Ok(Some(RemoteRegistry::new(url.clone(), token)));
        }
        return Err(anyhow!(
            "no registry named '{f}' in {} — add it with `dockagents registry add {f} <url>`",
            crate::config::config_path()?.display()
        ));
    }

    // No flag: check env var first.
    if let Some(env_url) = std::env::var("DOCKAGENTS_REGISTRY_URL").ok().filter(|s| !s.trim().is_empty()) {
        let token = env_token
            .clone()
            .or_else(|| cfg.auth_tokens.get(&env_url).cloned());
        return Ok(Some(RemoteRegistry::new(env_url, token)));
    }

    if let Some(name) = &cfg.default_registry {
        match cfg.registries.get(name) {
            Some(url) => {
                let token = env_token
                    .or_else(|| cfg.auth_tokens.get(name).cloned())
                    .or_else(|| cfg.auth_tokens.get(url).cloned());
                return Ok(Some(RemoteRegistry::new(url.clone(), token)));
            }
            None => tracing::warn!(
                "default_registry '{name}' has no matching entry under registries: in config"
            ),
        }
    }

    Ok(None)
}

fn resolve_login_target(flag: Option<&str>) -> Result<String> {
    let cfg = crate::config::Config::load()?;
    if let Some(f) = flag {
        return Ok(f.trim().to_string());
    }
    if let Some(name) = cfg.default_registry {
        return Ok(name);
    }
    Err(anyhow!(
        "no --registry given and no default registry set. \
         Use `dockagents registry add <name> <url>` first, \
         or pass --registry <name-or-url>."
    ))
}

fn cmd_login(token: &str, registry_flag: Option<&str>) -> Result<()> {
    use crate::config::Config;
    if !token.starts_with("dgkp_") || token.len() < 20 {
        return Err(anyhow!(
            "that doesn't look like a DockAgents token (expected `dgkp_…`)"
        ));
    }
    let target = resolve_login_target(registry_flag)?;
    let mut cfg = Config::load()?;
    cfg.auth_tokens.insert(target.clone(), token.to_string());
    cfg.save()?;
    println!("Saved token for '{target}' to {}", crate::config::config_path()?.display());
    println!("`dockagents publish` will now authenticate automatically.");
    Ok(())
}

fn cmd_logout(registry_flag: Option<&str>) -> Result<()> {
    use crate::config::Config;
    let target = resolve_login_target(registry_flag)?;
    let mut cfg = Config::load()?;
    let removed = cfg.auth_tokens.remove(&target).is_some();
    cfg.save()?;
    if removed {
        println!("Removed token for '{target}'.");
    } else {
        println!("No token was stored for '{target}'.");
    }
    Ok(())
}

fn looks_like_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

fn cmd_registry(action: RegistryCmd) -> Result<()> {
    use crate::config::Config;
    match action {
        RegistryCmd::Add { name, url } => {
            let url = url.trim().trim_end_matches('/').to_string();
            if !looks_like_url(&url) {
                return Err(anyhow!(
                    "url must start with http:// or https:// (got '{url}')"
                ));
            }
            let mut cfg = Config::load()?;
            let prior = cfg.registries.insert(name.clone(), url.clone());
            if cfg.default_registry.is_none() {
                cfg.default_registry = Some(name.clone());
            }
            cfg.save()?;
            match prior {
                Some(old) => println!("Updated registry '{name}': {old} → {url}"),
                None => println!("Added registry '{name}' = {url}"),
            }
            if cfg.default_registry.as_deref() == Some(name.as_str()) {
                println!("Default registry → {name}");
            }
        }
        RegistryCmd::Remove { name } => {
            let mut cfg = Config::load()?;
            if cfg.registries.remove(&name).is_none() {
                return Err(anyhow!("no registry named '{name}'"));
            }
            if cfg.default_registry.as_deref() == Some(name.as_str()) {
                cfg.default_registry = None;
                println!("Removed '{name}' (was default — no default now)");
            } else {
                println!("Removed '{name}'");
            }
            cfg.save()?;
        }
        RegistryCmd::Use { name } => {
            let mut cfg = Config::load()?;
            if !cfg.registries.contains_key(&name) {
                return Err(anyhow!(
                    "no registry named '{name}' — add it first with `dockagents registry add {name} <url>`"
                ));
            }
            cfg.default_registry = Some(name.clone());
            cfg.save()?;
            println!("Default registry → {name}");
        }
        RegistryCmd::List => {
            let cfg = Config::load()?;
            if cfg.registries.is_empty() {
                println!("(no registries configured)");
                println!();
                println!("Try:  dockagents registry add local http://localhost:8787");
                return Ok(());
            }
            let default = cfg.default_registry.as_deref();
            // Stable alphabetical order for readability.
            let mut names: Vec<&String> = cfg.registries.keys().collect();
            names.sort();
            for name in names {
                let url = &cfg.registries[name];
                let marker = if Some(name.as_str()) == default { "*" } else { " " };
                println!("{marker} {name:<16} {url}");
            }
            if let Ok(env_url) = std::env::var("DOCKAGENTS_REGISTRY_URL") {
                println!();
                println!(
                    "note: DOCKAGENTS_REGISTRY_URL is set to {env_url} — it overrides the default for this shell."
                );
            }
        }
        RegistryCmd::ClearDefault => {
            let mut cfg = Config::load()?;
            cfg.default_registry = None;
            cfg.save()?;
            println!("Cleared default registry");
        }
    }
    Ok(())
}

fn cmd_manifest(target: &str) -> Result<()> {
    let install_root = paths::sandbox_dir(target)?;
    let manifest_path = install_root.join("manifest.yaml");
    if !manifest_path.exists() {
        return Err(anyhow!("sandbox '{target}' is not installed"));
    }
    let raw = std::fs::read_to_string(&manifest_path)?;
    print!("{}", raw);
    Ok(())
}

#[cfg(unix)]
fn kill_pid(pid: u32) {
    use std::process::Command;
    let _ = Command::new("kill").arg(pid.to_string()).status();
}

#[cfg(windows)]
fn kill_pid(pid: u32) {
    use std::process::Command;
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .status();
}

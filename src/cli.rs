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
    version = crate::updater::CURRENT_VERSION,
    about = "DockAgents — distributable multi-agent sandboxes",
    propagate_version = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Scaffold a new sandbox in the current directory. Local-only —
    /// no registry account required. The generated sandbox is immediately
    /// runnable with `dockagents run ./<name>`.
    Init {
        /// Sandbox name. A directory with this name is created in the
        /// current working directory.
        name: String,
        /// Description for the manifest. Optional; you can edit later.
        #[arg(long)]
        description: Option<String>,
        /// LLM provider for the generated agent. Defaults to `anthropic`.
        #[arg(long, default_value = "anthropic")]
        provider: String,
        /// Model string passed to the provider. Defaults to a sensible model
        /// for the chosen provider.
        #[arg(long)]
        model: Option<String>,
        /// Overwrite an existing directory with the same name.
        #[arg(long)]
        force: bool,
    },
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
    /// Internal: regenerate the local Ed25519 signing key. Hidden because
    /// `dockagents login` already does this transparently the first time it
    /// runs on a new machine. Pass `--force` to rotate.
    #[command(hide = true)]
    Keygen {
        /// Overwrite an existing key.
        #[arg(long)]
        force: bool,
    },
    /// Internal: print the publisher key file content. Hidden — `dockagents
    /// login` already attaches the key to your account automatically.
    #[command(hide = true)]
    Pubkey {
        #[arg(long)]
        quiet: bool,
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
    /// Authenticate the CLI with dockagents.net. Opens a browser to approve
    /// the link; on approval the website returns an API token and auto-claims
    /// this machine's Ed25519 publisher key so the sandboxes you publish are
    /// attributed to your account.
    Login {
        /// Headless override: paste a token minted at https://dockagents.net/me
        /// instead of running the browser flow. Useful for CI.
        #[arg(long)]
        token: Option<String>,
        /// Registry alias or URL this token is for. Defaults to the current
        /// default registry.
        #[arg(long)]
        registry: Option<String>,
        /// Override the website used for the browser flow. Defaults to
        /// `DOCKAGENTS_WEBSITE_URL` env var, then `https://dockagents.net`.
        #[arg(long)]
        website: Option<String>,
        /// Don't try to open a browser; just print the URL to visit.
        #[arg(long)]
        no_browser: bool,
    },
    /// Forget the saved auth token for a registry.
    Logout {
        /// Registry alias or URL to forget. Defaults to the current default registry.
        #[arg(long)]
        registry: Option<String>,
    },
    /// Check GitHub Releases for a DockAgents update and install it.
    Update {
        /// Only check whether an update is available; do not install it.
        #[arg(long)]
        check: bool,
        /// Install without prompting.
        #[arg(long)]
        yes: bool,
        /// GitHub repository to use, as owner/repo or a github.com URL.
        #[arg(long)]
        repo: Option<String>,
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
    /// Configure GitHub update checks and optional automatic installs.
    SetUpdates {
        /// Enable or disable daily GitHub update checks.
        #[arg(long, value_name = "BOOL")]
        check: Option<bool>,
        /// Enable or disable automatic installs when an update is found.
        #[arg(long, value_name = "BOOL")]
        auto_install: Option<bool>,
        /// GitHub repository to check, as owner/repo or a github.com URL.
        #[arg(long)]
        github_repo: Option<String>,
    },
}

pub fn dispatch(cli: Cli) -> Result<ExitCode> {
    paths::ensure_layout()?;

    if should_check_for_updates(&cli.cmd) {
        crate::updater::maybe_notify_or_auto_update();
    }

    match cli.cmd {
        Cmd::Agent => match crate::agent::run() {
            Ok(()) => Ok(ExitCode::from(0)),
            Err(e) => {
                eprintln!("agent error: {e:#}");
                Ok(ExitCode::from(1))
            }
        },
        Cmd::Init {
            name,
            description,
            provider,
            model,
            force,
        } => cmd_init(&name, description.as_deref(), &provider, model.as_deref(), force)
            .map(|_| ExitCode::from(0)),
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
        Cmd::Login {
            token,
            registry,
            website,
            no_browser,
        } => cmd_login(
            token.as_deref(),
            registry.as_deref(),
            website.as_deref(),
            no_browser,
        )
        .map(|_| ExitCode::from(0)),
        Cmd::Logout { registry } => cmd_logout(registry.as_deref()).map(|_| ExitCode::from(0)),
        Cmd::Update { check, yes, repo } => {
            cmd_update(check, yes, repo.as_deref()).map(|_| ExitCode::from(0))
        }
        Cmd::Keygen { force } => cmd_keygen(force).map(|_| ExitCode::from(0)),
        Cmd::Pubkey { quiet } => cmd_pubkey(quiet).map(|_| ExitCode::from(0)),
        Cmd::Mcp => crate::mcp::run().map(|_| ExitCode::from(0)),
        Cmd::Serve { port, host } => crate::api::serve(&host, port).map(|_| ExitCode::from(0)),
        Cmd::Watch { target, debounce } => {
            crate::watcher::watch(&target, &debounce).map(|_| ExitCode::from(0))
        }
    }
}

fn should_check_for_updates(cmd: &Cmd) -> bool {
    !matches!(
        cmd,
        Cmd::Agent | Cmd::Mcp | Cmd::Update { .. } | Cmd::Config {
            action: ConfigCmd::SetUpdates { .. }
        }
    )
}

/// Scaffold a new sandbox directory the user can run locally. The generated
/// sandbox has a single agent (`reviewer`), so `dockagents run` works
/// against any text or directory input without further edits. Users can
/// extend or replace any of the generated files.
fn cmd_init(
    name: &str,
    description: Option<&str>,
    provider: &str,
    model: Option<&str>,
    force: bool,
) -> Result<()> {
    // Validate the name — has to be a usable directory name AND a valid
    // sandbox name per the manifest schema.
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("sandbox name cannot be empty"));
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(anyhow!(
            "sandbox name must contain only letters, digits, hyphens, or underscores"
        ));
    }
    if !trimmed.chars().next().unwrap().is_ascii_alphanumeric() {
        return Err(anyhow!(
            "sandbox name must start with a letter or digit"
        ));
    }

    let provider_norm = match provider.to_ascii_lowercase().as_str() {
        "anthropic" | "openai" | "openai-compatible" => provider.to_ascii_lowercase(),
        other => {
            return Err(anyhow!(
                "unknown provider '{other}' — use anthropic, openai, or openai-compatible"
            ))
        }
    };
    let default_model: &str = match provider_norm.as_str() {
        "anthropic" => "claude-sonnet-4-6",
        "openai" => "gpt-4o-mini",
        _ => "gpt-4o-mini",
    };
    let model_to_use = model.unwrap_or(default_model);
    let api_key_env = match provider_norm.as_str() {
        "anthropic" => "ANTHROPIC_API_KEY",
        _ => "OPENAI_API_KEY",
    };
    let desc = description.unwrap_or("One-agent code reviewer. Reads input, writes a Markdown report.");

    let root = std::path::PathBuf::from(trimmed);
    if root.exists() {
        if !force {
            return Err(anyhow!(
                "directory `{}` already exists. Pass --force to overwrite, or pick a different name.",
                root.display()
            ));
        }
        std::fs::remove_dir_all(&root)
            .with_context(|| format!("removing existing {}", root.display()))?;
    }
    std::fs::create_dir_all(root.join("skills"))
        .with_context(|| format!("creating {}", root.join("skills").display()))?;

    let manifest = format!(
        r#"name:        {name}
version:     0.1.0
description: {desc}

# `persistent` keeps the agent process warm between runs (faster on the
# second invocation). Switch to `ephemeral` to spin everything down between
# runs at the cost of a slightly slower start.
lifecycle: persistent

execution:
  mode:    sync       # sync = one input → one output; async = streaming
  timeout: 180s
  input:
    - type: text
    - type: directory
      accepts: [py, ts, rs, go, js, java, md, txt]
  output:
    - type: text

agents:
  - id:        reviewer
    model:     {model_to_use}
    skill:     ./skills/reviewer.md
    workspace: ./workspaces/reviewer/
    llm:
      provider:    {provider_norm}
      api_key_env: {api_key_env}
      max_tokens:  2048

capabilities:
  invoke:  []        # IDs of other sandboxes this one is allowed to call
  network: false     # set true if the agent needs the internet at runtime
"#,
        name = trimmed,
        desc = desc,
        model_to_use = model_to_use,
        provider_norm = provider_norm,
        api_key_env = api_key_env,
    );
    std::fs::write(root.join("manifest.yaml"), manifest)
        .context("writing manifest.yaml")?;

    let skill = r#"# Reviewer

You are a senior software engineer doing a focused code review.

For the input under `=== INPUT ===`, produce a Markdown report with:

1. **Summary** — one paragraph on what the code does.
2. **Strengths** — bullet list, be specific.
3. **Issues** — bullet list, ranked by severity. Cite file paths when known.
   Distinguish bugs from style/structure feedback.
4. **Suggestions** — concrete, actionable. Prefer code snippets over prose.

Constraints:
- Do not invent files or symbols that aren't in the input.
- If the input is empty or unrelated to code, say so plainly and stop.
- Keep the report under 600 words.
"#;
    std::fs::write(root.join("skills").join("reviewer.md"), skill)
        .context("writing skills/reviewer.md")?;

    let readme = format!(
        r#"# {trimmed}

{desc}

## Run it locally

This sandbox doesn't need to be published anywhere. Run it directly from this
directory:

```sh
# inline text
dockagents run . --text "function divide(a, b) {{ return a / b; }}"

# a file
dockagents run . --input ./some-file.js --output ./report

# a whole directory
dockagents run . --input ./src/ --output ./report
```

Set the API key the manifest declares before running:

```sh
export {api_key_env}=...
```

## Install into ~/.dockagents/

So you can run it as `dockagents run {trimmed}` from anywhere:

```sh
dockagents install .
dockagents run {trimmed} --input ./src/
```

## Publish (optional)

If you want others to install your sandbox by name:

```sh
dockagents login              # link this CLI to your dockagents.net account
dockagents publish .          # uploads + signs
```
"#,
        trimmed = trimmed,
        desc = desc,
        api_key_env = api_key_env,
    );
    std::fs::write(root.join("README.md"), readme).context("writing README.md")?;

    let gitignore = "workspaces/\noutput/\n.dockagents/\n";
    std::fs::write(root.join(".gitignore"), gitignore).context("writing .gitignore")?;

    println!("✓ Created {}/", root.display());
    println!("    manifest.yaml");
    println!("    skills/reviewer.md");
    println!("    README.md");
    println!("    .gitignore");
    println!();
    println!("Next steps:");
    println!("  cd {trimmed}");
    println!("  export {api_key_env}=...               # set your LLM key");
    println!("  dockagents run . --text \"hello\"      # try it locally");
    println!();
    println!("When you're ready to share it:");
    println!("  dockagents login                       # one-time, opens browser");
    println!("  dockagents publish .                   # uploads to {DEFAULT_REGISTRY_URL}");
    Ok(())
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
                 Mint one at https://dockagents.net/me and run `dockagents login --token <token>` first."
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

fn cmd_pubkey(quiet: bool) -> Result<()> {
    let pub_path = signing::public_key_path()?;
    if !pub_path.exists() {
        return Err(anyhow!(
            "no publisher key at {} — run `dockagents keygen` first",
            pub_path.display()
        ));
    }
    let raw = std::fs::read_to_string(&pub_path)
        .with_context(|| format!("reading {}", pub_path.display()))?;
    let pubkey_b64 = raw.trim();
    if pubkey_b64.is_empty() {
        return Err(anyhow!(
            "publisher key at {} is empty — regenerate with `dockagents keygen --force`",
            pub_path.display()
        ));
    }
    if quiet {
        println!("{pubkey_b64}");
    } else {
        println!("pubkey_b64: {pubkey_b64}");
        println!("file:       {}", pub_path.display());
        println!();
        println!("Claim it on https://dockagents.net/me so packages you publish are attributed to you.");
    }
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
        ConfigCmd::SetUpdates {
            check,
            auto_install,
            github_repo,
        } => {
            let mut cfg = Config::load()?;
            if let Some(check) = check {
                cfg.updates.check = check;
            }
            if let Some(auto_install) = auto_install {
                cfg.updates.auto_install = auto_install;
            }
            if let Some(repo) = github_repo {
                cfg.updates.github_repo = crate::updater::normalize_repo(&repo)?;
            }
            cfg.save()?;
            println!("Wrote {}", crate::config::config_path()?.display());
            println!(
                "updates: check={} auto_install={} github_repo={}",
                cfg.updates.check, cfg.updates.auto_install, cfg.updates.github_repo
            );
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
/// The hosted registry every CLI talks to out of the box. Users who want a
/// different one can override per-command with `--registry`, by setting
/// `DOCKAGENTS_REGISTRY_URL`, or by adding a named registry with
/// `dockagents registry add <name> <url>` + `dockagents registry use <name>`.
pub const DEFAULT_REGISTRY_URL: &str = "https://registry.dockagents.net";

fn resolve_registry(flag: Option<&str>) -> Result<Option<RemoteRegistry>> {
    let cfg = crate::config::Config::load()?;

    if let Some(f) = flag {
        let f = f.trim();
        if f.is_empty() {
            return Ok(None);
        }
        if looks_like_url(f) {
            // Flag is a URL. Find the alias that maps to it (if any) so we can
            // pick up a token stored under that alias.
            let alias = cfg.registries.iter().find_map(|(k, v)| {
                if normalize_url(v) == normalize_url(f) { Some(k.as_str()) } else { None }
            });
            let token = find_token(&cfg, alias, f);
            return Ok(Some(RemoteRegistry::new(f.to_string(), token)));
        }
        if let Some(url) = cfg.registries.get(f) {
            let token = find_token(&cfg, Some(f), url);
            return Ok(Some(RemoteRegistry::new(url.clone(), token)));
        }
        return Err(anyhow!(
            "no registry named '{f}' in {} — add it with `dockagents registry add {f} <url>`",
            crate::config::config_path()?.display()
        ));
    }

    // No flag: check env var first.
    if let Some(env_url) = std::env::var("DOCKAGENTS_REGISTRY_URL").ok().filter(|s| !s.trim().is_empty()) {
        let alias = cfg.registries.iter().find_map(|(k, v)| {
            if normalize_url(v) == normalize_url(&env_url) { Some(k.as_str()) } else { None }
        });
        let token = find_token(&cfg, alias, &env_url);
        return Ok(Some(RemoteRegistry::new(env_url, token)));
    }

    if let Some(name) = &cfg.default_registry {
        match cfg.registries.get(name) {
            Some(url) => {
                let token = find_token(&cfg, Some(name), url);
                return Ok(Some(RemoteRegistry::new(url.clone(), token)));
            }
            None => tracing::warn!(
                "default_registry '{name}' has no matching entry under registries: in config"
            ),
        }
    }

    // Final fallback: hit the hosted dockagents.net registry. A token, if the
    // user has logged in there, comes through find_token by URL lookup.
    let token = find_token(&cfg, None, DEFAULT_REGISTRY_URL);
    Ok(Some(RemoteRegistry::new(
        DEFAULT_REGISTRY_URL.to_string(),
        token,
    )))
}

fn normalize_url(s: &str) -> String {
    s.trim().trim_end_matches('/').to_lowercase()
}

/// Locate an auth token for a registry. Tries, in order:
///   1. `DOCKAGENTS_REGISTRY_TOKEN` env var (ignored if empty/whitespace).
///   2. `auth_tokens[<alias>]` when an alias is known.
///   3. `auth_tokens[<url>]`.
///   4. `auth_tokens[<any-alias-mapping-to-this-url>]` — catches the case
///      where the user logged in via one form (alias or URL) and is now
///      publishing via the other.
fn find_token(cfg: &crate::config::Config, alias: Option<&str>, url: &str) -> Option<String> {
    let non_empty = |s: String| -> Option<String> {
        if s.trim().is_empty() { None } else { Some(s) }
    };

    if let Some(t) = std::env::var("DOCKAGENTS_REGISTRY_TOKEN").ok().and_then(non_empty) {
        return Some(t);
    }
    if let Some(a) = alias {
        if let Some(t) = cfg.auth_tokens.get(a).cloned().and_then(non_empty) {
            return Some(t);
        }
    }
    if let Some(t) = cfg.auth_tokens.get(url).cloned().and_then(non_empty) {
        return Some(t);
    }
    let norm = normalize_url(url);
    for (k, v) in &cfg.registries {
        if normalize_url(v) == norm {
            if let Some(t) = cfg.auth_tokens.get(k).cloned().and_then(non_empty) {
                return Some(t);
            }
        }
    }
    None
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

fn cmd_login(
    token: Option<&str>,
    registry_flag: Option<&str>,
    website_flag: Option<&str>,
    no_browser: bool,
) -> Result<()> {
    // Headless / scripted: --token bypasses the browser flow entirely.
    if let Some(tok) = token {
        return save_token(tok, registry_flag);
    }

    // Browser flow.
    // 1. Resolve which website to talk to.
    let website = website_flag
        .map(str::to_string)
        .or_else(|| std::env::var("DOCKAGENTS_WEBSITE_URL").ok())
        .unwrap_or_else(|| "https://dockagents.net".to_string());
    let website = website.trim_end_matches('/').to_string();

    // 2. Make sure we have a publisher public key. If not, mint one — the
    //    whole point of this flow is to claim that key for the user's account.
    let pub_path = signing::public_key_path()?;
    if !pub_path.exists() {
        tracing::info!("no publisher key yet — generating one");
        let _ = signing::generate_keypair(false)?;
    }
    let pubkey_b64 = std::fs::read_to_string(&pub_path)
        .with_context(|| format!("reading {}", pub_path.display()))?
        .trim()
        .to_string();
    if pubkey_b64.is_empty() {
        return Err(anyhow!(
            "publisher key at {} is empty — regenerate with `dockagents keygen --force`",
            pub_path.display()
        ));
    }

    // 3. Init a CLI-login session on the website.
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(15))
        .build();
    let init_url = format!("{website}/api/cli/login/init");
    let init: serde_json::Value = agent
        .post(&init_url)
        .send_json(serde_json::json!({ "pubkey_b64": pubkey_b64 }))
        .map_err(|e| anyhow!("cannot reach {website}: {e}"))?
        .into_json()
        .context("parsing init response")?;
    let session_id = init
        .get("session")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("server did not return a session id"))?
        .to_string();
    let verify_url = init
        .get("verify_url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("server did not return a verify_url"))?
        .to_string();

    // 4. Open the browser (best-effort) and print the URL.
    println!("Opening {verify_url} to approve this CLI…");
    println!();
    println!("If the browser doesn't open automatically, copy this URL:");
    println!("    {verify_url}");
    println!();
    if !no_browser {
        let _ = open_browser(&verify_url);
    }

    // 5. Poll for completion.
    let poll_url = format!("{website}/api/cli/login/{session_id}/poll");
    let started = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(10 * 60);
    let mut last_status = "pending".to_string();
    println!("Waiting for approval…");
    loop {
        if started.elapsed() > timeout {
            return Err(anyhow!(
                "timed out after 10 minutes — re-run `dockagents login` when ready"
            ));
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
        let resp = match agent.get(&poll_url).call() {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("poll failed (will retry): {e}");
                continue;
            }
        };
        let body: serde_json::Value = match resp.into_json() {
            Ok(v) => v,
            Err(_) => continue,
        };
        let status = body
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("pending")
            .to_string();
        if status == "approved" {
            let token = body
                .get("token")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("approved but no token returned"))?
                .to_string();
            let username = body
                .get("username")
                .and_then(|v| v.as_str())
                .unwrap_or("(unknown)");
            save_token(&token, registry_flag)?;
            println!();
            println!("✓ Signed in as @{username}");
            println!("  Publisher key linked to your account.");
            println!("  `dockagents publish` will now authenticate automatically.");
            return Ok(());
        }
        if status == "expired" {
            return Err(anyhow!("session expired — re-run `dockagents login`"));
        }
        if status != last_status {
            last_status = status;
        }
    }
}

fn save_token(token: &str, registry_flag: Option<&str>) -> Result<()> {
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
    println!(
        "Saved token for '{target}' to {}",
        crate::config::config_path()?.display()
    );
    Ok(())
}

#[cfg(target_os = "windows")]
fn open_browser(url: &str) -> Result<()> {
    std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn()
        .map(|_| ())
        .map_err(|e| anyhow!("could not open browser: {e}"))
}

#[cfg(target_os = "macos")]
fn open_browser(url: &str) -> Result<()> {
    std::process::Command::new("open")
        .arg(url)
        .spawn()
        .map(|_| ())
        .map_err(|e| anyhow!("could not open browser: {e}"))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn open_browser(url: &str) -> Result<()> {
    std::process::Command::new("xdg-open")
        .arg(url)
        .spawn()
        .map(|_| ())
        .map_err(|e| anyhow!("could not open browser: {e}"))
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

fn cmd_update(check_only: bool, yes: bool, repo_flag: Option<&str>) -> Result<()> {
    let cfg = crate::config::Config::load()?;
    let repo = match repo_flag {
        Some(repo) => crate::updater::normalize_repo(repo)?,
        None => cfg.updates.github_repo.clone(),
    };
    let check = crate::updater::check_latest(&repo)?;
    let Some(info) = check.update else {
        println!(
            "dockagents is up to date ({}; latest GitHub release: {}).",
            crate::updater::CURRENT_VERSION,
            check.latest_tag
        );
        return Ok(());
    };

    println!(
        "dockagents update available: {} -> {}",
        info.current_version, info.latest_tag
    );
    println!("release: {}", info.release_url);
    println!("asset:   {}", info.asset_name);

    if check_only {
        return Ok(());
    }

    if !crate::updater::prompt_install(&info, yes)? {
        println!("Update skipped.");
        return Ok(());
    }

    match crate::updater::install_release(&info)? {
        crate::updater::InstallResult::Replaced { path } => {
            println!("Updated dockagents at {}", path.display());
        }
        crate::updater::InstallResult::Deferred { path } => {
            println!(
                "Update downloaded. Windows will replace {} after this process exits.",
                path.display()
            );
        }
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
                println!("(no registries configured — using the built-in default)");
                println!("  default          {DEFAULT_REGISTRY_URL}");
                println!();
                println!(
                    "Add another with:  dockagents registry add <name> <url>"
                );
                if let Some(env_url) = std::env::var("DOCKAGENTS_REGISTRY_URL")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
                {
                    println!();
                    println!(
                        "note: DOCKAGENTS_REGISTRY_URL is set to {env_url} — it overrides the built-in default in this shell."
                    );
                }
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
            if default.is_none() {
                println!();
                println!(
                    "no alias is set as default — falling back to {DEFAULT_REGISTRY_URL}"
                );
            }
            if let Some(env_url) = std::env::var("DOCKAGENTS_REGISTRY_URL")
                .ok()
                .filter(|s| !s.trim().is_empty())
            {
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
